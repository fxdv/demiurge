#!/usr/bin/env python3
"""Prefill handoff shim: vLLM prefill pool → Demiurge KV handoff headers.

Listens on the router-facing port (e.g. 9001). Forwards chat completions to the
local vLLM worker (e.g. 9101) with max_tokens=1, then returns only the
x-demiurge-* handoff headers the router expects on the disaggregated path.

Health and /v1/models are proxied unchanged for startup probes.
"""
from __future__ import annotations

import argparse
import json
import math
import os
import re
import threading
import urllib.error
import urllib.request
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

CACHE_BLOCK_TOKENS = 256
KV_METADATA_OVERHEAD = 0.08
KV_FRAGMENTATION_SLACK = 0.05

_handle_lock = threading.Lock()
_next_handle = 1


def kv_reserved(prompt_tokens: int, bytes_per_token: int) -> int:
    block = max(CACHE_BLOCK_TOKENS, 1)
    kv_tokens = math.ceil(prompt_tokens / block) * block
    kv_payload = kv_tokens * bytes_per_token
    kv_metadata = math.ceil(kv_payload * KV_METADATA_OVERHEAD)
    kv_fragment = math.ceil(kv_payload * KV_FRAGMENTATION_SLACK)
    return int(kv_payload + kv_metadata + kv_fragment)


def next_kv_handle() -> int:
    global _next_handle
    with _handle_lock:
        h = _next_handle
        _next_handle += 1
        return h


def parse_tokens(headers, body: bytes) -> int:
    raw = headers.get("X-Demiurge-Tokens") or headers.get("x-demiurge-tokens")
    if raw:
        try:
            return max(1, int(raw.strip()))
        except ValueError:
            pass
    try:
        payload = json.loads(body)
        messages = payload.get("messages") or []
        text = " ".join(
            str(m.get("content", "")) for m in messages if isinstance(m, dict)
        )
        # Rough token estimate (~4 chars/token) when header absent.
        return max(513, len(text) // 4 + 1)
    except json.JSONDecodeError:
        return 513


def patch_prefill_body(body: bytes, prefill_max_tokens: int) -> bytes:
    try:
        payload = json.loads(body)
    except json.JSONDecodeError:
        return body
    payload["max_tokens"] = prefill_max_tokens
    payload["temperature"] = payload.get("temperature", 0)
    return json.dumps(payload).encode()


def make_handler(backend_port: int, bytes_per_token: int, prefill_max_tokens: int):
    backend_base = f"http://127.0.0.1:{backend_port}"

    class Handler(BaseHTTPRequestHandler):
        protocol_version = "HTTP/1.1"

        def log_message(self, fmt: str, *args) -> None:
            print(f"[shim:{self.server.server_port}] {self.address_string()} {fmt % args}")

        def _proxy_raw(self, method: str, body: bytes | None = None) -> None:
            if body is None:
                length = int(self.headers.get("Content-Length", 0))
                body = self.rfile.read(length) if length else b""
            headers = {
                k: v
                for k, v in self.headers.items()
                if k.lower() not in ("host", "content-length", "transfer-encoding")
            }
            if body:
                headers["Content-Length"] = str(len(body))
            req = urllib.request.Request(
                f"{backend_base}{self.path}",
                data=body if body else None,
                headers=headers,
                method=method,
            )
            try:
                with urllib.request.urlopen(req, timeout=300) as resp:
                    data = resp.read()
                    self.send_response(resp.status)
                    for key, val in resp.headers.items():
                        if key.lower() in ("transfer-encoding", "connection"):
                            continue
                        self.send_header(key, val)
                    self.end_headers()
                    self.wfile.write(data)
            except urllib.error.HTTPError as e:
                data = e.read()
                self.send_response(e.code)
                self.send_header("Content-Length", str(len(data)))
                self.end_headers()
                self.wfile.write(data)

        def do_GET(self) -> None:
            self._proxy_raw("GET")

        def _send_handoff(self, handle: int, kv_bytes: int) -> None:
            self.send_response(200)
            self.send_header("x-demiurge-prefill-done", "1")
            self.send_header("x-demiurge-kv-handle", str(handle))
            self.send_header("x-demiurge-kv-bytes", str(kv_bytes))
            self.send_header("Content-Length", "0")
            self.send_header("Connection", "close")
            self.end_headers()
            self.close_connection = True

        def do_POST(self) -> None:
            if not self.path.rstrip("/").endswith("/chat/completions"):
                self._proxy_raw("POST")
                return

            length = int(self.headers.get("Content-Length", 0))
            body = self.rfile.read(length)
            tokens = parse_tokens(self.headers, body)
            kv_bytes = kv_reserved(tokens, bytes_per_token)
            handle = next_kv_handle()
            patched = patch_prefill_body(body, prefill_max_tokens)

            headers = {
                "Content-Type": "application/json",
                "Content-Length": str(len(patched)),
            }
            for key in ("Authorization",):
                if key in self.headers:
                    headers[key] = self.headers[key]

            req = urllib.request.Request(
                f"{backend_base}{self.path}",
                data=patched,
                headers=headers,
                method="POST",
            )
            try:
                with urllib.request.urlopen(req, timeout=300) as resp:
                    resp.read()
            except urllib.error.HTTPError as e:
                err_body = e.read()
                self.send_response(e.code)
                self.send_header("Content-Type", "application/json")
                self.send_header("Content-Length", str(len(err_body)))
                self.end_headers()
                self.wfile.write(err_body)
                return

            self._send_handoff(handle, kv_bytes)

        def do_HEAD(self) -> None:
            self._proxy_raw("HEAD")

    return Handler


def main() -> None:
    parser = argparse.ArgumentParser(description="vLLM prefill handoff shim")
    parser.add_argument("--listen", type=int, required=True, help="router-facing port")
    parser.add_argument("--backend", type=int, required=True, help="local vLLM port")
    parser.add_argument(
        "--bytes-per-token",
        type=int,
        default=int(os.environ.get("DEMIURGE_BYTES_PER_TOKEN", "128")),
    )
    parser.add_argument(
        "--prefill-max-tokens",
        type=int,
        default=int(os.environ.get("DEMIURGE_PREFILL_MAX_TOKENS", "1")),
    )
    args = parser.parse_args()

    handler = make_handler(args.backend, args.bytes_per_token, args.prefill_max_tokens)
    server = ThreadingHTTPServer(("127.0.0.1", args.listen), handler)
    print(
        f"prefill handoff shim listen=127.0.0.1:{args.listen} "
        f"backend=127.0.0.1:{args.backend} bpt={args.bytes_per_token}"
    )
    server.serve_forever()


if __name__ == "__main__":
    main()
