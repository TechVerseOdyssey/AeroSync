#!/bin/bash

# 测试准备脚本 - 为验证修复做准备

echo "=========================================="
echo "🎯 AeroSync 修复验证 - 测试准备"
echo "=========================================="
echo ""

# 创建测试目录
TEST_DIR="/tmp/aerosync_test_$(date +%Y%m%d_%H%M%S)"
echo "📁 创建测试目录..."
mkdir -p "$TEST_DIR"
echo "   目录路径: $TEST_DIR"
echo "   ✅ 创建成功"
echo ""

# 创建测试文件
echo "📝 创建测试文件..."
echo "Hello from AeroSync!" > /tmp/test_small.txt
echo "这是一个中文测试文件 - 创建时间: $(date)" >> /tmp/test_small.txt
echo "   文件: /tmp/test_small.txt"
echo "   大小: $(wc -c < /tmp/test_small.txt) 字节"
echo "   ✅ 创建成功"
echo ""

# 创建一个稍大的文件
echo "📦 创建中等大小测试文件..."
dd if=/dev/urandom of=/tmp/test_medium.dat bs=1024 count=100 2>/dev/null
echo "   文件: /tmp/test_medium.dat"
echo "   大小: $(ls -lh /tmp/test_medium.dat | awk '{print $5}')"
echo "   ✅ 创建成功"
echo ""

echo "=========================================="
echo "✅ 准备完成！"
echo "=========================================="
echo ""
echo "📋 测试环境信息："
echo "   测试目录: $TEST_DIR"
echo "   测试文件: /tmp/test_small.txt"
echo "   测试文件: /tmp/test_medium.dat"
echo ""
echo "📝 下一步操作："
echo ""
echo "1. 启动 AeroSync GUI："
echo "   $ cd /Users/cainliu/codes/AGI/AeroSync"
echo "   $ RUST_LOG=info cargo run --release --features egui"
echo ""
echo "2. 在 GUI 中配置："
echo "   - 进入 'Server' 标签"
echo "   - 点击 '📂 Browse'"
echo "   - 选择目录: $TEST_DIR"
echo "   - 点击 '▶ Start Server'"
echo ""
echo "3. 发送测试文件（在新终端）："
echo "   # 发送小文件"
echo "   $ curl -F 'file=@/tmp/test_small.txt' http://localhost:8080/upload"
echo ""
echo "   # 发送中等大小文件"
echo "   $ curl -F 'file=@/tmp/test_medium.dat' http://localhost:8080/upload"
echo ""
echo "4. 验证结果："
echo "   $ ls -lh $TEST_DIR/"
echo "   $ cat $TEST_DIR/test_small.txt"
echo ""
echo "=========================================="
echo ""
echo "💡 提示："
echo "   将此目录路径复制到剪贴板："
echo "   $ echo '$TEST_DIR' | pbcopy"
echo ""
echo "🧹 测试后清理："
echo "   $ rm -rf $TEST_DIR /tmp/test_*.txt /tmp/test_*.dat"
echo ""

# 保存测试目录路径
echo "$TEST_DIR" > /tmp/aerosync_last_test_dir.txt
echo "📌 测试目录路径已保存到: /tmp/aerosync_last_test_dir.txt"
echo ""

