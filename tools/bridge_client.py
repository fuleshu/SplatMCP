#!/usr/bin/env python3
"""Scripted client for the SplatMCP bridge.

Used by the milestone 4 verification steps to drive the desktop app the same way the
MCP server does, without needing an MCP client. Reads bridge.json from the app data
directory (LOCALAPPDATA/com.splatmcp.app or SPLATMCP_DATA_DIR) and speaks the
line-delimited JSON protocol over 127.0.0.1.

Examples:
    python tools/bridge_client.py ping
    python tools/bridge_client.py status
    python tools/bridge_client.py set-camera --fit
    python tools/bridge_client.py screenshot --width 640 --out shot.png
    python tools/bridge_client.py load-ply sample.ply
"""

from __future__ import annotations

import argparse
import base64
import json
import os
import socket
import sys
from pathlib import Path

PROTOCOL_VERSION = 1


def data_dir() -> Path:
    override = os.environ.get("SPLATMCP_DATA_DIR")
    if override:
        return Path(override)
    local = os.environ.get("LOCALAPPDATA")
    if local:
        return Path(local) / "com.splatmcp.app"
    raise SystemExit("neither SPLATMCP_DATA_DIR nor LOCALAPPDATA is set")


class Bridge:
    def __init__(self, timeout: float = 60.0) -> None:
        descriptor_path = data_dir() / "bridge.json"
        if not descriptor_path.is_file():
            raise SystemExit(f"no running app: {descriptor_path} does not exist")
        descriptor = json.loads(descriptor_path.read_text(encoding="utf-8"))
        if descriptor.get("protocol") != PROTOCOL_VERSION:
            raise SystemExit(f"unsupported protocol {descriptor.get('protocol')}")
        self.token = descriptor["token"]
        self.timeout = timeout
        self.next_id = 1
        self.sock = socket.create_connection(("127.0.0.1", descriptor["port"]), timeout=timeout)
        self.sock.settimeout(timeout)
        self.reader = self.sock.makefile("rb")
        hello = self.call(
            "hello", {"client": "tools/bridge_client.py", "protocol": PROTOCOL_VERSION}
        )
        self.app_version = hello.get("app_version")

    def call(self, method: str, params=None):
        request_id = self.next_id
        self.next_id += 1
        frame = json.dumps(
            {"id": request_id, "token": self.token, "method": method, "params": params or {}}
        )
        self.sock.sendall(frame.encode("utf-8") + b"\n")
        line = self.reader.readline()
        if not line:
            raise SystemExit("the app closed the connection")
        response = json.loads(line)
        if response.get("id") != request_id:
            raise SystemExit(f"out of order response: {response}")
        if not response.get("ok"):
            raise SystemExit(f"{method} failed: {response.get('error')}")
        return response.get("result")

    def close(self) -> None:
        self.reader.close()
        self.sock.close()


def main() -> int:
    parser = argparse.ArgumentParser(description="Drive the SplatMCP bridge")
    parser.add_argument("--timeout", type=float, default=60.0)
    sub = parser.add_subparsers(dest="command", required=True)
    sub.add_parser("ping")
    sub.add_parser("status")
    sub.add_parser("camera")

    camera = sub.add_parser("set-camera")
    camera.add_argument("--position", nargs=3, type=float)
    camera.add_argument("--target", nargs=3, type=float)
    camera.add_argument("--fov", type=float)
    camera.add_argument("--azimuth", type=float)
    camera.add_argument("--elevation", type=float)
    camera.add_argument("--distance", type=float)
    camera.add_argument("--fit", action="store_true")

    shot = sub.add_parser("screenshot")
    shot.add_argument("--width", type=int)
    shot.add_argument("--height", type=int)
    shot.add_argument("--format", default="png")
    shot.add_argument("--quality", type=int)
    shot.add_argument("--out", type=Path, default=Path("screenshot.png"))

    load = sub.add_parser("load-ply")
    load.add_argument("path", type=Path)

    args = parser.parse_args()
    bridge = Bridge(timeout=args.timeout)
    try:
        if args.command == "ping":
            print(json.dumps(bridge.call("app_ping"), indent=2))
        elif args.command == "status":
            print(json.dumps(bridge.call("viewer_status"), indent=2))
        elif args.command == "camera":
            print(json.dumps(bridge.call("viewer_get_camera"), indent=2))
        elif args.command == "set-camera":
            params = {}
            if args.position:
                params["position"] = args.position
            if args.target:
                params["target"] = args.target
            if args.fov is not None:
                params["fov"] = args.fov
            if args.azimuth is not None:
                params["azimuth"] = args.azimuth
            if args.elevation is not None:
                params["elevation"] = args.elevation
            if args.distance is not None:
                params["distance"] = args.distance
            if args.fit:
                params["fit"] = True
            print(json.dumps(bridge.call("viewer_set_camera", params), indent=2))
        elif args.command == "screenshot":
            params = {"format": args.format}
            if args.width:
                params["width"] = args.width
            if args.height:
                params["height"] = args.height
            if args.quality is not None:
                params["quality"] = args.quality
            capture = bridge.call("viewer_capture", params)
            payload = base64.b64decode(capture["data_base64"])
            args.out.write_bytes(payload)
            print(
                json.dumps(
                    {
                        "wrote": str(args.out),
                        "bytes": len(payload),
                        "mime_type": capture["mime_type"],
                        "frame_size": [capture.get("width"), capture.get("height")],
                        "camera": capture.get("camera"),
                    },
                    indent=2,
                )
            )
        elif args.command == "load-ply":
            payload = base64.b64encode(args.path.read_bytes()).decode("ascii")
            print(
                json.dumps(
                    bridge.call(
                        "viewer_load_ply",
                        {"ply_base64": payload, "file_name": args.path.name, "frame": True},
                    ),
                    indent=2,
                )
            )
    finally:
        bridge.close()
    return 0


if __name__ == "__main__":
    sys.exit(main())
