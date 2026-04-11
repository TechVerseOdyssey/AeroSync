# AeroSync Python Agent 示例

双向协作演示：`sender_agent.py` 监控目录发文件，`receiver_agent.py` 接收后处理并回传结果，两端通过 AeroSync 通信。

## 依赖

```bash
pip install websockets
```

AeroSync 二进制需在 PATH 中（**必须用最新编译的版本**，旧版不含 WebSocket 支持）：

```bash
cargo build --release
export PATH="$PATH:$(pwd)/target/release"
```

## 运行

**终端 1 — 启动接收端 agent**

```bash
python examples/receiver_agent.py \
  --save-dir /tmp/aerosync_recv \
  --host localhost:7788
```

接收端会：
1. 后台启动 `aerosync receive` 监听 7788 端口
2. 订阅 `/ws` 等待传输事件
3. 文件到达后计算 SHA-256、统计行数，写 `.result` 文件

**终端 2 — 启动发送端 agent**

```bash
python examples/sender_agent.py \
  --watch-dir /tmp/aerosync_send \
  --host localhost:7788 \
  --results-dir /tmp/aerosync_recv
```

发送端会：
1. 监控 `/tmp/aerosync_send` 目录（1s 轮询）
2. 发现新文件后调用 `aerosync send`
3. 订阅 `/ws` 打印传输进度，`completed` 后读取接收端回传的 `.result`

**终端 3 — 投递测试文件**

```bash
# 文本文件
echo "hello from sender agent" > /tmp/aerosync_send/hello.txt

# 批量投递
for i in $(seq 1 5); do
  echo "file $i content" > /tmp/aerosync_send/file_$i.txt
  sleep 0.5
done
```

## 预期输出

```
# receiver 端
[receiver] started aerosync receive  (pid=12345)
[aerosync] AeroSync receiver starting...
[aerosync]   HTTP:  0.0.0.0:7788
[aerosync]   Save:  /tmp/aerosync_recv
[receiver] connected to ws://localhost:7788/ws
[receiver] ✓ received: hello.txt  (107 bytes, sha256=f09df840…)
[receiver] wrote result → hello.txt.result

# sender 端
[sender] watching /tmp/aerosync_send  (polling every 1s)
[sender] connected to ws://localhost:7788/ws
[sender] → sending hello.txt …
[sender] ✓ completed: hello.txt  (107 bytes, sha256=f09df840…)
[sender] ✓ sent hello.txt
[sender] ← result: file:       hello.txt
                   size:       107 bytes
                   sha256:     f09df84026a95638e98ad7e26a0d836ef24cef39b59a60b793048a8aac274391
                   text_lines: 3
                   processed:  2026-04-11T16:23:32Z
                   status:     OK
```

## 架构图

```
┌─────────────────────────────┐        ┌─────────────────────────────────┐
│      sender_agent.py        │        │       receiver_agent.py         │
│                             │        │                                  │
│  watch_dir ──polling──►send │──────► │ aerosync receive (subprocess)   │
│                             │  HTTP  │      /tmp/aerosync_recv/         │
│  watch_events ◄─── /ws ─── │◄────── │ process_file() → .result file   │
└─────────────────────────────┘   WS   └─────────────────────────────────┘
         ▲ reads .result                            │ writes .result
         └───────────────────────────────────────────┘
               (via shared dir or --results-dir)
```

## 已知注意事项

### 1. 必须使用最新编译的二进制

`/ws` WebSocket 端点在 P1 需求实现后才加入。如果 `aerosync receive` 已在运行但 `/ws` 返回 404，说明二进制是旧版本，需要重新编译：

```bash
cargo build --release
```

### 2. 本地环境使用 HTTP，不要走 QUIC

aerosync 检测到对端后会自动尝试升级到 QUIC（端口 7789）。在 localhost 环境下 QUIC 地址解析可能失败，receiver_agent 启动时已加 `--http-only` 规避。send 目标地址也须使用完整 HTTP URL：

```
# 错误（会触发 QUIC auto-upgrade）
aerosync send file.txt localhost:7788

# 正确
aerosync send file.txt http://localhost:7788/upload
```

sender_agent.py 内部已自动拼接正确格式，无需手动处理。

### 3. `--results-dir` 需指向接收端保存目录

sender_agent 通过读取 `<filename>.result` 文件获取接收端处理结果。单机测试时 `--results-dir` 必须与 receiver 的 `--save-dir` 一致（或指向同一共享目录）。跨机器部署时需另行传输 `.result` 文件，或改用独立回传通道。

### 4. Python 版本

脚本兼容 Python 3.9+。不支持 Python 3.8 及以下（使用了 `asyncio.run`）。

## 扩展思路

| 扩展方向 | 实现方式 |
|----------|----------|
| LLM 处理收到的文件 | 在 `process_received_file()` 中调用 OpenAI/Claude API |
| 多接收端并行 | 启动多个 receiver_agent，配置不同端口 |
| TLS 加密传输 | `aerosync receive --tls-cert cert.pem --tls-key key.pem` |
| 认证 token | `--token my_secret_token` 两端一致 |
| 过滤事件类型 | `aerosync watch --filter completed` |
