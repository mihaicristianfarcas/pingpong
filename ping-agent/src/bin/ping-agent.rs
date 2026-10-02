//! `ping-agent`: computer use over pingpong from the command line.
//!
//!   ping-agent mcp [--host NAME] [--size WxH] [--image-dir DIR] [--events FILE] [--max-actions N]
//!   ping-agent mcp install APP ... | --all       the MCP server in other agents' settings
//!   ping-agent mcp uninstall APP ... | --all     (claude-code, codex, opencode, ...)
//!   ping-agent mcp status
//!   ping-agent identity                          the agent's public keys (for `pong add-client --agent`)
//!   ping-agent hosts                             hosts the agent is paired with
//!   ping-agent add-host NAME ADDR X25519 MLKEM   pair the agent by hand (see `pong identity`)
//!   ping-agent run --host NAME [--provider P] [--model M] [--effort E] [--max-actions N]
//!                  [--max-minutes N] [--base-url URL] [--approvals off|risky|every] [--confirm] [--yes] TASK
//!   ping-agent converse --host NAME [--provider P] [--model M] [--approvals A] [--answer yes|no]
//!                  MESSAGE [--then MESSAGE ...]  a conversation: each message a turn, one connection
//!   ping-agent providers                         what can run here
//!   ping-agent set-key PROVIDER                  save an API key (read from stdin)
//!   ping-agent set-key cloudflare                save the Cloudflare API token clef runs with
//!   ping-agent check SCREEN.png --click X,Y | --enter | --typing N | --personal
//!                                                clef's answer about a screenshot (see judge.rs)

use std::process::ExitCode;

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match args.first().map(String::as_str) {
        Some("mcp") => ping_agent::mcp::main(&args[1..]),
        Some("run") => run(&args[1..]),
        Some("converse") => converse(&args[1..]),
        Some("providers") => providers(),
        Some("set-key") => set_key(&args),
        Some("check") => check(&args[1..]),
        Some("identity") => identity(),
        Some("hosts") => {
            for h in ping_agent::headless::agent_hosts(&ping_core::store::data_dir()) {
                println!("{}\t{}", h.name, h.address);
            }
            ExitCode::SUCCESS
        }
        Some("add-host") => add_host(&args),
        _ => {
            eprintln!(
                "usage: ping-agent mcp [--host NAME] [--size WxH] [--image-dir DIR] [--events \
                    FILE] [--max-actions N]\n\
                    \x20      ping-agent mcp install APP ... | --all, mcp uninstall APP ..., mcp status\n\
                    \x20      ping-agent identity | hosts | add-host NAME ADDR X25519 MLKEM"
            );
            ExitCode::FAILURE
        }
    }
}

/// Print `e` and fail.
fn fail(e: impl std::fmt::Display) -> ExitCode {
    eprintln!("{e}");
    ExitCode::FAILURE
}

/// Log to stderr (Ping's own log file is the app's), `default` unless
/// `RUST_LOG` says otherwise.
fn init_logging(default: &str) {
    tracing_subscriber::fmt()
        .with_writer(std::io::stderr)
        .with_ansi(false)
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| default.into()),
        )
        .init();
}

/// The host the agent is to use: the one named, or the only one it may use.
fn pick_host(named: Option<String>, hosts: &[ping_core::store::KnownHost]) -> Option<String> {
    named.or_else(|| (hosts.len() == 1).then(|| hosts[0].name.clone()))
}

fn providers() -> ExitCode {
    let dir = ping_core::store::data_dir();
    let settings = ping_agent::providers::AgentSettings::load(&dir);
    for p in ping_agent::providers::Provider::ALL {
        let state = match ping_agent::providers::availability(&dir, &settings, p) {
            Ok(s) => format!("ready: {s}"),
            Err(e) => format!("not ready: {e}"),
        };
        let mark = if p == settings.provider { "*" } else { " " };
        println!("{mark} {:<12} {:<28} {state}", p.id(), p.label());
    }
    ExitCode::SUCCESS
}

fn set_key(args: &[String]) -> ExitCode {
    if args.get(1).map(String::as_str) == Some(ping_agent::judge::TOKEN_ID) {
        let mut token = String::new();
        let _ = std::io::stdin().read_line(&mut token);
        let token = token.trim();
        let dir = ping_core::store::data_dir();
        let id = ping_agent::judge::TOKEN_ID;
        return match ping_agent::providers::secrets::set_named(
            &dir,
            id,
            (!token.is_empty()).then_some(token),
        ) {
            Ok(()) => {
                let what = if token.is_empty() {
                    "forgotten"
                } else {
                    "saved"
                };
                println!("{id} token {what}");
                ExitCode::SUCCESS
            }
            Err(e) => fail(e),
        };
    }
    let Some(p) = args
        .get(1)
        .and_then(|a| ping_agent::providers::Provider::parse(a))
        .filter(|p| !p.is_subscription())
    else {
        eprintln!(
            "usage: ping-agent set-key anthropic|openai|openrouter|custom|cloudflare  (the key on \
                stdin; empty forgets it)"
        );
        return ExitCode::FAILURE;
    };
    let mut key = String::new();
    let _ = std::io::stdin().read_line(&mut key);
    let key = key.trim();
    let dir = ping_core::store::data_dir();
    match ping_agent::providers::secrets::set(&dir, p, (!key.is_empty()).then_some(key)) {
        Ok(()) => {
            let what = if key.is_empty() { "forgotten" } else { "saved" };
            println!("{} key {what}", p.id());
            ExitCode::SUCCESS
        }
        Err(e) => fail(e),
    }
}

/// Ask clef what it makes of a screenshot, as a check during a run would:
/// how the checks are measured on real screens (`judge::HOLD_AT`). Uses the
/// account and token of Agent setup, whichever checks are on there.
fn check(args: &[String]) -> ExitCode {
    use ping_agent::judge::{self, Judge, Subject};
    const USAGE: &str =
        "usage: ping-agent check SCREEN.png --click X,Y | --enter | --typing N | --personal";
    let (Some(file), Some(what)) = (args.first(), args.get(1)) else {
        return fail(USAGE);
    };
    let point = |v: Option<&String>| {
        let (x, y) = v?.split_once(',')?;
        Some((x.trim().parse().ok()?, y.trim().parse().ok()?))
    };
    let subject = match what.as_str() {
        "--click" => match point(args.get(2)) {
            Some(at) => Some(Subject::Click { at, doing: "click" }),
            None => return fail(USAGE),
        },
        "--enter" => Some(Subject::Enter),
        "--typing" => Some(Subject::Typing {
            chars: args.get(2).and_then(|n| n.parse().ok()).unwrap_or(8),
        }),
        "--personal" => None,
        _ => return fail(USAGE),
    };
    let screen = match std::fs::read(file)
        .map_err(|e| e.to_string())
        .and_then(|png| decode_png(&png))
    {
        Ok(s) => s,
        Err(e) => return fail(format!("{file}: {e}")),
    };
    let dir = ping_core::store::data_dir();
    let mut checks = ping_agent::providers::AgentSettings::load(&dir).checks;
    (checks.clicks, checks.secret_fields, checks.personal_info) = (true, true, true);
    let judge = Judge::new(&dir, &checks).expect("a check is on");
    let started = std::time::Instant::now();
    let body = match &subject {
        Some(s) => Ok(judge.request(&screen, s)),
        None => judge.personal_request(&ping_agent::computer::Shot {
            png: screen.png(),
            width: screen.width,
            height: screen.height,
        }),
    };
    let reply = match body.and_then(|b| judge.ask(&b)) {
        Ok(r) => r,
        Err(e) => return fail(e),
    };
    let Some(answers) = judge::answers(&reply) else {
        return fail(format!("no answers in {reply}"));
    };
    let verdict = match subject {
        Some(Subject::Typing { .. }) => judge::typing_verdict(answers),
        Some(_) => judge::effect_verdict(answers),
        None => judge::personal_verdict(answers).map(|found| {
            (!found.is_empty()).then(|| format!("It shows {}.", judge::describe_all(&found)))
        }),
    };
    println!(
        "{}",
        serde_json::to_string_pretty(answers).unwrap_or_default()
    );
    println!(
        "{} in {} ms: {}",
        judge.model(),
        started.elapsed().as_millis(),
        match verdict {
            Ok(Some(why)) => format!("holds. {why}"),
            Ok(None) => "lets it through.".into(),
            Err(e) => e,
        }
    );
    ExitCode::SUCCESS
}

/// An 8-bit RGB or RGBA PNG (what screenshot tools save) as RGB.
fn decode_png(png: &[u8]) -> Result<ping_agent::frame::Rgb, String> {
    let mut decoder = png::Decoder::new(std::io::Cursor::new(png));
    decoder.set_transformations(png::Transformations::EXPAND | png::Transformations::STRIP_16);
    let mut reader = decoder.read_info().map_err(|e| e.to_string())?;
    let mut buf = vec![0; reader.output_buffer_size().ok_or("the PNG is too large")?];
    let info = reader.next_frame(&mut buf).map_err(|e| e.to_string())?;
    let data = match info.color_type {
        png::ColorType::Rgb => buf[..info.buffer_size()].to_vec(),
        png::ColorType::Rgba => buf[..info.buffer_size()]
            .as_chunks::<4>()
            .0
            .iter()
            .flat_map(|p| [p[0], p[1], p[2]])
            .collect(),
        other => return Err(format!("{other:?} PNGs are not read here: save it as RGB")),
    };
    Ok(ping_agent::frame::Rgb {
        width: info.width,
        height: info.height,
        data,
    })
}

fn identity() -> ExitCode {
    let dir = ping_core::store::agent_dir(&ping_core::store::data_dir());
    match ping_core::store::identity(&dir) {
        Ok(id) => {
            let (x, m) = id.public().to_b64();
            println!("{x} {m}");
            ExitCode::SUCCESS
        }
        Err(e) => fail(e),
    }
}

fn add_host(args: &[String]) -> ExitCode {
    let (Some(name), Some(addr), Some(x), Some(m)) =
        (args.get(1), args.get(2), args.get(3), args.get(4))
    else {
        eprintln!("usage: ping-agent add-host NAME ADDR X25519 MLKEM");
        return ExitCode::FAILURE;
    };
    if pingpong_transport::PublicIdentity::from_b64(x, m).is_err() {
        return fail("malformed host keys");
    }
    let host = ping_core::store::KnownHost::by_hand(name, addr, x, m);
    let dir = ping_core::store::agent_dir(&ping_core::store::data_dir());
    match ping_core::store::Hosts::load(&dir).upsert(host) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => fail(e),
    }
}

fn run(args: &[String]) -> ExitCode {
    use ping_agent::providers::{AgentSettings, Approvals, Provider, RunEvent};
    init_logging("warn");
    let dir = ping_core::store::data_dir();
    let mut settings = AgentSettings::load(&dir);
    let mut host = None;
    let mut yes = false;
    let mut task = Vec::new();
    let mut it = args.iter();
    while let Some(a) = it.next() {
        match a.as_str() {
            "--host" => host = it.next().cloned(),
            "--provider" => match it.next().and_then(|p| Provider::parse(p)) {
                Some(p) => settings.provider = p,
                None => {
                    eprintln!(
                        "providers: codex, claude-code, anthropic, openai, openrouter, custom"
                    );
                    return ExitCode::FAILURE;
                }
            },
            "--model" => settings.model = it.next().cloned().unwrap_or_default(),
            "--effort" => settings.effort = it.next().cloned().unwrap_or_default(),
            "--max-actions" => {
                settings.max_actions = it
                    .next()
                    .and_then(|v| v.parse().ok())
                    .unwrap_or(settings.max_actions)
            }
            "--max-minutes" => {
                settings.max_minutes = it
                    .next()
                    .and_then(|v| v.parse().ok())
                    .unwrap_or(settings.max_minutes)
            }
            "--base-url" => settings.base_url = it.next().cloned().unwrap_or_default(),
            "--size" => {
                if let Some((w, h)) = it.next().and_then(|v| v.split_once('x')) {
                    settings.width = w.parse().unwrap_or(settings.width);
                    settings.height = h.parse().unwrap_or(settings.height);
                }
            }
            "--yes" => yes = true,
            // Which steps wait for a yes on the terminal (--confirm: every).
            "--approvals" => {
                settings.approvals = it
                    .next()
                    .and_then(|v| Approvals::parse(v))
                    .unwrap_or(settings.approvals)
            }
            "--confirm" => settings.approvals = Approvals::Every,
            other => task.push(other.to_string()),
        }
    }
    let hosts = ping_agent::headless::agent_hosts(&dir);
    let Some(host) = pick_host(host, &hosts) else {
        eprintln!(
            "name a host with --host (the agent may use: {})",
            hosts
                .iter()
                .map(|h| h.name.as_str())
                .collect::<Vec<_>>()
                .join(", ")
        );
        return ExitCode::FAILURE;
    };
    let task = ping_agent::runner::Task {
        data_dir: dir,
        host,
        task: task.join(" "),
        settings,
        session: None,
    };
    let (tx, rx) = crossbeam_channel::unbounded();
    let events: ping_agent::providers::Events = std::sync::Arc::new(move |e| {
        let _ = tx.send(e);
    });
    let confirm: ping_agent::providers::Confirm = std::sync::Arc::new(move |msg: &str| {
        if yes {
            eprintln!("{msg} -- yes (--yes)");
            return true;
        }
        eprint!("{msg} -- go on? [y/N] ");
        let mut line = String::new();
        let _ = std::io::stdin().read_line(&mut line);
        matches!(line.trim(), "y" | "Y" | "yes")
    });
    let run = ping_agent::runner::Run::start(task, events, confirm);
    // Ctrl-C stops the run (the model's program with it).
    let stop = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let s = stop.clone();
    let _ = ctrlc::set_handler(move || s.store(true, std::sync::atomic::Ordering::Relaxed));
    let mut ok = false;
    for e in rx.iter() {
        if stop.load(std::sync::atomic::Ordering::Relaxed) {
            run.stop();
        }
        match e {
            RunEvent::Status(s) => eprintln!("… {s}"),
            RunEvent::Thought(t) => println!("💬 {t}"),
            RunEvent::Action {
                text, ok, detail, ..
            } => {
                let detail = if detail == "OK" || detail.is_empty() {
                    String::new()
                } else {
                    format!(" — {detail}")
                };
                println!(
                    "{} {text}{}",
                    if ok { "▶" } else { "✗" },
                    detail.chars().take(200).collect::<String>()
                );
            }
            RunEvent::Usage {
                input,
                output,
                cached,
                cost_usd,
            } => eprintln!(
                "… tokens: {input} in ({cached} cached), {output} out{}",
                cost_usd.map(|c| format!(", ${c:.4}")).unwrap_or_default()
            ),
            RunEvent::Confirm(m) => eprintln!("? {}", m.question()),
            RunEvent::Plan(steps) => eprintln!("☰ plan: {}", plan_line(&steps)),
            RunEvent::Finished(s) => {
                println!("✓ {s}");
                ok = true;
                break;
            }
            RunEvent::Failed(e) => {
                eprintln!("✗ {e}");
                break;
            }
        }
    }
    run.join();
    if ok {
        ExitCode::SUCCESS
    } else {
        ExitCode::FAILURE
    }
}

/// A plan on one line: "[x] one · [>] two · [ ] three".
fn plan_line(steps: &[ping_agent::computer::PlanStep]) -> String {
    use ping_agent::computer::PlanStatus;
    steps
        .iter()
        .map(|s| {
            let mark = match s.status {
                PlanStatus::Done => "[x]",
                PlanStatus::InProgress => "[>]",
                PlanStatus::Pending => "[ ]",
            };
            format!("{mark} {}", s.text)
        })
        .collect::<Vec<_>>()
        .join(" · ")
}

/// A conversation from the command line: each `--then` starts another turn
/// on the same connection, the model continuing its thread.
fn converse(args: &[String]) -> ExitCode {
    use ping_agent::providers::{AgentSettings, Approvals, Provider, RunEvent};
    init_logging("info,mainline=error");
    let dir = ping_core::store::data_dir();
    let mut settings = AgentSettings::load(&dir);
    let mut host = None;
    let mut messages: Vec<Vec<String>> = vec![Vec::new()];
    // What every question is answered (no one is at a terminal to ask).
    let mut answer = false;
    let mut it = args.iter();
    while let Some(a) = it.next() {
        match a.as_str() {
            "--host" => host = it.next().cloned(),
            "--approvals" => {
                settings.approvals = it
                    .next()
                    .and_then(|v| Approvals::parse(v))
                    .unwrap_or(settings.approvals)
            }
            "--answer" => answer = it.next().is_some_and(|v| v == "yes"),
            "--provider" => {
                settings.provider = it
                    .next()
                    .and_then(|p| Provider::parse(p))
                    .unwrap_or(settings.provider)
            }
            "--model" => settings.model = it.next().cloned().unwrap_or_default(),
            "--effort" => settings.effort = it.next().cloned().unwrap_or_default(),
            "--max-actions" => {
                settings.max_actions = it
                    .next()
                    .and_then(|v| v.parse().ok())
                    .unwrap_or(settings.max_actions)
            }
            "--then" => messages.push(Vec::new()),
            other => messages.last_mut().expect("one").push(other.to_string()),
        }
    }
    let hosts = ping_agent::headless::agent_hosts(&dir);
    let Some(host) = pick_host(host, &hosts) else {
        eprintln!("name a host with --host");
        return ExitCode::FAILURE;
    };
    let (tx, rx) = crossbeam_channel::unbounded::<RunEvent>();
    let actions = tx.clone();
    let on_action: ping_agent::computer::Observer = std::sync::Arc::new(move |d| {
        let _ = actions.send(RunEvent::Action {
            text: d.action,
            ok: d.ok,
            detail: d.text,
            thumbnail: d.thumbnail.map(|t| t.png),
        });
    });
    let plans = tx.clone();
    let on_plan: ping_agent::computer::PlanObserver = std::sync::Arc::new(move |steps| {
        let _ = plans.send(RunEvent::Plan(steps));
    });
    let mut conversation = match ping_agent::conversation::Conversation::open(
        &dir, &host, &settings, on_action, on_plan,
    ) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("{e}");
            return ExitCode::FAILURE;
        }
    };
    let mut ok = true;
    for (n, words) in messages.iter().enumerate() {
        let message = words.join(" ");
        println!("\n── you ({}): {message}", n + 1);
        let tx = tx.clone();
        let events: ping_agent::providers::Events = std::sync::Arc::new(move |e| {
            let _ = tx.send(e);
        });
        if let Err(e) = conversation.send(
            &message,
            &settings,
            events,
            std::sync::Arc::new(move |_: &str| answer),
        ) {
            eprintln!("{e}");
            ok = false;
            break;
        }
        for e in rx.iter() {
            match e {
                RunEvent::Status(s) => eprintln!("… {s}"),
                RunEvent::Thought(t) => println!("💬 {t}"),
                RunEvent::Action {
                    text,
                    ok,
                    detail,
                    thumbnail,
                } => println!(
                    "{} {text}{} {}",
                    if ok { "▶" } else { "✗" },
                    if detail == "OK" || detail.is_empty() {
                        String::new()
                    } else {
                        format!(" — {}", detail.chars().take(160).collect::<String>())
                    },
                    thumbnail
                        .map(|t| format!("[screen {} KB]", t.len() / 1024))
                        .unwrap_or_default()
                ),
                RunEvent::Usage { input, output, .. } => {
                    eprintln!("… tokens: {input} in, {output} out")
                }
                RunEvent::Confirm(m) => {
                    eprintln!("? {} ({})", m.question(), if answer { "yes" } else { "no" })
                }
                RunEvent::Plan(steps) => println!("☰ plan: {}", plan_line(&steps)),
                RunEvent::Finished(s) => {
                    println!("✓ {s}");
                    break;
                }
                RunEvent::Failed(e) => {
                    eprintln!("✗ {e}");
                    ok = false;
                    break;
                }
            }
        }
        println!(
            "   (connected after the turn: {:?}; the model continues its thread: {})",
            conversation.connected(),
            conversation.continues()
        );
    }
    conversation.end();
    if ok {
        ExitCode::SUCCESS
    } else {
        ExitCode::FAILURE
    }
}
