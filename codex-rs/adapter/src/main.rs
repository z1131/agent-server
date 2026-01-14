use tonic::{transport::Server, Request, Response, Status};
use tokio::process::Command;
use tokio::io::{AsyncBufReadExt, BufReader, AsyncWriteExt};
use tokio_stream::wrappers::ReceiverStream;
use std::process::Stdio;
use std::path::{Path, PathBuf};
use tempfile::TempDir;
use tracing::{info, error};
use chrono::Datelike;

pub mod agent {
    tonic::include_proto!("codex.agent");
}

use agent::agent_service_server::{AgentService, AgentServiceServer};
use agent::{RunTaskRequest, RunTaskResponse, run_task_response::Event, SessionConfig, WireApi, SandboxPolicy};

#[derive(Debug, Default)]
pub struct MyAgentService;

#[tonic::async_trait]
impl AgentService for MyAgentService {
    type RunTaskStream = ReceiverStream<Result<RunTaskResponse, Status>>;

    async fn run_task(&self, request: Request<RunTaskRequest>) -> Result<Response<Self::RunTaskStream>, Status> {
        let req = request.into_inner();
        let (tx, rx) = tokio::sync::mpsc::channel(100);

        tokio::spawn(async move {
            if let Err(e) = handle_run(req, tx.clone()).await {
                error!("Task failed: {:?}", e);
                let _ = tx.send(Ok(RunTaskResponse {
                    event: Some(Event::Error(format!("Agent error: {}", e))),
                })).await;
            }
        });

        Ok(Response::new(ReceiverStream::new(rx)))
    }
}

async fn handle_run(mut req: RunTaskRequest, tx: tokio::sync::mpsc::Sender<Result<RunTaskResponse, Status>>) -> anyhow::Result<()> {
    // 1. 准备隔离的工作环境
    let temp_dir = TempDir::new()?;
    let codex_home = temp_dir.path();
    let work_dir = if !req.base_dir.is_empty() {
        Path::new(&req.base_dir).to_path_buf()
    } else {
        codex_home.join("workspace")
    };
    tokio::fs::create_dir_all(&work_dir).await?;

    // 2. 灵魂复活逻辑 (State Revival)
    let is_resuming = !req.history_rollout.is_empty();
    if is_resuming {
        let now = chrono::Utc::now();
        let session_path = codex_home.join(format!("sessions/{}/{:02}/{:02}", now.year(), now.month(), now.day()));
        tokio::fs::create_dir_all(&session_path).await?;
        let history_file = session_path.join(format!("rollout-{}.jsonl", req.session_id));
        tokio::fs::write(&history_file, &req.history_rollout).await?;
        info!(session_id = %req.session_id, "Revived session state");
    }

    // 3. 动态配置注入
    if let Some(config) = &mut req.session_config {
        if let Some(prov) = &mut config.provider_info {
            if let Some(key) = &prov.env_key {
                if let Some(val) = req.env_vars.get(key) {
                    prov.experimental_bearer_token = Some(val.clone());
                }
            }
        }
        tokio::fs::write(codex_home.join("config.toml"), generate_config_toml(config)?).await?;
    }

    // 4. 注入上下文文件
    for file in &req.context_files {
        if file.path.contains("..") || file.path.starts_with("/") { continue; }
        let path = work_dir.join(&file.path);
        if let Some(parent) = path.parent() { tokio::fs::create_dir_all(parent).await?; }
        tokio::fs::write(&path, &file.content).await?;
    }

    // 5. 构建并启动 Codex 子进程
    let mut cmd = build_codex_command(&req, codex_home, &work_dir);
    let mut child = cmd.spawn()?;

    // 注入 Prompt
    if let Some(mut stdin) = child.stdin.take() {
        let full_prompt = build_full_prompt(&req.prompt, req.session_config.as_ref());
        stdin.write_all(full_prompt.as_bytes()).await?;
        drop(stdin);
    }

    // 6. 实时流处理与灵魂提取
    process_streams(child, tx, codex_home, &req.session_id).await?;

    Ok(())
}

fn build_codex_command(req: &RunTaskRequest, codex_home: &Path, work_dir: &Path) -> Command {
    let mut cmd = Command::new("codex");
    
    // 配置全局覆盖参数 (必须在子命令前)
    if let Some(config) = &req.session_config {
        if !config.model.is_empty() {
            cmd.arg("-c").arg(format!("model={}", config.model));
        }
        if !config.model_provider.is_empty() {
            cmd.arg("-c").arg(format!("model_provider={}", config.model_provider));
        }
    }

    cmd.arg("exec").arg("--json").arg("--skip-git-repo-check").arg("--dangerously-bypass-approvals-and-sandbox");

    if let Some(config) = &req.session_config {
        match SandboxPolicy::try_from(config.sandbox_policy).unwrap_or(SandboxPolicy::Unspecified) {
            SandboxPolicy::WorkspaceWrite => { cmd.arg("--sandbox").arg("workspace-write"); },
            SandboxPolicy::ReadOnly => { cmd.arg("--sandbox").arg("read-only"); },
            SandboxPolicy::DangerFullAccess => { cmd.arg("--sandbox").arg("danger-full-access"); },
            _ => {}
        }
    }

    if !req.history_rollout.is_empty() {
        cmd.arg("resume").arg(&req.session_id);
    }

    cmd.arg("-")
       .current_dir(work_dir)
       .env("CODEX_HOME", codex_home)
       .env("RUST_LOG", "info")
       .envs(&req.env_vars)
       .stdin(Stdio::piped())
       .stdout(Stdio::piped())
       .stderr(Stdio::piped());
    
    cmd
}

async fn process_streams(mut child: tokio::process::Child, tx: tokio::sync::mpsc::Sender<Result<RunTaskResponse, Status>>, codex_home: &Path, session_id: &str) -> anyhow::Result<()> {
    let stdout = child.stdout.take().unwrap();
    let stderr = child.stderr.take().unwrap();
    
    let mut out_reader = BufReader::new(stdout).lines();
    let mut err_reader = BufReader::new(stderr).lines();

    // 异步转发 STDERR 日志
    let tx_err = tx.clone();
    tokio::spawn(async move {
        while let Ok(Some(line)) = err_reader.next_line().await {
             let _ = tx_err.send(Ok(RunTaskResponse {
                event: Some(agent::run_task_response::Event::AdapterLog(format!("[STDERR] {}", line)))
            })).await;
        }
    });

    // 主循环：转发 STDOUT 中的 JSON 事件
    while let Ok(Some(line)) = out_reader.next_line().await {
        if tx.send(Ok(RunTaskResponse {
            event: Some(agent::run_task_response::Event::CodexEventJson(line))
        })).await.is_err() {
            let _ = child.kill().await;
            return Ok(());
        }
    }

    // 等待子进程退出并提取最终“灵魂”
    let status = child.wait().await?;
    if status.success() {
        if let Some(data) = extract_updated_rollout(codex_home, session_id).await? {
            info!(bytes = data.len(), "Captured updated session rollout");
            let _ = tx.send(Ok(RunTaskResponse {
                event: Some(agent::run_task_response::Event::UpdatedRollout(data))
            })).await;
        }
    }
    Ok(())
}

async fn extract_updated_rollout(home: &Path, _id: &str) -> anyhow::Result<Option<Vec<u8>>> {
    let root = home.join("sessions");
    if !root.exists() { return Ok(None); }
    let mut latest: Option<(std::time::SystemTime, PathBuf)> = None;
    
    // 递归寻找最新的 .jsonl 文件，不再校验 ID
    fn walk(dir: &Path, latest: &mut Option<(std::time::SystemTime, PathBuf)>) -> anyhow::Result<()> {
        if !dir.is_dir() { return Ok(()); }
        for entry in std::fs::read_dir(dir)? {
            let entry = entry?;
            let path = entry.path();
            if path.is_dir() { 
                walk(&path, latest)?; 
            } else if path.extension().map_or(false, |ext| ext == "jsonl") {
                let mtime = entry.metadata()?.modified()?;
                if latest.as_ref().map_or(true, |(t, _)| mtime > *t) {
                    *latest = Some((mtime, path));
                }
            }
        }
        Ok(())
    }
    
    walk(&root, &mut latest)?;
    if let Some((_, p)) = latest { 
        info!(path = %p.display(), "Extracted latest rollout file");
        Ok(Some(tokio::fs::read(p).await?)) 
    } else { 
        Ok(None) 
    }
}

fn build_full_prompt(prompt: &str, config: Option<&SessionConfig>) -> String {
    let mut p = Vec::new();
    if let Some(c) = config {
        if let Some(s) = &c.instructions { p.push(s.clone()); }
        if let Some(d) = &c.developer_instructions { p.push(d.clone()); }
    }
    p.push(prompt.to_string());
    p.join("\n\n")
}

fn generate_config_toml(config: &SessionConfig) -> anyhow::Result<String> {
    let mut toml = String::new();
    toml.push_str("model_auto_compact_token_limit = 100000\n[history]\npersistence = \"save-all\"\n\n");
    if !config.model.is_empty() { toml.push_str(&format!("model = {:?}\n", config.model)); }
    if !config.model_provider.is_empty() { toml.push_str(&format!("model_provider = {:?}\n", config.model_provider)); }
    if let Some(instr) = &config.instructions { toml.push_str(&format!("instructions = {:?}\n", instr)); }
    if let Some(dev) = &config.developer_instructions { toml.push_str(&format!("developer_instructions = {:?}\n", dev)); }
    
    if let Some(provider) = &config.provider_info {
        toml.push_str(&format!("\n[model_providers.{}]\nname = {:?}\n", provider.name, provider.name));
        if let Some(url) = &provider.base_url { toml.push_str(&format!("base_url = {:?}\n", url)); }
        let wire = match WireApi::try_from(provider.wire_api).unwrap_or(WireApi::Chat) {
            WireApi::Chat => "chat",
            WireApi::Responses => "responses",
            WireApi::ResponsesWebsocket => "responses_websocket",
        };
        toml.push_str(&format!("wire_api = {:?}\n", wire));
        if let Some(tok) = &provider.experimental_bearer_token {
            toml.push_str(&format!("experimental_bearer_token = {:?}\n", tok));
        } else if let Some(key) = &provider.env_key {
            toml.push_str(&format!("env_key = {:?}\n", key));
        }
        toml.push_str(&format!("requires_openai_auth = {}\n", provider.requires_openai_auth));
    }
    
    if !config.mcp_servers.is_empty() {
        for (name, def) in &config.mcp_servers {
            toml.push_str(&format!("\n[mcp_servers.{}]\ntype = {:?}\n", name, def.server_type));
            if !def.command.is_empty() { toml.push_str(&format!("command = {:?}\n", def.command)); }
            if !def.url.is_empty() { toml.push_str(&format!("url = {:?}\n", def.url)); }
        }
    }
    Ok(toml)
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    tracing_subscriber::fmt().with_env_filter("info").init();
    let addr = "0.0.0.0:50051".parse()?;
    let adapter = MyAgentService::default();
    info!("Codex Agent Service listening on {}", addr);
    Server::builder().add_service(AgentServiceServer::new(adapter)).serve(addr).await?;
    Ok(())
}