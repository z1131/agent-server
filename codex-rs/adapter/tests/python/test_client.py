import grpc
import sys
import os
import json
import time
import uuid

# 添加 proto 生成代码的路径
sys.path.append(os.path.join(os.path.dirname(__file__), 'proto_gen'))

try:
    import adapter_pb2
    import adapter_pb2_grpc
except ImportError:
    print("❌ 错误: 找不到生成的 Proto 代码。 সন")
    sys.exit(1)

def execute_task(stub, prompt, session_id, history_bytes=b""):
    req = adapter_pb2.RunTaskRequest(
        request_id=f"req-{int(time.time())}",
        session_id=session_id,
        prompt=prompt,
        history_rollout=history_bytes,
        session_config=adapter_pb2.SessionConfig(
            model="qwen-plus",
            model_provider="aliyun",
            provider_info=adapter_pb2.ModelProviderInfo(
                name="aliyun",
                base_url="https://dashscope.aliyuncs.com/compatible-mode/v1",
                env_key="DASH_API_KEY", 
                wire_api=adapter_pb2.WireApi.WIRE_API_CHAT, 
                requires_openai_auth=False 
            ),
            sandbox_policy=adapter_pb2.SandboxPolicy.WORKSPACE_WRITE,
            approval_policy=adapter_pb2.ApprovalPolicy.NEVER,
            cwd="/tmp/codex_test_wd"
        ),
        env_vars={"DASH_API_KEY": "sk-4438e8cfa0494e17b93845b7aa8b0bab"}
    )

    print(f"\n={'='*40}\n🚀 [发送指令]: {prompt}\n={'='*40}")
    
    last_rollout = None
    try:
        for response in stub.RunTask(req):
            field = response.WhichOneof("event")
            
            # 暴力打印：不进行任何过滤或解析，看到什么打什么
            if field == "codex_event_json":
                print(f"📄 [RAW_JSON]: {response.codex_event_json}")
            elif field == "adapter_log":
                print(f"📋 [RAW_LOG]: {response.adapter_log.strip()}")
            elif field == "error":
                print(f"❌ [RAW_ERR]: {response.error}")
            elif field == "updated_rollout":
                last_rollout = response.updated_rollout
                print(f"✨ [RAW_ROLLOUT]: {len(last_rollout)} 字节")
                
    except grpc.RpcError as e:
        print(f"❌ gRPC 异常: {e.details()}")
    
    return last_rollout

def run_test():
    channel = grpc.insecure_channel('localhost:50051')
    stub = adapter_pb2_grpc.AgentServiceStub(channel)

    my_session_id = str(uuid.uuid4())
    print(f"🔥 开启全量原始数据实时监控 | Session: {my_session_id}")

    # 第一轮
    r1 = execute_task(stub, "确认一下，我的幸运数字是 888。请确认你记住了。", my_session_id)
    
    if r1:
        print("\n" + "#"*60 + "\n# 跨请求搬运中...\n" + "#"*60)
        # 第二轮
        execute_task(stub, "我刚才说的幸运数字是多少？直接说数字。", my_session_id, r1)

    print("\n✅ 测试序列执行完毕")

if __name__ == "__main__":
    run_test()
