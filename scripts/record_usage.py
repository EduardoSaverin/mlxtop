#!/usr/bin/env python3
# SPDX-License-Identifier: MIT
"""Append allowlisted completion counters to MLXTOP_USAGE_FILE (macOS/Linux).

Import append_usage in your client, or pipe one completed JSON response to this
script. It performs no network requests and never writes response content.
"""

import argparse
import fcntl
import json
import os
import sys
import time
import uuid


PROVIDERS = {
    "omlx": "oMLX", "mlx-lm": "mlx-lm", "mlx_lm": "mlx-lm",
    "mlx_lm.server": "mlx-lm", "ollama": "Ollama",
    "llama.cpp": "llama.cpp", "llama-server": "llama.cpp",
    "lmstudio": "LM Studio", "lm studio": "LM Studio",
    "koboldcpp": "KoboldCpp", "localai": "LocalAI",
}


def _count(value):
    return type(value) is int and 0 <= value < 2**64


def _identifier(value):
    if not isinstance(value, str) or not value or len(value) > 120:
        raise ValueError("Identifiers must be nonempty strings of at most 120 characters")
    if any(not ch.isprintable() for ch in value):
        raise ValueError("Identifiers must be printable")
    return value


def usage_record(provider, response, *, request_id=None, model=None,
                 observed_at=None, ttft_ms=None):
    """Extract a completed response; callers must pass the final usage chunk."""
    provider = PROVIDERS.get(provider.lower())
    if provider is None:
        raise ValueError("Unsupported provider")
    if not isinstance(response, dict) or response.get("done") is False:
        raise ValueError("Expected a completed response")
    usage = response.get("usage") or response.get("stats") or response
    if not isinstance(usage, dict):
        raise ValueError("Missing usage counters")
    counters = {}
    for target, aliases in (
        ("prompt_tokens", ("prompt_tokens", "input_tokens", "prompt_eval_count")),
        ("completion_tokens", ("completion_tokens", "output_tokens", "total_output_tokens", "eval_count")),
    ):
        for key in aliases:
            if key in usage:
                if not _count(usage[key]):
                    raise ValueError("Token counters must be unsigned integers")
                counters[target] = usage[key]
                break
    if "prompt_tokens" not in counters:
        raise ValueError("Missing full prompt count; enable final streaming usage")
    cached = usage.get("cached_tokens")
    for key in ("input_tokens_details", "prompt_tokens_details"):
        if isinstance(usage.get(key), dict):
            cached = usage[key].get("cached_tokens", cached)
    if cached is not None:
        if not _count(cached) or cached > counters["prompt_tokens"]:
            raise ValueError("Invalid cached token count")
        counters["prompt_tokens_details"] = {"cached_tokens": cached}
    now = int(time.time())
    timestamp = now if observed_at is None else observed_at
    if not _count(timestamp) or timestamp > now:
        raise ValueError("Completion time must be Unix seconds, not in the future")
    record = {"provider": provider,
              "request_id": _identifier(str(uuid.uuid4()) if request_id is None else request_id),
              "observed_at": timestamp, "usage": counters}
    model = model if model is not None else response.get("model", response.get("model_instance_id"))
    if model is not None:
        record["model"] = _identifier(model)
    if ttft_ms is not None:
        if not _count(ttft_ms):
            raise ValueError("TTFT must be an explicitly measured integer in milliseconds")
        record["timings"] = {"time_to_first_token_ms": ttft_ms}
    return record


def append_usage(path, provider, response, **kwargs):
    """Append one record, using a file lock for cooperating concurrent clients."""
    record = usage_record(provider, response, **kwargs)
    data = (json.dumps(record, separators=(",", ":")) + "\n").encode()
    fd = os.open(path, os.O_WRONLY | os.O_CREAT | os.O_APPEND, 0o600)
    with os.fdopen(fd, "wb") as output:
        fcntl.flock(output, fcntl.LOCK_EX)
        output.write(data)
        output.flush()
    return record["request_id"]


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--provider", required=True)
    parser.add_argument("--file", default=os.environ.get("MLXTOP_USAGE_FILE"))
    parser.add_argument("--request-id")
    parser.add_argument("--model")
    parser.add_argument("--observed-at", type=int)
    parser.add_argument("--ttft-ms", type=int)
    args = parser.parse_args()
    if not args.file:
        parser.error("Set --file or MLXTOP_USAGE_FILE")
    try:
        append_usage(args.file, args.provider, json.load(sys.stdin),
                     request_id=args.request_id, model=args.model,
                     observed_at=args.observed_at, ttft_ms=args.ttft_ms)
    except (ValueError, OSError):
        # Do not echo an input response or credentials in errors.
        parser.exit(1, "Could not record usage: check completion counters, metadata and file permissions.\n")


if __name__ == "__main__":
    main()
