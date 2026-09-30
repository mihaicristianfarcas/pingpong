#!/usr/bin/env python3
"""A scripted MCP client: runs tool calls against a stdio MCP server and saves images."""
import base64, json, subprocess, sys, time, os

out = sys.argv[1]
cmd = sys.argv[2:]
os.makedirs(out, exist_ok=True)
p = subprocess.Popen(cmd, stdin=subprocess.PIPE, stdout=subprocess.PIPE, text=True)
n = 0
def rpc(method, params=None, notify=False):
    global n
    msg = {"jsonrpc": "2.0", "method": method}
    if params is not None: msg["params"] = params
    if not notify:
        n += 1; msg["id"] = n
    p.stdin.write(json.dumps(msg) + "\n"); p.stdin.flush()
    if notify: return None
    line = p.stdout.readline()
    return json.loads(line)

shots = 0
def call(name, **args):
    global shots
    t = time.time()
    r = rpc("tools/call", {"name": name, "arguments": args})
    dt = time.time() - t
    res = r.get("result", {})
    texts = [c["text"] for c in res.get("content", []) if c["type"] == "text"]
    for c in res.get("content", []):
        if c["type"] == "image":
            shots += 1
            path = f"{out}/{shots:02d}-{name}.png"
            open(path, "wb").write(base64.b64decode(c["data"]))
            texts.append(f"[image {path}]")
    print(f"{name} {args} -> err={res.get('isError')} {dt:.2f}s {' | '.join(texts)[:300]}")
    return res

init = rpc("initialize", {"protocolVersion": "2025-06-18", "capabilities": {}, "clientInfo": {"name": "script", "version": "1"}})
print("server:", init["result"]["serverInfo"], init["result"]["protocolVersion"])
rpc("notifications/initialized", notify=True)
tools = rpc("tools/list")["result"]["tools"]
print(len(tools), "tools")
steps = json.loads(open(os.environ["STEPS_FILE"]).read()) if os.environ.get("STEPS_FILE") else json.loads(os.environ.get("STEPS", "[]"))
for s in steps:
    name = s.pop("tool")
    call(name, **s)
p.stdin.close()
p.wait(timeout=30)
