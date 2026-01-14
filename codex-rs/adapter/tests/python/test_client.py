import grpc
import sys
import os
import json
import time

# 添加 proto 生成代码的路径
sys.path.append(os.path.join(os.path.dirname(__file__), 'proto_gen'))

try:
    import adapter_pb2
    import adapter_pb2_grpc
except ImportError:
    print("❌ 错误: 找不到生成的 Proto 代码。")
    sys.exit(1)

def run_test():
    print("🔌 连接 gRPC 服务 (localhost:50051)...")
    channel = grpc.insecure_channel('localhost:50051')
    stub = adapter_pb2_grpc.AdapterServiceStub(channel)

    prompt_text = "请使用 Bing 搜索 Rust 语言目前的最新稳定版本是多少。创建一个名为 rust_info.txt 的文件，把版本号写进去。最后，请读取并显示该文件的内容，以确认写入成功。"
    
    req = adapter_pb2.RunRequest(
        request_id=f"test-{int(time.time())}",
        prompt=prompt_text,
        session_config=adapter_pb2.SessionConfig(
            model="qwen-plus",
            provider=adapter_pb2.ModelProviderInfo(
                name="aliyun",
                base_url="https://dashscope.aliyuncs.com/compatible-mode/v1",
                env_key="DASH_API_KEY", 
                wire_api=adapter_pb2.WireApi.WIRE_API_CHAT, 
                requires_openai_auth=False 
            ),
            sandbox_policy=adapter_pb2.SandboxPolicy.WORKSPACE_WRITE,
            approval_policy=adapter_pb2.ApprovalPolicy.NEVER,
            cwd="/tmp/codex_test_wd",
            mcp_servers={
                "bing-cn-mcp-server": adapter_pb2.McpServerDef(
                    server_type="streamable_http",
                    url="https://mcp.api-inference.modelscope.net/66eae62e82fe40/mcp"
                )
            }
        ),
        env_vars={"DASH_API_KEY": "sk-4438e8cfa0494e17b93845b7aa8b0bab"}
    )

    print(f"📝 任务: {prompt_text}\n" + "="*50)

    try:
        for response in stub.Run(req):
            if response.HasField("adapter_log"):
                # 过滤掉 SSE 这种心跳日志，只看关键日志
                log = response.adapter_log
                if "sse_event" not in log and "otel_manager" not in log:
                    print(f"📋 [SYSTEM] {log.strip()}")
            
            elif response.HasField("error"):
                print(f"❌ [ERROR] {response.error}")
            
            elif response.HasField("codex_event_json"):
                try:
                    data = json.loads(response.codex_event_json)
                    msg = data.get("msg", {})
                    method = data.get("method")
                    
                    # 适配 Codex App-Server 协议风格
                    if method == "item/agentMessage/delta":
                        content = data.get("params", {}).get("delta", "")
                        if content:
                            sys.stdout.write(content)
                            sys.stdout.flush()
                    
                    elif method == "item/started":
                        item = data.get("params", {}).get("item", {})
                        item_type = item.get("type")
                        if item_type == "reasoning":
                            print(f"\n🧠 [思考] {item.get('summary', '思考中...')}")
                        elif item_type == "commandExecution":
                            print(f"\n🛠️  [执行命令] {item.get('command')}")
                        elif item_type == "mcpToolCall":
                            print(f"\n🔗 [调用工具] {item.get('tool')}")
                    
                    elif method == "item/completed":
                        item = data.get("params", {}).get("item", {})
                        if item.get("type") == "commandExecution":
                            print(f"\n✅ [执行结果] ExitCode: {item.get('exitCode')}")
                    
                    elif method == "turn/completed":
                        print("\n\n🏁 [任务完成]")
                    
                    # 如果是原始 JSONL 格式 (Exec 模式)
                    elif "type" in data:
                        t = data["type"]
                        if t == "message" and "content" in data:
                            print(f"\n💬 [Agent] {data['content']}")
                        elif t == "reasoning":
                            print(f"\n🧠 [思考] {data.get('content')}")

                except Exception as e:
                    # 实在解析不动，但又不是噪音，就打出来
                    # print(f"DEBUG: {response.codex_event_json}") 
                    pass

    except grpc.RpcError as e:
        print(f"\n❌ gRPC 调用失败: {e.details()}")

    print("\n" + "="*50 + "\n✅ 测试结束")

if __name__ == "__main__":
    run_test()
