# 🚀 AeroSync 快速启动指南

## 当前状态

✅ **代码已修复**：接收文件存储目录问题已修复  
🔄 **正在编译**：GUI 版本正在编译中...

## 启动方式

### 方式 1: GUI 图形界面（推荐）

```bash
cd /Users/cainliu/codes/AGI/AeroSync
RUST_LOG=info cargo run --release --features egui
```

**GUI 将显示：**
- 🎨 现代化的图形界面
- 📂 文件选择和传输管理
- 🖥️ 服务器配置和状态监控

### 方式 2: CLI 命令行界面

```bash
cd /Users/cainliu/codes/AGI/AeroSync
cargo run --release
```

**CLI 提供交互式菜单：**
```
AeroSync - Cross-Platform File Transfer Engine
==============================================

Main Menu:
1. Select files to transfer
2. Select folder to transfer
3. View transfer status
4. Settings
5. Exit
```

## 验证修复效果

### 步骤 1: 启动 GUI 并配置服务器

1. 启动 GUI（等待编译完成）
2. 点击顶部的 **"Server"** 标签
3. 在 "Server Configuration" 区域：
   - 点击 **"📂 Browse"** 按钮
   - 选择一个测试目录，例如：`/tmp/aerosync_test`
4. 点击 **"▶ Start Server"** 启动服务器
5. 等待状态变为 **"Running"**

### 步骤 2: 查看日志确认

在日志输出中确认服务器使用了正确的目录：

```
INFO  Starting file receiver server...
INFO  Configuration:
INFO    - Receive directory: /tmp/aerosync_test  ← 确认是你选择的目录
INFO    - HTTP port: 8080
INFO    - QUIC port: 4433
INFO  HTTP server is ready to accept file uploads
```

### 步骤 3: 发送测试文件

打开新终端窗口：

```bash
# 创建测试文件
echo "Hello AeroSync - 测试时间: $(date)" > /tmp/test_upload.txt

# 发送文件到服务器
curl -F 'file=@/tmp/test_upload.txt' http://localhost:8080/upload

# 期望输出: {"status":"success","message":"File uploaded successfully",...}
```

### 步骤 4: 验证文件位置

```bash
# 检查文件是否在正确的目录
ls -la /tmp/aerosync_test/

# 应该看到：
# -rw-r--r--  1 user  staff  XX Dec 28 XX:XX test_upload.txt

# 查看文件内容
cat /tmp/aerosync_test/test_upload.txt
```

### ✅ 成功标志

- ✅ 文件出现在你选择的目录中（`/tmp/aerosync_test/`）
- ✅ 项目根目录**没有**生成 `./received_files` 目录
- ✅ 日志显示正确的接收目录路径

### ❌ 如果失败

如果文件出现在 `./received_files` 而不是你选择的目录：
1. 确认使用的是重新编译后的版本
2. 检查是否正确选择了目录
3. 查看日志中的 "Receive directory" 路径

## 自动化测试脚本

使用提供的演示脚本：

```bash
cd /Users/cainliu/codes/AGI/AeroSync

# 运行演示脚本（先启动GUI服务器）
./demo_fix_verification.sh
```

## 常见问题

### Q1: 编译失败

```bash
# 清理并重新编译
cargo clean
cargo build --release --features egui
```

### Q2: 端口被占用

```bash
# 检查端口占用
lsof -i :8080
lsof -i :4433

# 如果被占用，修改配置或杀掉进程
```

### Q3: GUI 无法启动

```bash
# 检查依赖
cargo tree | grep egui

# 尝试重新编译
cargo build --release --features egui --verbose
```

### Q4: 找不到文件

```bash
# 检查服务器日志中的实际保存路径
# 日志会显示：HTTP: Target file path: /path/to/file
```

## 完整演示流程

```bash
# 终端 1: 启动 GUI 服务器
cd /Users/cainliu/codes/AGI/AeroSync
RUST_LOG=info cargo run --release --features egui

# 在 GUI 中：
# 1. 进入 Server 标签
# 2. 选择接收目录: /tmp/aerosync_test
# 3. 点击 Start Server

# 终端 2: 发送测试文件
cd /Users/cainliu/codes/AGI/AeroSync
./demo_fix_verification.sh

# 或手动测试：
echo "Test $(date)" > /tmp/test.txt
curl -F 'file=@/tmp/test.txt' http://localhost:8080/upload
ls -la /tmp/aerosync_test/
```

## 日志级别

调整日志详细程度：

```bash
# 调试级别（最详细）
RUST_LOG=debug cargo run --release --features egui

# 信息级别（推荐）
RUST_LOG=info cargo run --release --features egui

# 警告级别（仅错误和警告）
RUST_LOG=warn cargo run --release --features egui

# 针对特定模块
RUST_LOG=aerosync_core=debug,aerosync_ui=info cargo run --features egui
```

## 性能测试

### 发送大文件

```bash
# 创建 100MB 测试文件
dd if=/dev/zero of=/tmp/bigfile.dat bs=1m count=100

# 发送文件
curl -F 'file=@/tmp/bigfile.dat' http://localhost:8080/upload

# 检查传输速度和完整性
```

### 并发测试

```bash
# 同时发送多个文件
for i in {1..10}; do
  echo "File $i" > /tmp/file_$i.txt
  curl -F "file=@/tmp/file_$i.txt" http://localhost:8080/upload &
done
wait

# 检查所有文件是否都收到
ls -la /tmp/aerosync_test/
```

## 相关文档

- **BUGFIX_QUICK.md** - Bug 修复说明
- **VERIFICATION_GUIDE.md** - 详细验证指南
- **FIX_COMPLETED.md** - 修复完成报告
- **README.md** - 完整项目文档

## 获取帮助

如遇问题，请提供：
1. 完整的错误信息或日志
2. 你的操作步骤
3. 系统信息：`uname -a` 和 `cargo --version`

---

**提示**：首次运行需要编译，可能需要几分钟时间。后续启动会快很多！

当前编译正在进行中，请稍候... 🔄

