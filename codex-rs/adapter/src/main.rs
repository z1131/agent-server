use tonic::{transport::Server, Request, Response, Status};
use tokio::process::Command;
use tokio::io::{AsyncBufReadExt, BufReader, AsyncWriteExt};
use tokio_stream::wrappers::ReceiverStream;
use std::process::Stdio;
use std::path::Path;
use tempfile::TempDir;
use tracing::{info, warn, error};

pub mod agent {
    tonic::include_proto!("codex.agent");
}

use agent::adapter_service_server::{AdapterService, AdapterServiceServer};
use agent::{RunRequest, RunResponse, run_response::Event, SessionConfig, WireApi, SandboxPolicy};

#[derive(Debug, Default)]
pub struct MyAdapterService;

#[tonic::async_trait]
impl AdapterService for MyAdapterService {
    type RunStream = ReceiverStream<Result<RunResponse, Status>>;

    async fn run(
        &self,
        request: Request<RunRequest>,
    ) -> Result<Response<Self::RunStream>, Status> {
        let req = request.into_inner();
        let (tx, rx) = tokio::sync::mpsc::channel(100);

        tokio::spawn(async move {
            if let Err(e) = handle_run(req, tx.clone()).await {
                error!("Task execution failed: {:?}", e);
                let _ = tx.send(Ok(RunResponse {
                    event: Some(Event::Error(format!("Adapter error: {}", e))),
                })).await;
            }
        });

        Ok(Response::new(ReceiverStream::new(rx)))
    }
}

async fn handle_run(mut req: RunRequest, tx: tokio::sync::mpsc::Sender<Result<RunResponse, Status>>) -> anyhow::Result<()> {
    // 1. Setup Isolated Workspace
    let temp_dir = TempDir::new()?;
    let codex_home = temp_dir.path();
    let work_dir = if !req.base_dir.is_empty() {
        Path::new(&req.base_dir).to_path_buf()
    } else {
        codex_home.join("workspace")
    };
    tokio::fs::create_dir_all(&work_dir).await?;

    // 2. Prepare Configuration
    if let Some(session_config) = &mut req.session_config {
        if let Some(provider) = &mut session_config.provider {
            if let Some(env_key) = &provider.env_key {
                if let Some(key_val) = req.env_vars.get(env_key) {
                    provider.experimental_bearer_token = Some(key_val.clone());
                }
            }
        }

        let config_content = generate_config_toml(session_config)?;
        tokio::fs::write(codex_home.join("config.toml"), config_content).await?;
    }

    // 3. Inject Context Files
    for file in req.context_files {
        if file.path.contains("..") || file.path.starts_with("/") {
             warn!("Skipping suspicious file path: {}", file.path);
             continue;
        }
        let path = work_dir.join(&file.path);
        if let Some(parent) = path.parent() {
            tokio::fs::create_dir_all(parent).await?;
        }
        tokio::fs::write(&path, file.content).await?;
    }

    // 4. Build Execution Command
    let mut cmd = Command::new("codex");

    if let Some(config) = &req.session_config {
        if !config.model.is_empty() {
            cmd.arg("-c").arg(format!("model={}", config.model));
        }
        if let Some(provider) = &config.provider {
            cmd.arg("-c").arg(format!("model_provider={}", provider.name));
        }
    }

    cmd.arg("exec")
       .arg("--json")
       .arg("--skip-git-repo-check")
       .arg("--dangerously-bypass-approvals-and-sandbox");

    if let Some(config) = &req.session_config {
        match SandboxPolicy::try_from(config.sandbox_policy).unwrap_or(SandboxPolicy::Unspecified) {
            SandboxPolicy::WorkspaceWrite => { cmd.arg("--sandbox").arg("workspace-write"); },
            SandboxPolicy::ReadOnly => { cmd.arg("--sandbox").arg("read-only"); },
            SandboxPolicy::DangerFullAccess => { cmd.arg("--sandbox").arg("danger-full-access"); },
            _ => {}
        }
    }

    cmd.current_dir(&work_dir)
       .env("CODEX_HOME", codex_home)
       .env("RUST_LOG", "info")
       .envs(&req.env_vars)
       .arg("-") // Read prompt from stdin
       .stdin(Stdio::piped())
       .stdout(Stdio::piped())
       .stderr(Stdio::piped());

    info!("Starting Codex process for request: {}", req.request_id);

    // 5. Spawn and Provide Input
    let mut child = cmd.spawn()?;
    if let Some(mut stdin) = child.stdin.take() {
        let full_prompt = build_full_prompt(&req.prompt, req.session_config.as_ref());
        stdin.write_all(full_prompt.as_bytes()).await?;
        drop(stdin); 
    }

    // 6. Stream Events
    let stdout = child.stdout.take().ok_or_else(|| anyhow::anyhow!("No stdout"))?;
    let stderr = child.stderr.take().ok_or_else(|| anyhow::anyhow!("No stderr"))?;
    
    let mut out_reader = BufReader::new(stdout).lines();
    let mut err_reader = BufReader::new(stderr).lines();

    let tx_err = tx.clone();
    tokio::spawn(async move {
        while let Ok(Some(line)) = err_reader.next_line().await {
             let _ = tx_err.send(Ok(RunResponse {
                event: Some(Event::AdapterLog(format!("[STDERR] {}", line))),
            })).await;
        }
    });

    while let Ok(Some(line)) = out_reader.next_line().await {
        if tx.send(Ok(RunResponse {
            event: Some(Event::CodexEventJson(line)),
        })).await.is_err() {
            let _ = child.kill().await;
            return Ok(())
        }
    }

    let status = child.wait().await?;
    info!("Codex process exited for {} with status: {}", req.request_id, status);

    Ok(())
}

fn build_full_prompt(base_prompt: &str, config: Option<&SessionConfig>) -> String {
    let mut parts = Vec::new();
    if let Some(cfg) = config {
        if let Some(sys) = &cfg.base_instructions { parts.push(sys.clone()); }
        if let Some(dev) = &cfg.developer_instructions { parts.push(dev.clone()); }
        if let Some(user) = &cfg.user_instructions { parts.push(user.clone()); }
    }
    parts.push(base_prompt.to_string());
    parts.join("\n\n")
}

fn generate_config_toml(config: &SessionConfig) -> anyhow::Result<String> {
    let mut toml = String::new();
    
    if !config.model.is_empty() { toml.push_str(&format!("model = {:?}\n", config.model)); }
    if let Some(provider) = &config.provider { toml.push_str(&format!("model_provider = {:?}\n", provider.name)); }
    if let Some(instr) = &config.base_instructions { toml.push_str(&format!("instructions = {:?}\n", instr)); }
    if let Some(dev) = &config.developer_instructions { toml.push_str(&format!("developer_instructions = {:?}\n", dev)); }

    if let Some(provider) = &config.provider {
        toml.push_str(&format!("\n[model_providers.{}]\n", provider.name));
        toml.push_str(&format!("name = {:?}\n", provider.name));
        if let Some(base_url) = &provider.base_url { toml.push_str(&format!("base_url = {:?}\n", base_url)); }
        
        let wire_api_str = match WireApi::try_from(provider.wire_api).unwrap_or(WireApi::Chat) {
            WireApi::Chat => "chat",
            WireApi::Responses => "responses",
            WireApi::ResponsesWebsocket => "responses_websocket",
        };
        toml.push_str(&format!("wire_api = {:?}\n", wire_api_str));
        
        if let Some(token) = &provider.experimental_bearer_token {
            toml.push_str(&format!("experimental_bearer_token = {:?}\n", token));
        } else if let Some(key) = &provider.env_key {
            toml.push_str(&format!("env_key = {:?}\n", key));
        }
        
        toml.push_str(&format!("requires_openai_auth = {}
", provider.requires_openai_auth));
    }

    if !config.mcp_servers.is_empty() {
        for (name, def) in &config.mcp_servers {
            toml.push_str(&format!("\n[mcp_servers.{}]\n", name));
            if !def.server_type.is_empty() { toml.push_str(&format!("type = {:?}\n", def.server_type)); }
            if !def.command.is_empty() { toml.push_str(&format!("command = {:?}\n", def.command)); }
            if !def.url.is_empty() { toml.push_str(&format!("url = {:?}\n", def.url)); }
        }
    }
    Ok(toml)
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    tracing_subscriber::fmt().with_env_filter(
        tracing_subscriber::EnvFilter::from_default_env()
            .add_directive(tracing::Level::INFO.into())
    ).init();

    let addr = "0.0.0.0:50051".parse()?;
    let adapter = MyAdapterService::default();
    info!("Codex Agent Adapter listening on {}", addr);
    Server::builder().add_service(AdapterServiceServer::new(adapter)).serve(addr).await?;
    Ok(())
}
