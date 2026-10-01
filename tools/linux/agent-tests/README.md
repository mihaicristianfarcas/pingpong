# Agent tests on the container's desktop

Helpers for `tools/linux/agent-desktop` (run them inside it with
`tools/linux/agent-desktop exec /src/tools/linux/agent-tests/NAME`):

- `mcp-client.py OUTDIR CMD...` (on the Mac): a scripted MCP client. Steps
  come from `STEPS_FILE` (JSON: `[{"tool": "left_click", "coordinate": [300, 300]}, ...]`);
  images land in OUTDIR. E.g.
  `STEPS_FILE=steps.json tools/linux/agent-tests/mcp-client.py /tmp/out tools/linux/agent-desktop mcp --host linux-desk`
- `watcher.sh`: a person watching the agent from a second screen (:2),
  taking over after 6 s and handing back 4.5 s later.
- `person.sh [SECS]`: a person streaming the desktop (an agent must not take
  it over; it takes over from an agent).
- `web.sh get|post PATH [JSON]`: Pong's web API, signed in as a test admin.
- `mock-llm.py`: scripted Anthropic Messages, OpenAI Responses and
  OpenAI-compatible chat APIs on 127.0.0.1:8900 that check every request's
  shape (problems go to `target/linux-out/agent/mock-llm.report`); run
  `ping-agent run` with `ANTHROPIC_BASE_URL=http://127.0.0.1:8900`,
  `OPENAI_BASE_URL=http://127.0.0.1:8900/v1` or `--provider custom
  --base-url http://127.0.0.1:8900/v1`, any key. It is Jev too, with
  `TYPESAFE_BASE_URL=http://127.0.0.1:8900` and any `TYPESAFE_API_KEY`: a
  click on anything named "Quit" or "Delete" is judged to delete, every turn
  ends with a question, every request is routine (the states it was asked
  about go to `target/linux-out/agent/mock-jev.log`).

What was run with them, and what it showed: [docs/ai-agents.md](../../../docs/ai-agents.md).
