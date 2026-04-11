#!/usr/bin/env python3
"""
sender_agent.py — AeroSync 双向协作示例：发送端 Agent

功能：
  1. 监控本地 watch_dir 目录，发现新文件后调用 aerosync send 发送
  2. 维护 SHA-256 记录，避免重复发送
  3. 订阅接收端 /ws，实时监听 completed / failed 事件
  4. 收到接收端回传的 .result 文件后打印处理结果

依赖：
  pip install websockets

运行：
  python examples/sender_agent.py --watch-dir /tmp/send_inbox
"""

import argparse
import asyncio
import hashlib
import json
import os
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


def run_send(file_path: Path, host: str, token: Optional[str]) -> bool:
    """调用 aerosync send，返回是否成功。"""
    # 使用完整 HTTP URL 避免 auto-upgrade 到 QUIC
    destination = f"http://{host}/upload"
    cmd = ["aerosync", "send", str(file_path), destination]
    if token:
        cmd += ["--token", token]
    print(f"[sender] → sending {file_path.name} …")
    result = subprocess.run(cmd, capture_output=True, text=True)
    if result.returncode == 0:
        print(f"[sender] ✓ sent {file_path.name}")
        return True
    else:
        print(f"[sender] ✗ send failed:\n{result.stderr.strip()}")
        return False


# ── WS 事件监听 ───────────────────────────────────────────────────────────────

async def watch_events(ws_url: str, results_dir: Path, stop_event: asyncio.Event):
    """订阅接收端 /ws，打印传输事件；收到 .result 文件时读取并打印。"""
    retry_delay = 2
    while not stop_event.is_set():
        try:
            async with websockets.connect(ws_url) as ws:
                print(f"[sender] connected to {ws_url}")
                retry_delay = 2  # 重连成功后重置
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
                        print(f"[sender] ↗ started:   {fname}")
                    elif event_type == "progress":
                        pct = ev.get("percent", 0)
                        print(f"[sender] ↗ progress:  {fname}  {pct:.0f}%")
                    elif event_type == "completed":
                        size = ev.get("size", 0)
                        sha256 = ev.get("sha256", "")
                        print(
                            f"[sender] ✓ completed: {fname}"
                            f"  ({size} bytes, sha256={sha256[:8]}…)"
                        )
                        # 检查是否有接收端回传的 .result 文件
                        result_file = results_dir / (fname + ".result")
                        await asyncio.sleep(0.5)  # 给接收端处理时间
                        if result_file.exists():
                            print(f"[sender] ← result: {result_file.read_text().strip()}")
                    elif event_type == "failed":
                        reason = ev.get("reason", "unknown")
                        print(f"[sender] ✗ failed:    {fname}  ({reason})")
        except (websockets.exceptions.ConnectionClosedOK,
                websockets.exceptions.ConnectionClosedError):
            if stop_event.is_set():
                break
            print(f"[sender] WS disconnected, retrying in {retry_delay}s …")
        except OSError:
            if stop_event.is_set():
                break
            print(f"[sender] WS unreachable, retrying in {retry_delay}s …")

        await asyncio.sleep(retry_delay)
        retry_delay = min(retry_delay * 2, 60)


# ── 目录监控 ──────────────────────────────────────────────────────────────────

async def watch_dir(
    watch_path: Path,
    host: str,
    token: Optional[str],
    stop_event: asyncio.Event,
):
    """轮询 watch_dir，发现新文件后发送。"""
    sent: dict[str, str] = {}  # filename → sha256

    print(f"[sender] watching {watch_path}  (polling every 1s)")
    while not stop_event.is_set():
        try:
            entries = list(watch_path.iterdir())
        except FileNotFoundError:
            await asyncio.sleep(1)
            continue

        for entry in entries:
            if not entry.is_file():
                continue
            if entry.suffix == ".result":
                continue  # 忽略回传文件

            digest = sha256_file(entry)
            if sent.get(entry.name) == digest:
                continue  # 未变化，跳过

            sent[entry.name] = digest
            # 在线程池中运行阻塞的 subprocess
            loop = asyncio.get_event_loop()
            await loop.run_in_executor(None, run_send, entry, host, token)

        await asyncio.sleep(1)


# ── 主入口 ────────────────────────────────────────────────────────────────────

async def main():
    parser = argparse.ArgumentParser(description="AeroSync sender agent")
    parser.add_argument(
        "--watch-dir",
        default="/tmp/aerosync_send",
        help="监控目录，新文件将自动发送 (default: /tmp/aerosync_send)",
    )
    parser.add_argument(
        "--host",
        default="localhost:7788",
        help="接收端地址 (default: localhost:7788)",
    )
    parser.add_argument("--token", default=None, help="认证 token（可选）")
    parser.add_argument(
        "--results-dir",
        default=None,
        help="接收端回传结果目录 (default: same as --watch-dir)",
    )
    args = parser.parse_args()

    watch_path = Path(args.watch_dir)
    watch_path.mkdir(parents=True, exist_ok=True)

    results_dir = Path(args.results_dir) if args.results_dir else watch_path

    host = args.host
    ws_url = f"ws://{host}/ws" if not host.startswith("ws") else host

    stop_event = asyncio.Event()

    try:
        await asyncio.gather(
            watch_events(ws_url, results_dir, stop_event),
            watch_dir(watch_path, host, args.token, stop_event),
        )
    except (KeyboardInterrupt, asyncio.CancelledError):
        print("\n[sender] shutting down …")
        stop_event.set()


if __name__ == "__main__":
    try:
        asyncio.run(main())
    except KeyboardInterrupt:
        pass
