//! `ping-agent`: computer use over pingpong from the command line.
//!
//!   ping-agent mcp [--host NAME] [--size WxH] [--image-dir DIR] [--events FILE] [--max-actions N]
//!   ping-agent identity                          the agent's public keys (for `pong add-client --agent`)
//!   ping-agent hosts                             hosts the agent is paired with
//!   ping-agent add-host NAME ADDR X25519 MLKEM   pair the agent by hand (see `pong identity`)
//!   ping-agent run --host NAME [--provider P] [--model M] [--effort E] [--max-actions N]
//!                  [--max-minutes N] [--base-url URL] [--approvals off|risky|every] [--confirm] [--yes] TASK
//!   ping-agent converse --host NAME [--provider P] [--model M] [--approvals A] [--answer yes|no]
//!                  MESSAGE [--then MESSAGE ...]  a conversation: each message a turn, one connection
//!   ping-agent providers                         what can run here
//!   ping-agent set-key PROVIDER                  save an API key (read from stdin)

use std::process::ExitCode;

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match args.first().map(String::as_str) {
        Some("mcp") => ping_agent::mcp::main(&args[1..]),
        Some("run") => run(&args[1..]),
        Some("converse") => converse(&args[1..]),
        Some("providers") => providers(),
        Some("set-key") => set_key(&args),
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
    let Some(p) = args
        .get(1)
        .and_then(|a| ping_agent::providers::Provider::parse(a))
        .filter(|p| !p.is_subscription())
    else {
        eprintln!(
            "usage: ping-agent set-key anthropic|openai|openrouter|custom  (the key on stdin; \
                empty forgets it)"
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
