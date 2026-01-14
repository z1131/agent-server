#!/bin/bash
set -e

# 进入脚本所在目录
cd "$(dirname "$0")"

echo "🔧 创建 Python 虚拟环境..."
if [ ! -d "venv" ]; then
    python3 -m venv venv
fi
source venv/bin/activate

echo "📦 安装依赖 (grpcio-tools)..."
# 为了加快速度，如果已安装则跳过
if ! python3 -c "import grpc_tools" 2>/dev/null; then
    pip install grpcio grpcio-tools
fi

echo "🔨 编译 Protobuf..."
# 确保输出目录存在
mkdir -p proto_gen
# 修正 import 路径问题，需要在 proto_gen 里创建一个 __init__.py
touch proto_gen/__init__.py

# 注意：adapter.proto 在 ../../proto/adapter.proto
# 我们需要正确设置 -I 路径，以便 python 代码生成的 import 正确
python3 -m grpc_tools.protoc \
    -I../../proto \
    --python_out=./proto_gen \
    --grpc_python_out=./proto_gen \
    adapter.proto

echo "✅ 编译完成"
echo "🚀 运行测试客户端..."
echo "---------------------------------------------------"
echo "请确保在另一个终端运行了: cargo run -p codex-adapter"
echo "---------------------------------------------------"
# 自动检查端口是否开启（可选，简单起见直接运行）

python3 test_client.py
