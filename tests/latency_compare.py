#!/usr/bin/env python3
"""Compare streaming Chat Completions latency for two loopback gateways."""

import argparse
import hashlib
import http.client
import ipaddress
import json
import math
import re
import sys
import time
from urllib.parse import urlsplit

VERSION = 1
MAX_STDIN = 64 * 1024
MAX_EVENT = 1024 * 1024
MAX_OUTPUT = 8 * 1024 * 1024
TIMEOUT = 180
SAFE_CODES = {
    "invalid_request",
    "unsupported_parameter",
    "invalid_api_key",
    "insufficient_scope",
    "rate_limit_exceeded",
    "model_not_found",
    "provider_unavailable",
    "busy",
    "request_timeout",
    "upstream_error",
    "internal_error",
    "input_too_large",
    "unauthorized",
    "forbidden",
    "not_found",
    "unknown_error",
}


class BenchmarkFailure(Exception):
    def __init__(self, status, code):
        super().__init__()
        self.status = status
        self.code = code


def parse_args():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--model", required=True, help="exact model ID used by both gateways")
    parser.add_argument("--samples", type=int, default=5, help="matched pairs, from 1 to 20 (default: 5)")
    parser.add_argument("--cli-url", default="http://127.0.0.1:8091/v1", help="CLI gateway base URL")
    parser.add_argument("--direct-url", default="http://127.0.0.1:8092/v1", help="direct gateway base URL")
    parser.add_argument("--prompt", default="Reply exactly ROUTER_OK", help="identical prompt sent to both gateways")
    args = parser.parse_args()
    if not 1 <= args.samples <= 20:
        parser.error("--samples must be between 1 and 20")
    for name in ("cli_url", "direct_url"):
        try:
            validate_loopback_url(getattr(args, name))
        except ValueError:
            parser.error(f"--{name.replace('_', '-')} must be an HTTP(S) loopback URL without user info, query, or fragment")
    return args


def validate_loopback_url(value):
    parsed = urlsplit(value)
    if parsed.scheme not in ("http", "https") or not parsed.hostname:
        raise ValueError
    if parsed.username is not None or parsed.password is not None or parsed.query or parsed.fragment:
        raise ValueError
    host = parsed.hostname.lower()
    if host != "localhost":
        try:
            if not ipaddress.ip_address(host).is_loopback:
                raise ValueError
        except ValueError:
            raise ValueError from None
    if parsed.port is not None and not 1 <= parsed.port <= 65535:
        raise ValueError
    path = parsed.path.rstrip("/")
    if not path:
        path = "/v1"
    return urlsplit(f"{parsed.scheme}://{parsed.netloc}{path}/chat/completions")


def read_credentials():
    line = sys.stdin.buffer.readline(MAX_STDIN + 1)
    if len(line) > MAX_STDIN or not line.endswith(b"\n"):
        raise ValueError("stdin must contain one JSON line no larger than 64 KiB")
    try:
        data = json.loads(line)
    except (UnicodeDecodeError, json.JSONDecodeError):
        raise ValueError("stdin must contain one JSON object") from None
    if not isinstance(data, dict):
        raise ValueError("stdin must contain one JSON object")
    common = data.get("api_key")
    cli_key = data.get("cli_key", common)
    direct_key = data.get("direct_key", common)
    if not isinstance(cli_key, str) or not cli_key or not isinstance(direct_key, str) or not direct_key:
        raise ValueError("provide api_key or both cli_key and direct_key as nonempty strings")
    return {"cli": cli_key, "direct": direct_key}


def safe_code(value):
    if isinstance(value, str) and re.fullmatch(r"[a-z0-9_]{1,64}", value) and value in SAFE_CODES:
        return value
    return "other"


def error_code_from_response(response, sock, deadline):
    code = "other"
    try:
        set_socket_deadline(sock, deadline)
        body = response.read(64 * 1024 + 1)
        if len(body) <= 64 * 1024:
            payload = json.loads(body)
            if isinstance(payload, dict):
                detail = payload.get("error")
                if isinstance(detail, dict):
                    code = safe_code(detail.get("code"))
    except Exception:
        pass
    return code


def set_socket_deadline(sock, deadline):
    remaining = deadline - time.perf_counter()
    if remaining <= 0:
        raise BenchmarkFailure("timeout", "deadline_exceeded")
    if sock is None:
        raise BenchmarkFailure("transport_error", "connection_failed")
    sock.settimeout(remaining)


def http_connection(url, timeout):
    if url.scheme == "https":
        return http.client.HTTPSConnection(url.hostname, url.port, timeout=timeout)
    return http.client.HTTPConnection(url.hostname, url.port, timeout=timeout)

def visible_text(chunk):
    choices = chunk.get("choices")
    if not isinstance(choices, list):
        return ""
    fragments = []
    for choice in choices:
        if not isinstance(choice, dict):
            continue
        delta = choice.get("delta")
        if not isinstance(delta, dict):
            continue
        content = delta.get("content")
        if isinstance(content, str):
            fragments.append(content)
        elif isinstance(content, list):
            for part in content:
                if isinstance(part, dict) and isinstance(part.get("text"), str):
                    fragments.append(part["text"])
        refusal = delta.get("refusal")
        if isinstance(refusal, str):
            fragments.append(refusal)
    return "".join(fragments)


def run_request(url, key, model, prompt):
    payload = json.dumps({
        "model": model,
        "messages": [{"role": "user", "content": prompt}],
        "stream": True,
    }, separators=(",", ":")).encode("utf-8")
    headers = {
        "Authorization": f"Bearer {key}",
        "Content-Type": "application/json",
        "Accept": "text/event-stream",
    }
    start = time.perf_counter()
    deadline = start + TIMEOUT
    first_visible = None
    output_hash = hashlib.sha256()
    output_bytes = 0
    saw_finish = False
    saw_done = False
    event_data = []
    event_size = 0
    connection = http_connection(url, TIMEOUT)
    try:
        # HTTPConnection talks directly to the parsed loopback host. It does
        # not use environment proxies or follow redirects.
        connection.request("POST", url.path, body=payload, headers=headers)
        sock = connection.sock
        set_socket_deadline(sock, deadline)
        response = connection.getresponse()
        if time.perf_counter() > deadline:
            raise BenchmarkFailure("timeout", "deadline_exceeded")
        if response.status < 200 or response.status >= 300:
            code = error_code_from_response(response, sock, deadline)
            raise BenchmarkFailure(f"http_{response.status}", code)
        while True:
            set_socket_deadline(sock, deadline)
            line = response.readline(MAX_EVENT + 1)
            if time.perf_counter() > deadline:
                raise BenchmarkFailure("timeout", "deadline_exceeded")
            if not line:
                break
            if len(line) > MAX_EVENT:
                raise BenchmarkFailure("protocol_error", "event_too_large")
            if line in (b"\n", b"\r\n"):
                if not event_data:
                    event_size = 0
                    continue
                data = b"\n".join(event_data)
                event_data = []
                event_size = 0
                if data == b"[DONE]":
                    saw_done = True
                    break
                if not data:
                    continue
                try:
                    chunk = json.loads(data)
                except (UnicodeDecodeError, json.JSONDecodeError):
                    raise BenchmarkFailure("protocol_error", "invalid_event") from None
                if not isinstance(chunk, dict):
                    raise BenchmarkFailure("protocol_error", "invalid_event")
                text = visible_text(chunk)
                if text:
                    encoded = text.encode("utf-8")
                    output_bytes += len(encoded)
                    if output_bytes > MAX_OUTPUT:
                        raise BenchmarkFailure("protocol_error", "output_limit")
                    if first_visible is None:
                        first_visible = time.perf_counter()
                    output_hash.update(encoded)
                choices = chunk.get("choices")
                if isinstance(choices, list):
                    for choice in choices:
                        if isinstance(choice, dict) and choice.get("finish_reason") is not None:
                            saw_finish = True
                continue
            if line.startswith(b"data:"):
                part = line[5:].lstrip()
                event_size += len(part)
                if event_size > MAX_EVENT:
                    raise BenchmarkFailure("protocol_error", "event_too_large")
                event_data.append(part.rstrip(b"\r\n"))
        total = time.perf_counter() - start
    except BenchmarkFailure:
        raise
    except TimeoutError:
        raise BenchmarkFailure("timeout", "deadline_exceeded") from None
    except (OSError, http.client.HTTPException):
        raise BenchmarkFailure("transport_error", "connection_failed") from None
    finally:
        connection.close()
    if not saw_done or not saw_finish:
        raise BenchmarkFailure("protocol_error", "missing_terminal")
    if first_visible is None:
        raise BenchmarkFailure("protocol_error", "no_visible_output")
    return {
        "status": "ok",
        "code": None,
        "ttft_ms": round((first_visible - start) * 1000, 3),
        "total_ms": round(total * 1000, 3),
        "output_sha256": output_hash.hexdigest(),
    }

def nearest_rank(values, percentile):
    if not values:
        return None
    ordered = sorted(values)
    return ordered[max(0, math.ceil(percentile * len(ordered)) - 1)]


def summarize(records):
    success = [record for record in records if record["status"] == "ok"]
    ttft = [record["ttft_ms"] for record in success]
    total = [record["total_ms"] for record in success]
    return {
        "sample_count": len(records),
        "failed_count": len(records) - len(success),
        "ttft_ms": {"raw": ttft, "p50": nearest_rank(ttft, 0.50), "p95": nearest_rank(ttft, 0.95)},
        "total_ms": {"raw": total, "p50": nearest_rank(total, 0.50), "p95": nearest_rank(total, 0.95)},
    }


def main():
    args = parse_args()
    try:
        keys = read_credentials()
    except ValueError as error:
        print(json.dumps({"error": str(error)}), file=sys.stderr)
        return 2

    urls = {
        "cli": validate_loopback_url(args.cli_url),
        "direct": validate_loopback_url(args.direct_url),
    }
    prompt_hash = hashlib.sha256(args.prompt.encode("utf-8")).hexdigest()
    records = {"cli": [], "direct": []}
    pairs = []
    for sample in range(args.samples):
        order = ("cli", "direct") if sample % 2 == 0 else ("direct", "cli")
        pair = {"sample": sample + 1, "order": list(order), "results": {}}
        for target in order:
            try:
                record = run_request(urls[target], keys[target], args.model, args.prompt)
            except BenchmarkFailure as failure:
                record = {
                    "status": failure.status,
                    "code": failure.code,
                    "ttft_ms": None,
                    "total_ms": None,
                    "output_sha256": None,
                }
            except Exception:
                # Never expose exception text: networking libraries may include
                # response content or request details in their diagnostics.
                record = {
                    "status": "transport_error",
                    "code": "request_failed",
                    "ttft_ms": None,
                    "total_ms": None,
                    "output_sha256": None,
                }
            records[target].append(record)
            pair["results"][target] = record
        pairs.append(pair)

    report = {
        "version": VERSION,
        "model": args.model,
        "prompt_sha256": prompt_hash,
        "sample_count": args.samples,
        "statistics": {target: summarize(values) for target, values in records.items()},
        "pairs": pairs,
        "interpretation": "Small local sample only; do not treat as a performance claim.",
    }
    print(json.dumps(report, separators=(",", ":")))
    return 1 if any(record["status"] != "ok" for values in records.values() for record in values) else 0


if __name__ == "__main__":
    try:
        raise SystemExit(main())
    except KeyboardInterrupt:
        print(json.dumps({"error": "interrupted"}), file=sys.stderr)
        raise SystemExit(130)
