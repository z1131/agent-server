#!/bin/bash
set -e

# 进入脚本所在目录
cd "$(dirname "$0")"

echo "🔧 激活环境并更新依赖..."
if [ ! -d "venv" ]; then
    python3 -m venv venv
fi
source venv/bin/activate
pip install -q grpcio grpcio-tools

echo "🔨 重新编译 Protobuf (同步最新 session_id 定义)..."
mkdir -p proto_gen
touch proto_gen/__init__.py

# 精准编译：从 ../../proto 目录读取，输出到 ./proto_gen
python3 -m grpc_tools.protoc \
    -I../../proto \
    --python_out=./proto_gen \
    --grpc_python_out=./proto_gen \
    ../../proto/adapter.proto

echo "✅ 同步完成！"
echo "🚀 启动测试客户端..."
python3 test_client.py