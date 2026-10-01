#!/usr/bin/env python3
"""Scripted stand-ins for Anthropic's Messages API, OpenAI's Responses API and
an OpenAI-compatible chat API: each asks for two computer actions (click the
terminal, type a command), then says it is done. And for TypeSafe's System
One API (Jev): a click on anything named "Quit" or "Delete" is judged to
delete, every turn ends with a question, every request is routine. Every
request is checked against the shapes the real APIs take; problems go to
mock-llm.report, and Jev's states to mock-jev.log."""
import json, sys
from http.server import BaseHTTPRequestHandler, HTTPServer

REPORT = "/src/target/linux-out/agent/mock-llm.report"
JEV_LOG = "/src/target/linux-out/agent/mock-jev.log"
FUNCTIONS = ["share_plan", "ask_approval", "read_screen"]
problems = []
def check(cond, what):
    if not cond:
        problems.append(what)
        open(REPORT, "a").write("PROBLEM " + what + "\n")

def anthropic(req):
    tools = req.get("tools") or []
    check(tools[:1] == [{"type": "computer_toolset_20260801"}] and [t.get("name") for t in tools[1:]] == FUNCTIONS, "anthropic: tools are the computer toolset and the function tools")
    check(req.get("thinking", {}).get("type") == "adaptive", "anthropic: adaptive thinking")
    msgs = req["messages"]
    check(msgs[0]["role"] == "user" and any(b.get("type") == "image" for b in msgs[0]["content"]), "anthropic: first message has a screenshot")
    last = msgs[-1]
    results = [b for b in last["content"] if isinstance(b, dict) and b.get("type") == "tool_result"]
    if not results:
        return {"id": "m1", "type": "message", "role": "assistant", "stop_reason": "tool_use",
                "usage": {"input_tokens": 1000, "output_tokens": 50},
                "content": [
                    {"type": "thinking", "thinking": "", "signature": "sig"},
                    {"type": "text", "text": "I'll type in the terminal."},
                    {"type": "tool_use", "id": "tu1", "name": "left_click", "toolset_name": "computer", "input": {"coordinate": [300, 300]}},
                    {"type": "tool_use", "id": "tu2", "name": "type", "toolset_name": "computer", "input": {"text": "echo anthropic mock\n"}},
                ]}
    check(len(results) == 2, "anthropic: every tool_use answered")
    check(all(r.get("toolset_name") == "computer" for r in results), "anthropic: results echo toolset_name")
    check([r["tool_use_id"] for r in results] == ["tu1", "tu2"], "anthropic: results in order")
    # (A batch that stopped on an error ends with text only.)
    if not any(r.get("is_error") for r in results):
        check(any(c.get("type") == "image" for c in results[-1].get("content", []) if isinstance(c, dict)), "anthropic: the batch's last result has the screen")
    prev = msgs[-2]
    check(prev["role"] == "assistant" and prev["content"][0]["type"] == "thinking", "anthropic: the assistant turn went back as it came")
    return {"id": "m2", "type": "message", "role": "assistant", "stop_reason": "end_turn",
            "usage": {"input_tokens": 3000, "output_tokens": 20, "cache_read_input_tokens": 900},
            "content": [{"type": "text", "text": "Done: typed the command (anthropic mock)."}]}

def openai(req):
    tools = req.get("tools") or []
    check(tools[:1] == [{"type": "computer"}] and [(t.get("type"), t.get("name")) for t in tools[1:]] == [("function", f) for f in FUNCTIONS], "openai: tools are the computer tool and the function tools")
    inp = req["input"]
    if "previous_response_id" not in req:
        check(inp[0]["role"] == "user" and any(c["type"] == "input_image" for c in inp[0]["content"]), "openai: first input has a screenshot")
        return {"id": "r1", "status": "completed", "usage": {"input_tokens": 1000, "output_tokens": 40},
                "output": [
                    {"type": "message", "content": [{"type": "output_text", "text": "Typing in the terminal."}]},
                    {"type": "computer_call", "call_id": "c1", "status": "completed",
                     "actions": [{"type": "click", "button": "left", "x": 300, "y": 300}, {"type": "type", "text": "echo openai mock"}, {"type": "keypress", "keys": ["ENTER"]}],
                     "pending_safety_checks": [{"id": "sc1", "code": "malicious_instructions", "message": "Check this is what you want."}]},
                ]}
    check(req["previous_response_id"] == "r1", "openai: previous_response_id chains")
    out = [i for i in inp if i.get("type") == "computer_call_output"]
    check(len(out) == 1 and out[0]["call_id"] == "c1", "openai: the call answered")
    check(out and out[0]["output"]["type"] == "computer_screenshot" and out[0]["output"]["image_url"].startswith("data:image/png;base64,"), "openai: a screenshot data URL")
    check(out and out[0].get("acknowledged_safety_checks", [{}])[0].get("id") == "sc1", "openai: the safety check acknowledged")
    return {"id": "r2", "status": "completed", "usage": {"input_tokens": 3000, "output_tokens": 15},
            "output": [{"type": "message", "content": [{"type": "output_text", "text": "Done: typed the command (openai mock)."}]}]}

def chat(req):
    names = [t["function"]["name"] for t in req.get("tools", [])]
    check("left_click" in names and "type" in names, "chat: the actions are offered as functions")
    msgs = req["messages"]
    check(msgs[0]["role"] == "system", "chat: a system prompt")
    if not any(m["role"] == "tool" for m in msgs):
        return {"choices": [{"message": {"role": "assistant", "content": "Typing.", "tool_calls": [
            {"id": "t1", "type": "function", "function": {"name": "left_click", "arguments": json.dumps({"coordinate": [300, 300]})}},
            {"id": "t2", "type": "function", "function": {"name": "type", "arguments": json.dumps({"text": "echo chat mock\n"})}}]}}],
            "usage": {"prompt_tokens": 900, "completion_tokens": 30}}
    tools = [m for m in msgs if m["role"] == "tool"]
    check([t["tool_call_id"] for t in tools] == ["t1", "t2"], "chat: every call answered in order")
    check(msgs[-1]["role"] == "user" and any(p.get("type") == "image_url" for p in msgs[-1]["content"]), "chat: the screen after the calls")
    return {"choices": [{"message": {"role": "assistant", "content": "Done: typed the command (chat mock)."}}], "usage": {"prompt_tokens": 2000, "completion_tokens": 10}}

HAZARDS = {"deletes", "spends", "sends", "settings", "installs", "discards"}

def systemone(req):
    check(req.get("model") in ("jev-1.13.0", "jev-1.13"), "jev: the pinned model, as the service names it")
    qs, state = req.get("questions") or {}, req.get("state")
    open(JEV_LOG, "a").write(json.dumps({"questions": sorted(qs), "state": state}) + "\n")
    answers = {}
    if set(qs) == HAZARDS:
        check(all(q["type"] == "noul" and q.get("criteria") for q in qs.values()), "jev: a click's hazards are Nouls with criteria")
        check(isinstance(state, dict) and state.get("target", {}).get("element"), "jev: a click's state names what is clicked")
        named = json.dumps(state.get("target", {}).get("element", ""))
        risky = "Quit" in named or "Delete" in named
        answers = {k: {"type": "noul", "noul": 0.93 if risky and k == "deletes" else 0.03} for k in qs}
    elif set(qs) == {"ending"}:
        check(qs["ending"]["type"] == "choice" and {"done", "question", "needs_person", "not_done"} == set(qs["ending"]["criteria"]), "jev: an ending is one of four")
        check(isinstance(state, dict) and state.get("reply"), "jev: an ending reads the reply")
        answers = {"ending": {"type": "choice", "choice": "question", "probabilities": {"question": 0.95, "done": 0.05}, "confidence": 0.93}}
    elif set(qs) == {"work", "stakes"}:
        check(qs["work"]["type"] == "score" and len(qs["work"]["criteria"]) == 3, "jev: work is a three-level score")
        answers = {"work": {"type": "score", "score": 0.1, "confidence": 0.9}, "stakes": {"type": "noul", "noul": 0.02}}
    else:
        check(False, "jev: questions it knows: " + ",".join(sorted(qs)))
    return {"model": "jev-1.13.0", "answers": answers, "usage": {"input_tokens": 300, "output_tokens": 20}}

class H(BaseHTTPRequestHandler):
    def do_POST(self):
        body = json.loads(self.rfile.read(int(self.headers["content-length"])))
        if self.path.endswith("/v1/messages"):
            check(self.headers.get("x-api-key") and self.headers.get("anthropic-version"), "anthropic: key and version headers")
            out = anthropic(body)
        elif self.path.endswith("/responses"):
            check((self.headers.get("authorization") or "").startswith("Bearer "), "openai: bearer key")
            out = openai(body)
        elif self.path.endswith("/chat/completions"):
            out = chat(body)
        elif self.path.endswith("/v1/systemone"):
            check((self.headers.get("authorization") or "").startswith("Bearer "), "jev: bearer key")
            out = systemone(body)
        else:
            self.send_response(404); self.end_headers(); return
        data = json.dumps(out).encode()
        self.send_response(200); self.send_header("content-type", "application/json"); self.send_header("content-length", str(len(data))); self.end_headers(); self.wfile.write(data)
    def log_message(self, *a): pass

open(REPORT, "w").write("")
open(JEV_LOG, "w").write("")
HTTPServer(("127.0.0.1", 8900), H).serve_forever()
