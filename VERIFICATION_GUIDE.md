# 快速验证修复指南

## 🚀 一键验证修复

本指南帮助您快速验证"接收文件存储目录不生效"的问题已经修复。

## 准备工作

```bash
# 1. 确保在项目根目录
cd /Users/cainliu/codes/AGI/AeroSync

# 2. 查看修复的文件
git status
```

## 方法一：快速自动化测试（推荐）

### 步骤 1: 准备测试环境

```bash
# 创建测试目录
TEST_DIR="/tmp/aerosync_test_$(date +%s)"
mkdir -p "$TEST_DIR"
echo "测试目录: $TEST_DIR"

# 创建测试文件
echo "Hello AeroSync - 这是一个测试文件" > /tmp/test_upload.txt
```

### 步骤 2: 启动服务器（在终端 1）

```bash
# 方式 A: 使用 GUI（需要手动操作）
cargo run --release

# 然后在 GUI 中：
# 1. 点击 "Server" 标签
# 2. 点击 "📂 Browse"，选择 $TEST_DIR
# 3. 点击 "▶ Start Server"
```

或者

```bash
# 方式 B: 直接运行（如果已编译）
./target/release/aerosync
```

### 步骤 3: 发送测试文件（在终端 2）

```bash
# 等待服务器启动完成后执行
sleep 2

# 发送文件
curl -v -F 'file=@/tmp/test_upload.txt' http://localhost:8080/upload

# 期望看到: HTTP 200 OK
```

### 步骤 4: 验证文件位置

```bash
# 检查文件是否在正确的目录
ls -la "$TEST_DIR"

# 应该看到类似输出：
# -rw-r--r--  1 user  staff  42 Dec 28 10:00 test_upload.txt

# 读取文件内容确认
cat "$TEST_DIR/test_upload.txt"

# 应该输出: Hello AeroSync - 这是一个测试文件
```

### 步骤 5: 清理

```bash
# 清理测试文件
rm -rf "$TEST_DIR" /tmp/test_upload.txt
```

## 方法二：手动 GUI 测试

### 步骤 1: 启动 GUI

```bash
cargo run --release --features egui
```

### 步骤 2: 配置服务器

在 GUI 中：
1. 点击顶部的 **"Server"** 标签
2. 在 "Server Configuration" 区域：
   - 点击 **"📂 Browse"** 按钮
   - 选择一个容易找到的目录（如：`~/Desktop/aerosync_test`）
   - 查看界面显示的路径是否正确更新

### 步骤 3: 启动服务器

1. 点击 **"▶ Start Server"** 按钮
2. 等待状态变为 **"Running"**
3. 记录显示的服务器地址（通常是 `http://localhost:8080`）

### 步骤 4: 检查日志

在终端中查看日志输出：
```
INFO  Starting file receiver server...
INFO  Configuration:
INFO    - Receive directory: /Users/xxx/Desktop/aerosync_test  ← 确认路径正确
INFO  HTTP server is ready to accept file uploads
```

### 步骤 5: 发送测试文件

打开另一个终端：
```bash
# 创建测试文件
echo "测试内容 $(date)" > ~/test_file.txt

# 发送文件
curl -F 'file=@/Users/你的用户名/test_file.txt' http://localhost:8080/upload
```

### 步骤 6: 验证结果

1. 在 Finder 中打开你选择的接收目录
2. 应该看到 `test_file.txt` 文件
3. 打开文件确认内容正确

## 方法三：观察日志验证

### 修复前的日志（错误）

```
INFO  Starting file receiver server...
INFO    - Receive directory: ./received_files  ← 使用默认目录
INFO  HTTP: Target file path: ./received_files/test.txt  ← 错误位置
```

### 修复后的日志（正确）

```
INFO  Starting file receiver server...
INFO    - Receive directory: /tmp/aerosync_test  ← 使用用户设置的目录
INFO  HTTP: Target file path: /tmp/aerosync_test/test.txt  ← 正确位置
```

## 常见问题排查

### Q1: 服务器启动失败

```bash
# 检查端口是否被占用
lsof -i :8080
lsof -i :4433

# 如果被占用，杀掉进程或修改端口
```

### Q2: 文件发送失败

```bash
# 检查服务器是否运行
curl http://localhost:8080/status

# 期望返回 JSON 状态信息
```

### Q3: 找不到文件

```bash
# 确认使用的目录
# 查看日志中的 "Receive directory" 路径
# 确保该路径与你选择的一致
```

### Q4: 权限问题

```bash
# 确保接收目录有写权限
chmod 755 /path/to/receive/dir

# 或选择用户主目录下的文件夹
```

## 成功标志 ✅

如果看到以下现象，说明修复成功：

1. ✅ 日志显示使用了你选择的目录
2. ✅ 文件保存在你选择的目录中
3. ✅ 不会在项目根目录生成 `./received_files`
4. ✅ GUI 显示的路径与实际使用的路径一致

## 失败标志 ❌

如果出现以下情况，说明可能还有问题：

1. ❌ 文件保存在 `./received_files` 而不是你选择的目录
2. ❌ 日志显示的目录与你选择的不一致
3. ❌ 服务器启动失败
4. ❌ 文件上传成功但找不到文件

## 获取帮助

如果验证失败，请提供以下信息：

1. 完整的日志输出
2. 你选择的接收目录路径
3. 文件实际保存的位置
4. 操作系统和 Rust 版本

```bash
# 收集系统信息
echo "OS: $(uname -a)"
echo "Rust: $(rustc --version)"
echo "Cargo: $(cargo --version)"
```

## 自动化测试脚本

使用提供的测试脚本：

```bash
# 运行自动化测试
./test_receive_dir_fix.sh
```

或手动执行测试逻辑：

```bash
#!/bin/bash
set -e

TEST_DIR="/tmp/aerosync_verify_$(date +%s)"
mkdir -p "$TEST_DIR"

echo "✓ 创建测试目录: $TEST_DIR"
echo "→ 请在 GUI 中选择此目录并启动服务器"
echo "→ 按 Enter 继续..."
read

echo "✓ 创建测试文件..."
echo "Test $(date)" > /tmp/test.txt

echo "✓ 上传文件..."
curl -F 'file=@/tmp/test.txt' http://localhost:8080/upload

echo "✓ 验证文件位置..."
if [ -f "$TEST_DIR/test.txt" ]; then
    echo "✅ 成功！文件在正确的目录"
    cat "$TEST_DIR/test.txt"
else
    echo "❌ 失败！文件不在预期目录"
    exit 1
fi

echo "✓ 清理..."
rm -rf "$TEST_DIR" /tmp/test.txt
echo "✅ 测试完成！"
```

---

**修复完成时间:** 2025-12-28  
**修复文件:** `aerosync-ui/src/egui_app.rs`  
**详细文档:** 参见 `BUGFIX.md` 和 `BUGFIX_SUMMARY.md`

