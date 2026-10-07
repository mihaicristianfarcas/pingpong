//! Any OpenAI-compatible chat endpoint with function calling and images:
//! OpenRouter (hundreds of models behind one key), a local Ollama or LM
//! Studio, a company gateway. The computer's actions are offered as
//! functions -- the MCP server's tools -- and the screen after each turn
//! goes back as an image (tool messages carry no images in this API, so it
//! rides in a user message).
//!
//! Only the last few screenshots stay in the conversation: smaller models
//! have small windows, and an old screen is worth nothing.

use base64::Engine;
use serde_json::{json, Value};

use super::{Provider, RunContext, RunEvent};
use crate::computer::{Action, Computer};

/// Screenshots kept in the conversation.
const KEEP_IMAGES: usize = 3;
/// Tools a run offers: the actions (it is already connected).
const TOOLS: &[&str] = &[
    "screenshot",
    "zoom",
    "left_click",
    "right_click",
    "middle_click",
    "double_click",
    "triple_click",
    "left_click_drag",
    "mouse_move",
    "scroll",
    "type",
    "key",
    "wait",
    "wait_for_control",
    "share_plan",
    "ask_approval",
];

pub fn functions() -> Vec<Value> {
    crate::mcp::tool_list()
        .as_array()
        .into_iter()
        .flatten()
        .filter(|t| TOOLS.contains(&t["name"].as_str().unwrap_or_default()))
        .map(|t| json!({"type": "function", "function": {"name": t["name"], "description": t["description"], "parameters": t["inputSchema"]}}))
        .collect()
}

fn image_part(png: &[u8]) -> Value {
    json!({"type": "image_url", "image_url": {"url": format!("data:image/png;base64,{}", base64::engine::general_purpose::STANDARD.encode(png))}})
}

/// Replace all but the last `keep` images with a note.
fn prune_images(messages: &mut [Value], keep: usize) {
    let mut seen = 0;
    for m in messages.iter_mut().rev() {
        let Some(parts) = m["content"].as_array_mut() else {
            continue;
        };
        for p in parts.iter_mut().rev() {
            if p["type"] == "image_url" {
                seen += 1;
                if seen > keep {
                    *p = json!({"type": "text", "text": "[an earlier screenshot, removed]"});
                }
            }
        }
    }
}

pub fn endpoint(ctx: &RunContext) -> (String, Option<String>) {
    match ctx.settings.provider {
        Provider::Openrouter => (
            "https://openrouter.ai/api/v1".to_string(),
            super::secrets::key(&ctx.data_dir, Provider::Openrouter),
        ),
        _ => (
            ctx.settings
                .base_url
                .trim()
                .trim_end_matches('/')
                .to_string(),
            super::secrets::key(&ctx.data_dir, Provider::Custom),
        ),
    }
}

pub fn run(ctx: &RunContext, computer: &mut Computer) -> Result<String, String> {
    let (base, key) = endpoint(ctx);
    if base.is_empty() {
        return Err("Set the endpoint's URL (e.g. http://localhost:11434/v1 for Ollama).".into());
    }
    if ctx.settings.provider == Provider::Openrouter && key.is_none() {
        return Err("No OpenRouter API key.".into());
    }
    let model = ctx.settings.model_or_default();
    if model.is_empty() {
        return Err("Name the model to use.".into());
    }
    let http = super::http();
    let mut headers = Vec::new();
    if let Some(k) = key {
        headers.push(("authorization", format!("Bearer {k}")));
    }
    if ctx.settings.provider == Provider::Openrouter {
        headers.push(("x-title", "Ping".to_string()));
        headers.push((
            "http-referer",
            "https://github.com/mihaicristianfarcas/pingpong".to_string(),
        ));
    }
    let url = format!("{base}/chat/completions");
    let first = computer.act(Action::Screenshot)?;
    let mut messages = vec![
        json!({"role": "system", "content": super::prompt_for(ctx)}),
        json!({"role": "user", "content": [
            {"type": "text", "text": format!("{}\nThe screen now:", super::task_line(ctx))},
            image_part(&first.shot.as_ref().ok_or("no screenshot")?.png),
        ]}),
    ];
    let tools = functions();
    let mut totals = (0u64, 0u64, 0u64, 0.0f64);
    let mut last_text = String::new();
    loop {
        if let Some(r) = ctx.stop_reason() {
            return Err(r);
        }
        prune_images(&mut messages, KEEP_IMAGES);
        let mut body =
            json!({"model": model, "messages": messages, "tools": tools, "tool_choice": "auto"});
        if ctx.settings.provider == Provider::Openrouter {
            body["usage"] = json!({"include": true});
            // Test runs may insist on free models only (no charge, ever).
            if std::env::var("PING_AGENT_FREE_ONLY").is_ok_and(|v| v == "1") {
                body["provider"] = json!({"max_price": {"prompt": 0, "completion": 0}});
            }
        }
        ctx.emit(RunEvent::Status(format!("Asking {model}…")));
        let resp = super::post_json(&http, &url, &headers, &body)?;
        let u = &resp["usage"];
        totals.0 += u["prompt_tokens"].as_u64().unwrap_or(0);
        totals.1 += u["completion_tokens"].as_u64().unwrap_or(0);
        totals.2 += u["prompt_tokens_details"]["cached_tokens"]
            .as_u64()
            .unwrap_or(0);
        totals.3 += u["cost"].as_f64().unwrap_or(0.0);
        ctx.emit(RunEvent::Usage {
            input: totals.0,
            output: totals.1,
            cached: totals.2,
            cost_usd: u.get("cost").map(|_| totals.3),
        });
        let msg = resp["choices"][0]["message"].clone();
        if msg.is_null() {
            return Err(format!(
                "The endpoint answered without a message: {}",
                resp.to_string().chars().take(300).collect::<String>()
            ));
        }
        if let Some(t) = msg["content"].as_str().filter(|t| !t.trim().is_empty()) {
            ctx.emit(RunEvent::Thought(t.trim().to_string()));
            last_text = t.trim().to_string();
        }
        let calls = msg["tool_calls"].as_array().cloned().unwrap_or_default();
        // The assistant's message back as it came (content may be null).
        messages.push(json!({"role": "assistant", "content": msg["content"], "tool_calls": if calls.is_empty() { Value::Null } else { json!(calls) }}));
        if let Some(last) = messages.last_mut() {
            if calls.is_empty() {
                last.as_object_mut().map(|o| o.remove("tool_calls"));
            }
        }
        if calls.is_empty() {
            return Ok(last_text);
        }
        let mut shot = None;
        // A zoom's picture is a region, not the screen: say which.
        let mut zoomed = false;
        for call in &calls {
            let id = call["id"].as_str().unwrap_or_default();
            let name = call["function"]["name"].as_str().unwrap_or_default();
            let args: Value = match &call["function"]["arguments"] {
                Value::String(s) => serde_json::from_str(s).unwrap_or(json!({})),
                v @ Value::Object(_) => v.clone(),
                _ => json!({}),
            };
            let out = if name == "wait_for_control" {
                computer
                    .wait_for_control(std::time::Duration::from_secs(60))
                    .map(|t| crate::computer::Outcome {
                        text: t,
                        shot: None,
                    })
            } else if crate::mcp::FUNCTION_TOOLS.contains(&name) {
                crate::mcp::function_call(computer, name, &args)
            } else {
                match crate::mcp::action_for(name, &args) {
                    Ok(Some(action)) => computer.act(action),
                    Ok(None) => Err(format!("no tool named {name}")),
                    Err(e) => Err(e),
                }
            };
            let text = match &out {
                Ok(o) => {
                    if o.shot.is_some() {
                        shot = o.shot.clone();
                        zoomed = name == "zoom";
                    }
                    if o.text.is_empty() {
                        "OK".to_string()
                    } else {
                        o.text.clone()
                    }
                }
                Err(e) => format!("Error: {e}"),
            };
            messages.push(json!({"role": "tool", "tool_call_id": id, "content": text}));
            if ctx.stopped() {
                break;
            }
        }
        // Answer every call, even when stopping.
        let answered: std::collections::HashSet<String> = messages
            .iter()
            .filter(|m| m["role"] == "tool")
            .filter_map(|m| m["tool_call_id"].as_str().map(str::to_string))
            .collect();
        for call in &calls {
            let id = call["id"].as_str().unwrap_or_default();
            if !answered.contains(id) {
                messages.push(json!({"role": "tool", "tool_call_id": id, "content": "Not executed: the run was stopped."}));
            }
        }
        if let Some(s) = shot {
            let caption = if zoomed {
                "The region you zoomed into, enlarged (coordinates stay those of the full screen):"
            } else {
                "The screen after your last action:"
            };
            messages.push(json!({"role": "user", "content": [
                {"type": "text", "text": caption},
                image_part(&s.png),
            ]}));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_the_last_screenshots_stay() {
        let img = || json!({"type": "image_url", "image_url": {"url": "data:x"}});
        let mut m = vec![
            json!({"role": "user", "content": [img()]}),
            json!({"role": "user", "content": [img(), {"type": "text", "text": "t"}]}),
            json!({"role": "user", "content": [img()]}),
            json!({"role": "user", "content": [img()]}),
        ];
        prune_images(&mut m, 2);
        let kept: usize = m
            .iter()
            .flat_map(|x| x["content"].as_array().unwrap().iter())
            .filter(|p| p["type"] == "image_url")
            .count();
        assert_eq!(kept, 2);
        assert_eq!(m[0]["content"][0]["type"], "text");
        assert_eq!(m[3]["content"][0]["type"], "image_url");
    }

    #[test]
    fn functions_are_the_actions() {
        let f = functions();
        let names: Vec<&str> = f
            .iter()
            .map(|t| t["function"]["name"].as_str().unwrap())
            .collect();
        assert!(
            names.contains(&"left_click") && names.contains(&"type") && !names.contains(&"connect")
        );
        assert!(f
            .iter()
            .all(|t| t["function"]["parameters"]["type"] == "object"));
    }
}
