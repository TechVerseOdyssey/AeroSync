#!/usr/bin/env python3
"""
receiver_agent.py — AeroSync 双向协作示例：接收端 Agent

功能：
  1. 启动 aerosync receive 子进程（后台监听 7788 端口）
  2. 订阅 /ws，实时监听传输事件
  3. 收到 transfer_completed 后处理文件：
       - 计算 SHA-256、统计文件大小
       - 写入 .result 文件回传给 sender（sender_agent.py 会读取）
  4. Ctrl-C 时优雅停止 aerosync receive 子进程

依赖：
  pip install websockets

运行：
  python examples/receiver_agent.py --save-dir /tmp/aerosync_recv
"""

import argparse
import asyncio
import hashlib
import json
import os
import signal
import subprocess
import sys
import time
from pathlib import Path
from typing import Optional

try:
    import websockets
except ImportError:
    print("Error: missing 'websockets' package.  Run: pip install websockets")
    sys.exit(1)


# ── 工具函数 ─────────────────────────────────────────────────────────────────

def sha256_file(path: Path) -> str:
    h = hashlib.sha256()
    with open(path, "rb") as f:
        for chunk in iter(lambda: f.read(65536), b""):
            h.update(chunk)
    return h.hexdigest()


def process_received_file(file_path: Path) -> dict:
    """对收到的文件做简单分析，返回结果摘要。"""
    stat = file_path.stat()
    digest = sha256_file(file_path)
    lines = 0
    try:
        text = file_path.read_text(errors="replace")
        lines = text.count("\n")
        is_text = True
    except Exception:
        is_text = False

    result = {
        "filename": file_path.name,
        "size_bytes": stat.st_size,
        "sha256": digest,
        "is_text": is_text,
        "lines": lines if is_text else None,
        "processed_at": time.strftime("%Y-%m-%dT%H:%M:%SZ", time.gmtime()),
    }
    return result


def write_result(save_dir: Path, filename: str, result: dict):
    """将处理结果写入 <filename>.result，供 sender 读取。"""
    result_path = save_dir / (filename + ".result")
    summary_lines = [
        f"file:       {result['filename']}",
        f"size:       {result['size_bytes']} bytes",
        f"sha256:     {result['sha256']}",
        f"text_lines: {result['lines'] if result['is_text'] else 'N/A (binary)'}",
        f"processed:  {result['processed_at']}",
        "status:     OK",
    ]
    result_path.write_text("\n".join(summary_lines) + "\n")
    print(f"[receiver] wrote result → {result_path.name}")


# ── aerosync receive 子进程 ───────────────────────────────────────────────────

def start_receiver_process(save_dir: Path, host: str, token: Optional[str]) -> subprocess.Popen:
    """后台启动 aerosync receive，返回 Popen 句柄。"""
    # 从 host:port 中分离出 port
    port = host.split(":")[-1] if ":" in host else "7788"
    cmd = ["aerosync", "receive", "--save-to", str(save_dir), "--port", port, "--http-only"]
    if token:
        cmd += ["--auth-token", token]

    proc = subprocess.Popen(
        cmd,
        stdout=subprocess.PIPE,
        stderr=subprocess.STDOUT,
        text=True,
    )
    print(f"[receiver] started aerosync receive  (pid={proc.pid})")
    return proc


async def drain_receiver_logs(proc: subprocess.Popen, stop_event: asyncio.Event):
    """异步读取 aerosync receive 的 stdout/stderr 并打印。"""
    loop = asyncio.get_event_loop()
    while not stop_event.is_set():
        line = await loop.run_in_executor(None, proc.stdout.readline)
        if not line:
            break
        print(f"[aerosync] {line.rstrip()}")


# ── WS 事件监听 ───────────────────────────────────────────────────────────────

async def watch_events(
    ws_url: str,
    save_dir: Path,
    stop_event: asyncio.Event,
    startup_grace: float = 3.0,
):
    """订阅 /ws，处理文件到达事件。"""
    # 等待 aerosync receive 启动完毕
    await asyncio.sleep(startup_grace)

    retry_delay = 2
    while not stop_event.is_set():
        try:
            async with websockets.connect(ws_url) as ws:
                print(f"[receiver] connected to {ws_url}")
                retry_delay = 2
                async for raw in ws:
                    if stop_event.is_set():
                        break
                    try:
                        ev = json.loads(raw)
                    except json.JSONDecodeError:
                        continue

                    event_type = ev.get("event", "?")
                    fname = ev.get("filename", "?")

                    if event_type == "transfer_started":
                        print(f"[receiver] ↙ incoming: {fname}")
                    elif event_type == "progress":
                        pct = ev.get("percent", 0)
                        print(f"[receiver] ↙ progress: {fname}  {pct:.0f}%")
                    elif event_type == "completed":
                        size = ev.get("size", 0)
                        sha256 = ev.get("sha256", "")
                        print(
                            f"[receiver] ✓ received: {fname}"
                            f"  ({size} bytes, sha256={sha256[:8]}…)"
                        )
                        # 处理文件并回写结果
                        file_path = save_dir / fname
                        if file_path.exists():
                            result = process_received_file(file_path)
                            write_result(save_dir, fname, result)
                        else:
                            print(f"[receiver] ⚠ file not found: {file_path}")
                    elif event_type == "failed":
                        reason = ev.get("reason", "unknown")
                        print(f"[receiver] ✗ failed:   {fname}  ({reason})")

        except (websockets.exceptions.ConnectionClosedOK,
                websockets.exceptions.ConnectionClosedError):
            if stop_event.is_set():
                break
            print(f"[receiver] WS disconnected, retrying in {retry_delay}s …")
        except OSError:
            if stop_event.is_set():
                break
            print(f"[receiver] WS unreachable, retrying in {retry_delay}s …")

        await asyncio.sleep(retry_delay)
        retry_delay = min(retry_delay * 2, 60)


# ── 主入口 ────────────────────────────────────────────────────────────────────

async def main():
    parser = argparse.ArgumentParser(description="AeroSync receiver agent")
    parser.add_argument(
        "--save-dir",
        default="/tmp/aerosync_recv",
        help="文件保存目录 (default: /tmp/aerosync_recv)",
    )
    parser.add_argument(
        "--host",
        default="localhost:7788",
        help="监听地址 (default: localhost:7788)",
    )
    parser.add_argument("--token", default=None, help="认证 token（可选）")
    args = parser.parse_args()

    save_dir = Path(args.save_dir)
    save_dir.mkdir(parents=True, exist_ok=True)

    host = args.host
    ws_url = f"ws://{host}/ws" if not host.startswith("ws") else host

    stop_event = asyncio.Event()

    # 启动 aerosync receive 子进程
    proc = start_receiver_process(save_dir, host, args.token)

    def _shutdown(*_):
        print("\n[receiver] shutting down …")
        stop_event.set()

    # 注册信号处理
    loop = asyncio.get_event_loop()
    for sig in (signal.SIGINT, signal.SIGTERM):
        loop.add_signal_handler(sig, _shutdown)

    try:
        await asyncio.gather(
            drain_receiver_logs(proc, stop_event),
            watch_events(ws_url, save_dir, stop_event),
        )
    finally:
        stop_event.set()
        if proc.poll() is None:
            print("[receiver] stopping aerosync receive …")
            proc.terminate()
            try:
                proc.wait(timeout=5)
            except subprocess.TimeoutExpired:
                proc.kill()
        print("[receiver] stopped.")


if __name__ == "__main__":
    try:
        asyncio.run(main())
    except KeyboardInterrupt:
        pass
