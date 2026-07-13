#!/usr/bin/env python3
"""Track C live smoke — router + vLLM P/D paths on reference GPU fleet.

Writes JSON summary to stdout; exit 0 only if all checks pass.
"""
from __future__ import annotations

import json
import os
import sys
import time
import urllib.error
import urllib.request

ROUTER = os.environ.get("DEMIURGE_ROUTER", "http://127.0.0.1:8080").rstrip("/")
MODEL = os.environ.get("VLLM_SERVED_NAME", "Meta-Llama-3.1-8B-Instruct")
TIMEOUT = float(os.environ.get("TRACK_C_SMOKE_TIMEOUT", "120"))


def probe(url: str) -> tuple[bool, int, str]:
    try:
        with urllib.request.urlopen(url, timeout=10) as resp:
            return resp.status == 200, resp.status, ""
    except urllib.error.HTTPError as e:
        return False, e.code, str(e)
    except Exception as e:
        return False, 0, str(e)


def chat(tokens_hdr: int, max_tokens: int = 4) -> tuple[bool, int, float, str]:
    body = json.dumps(
        {
            "model": MODEL,
            "messages": [{"role": "user", "content": "Say hi in one word."}],
            "max_tokens": max_tokens,
            "temperature": 0,
        }
    ).encode()
    req = urllib.request.Request(
        f"{ROUTER}/v1/chat/completions",
        data=body,
        headers={
            "Content-Type": "application/json",
            "X-Demiurge-Tokens": str(tokens_hdr),
        },
        method="POST",
    )
    t0 = time.perf_counter()
    try:
        with urllib.request.urlopen(req, timeout=TIMEOUT) as resp:
            data = json.loads(resp.read())
            ms = (time.perf_counter() - t0) * 1000
            ok = resp.status == 200 and bool(data.get("choices"))
            return ok, resp.status, ms, ""
    except urllib.error.HTTPError as e:
        ms = (time.perf_counter() - t0) * 1000
        return False, e.code, ms, e.read().decode(errors="replace")[:200]
    except Exception as e:
        ms = (time.perf_counter() - t0) * 1000
        return False, 0, ms, str(e)


def main() -> int:
    checks: list[dict] = []

    for port, label in (
        (8080, "router"),
        (9001, "prefill-shim-9001"),
        (9003, "decode-9003"),
    ):
        ok, code, err = probe(f"http://127.0.0.1:{port}/health" if port != 8080 else f"{ROUTER}/v1/models")
        checks.append(
            {
                "id": f"TC-PROBE-{label}",
                "pass": ok,
                "http_code": code,
                "error": err,
            }
        )

    ok, code, ms, err = chat(64)
    checks.append(
        {
            "id": "TC-LIVE-COLOCATED",
            "pass": ok,
            "http_code": code,
            "latency_ms": round(ms, 1),
            "error": err,
        }
    )

    ok, code, ms, err = chat(1024)
    checks.append(
        {
            "id": "TC-LIVE-DISAGG",
            "pass": ok,
            "http_code": code,
            "latency_ms": round(ms, 1),
            "error": err,
        }
    )

    summary = {
        "router": ROUTER,
        "model": MODEL,
        "checks": checks,
        "pass": all(c["pass"] for c in checks),
    }
    json.dump(summary, sys.stdout, indent=2)
    sys.stdout.write("\n")
    return 0 if summary["pass"] else 1


if __name__ == "__main__":
    raise SystemExit(main())
