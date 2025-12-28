#!/bin/bash

# AeroSync 接收目录修复演示脚本
# 这个脚本演示如何验证修复效果

set -e

echo "=========================================="
echo "🚀 AeroSync 接收目录修复验证 Demo"
echo "=========================================="
echo ""

# 创建测试目录
TEST_DIR="/tmp/aerosync_demo_$(date +%s)"
echo "📁 创建测试目录: $TEST_DIR"
mkdir -p "$TEST_DIR"

# 创建测试文件
echo "📝 创建测试文件..."
echo "Hello AeroSync! 这是测试文件 - 创建时间: $(date)" > /tmp/demo_test.txt
echo "   文件路径: /tmp/demo_test.txt"
echo "   文件大小: $(wc -c < /tmp/demo_test.txt) 字节"
echo ""

echo "=========================================="
echo "📋 测试步骤"
echo "=========================================="
echo ""
echo "步骤 1: 启动 AeroSync GUI"
echo "   请在另一个终端运行："
echo "   $ cd /Users/cainliu/codes/AGI/AeroSync"
echo "   $ cargo run --release"
echo ""
echo "步骤 2: 在 GUI 中配置接收目录"
echo "   - 点击顶部的 'Server' 标签"
echo "   - 点击 '📂 Browse' 按钮"
echo "   - 选择目录: $TEST_DIR"
echo "   - 点击 '▶ Start Server' 启动服务器"
echo ""
echo "步骤 3: 发送测试文件"
echo "   回到这个终端，按 Enter 键继续..."
read -p ""

echo ""
echo "🚀 发送测试文件到服务器..."
echo ""

# 尝试发送文件
if curl -f -F 'file=@/tmp/demo_test.txt' http://localhost:8080/upload 2>&1; then
    echo ""
    echo "✅ 文件发送成功！"
    echo ""
    
    # 等待文件写入
    sleep 1
    
    # 验证文件位置
    echo "=========================================="
    echo "🔍 验证结果"
    echo "=========================================="
    echo ""
    
    echo "检查文件是否在正确的目录中..."
    if [ -f "$TEST_DIR/demo_test.txt" ]; then
        echo ""
        echo "✅ 成功！文件在正确的目录中！"
        echo ""
        echo "📁 文件位置: $TEST_DIR/demo_test.txt"
        echo "📝 文件内容:"
        cat "$TEST_DIR/demo_test.txt"
        echo ""
        echo "📊 目录内容:"
        ls -lh "$TEST_DIR/"
        echo ""
        echo "=========================================="
        echo "✅ 修复验证成功！"
        echo "=========================================="
        echo ""
        echo "说明："
        echo "- ✅ 文件保存在用户选择的目录: $TEST_DIR"
        echo "- ✅ 没有使用默认目录: ./received_files"
        echo "- ✅ 接收目录设置生效！"
    else
        echo ""
        echo "❌ 失败！文件不在预期目录"
        echo ""
        echo "检查默认目录..."
        if [ -f "./received_files/demo_test.txt" ]; then
            echo "❌ 文件在默认目录: ./received_files/"
            echo "   这说明修复未生效，可能使用的是旧版本"
        else
            echo "❓ 文件位置未知，请检查服务器日志"
        fi
    fi
else
    echo ""
    echo "❌ 文件发送失败"
    echo ""
    echo "可能的原因："
    echo "1. 服务器未启动（请确认步骤1和2）"
    echo "2. 端口被占用"
    echo "3. 网络问题"
    echo ""
    echo "请检查服务器日志获取更多信息"
fi

echo ""
echo "=========================================="
echo "🧹 清理"
echo "=========================================="
read -p "是否清理测试文件？(y/n) " -n 1 -r
echo ""
if [[ $REPLY =~ ^[Yy]$ ]]; then
    rm -rf "$TEST_DIR" /tmp/demo_test.txt
    echo "✅ 清理完成"
else
    echo "保留测试文件:"
    echo "  测试目录: $TEST_DIR"
    echo "  测试文件: /tmp/demo_test.txt"
    echo ""
    echo "手动清理命令:"
    echo "  rm -rf $TEST_DIR /tmp/demo_test.txt"
fi

echo ""
echo "=========================================="
echo "Demo 完成！"
echo "=========================================="

