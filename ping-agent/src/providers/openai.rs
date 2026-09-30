//! OpenAI's Responses API with its computer tool (`{"type": "computer"}`):
//! the model answers with `computer_call`s, each a batch of actions (click,
//! double_click, drag, move, scroll, keypress, type, wait, screenshot), and
//! is sent the screen after them (`computer_call_output`, at full detail).
//! A call can carry safety checks the user must acknowledge before it runs.

use base64::Engine;
use serde_json::{json, Value};

use super::{RunContext, RunEvent};
use crate::computer::{Action, Computer, Mouse};

/// The Responses API (`OPENAI_BASE_URL` points it at a gateway or proxy, as
/// for OpenAI's own SDKs).
fn url() -> String {
    let base =
        std::env::var("OPENAI_BASE_URL").unwrap_or_else(|_| "https://api.openai.com/v1".into());
    format!("{}/responses", base.trim_end_matches('/'))
}

fn data_url(png: &[u8]) -> String {
    format!(
        "data:image/png;base64,{}",
        base64::engine::general_purpose::STANDARD.encode(png)
    )
}

fn xy(a: &Value) -> Option<(u32, u32)> {
    Some((
        a["x"].as_f64()?.max(0.0).round() as u32,
        a["y"].as_f64()?.max(0.0).round() as u32,
    ))
}

/// Wheel notches for OpenAI's scroll pixels (about 100 a notch).
fn notches(px: f64) -> i32 {
    if px == 0.0 {
        return 0;
    }
    match (px / 100.0).round() as i32 {
        // Less than a notch still scrolls one.
        0 => px.signum() as i32,
        n => n.clamp(-50, 50),
    }
}

/// One of OpenAI's computer actions as ours.
pub fn action(a: &Value) -> Result<Action, String> {
    let kind = a["type"].as_str().unwrap_or_default();
    let at = || xy(a).ok_or_else(|| format!("{kind} needs x and y"));
    Ok(match kind {
        "click" => Action::Click {
            at: Some(at()?),
            button: Mouse::parse(a["button"].as_str().unwrap_or("left")).unwrap_or(Mouse::Left),
            count: 1,
            modifiers: None,
        },
        "double_click" => Action::Click {
            at: Some(at()?),
            button: Mouse::Left,
            count: 2,
            modifiers: None,
        },
        "move" => Action::MouseMove { at: at()? },
        "drag" => {
            let path: Vec<(u32, u32)> = a["path"]
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(xy)
                .collect();
            if path.len() < 2 {
                return Err("drag needs a path of two points or more".into());
            }
            Action::Drag {
                path,
                modifiers: None,
            }
        }
        "scroll" => Action::Scroll {
            at: xy(a),
            down: notches(a["scroll_y"].as_f64().unwrap_or(0.0)),
            right: notches(a["scroll_x"].as_f64().unwrap_or(0.0)),
            modifiers: None,
        },
        "keypress" => {
            let keys: Vec<&str> = a["keys"]
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(Value::as_str)
                .collect();
            if keys.is_empty() {
                return Err("keypress needs keys".into());
            }
            Action::Key {
                keys: keys.join("+"),
                repeat: 1,
            }
        }
        "type" => Action::Type {
            text: a["text"].as_str().unwrap_or_default().to_string(),
        },
        "wait" => Action::Wait {
            seconds: a["ms"].as_f64().map_or(2.0, |ms| (ms / 1000.0) as f32),
        },
        "screenshot" => Action::Screenshot,
        other => return Err(format!("unknown computer action {other}")),
    })
}

/// The computer tool, and `share_plan` and `ask_approval` beside it.
fn tools() -> Value {
    let mut tools = vec![json!({"type": "computer"})];
    for name in crate::mcp::FUNCTION_TOOLS {
        let (description, schema) = crate::mcp::tool_spec(name);
        tools.push(json!({"type": "function", "name": name, "description": description, "parameters": schema}));
    }
    Value::Array(tools)
}

pub fn run(ctx: &RunContext, computer: &mut Computer) -> Result<String, String> {
    let key =
        super::secrets::key(&ctx.data_dir, super::Provider::Openai).ok_or("No OpenAI API key.")?;
    let model = ctx.settings.model_or_default();
    let http = super::http();
    let headers = vec![("authorization", format!("Bearer {key}"))];
    let effort = match ctx.settings.effort.as_str() {
        e @ ("low" | "medium" | "high") => e,
        "xhigh" | "max" => "high",
        _ => "medium",
    };
    let first = computer.act(Action::Screenshot)?;
    let mut input = json!([{"role": "user", "content": [
        {"type": "input_text", "text": super::task_line(ctx)},
        {"type": "input_image", "image_url": data_url(&first.shot.as_ref().ok_or("no screenshot")?.png), "detail": "original"},
    ]}]);
    let mut previous: Option<String> = None;
    let mut totals = (0u64, 0u64, 0u64);
    let mut last_text = String::new();
    loop {
        if let Some(r) = ctx.stop_reason() {
            return Err(r);
        }
        let mut body = json!({
            "model": model,
            "tools": tools(),
            "instructions": super::prompt_for(ctx),
            "input": input,
            "reasoning": {"effort": effort},
            "truncation": "auto",
        });
        if let Some(p) = &previous {
            body["previous_response_id"] = json!(p);
        }
        ctx.emit(RunEvent::Status(format!("Asking {model}…")));
        let resp = super::post_json(&http, &url(), &headers, &body)?;
        previous = resp["id"].as_str().map(str::to_string);
        let u = &resp["usage"];
        totals.0 += u["input_tokens"].as_u64().unwrap_or(0);
        totals.1 += u["output_tokens"].as_u64().unwrap_or(0);
        totals.2 += u["input_tokens_details"]["cached_tokens"]
            .as_u64()
            .unwrap_or(0);
        ctx.emit(RunEvent::Usage {
            input: totals.0,
            output: totals.1,
            cached: totals.2,
            cost_usd: None,
        });
        let output = resp["output"].as_array().cloned().unwrap_or_default();
        let mut outputs = Vec::new();
        for item in &output {
            match item["type"].as_str() {
                Some("message") => {
                    for part in item["content"].as_array().into_iter().flatten() {
                        if let Some(t) = part["text"].as_str().filter(|t| !t.trim().is_empty()) {
                            ctx.emit(RunEvent::Thought(t.trim().to_string()));
                            last_text = t.trim().to_string();
                        }
                    }
                }
                // Ours, beside the computer tool.
                Some("function_call")
                    if crate::mcp::FUNCTION_TOOLS
                        .contains(&item["name"].as_str().unwrap_or_default()) =>
                {
                    let args: Value = item["arguments"]
                        .as_str()
                        .and_then(|a| serde_json::from_str(a).ok())
                        .unwrap_or(json!({}));
                    let text = match crate::mcp::function_call(
                        computer,
                        item["name"].as_str().unwrap_or_default(),
                        &args,
                    ) {
                        Ok(o) => o.text,
                        Err(e) => format!("Error: {e}"),
                    };
                    outputs.push(json!({"type": "function_call_output", "call_id": item["call_id"], "output": text}));
                }
                Some("computer_call") => {
                    let call_id = item["call_id"].as_str().unwrap_or_default().to_string();
                    let checks: Vec<Value> = item["pending_safety_checks"]
                        .as_array()
                        .cloned()
                        .unwrap_or_default();
                    for c in &checks {
                        let msg = c["message"]
                            .as_str()
                            .unwrap_or("OpenAI asks for confirmation before this action.");
                        let ask = crate::providers::Ask {
                            what: "go on past OpenAI's safety check".into(),
                            why: msg.to_string(),
                        };
                        ctx.emit(RunEvent::Confirm(ask.clone()));
                        if !(ctx.confirm)(&ask.question()) {
                            return Err(format!("Stopped at a safety check: {msg}"));
                        }
                    }
                    // One action (older models) or a batch.
                    let actions: Vec<Value> = match item["actions"].as_array() {
                        Some(a) => a.clone(),
                        None => vec![item["action"].clone()],
                    };
                    let mut shot = None;
                    for a in &actions {
                        match action(a).and_then(|act| computer.act(act)) {
                            Ok(o) => shot = o.shot.or(shot),
                            Err(e) => {
                                ctx.emit(RunEvent::Status(format!("An action failed: {e}")));
                                break;
                            }
                        }
                        if ctx.stopped() {
                            break;
                        }
                    }
                    let shot = match shot {
                        Some(s) => s,
                        None => computer
                            .act(Action::Screenshot)?
                            .shot
                            .ok_or("no screenshot")?,
                    };
                    let mut out = json!({"type": "computer_call_output", "call_id": call_id,
                        "output": {"type": "computer_screenshot", "image_url": data_url(&shot.png), "detail": "original"}});
                    if !checks.is_empty() {
                        out["acknowledged_safety_checks"] = json!(checks);
                    }
                    outputs.push(out);
                }
                _ => {}
            }
        }
        if outputs.is_empty() {
            return Ok(last_text);
        }
        input = Value::Array(outputs);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn share_plan_and_ask_approval_are_offered_beside_the_computer_tool() {
        let t = tools();
        assert_eq!(t[0]["type"], "computer");
        assert_eq!(
            (t[1]["type"].as_str(), t[1]["name"].as_str()),
            (Some("function"), Some("share_plan"))
        );
        assert_eq!(t[1]["parameters"]["required"][0], "steps");
        assert_eq!(
            (t[2]["type"].as_str(), t[2]["name"].as_str()),
            (Some("function"), Some("ask_approval"))
        );
    }

    #[test]
    fn openai_actions_map_onto_ours() {
        assert_eq!(
            action(&json!({"type": "click", "button": "right", "x": 10, "y": 20})).unwrap(),
            Action::Click {
                at: Some((10, 20)),
                button: Mouse::Right,
                count: 1,
                modifiers: None
            }
        );
        assert_eq!(
            action(&json!({"type": "keypress", "keys": ["CTRL", "L"]})).unwrap(),
            Action::Key {
                keys: "CTRL+L".into(),
                repeat: 1
            }
        );
        assert_eq!(
            action(&json!({"type": "scroll", "x": 5, "y": 6, "scroll_x": 0, "scroll_y": 250}))
                .unwrap(),
            Action::Scroll {
                at: Some((5, 6)),
                down: 3,
                right: 0,
                modifiers: None
            }
        );
        assert_eq!(
            action(&json!({"type": "scroll", "x": 5, "y": 6, "scroll_y": -30})).unwrap(),
            Action::Scroll {
                at: Some((5, 6)),
                down: -1,
                right: 0,
                modifiers: None
            }
        );
        assert_eq!(
            action(&json!({"type": "drag", "path": [{"x": 1, "y": 2}, {"x": 3, "y": 4}]})).unwrap(),
            Action::Drag {
                path: vec![(1, 2), (3, 4)],
                modifiers: None
            }
        );
        assert!(action(&json!({"type": "teleport"})).is_err());
    }
}
