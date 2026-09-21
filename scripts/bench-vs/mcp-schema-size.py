#!/usr/bin/env python3
"""Measure what an MCP server costs in context before it answers anything.

Drives a real stdio handshake (initialize -> notifications/initialized ->
tools/list) and reports the serialized size of the tool schemas the model has to
carry on every turn. Reading the numbers off the source would miss whatever the
server composes at runtime, so the server is actually started.

Token figures are bytes/4, pixel's own accounting convention -- an estimate,
not a tokenizer count.

Usage: mcp-schema-size.py <label> -- <command> [args...]
"""
import json
import subprocess
import sys
import time


def main():
    label = sys.argv[1]
    cmd = sys.argv[sys.argv.index("--") + 1:]
    p = subprocess.Popen(cmd, stdin=subprocess.PIPE, stdout=subprocess.PIPE,
                         stderr=subprocess.DEVNULL, text=True, bufsize=1)

    def send(obj):
        p.stdin.write(json.dumps(obj) + "\n")
        p.stdin.flush()

    send({"jsonrpc": "2.0", "id": 1, "method": "initialize",
          "params": {"protocolVersion": "2024-11-05", "capabilities": {},
                     "clientInfo": {"name": "bench", "version": "1"}}})
    p.stdout.readline()
    send({"jsonrpc": "2.0", "method": "notifications/initialized", "params": {}})
    send({"jsonrpc": "2.0", "id": 2, "method": "tools/list", "params": {}})

    tools, deadline = None, time.time() + 90
    while time.time() < deadline:
        line = p.stdout.readline()
        if not line:
            break
        try:
            msg = json.loads(line)
        except json.JSONDecodeError:
            continue           # servers that print a banner before the protocol
        if msg.get("id") == 2:
            tools = msg.get("result", {}).get("tools")
            break
    p.kill()

    if tools is None:
        print(json.dumps({"label": label, "error": "no tools/list response"}))
        sys.exit(1)

    payload = json.dumps(tools)
    print(json.dumps({
        "label": label,
        "tools": len(tools),
        "bytes": len(payload),
        "approx_tokens": len(payload) // 4,
        "per_tool": sorted(
            ({"name": t.get("name"), "bytes": len(json.dumps(t))} for t in tools),
            key=lambda d: -d["bytes"]),
    }, indent=2))


if __name__ == "__main__":
    main()
