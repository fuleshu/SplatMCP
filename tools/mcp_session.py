#!/usr/bin/env python3
"""Drive the SplatMCP MCP server over stdio, the way an MCP client does.

Starts `splatmcp-mcp.exe` (or uses --server), performs the MCP handshake, then runs the
requested tool calls and prints their content blocks. A tool reply that carries an image
is written to --out-dir and reported by size instead of being printed.

Examples:
    python tools/mcp_session.py --list
    python tools/mcp_session.py --call create_splat '{"shape":"sphere","count":500}'
    python tools/mcp_session.py --call get_screenshot '{"width":640}' --out-dir .tmp
"""

from __future__ import annotations

import argparse
import json
import os
import subprocess
import sys
from pathlib import Path

PROTOCOL_VERSION = "2025-06-18"


class McpSession:
    def __init__(self, server: Path, env: dict[str, str], cwd: Path) -> None:
        self.process = subprocess.Popen(
            [str(server)],
            stdin=subprocess.PIPE,
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            env=env,
            cwd=str(cwd),
            text=True,
            encoding="utf-8",
            bufsize=1,
        )
        self.next_id = 1

    def send(self, message: dict) -> None:
        assert self.process.stdin is not None
        self.process.stdin.write(json.dumps(message) + "\n")
        self.process.stdin.flush()

    def receive(self) -> dict:
        assert self.process.stdout is not None
        while True:
            line = self.process.stdout.readline()
            if not line:
                stderr = self.process.stderr.read() if self.process.stderr else ""
                raise SystemExit(f"the server closed the connection.\nstderr:\n{stderr}")
            line = line.strip()
            if line:
                return json.loads(line)

    def request(self, method: str, params: dict | None = None) -> dict:
        request_id = self.next_id
        self.next_id += 1
        self.send({"jsonrpc": "2.0", "id": request_id, "method": method, "params": params or {}})
        while True:
            message = self.receive()
            if message.get("id") == request_id:
                if "error" in message:
                    raise SystemExit(f"{method} failed: {message['error']}")
                return message.get("result", {})

    def notify(self, method: str, params: dict | None = None) -> None:
        self.send({"jsonrpc": "2.0", "method": method, "params": params or {}})

    def initialize(self) -> dict:
        result = self.request(
            "initialize",
            {
                "protocolVersion": PROTOCOL_VERSION,
                "capabilities": {},
                "clientInfo": {"name": "tools/mcp_session.py", "version": "1"},
            },
        )
        self.notify("notifications/initialized")
        return result

    def close(self) -> None:
        if self.process.stdin:
            self.process.stdin.close()
        try:
            self.process.wait(timeout=5)
        except subprocess.TimeoutExpired:
            self.process.kill()


def content_summary(result: dict, out_dir: Path, label: str) -> str:
    lines = []
    for index, block in enumerate(result.get("content", [])):
        kind = block.get("type")
        if kind == "text":
            lines.append(block.get("text", ""))
        elif kind == "image":
            data = block.get("data", "")
            # MCP spells this `mimeType`; accept both spellings so the helper keeps
            # working if the field name is normalised either way.
            mime = block.get("mime_type") or block.get("mimeType") or "image/png"
            extension = "jpg" if "jpeg" in mime else "png"
            path = out_dir / f"{label}-{index}.{extension}"
            import base64

            path.write_bytes(base64.b64decode(data))
            lines.append(
                json.dumps(
                    {
                        "image_written": str(path),
                        "mime_type": mime,
                        "base64_chars": len(data),
                    }
                )
            )
        else:
            lines.append(json.dumps({"unsupported_content": kind}))
    if result.get("isError"):
        lines.append("isError: true")
    return "\n".join(lines)


def main() -> int:
    root = Path(__file__).resolve().parent.parent
    parser = argparse.ArgumentParser(description="Run tools against the SplatMCP MCP server")
    parser.add_argument("--server", type=Path, default=root / "target" / "debug" / "splatmcp-mcp.exe")
    parser.add_argument("--data-dir", type=Path, default=root / ".tmp" / "appdata")
    parser.add_argument("--out-dir", type=Path, default=root / ".tmp")
    parser.add_argument(
        "--prefix",
        default="call",
        help="file name prefix for captured frames, so several calls do not collide",
    )
    parser.add_argument("--list", action="store_true", help="print the tool listing")
    parser.add_argument("--raw-listing", action="store_true", help="print tools/list verbatim")
    parser.add_argument(
        "--call",
        nargs=2,
        action="append",
        metavar=("TOOL", "JSON"),
        default=[],
        help="call a tool with JSON arguments",
    )
    args = parser.parse_args()

    env = dict(os.environ)
    env["SPLATMCP_DATA_DIR"] = str(args.data_dir)
    args.out_dir.mkdir(parents=True, exist_ok=True)

    session = McpSession(args.server, env, root)
    try:
        info = session.initialize()
        print(f"server: {info.get('serverInfo')} protocol {info.get('protocolVersion')}")

        if args.list or args.raw_listing or not args.call:
            listing = session.request("tools/list")
            tools = listing.get("tools", [])
            if args.raw_listing:
                print(json.dumps(listing, indent=2))
            else:
                # Report the size of the listing, because keeping it cheap in a model's
                # context is a requirement, not a nicety.
                encoded = json.dumps(listing)
                print(f"tools: {len(tools)}  listing bytes: {len(encoded)}")
                for tool in tools:
                    schema = json.dumps(tool.get("inputSchema", {}))
                    print(
                        f"  - {tool['name']}  desc={len(tool.get('description') or '')}b "
                        f"schema={len(schema)}b"
                    )

        for index, (tool, raw) in enumerate(args.call):
            arguments = json.loads(raw) if raw.strip() else {}
            result = session.request("tools/call", {"name": tool, "arguments": arguments})
            print(f"=== {tool} {raw} ===")
            print(content_summary(result, args.out_dir, f"{args.prefix}{index}-{tool}"))
    finally:
        session.close()
    return 0


if __name__ == "__main__":
    sys.exit(main())
