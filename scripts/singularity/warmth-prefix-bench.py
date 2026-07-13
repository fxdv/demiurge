#!/usr/bin/env python3
"""Warmth / prefix-locality bench for live P/D on singularity.

Sends repeated long-context requests with a shared prefix so the state plane
should accumulate warmth on the prefill worker that serves early traffic,
then measures whether later requests prefer that backend (via vLLM log deltas).
"""
from __future__ import annotations

import json
import os
import re
import subprocess
import time
import urllib.request
from concurrent.futures import ThreadPoolExecutor, as_completed

ROUTER = os.environ.get("DEMIURGE_ROUTER", "http://127.0.0.1:8080")
MODEL = os.environ.get("VLLM_SERVED_NAME", "Meta-Llama-3.1-8B-Instruct")
TOKENS_HDR = int(os.environ.get("BENCH_PROMPT_TOKENS", "2048"))
WARMUP = int(os.environ.get("BENCH_WARMUP", "8"))
RUNS = int(os.environ.get("BENCH_RUNS", "24"))
CONC = int(os.environ.get("BENCH_CONC", "2"))
PF_LOGS = os.environ.get(
    "PREFILL_VLLM_LOGS",
    f"{os.path.expanduser('~')}/vllm-workers/vllm-9101.log,"
    f"{os.path.expanduser('~')}/vllm-workers/vllm-9102.log",
)


def shared_prefix() -> str:
    chunk = (
        "The following system design document describes GPU prefill and decode "
        "disaggregation for large language model serving. "
    )
    # ~4 chars/token → repeat for long context
    reps = max(1, TOKENS_HDR // 16)
    return (chunk * reps)[: TOKENS_HDR * 4]


def count_post_lines(path: str) -> int:
    try:
        text = open(path, encoding="utf-8", errors="replace").read()
    except OSError:
        return 0
    return len(re.findall(r"POST /v1/chat/completions", text))


def chat(user_suffix: str) -> tuple[int, float]:
    prompt = shared_prefix() + user_suffix
    body = json.dumps(
        {
            "model": MODEL,
            "messages": [{"role": "user", "content": prompt}],
            "max_tokens": 16,
            "temperature": 0,
        }
    ).encode()
    req = urllib.request.Request(
        f"{ROUTER}/v1/chat/completions",
        data=body,
        headers={
            "Content-Type": "application/json",
            "X-Demiurge-Tokens": str(TOKENS_HDR),
        },
        method="POST",
    )
    t0 = time.perf_counter()
    with urllib.request.urlopen(req, timeout=180) as resp:
        code = resp.status
        resp.read()
    return code, (time.perf_counter() - t0) * 1000


def run_phase(label: str, n: int) -> list[float]:
    print(f"\n=== {label} (n={n}, conc={CONC}, tokens={TOKENS_HDR}) ===")
    latencies: list[float] = []
    with ThreadPoolExecutor(max_workers=CONC) as ex:
        futs = [
            ex.submit(chat, f" Question {i}: reply with one short sentence.")
            for i in range(n)
        ]
        for f in as_completed(futs):
            try:
                code, ms = f.result()
                if code == 200:
                    latencies.append(ms)
            except Exception as e:
                print(f"  err: {e}")
    if latencies:
        latencies.sort()
        p50 = latencies[len(latencies) // 2]
        p99 = latencies[int(len(latencies) * 0.99)]
        print(f"  ok={len(latencies)}/{n}  p50={p50:.0f}ms  p99={p99:.0f}ms")
    else:
        print("  no successful samples")
    return latencies


def log_snapshot() -> dict[str, int]:
    out: dict[str, int] = {}
    for path in PF_LOGS.split(","):
        path = path.strip()
        if path:
            out[path] = count_post_lines(path)
    return out


def main() -> None:
    print("Warmth prefix-locality bench")
    print(f"  router={ROUTER}  model={MODEL}")
    urllib.request.urlopen(f"{ROUTER}/v1/models", timeout=10).read()

    before = log_snapshot()
    run_phase("warmup", WARMUP)
    mid = log_snapshot()
    run_phase("measured", RUNS)
    after = log_snapshot()

    print("\n=== prefill vLLM POST /v1/chat/completions (log line counts) ===")
    for path in sorted(set(before) | set(after)):
        b, m, a = before.get(path, 0), mid.get(path, 0), after.get(path, 0)
        name = path.split("/")[-1]
        print(f"  {name}: before={b} after_warmup={m} final={a} (delta={a - b})")

    deltas = {p: after.get(p, 0) - before.get(p, 0) for p in after}
    if len(deltas) >= 2:
        vals = list(deltas.values())
        total = sum(vals)
        if total > 0:
            dominant = max(deltas, key=deltas.get)  # type: ignore[arg-type]
            share = deltas[dominant] / total
            print(f"\n  dominant prefill log: {dominant.split('/')[-1]} ({share:.0%} of vLLM forwards)")
            if share >= 0.55:
                print("  PASS: warmth skew visible (>55% on one prefill worker)")
            else:
                print("  NOTE: traffic still split — warmth may need more rounds or check state plane")


if __name__ == "__main__":
    main()
