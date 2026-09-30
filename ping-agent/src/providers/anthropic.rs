//! Anthropic's Messages API with the computer toolset
//! (`computer_toolset_20260801`): Claude calls the toolset's members by name
//! (`screenshot`, `left_click`, `type`, ...), several a turn at times, and
//! each is answered with a `tool_result` echoing `toolset_name`. The member
//! names and inputs are the ones the MCP server takes, so they map through
//! the same code. Coordinates are the screenshot's pixels (the host's
//! display is the size the model sees: nothing is scaled).
//!
//! The history is only ever appended to (thinking blocks go back as they
//! came), with prompt caching over it; the server may fall back to another
//! model when one refuses (`fallbacks: "default"`).

use base64::Engine;
use serde_json::{json, Value};

use super::{RunContext, RunEvent};
use crate::computer::{Computer, Outcome};

/// The Messages API (`ANTHROPIC_BASE_URL` points it at a gateway or proxy,
/// as for Anthropic's own SDKs).
fn url() -> String {
    let base =
        std::env::var("ANTHROPIC_BASE_URL").unwrap_or_else(|_| "https://api.anthropic.com".into());
    format!("{}/v1/messages", base.trim_end_matches('/'))
}

fn image(png: &[u8]) -> Value {
    json!({"type": "image", "source": {"type": "base64", "media_type": "image/png",
        "data": base64::engine::general_purpose::STANDARD.encode(png)}})
}

fn result_block(id: &str, out: &Result<Outcome, String>, with_image: bool) -> Value {
    match out {
        Ok(o) => {
            let mut content = vec![
                json!({"type": "text", "text": if o.text.is_empty() { "OK" } else { &o.text }}),
            ];
            if let (true, Some(shot)) = (with_image, &o.shot) {
                content.push(image(&shot.png));
            }
            json!({"type": "tool_result", "tool_use_id": id, "toolset_name": "computer", "content": content})
        }
        Err(e) => {
            json!({"type": "tool_result", "tool_use_id": id, "toolset_name": "computer", "is_error": true, "content": e})
        }
    }
}

/// The computer toolset, and `share_plan` and `ask_approval` beside it.
fn tools() -> Value {
    let mut tools = vec![json!({"type": "computer_toolset_20260801"})];
    for name in crate::mcp::FUNCTION_TOOLS {
        let (description, schema) = crate::mcp::tool_spec(name);
        tools.push(json!({"name": name, "description": description, "input_schema": schema}));
    }
    Value::Array(tools)
}

/// Models that get the server's refusal fallbacks by default.
fn wants_fallbacks(model: &str) -> bool {
    model.starts_with("claude-opus-5") || model.starts_with("claude-fable-5")
}

pub fn run(ctx: &RunContext, computer: &mut Computer) -> Result<String, String> {
    let key = super::secrets::key(&ctx.data_dir, super::Provider::Anthropic)
        .ok_or("No Anthropic API key.")?;
    let model = ctx.settings.model_or_default();
    let http = super::http();
    let mut headers = vec![
        ("x-api-key", key),
        ("anthropic-version", "2023-06-01".to_string()),
    ];
    if wants_fallbacks(&model) {
        headers.push((
            "anthropic-beta",
            "server-side-fallback-2026-07-01".to_string(),
        ));
    }
    let first = computer.act(crate::computer::Action::Screenshot)?;
    let mut messages = vec![json!({"role": "user", "content": [
        {"type": "text", "text": super::task_line(ctx)},
        image(&first.shot.as_ref().ok_or("no screenshot")?.png),
    ]})];
    let effort = match ctx.settings.effort.as_str() {
        e @ ("low" | "medium" | "high" | "xhigh" | "max") => e,
        _ => "medium",
    };
    let mut totals = (0u64, 0u64, 0u64);
    let mut last_text = String::new();
    loop {
        if let Some(r) = ctx.stop_reason() {
            return Err(r);
        }
        let mut body = json!({
            "model": model,
            "max_tokens": 16000,
            "system": super::prompt_for(ctx),
            "thinking": {"type": "adaptive"},
            "output_config": {"effort": effort},
            "tools": tools(),
            "messages": messages,
            "cache_control": {"type": "ephemeral"},
        });
        if wants_fallbacks(&model) {
            body["fallbacks"] = json!("default");
        }
        ctx.emit(RunEvent::Status(format!("Asking {model}…")));
        let resp = super::post_json(&http, &url(), &headers, &body)?;
        let u = &resp["usage"];
        totals.0 += u["input_tokens"].as_u64().unwrap_or(0)
            + u["cache_creation_input_tokens"].as_u64().unwrap_or(0);
        totals.1 += u["output_tokens"].as_u64().unwrap_or(0);
        totals.2 += u["cache_read_input_tokens"].as_u64().unwrap_or(0);
        ctx.emit(RunEvent::Usage {
            input: totals.0,
            output: totals.1,
            cached: totals.2,
            cost_usd: None,
        });
        let content = resp["content"].as_array().cloned().unwrap_or_default();
        for block in &content {
            if block["type"] == "text" {
                let t = block["text"].as_str().unwrap_or_default().trim();
                if !t.is_empty() {
                    ctx.emit(RunEvent::Thought(t.to_string()));
                    last_text = t.to_string();
                }
            } else if block["type"] == "thinking" {
                // Progress notes, when the model shares them.
                if let Some(t) = block["thinking"].as_str().filter(|t| !t.trim().is_empty()) {
                    ctx.emit(RunEvent::Thought(t.trim().to_string()));
                }
            }
        }
        match resp["stop_reason"].as_str() {
            Some("refusal") => {
                let why = resp["stop_details"]["explanation"]
                    .as_str()
                    .unwrap_or("the model declined");
                return Err(format!("Claude declined to go on: {why}"));
            }
            Some("end_turn") | Some("stop_sequence") => return Ok(last_text),
            _ => {}
        }
        // The assistant's turn goes back exactly as it came.
        messages.push(json!({"role": "assistant", "content": content}));
        let calls: Vec<&Value> = content.iter().filter(|b| b["type"] == "tool_use").collect();
        if calls.is_empty() {
            // max_tokens or pause_turn: let it go on.
            messages.push(json!({"role": "user", "content": [{"type": "text", "text": "Go on."}]}));
            continue;
        }
        let mut results = Vec::new();
        let mut failed = false;
        for (i, call) in calls.iter().enumerate() {
            let id = call["id"].as_str().unwrap_or_default();
            if failed {
                results.push(json!({"type": "tool_result", "tool_use_id": id, "toolset_name": "computer", "is_error": true,
                    "content": "Not executed: an earlier computer action in this turn failed."}));
                continue;
            }
            let name = call["name"].as_str().unwrap_or_default();
            // Ours, not the toolset's: no computer action, and no toolset name.
            if call.get("toolset_name").is_none() && crate::mcp::FUNCTION_TOOLS.contains(&name) {
                let out = crate::mcp::function_call(computer, name, &call["input"]);
                results.push(match out {
                    Ok(o) => json!({"type": "tool_result", "tool_use_id": id, "content": o.text}),
                    Err(e) => json!({"type": "tool_result", "tool_use_id": id, "is_error": true, "content": e}),
                });
                continue;
            }
            let out = match crate::mcp::action_for(name, &call["input"]) {
                Ok(Some(action)) => computer.act(action),
                Ok(None) => Err(format!("{name} is not a computer action here")),
                Err(e) => Err(e),
            };
            failed = out.is_err();
            // A picture for a screenshot or zoom, and for the batch's last
            // action: the ones in between would be seen once and dropped.
            let with_image = matches!(name, "screenshot" | "zoom") || i + 1 == calls.len();
            results.push(result_block(id, &out, with_image));
            if ctx.stopped() {
                break;
            }
        }
        // Every tool_use answered, even when stopping.
        while results.len() < calls.len() {
            let id = calls[results.len()]["id"].as_str().unwrap_or_default();
            results.push(json!({"type": "tool_result", "tool_use_id": id, "toolset_name": "computer", "is_error": true,
                "content": "Not executed: the run was stopped."}));
        }
        messages.push(json!({"role": "user", "content": results}));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tool_results_echo_the_toolset() {
        let ok = result_block(
            "t1",
            &Ok(Outcome {
                text: "OK".into(),
                shot: None,
            }),
            true,
        );
        assert_eq!(ok["toolset_name"], "computer");
        assert_eq!(ok["content"][0]["text"], "OK");
        let err = result_block("t2", &Err("nope".into()), true);
        assert_eq!(err["is_error"], true);
        assert!(
            wants_fallbacks("claude-opus-5")
                && wants_fallbacks("claude-fable-5-1")
                && !wants_fallbacks("claude-sonnet-5")
        );
    }

    #[test]
    fn share_plan_and_ask_approval_are_offered_beside_the_toolset() {
        let t = tools();
        assert_eq!(t[0]["type"], "computer_toolset_20260801");
        assert_eq!(t[1]["name"], "share_plan");
        assert_eq!(t[1]["input_schema"]["required"][0], "steps");
        assert_eq!(t[2]["name"], "ask_approval");
        assert_eq!(t[2]["input_schema"]["required"][0], "action");
    }
}
