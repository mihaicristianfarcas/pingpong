//! `ping xbox`: a console of the account's, or a game in Xbox Cloud Gaming.
//!
//!   ping xbox                       who is signed in
//!   ping xbox sign-in               sign in with a Microsoft account (a code to type)
//!   ping xbox sign-out
//!   ping xbox consoles              the account's consoles
//!   ping xbox wake NAME | off NAME  turn a console on, or off
//!   ping xbox games                 the cloud games the account may play
//!   ping xbox friends               the account's friends, and what they play
//!   ping xbox stream CONSOLE [--keyboard] [stream flags]
//!   ping xbox play GAME [--keyboard] [--region NAME] [stream flags]
//!
//! A console or game is named by its name (any case) or its id. Keys drive
//! the first controller unless `--keyboard` sends them as a keyboard. The
//! stream flags are `ping stream`'s; the console chooses its own codec, rate
//! and bitrate. `PING_XBOX_MOCK=URL` uses a mock console (`xbox-mock`).

use std::process::ExitCode;
use std::sync::atomic::AtomicBool;

use ping_core::xbox::account::{self, AuthError, Command, Console};
use ping_core::xbox::{Target, XboxSource};

use crate::stream::{self, What};
use crate::{fail, usage};

pub fn run(args: &[String]) -> ExitCode {
    match args.first().map(String::as_str) {
        None => status(),
        Some("sign-in") => sign_in(),
        Some("sign-out") => match account::sign_out() {
            Ok(()) => ExitCode::SUCCESS,
            Err(e) => fail(e),
        },
        Some("consoles") => consoles(),
        Some("wake") | Some("off") => match args.get(1) {
            Some(name) => power(name, args[0] == "wake"),
            None => usage(),
        },
        Some("games") => games(),
        Some("friends") => friends(),
        Some("stream") => match args.get(1) {
            Some(name) => stream_console(name, &args[2..]),
            None => usage(),
        },
        Some("play") => match args.get(1) {
            Some(name) => play(name, &args[2..]),
            None => usage(),
        },
        _ => usage(),
    }
}

fn auth_fail(e: AuthError) -> ExitCode {
    match e {
        AuthError::SignedOut => fail("Not signed in to Xbox: run `ping xbox sign-in`."),
        e => fail(e),
    }
}

fn status() -> ExitCode {
    match account::signed_in() {
        Some(gamertag) => println!("Signed in to Xbox as {gamertag}."),
        None => println!("Not signed in to Xbox: run `ping xbox sign-in`."),
    }
    ExitCode::SUCCESS
}

fn sign_in() -> ExitCode {
    let code = match account::start_sign_in() {
        Ok(c) => c,
        Err(e) => return auth_fail(e),
    };
    eprintln!(
        "\nOn any device, open {} and enter this code:\n\n    {}\n\nWaiting…",
        code.verification_uri, code.user_code
    );
    match account::finish_sign_in(&code, &AtomicBool::new(false)) {
        Ok(gamertag) => {
            println!("Signed in to Xbox as {gamertag}.");
            ExitCode::SUCCESS
        }
        Err(e) => auth_fail(e),
    }
}

fn consoles() -> ExitCode {
    match account::consoles() {
        Ok(list) if list.is_empty() => {
            println!("This account has no consoles.");
            ExitCode::SUCCESS
        }
        Ok(list) => {
            for c in list {
                let note = c
                    .cannot_stream()
                    .map(|n| format!("\t{n}"))
                    .unwrap_or_default();
                println!("{}\t{}\t{}\t{}{note}", c.name, c.model(), c.state(), c.id);
            }
            ExitCode::SUCCESS
        }
        Err(e) => auth_fail(e),
    }
}

fn find_console(name: &str) -> Result<Console, ExitCode> {
    let list = account::consoles().map_err(auth_fail)?;
    list.into_iter()
        .find(|c| c.name.eq_ignore_ascii_case(name) || c.id.eq_ignore_ascii_case(name))
        .ok_or_else(|| {
            fail(format!(
                "No console named {name}; see `ping xbox consoles`."
            ))
        })
}

fn power(name: &str, on: bool) -> ExitCode {
    let console = match find_console(name) {
        Ok(c) => c,
        Err(code) => return code,
    };
    let command = if on {
        Command::WakeUp
    } else {
        Command::TurnOff
    };
    match account::command(&console.id, command) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => auth_fail(e),
    }
}

fn games() -> ExitCode {
    match account::cloud_library(true) {
        Ok(None) => fail("Xbox Cloud Gaming is not offered to this account here."),
        Ok(Some(library)) => {
            if !library.game_pass {
                eprintln!("Without Game Pass Ultimate: free-to-play games only.");
            }
            for g in library.games {
                println!("{}\t{}\t{}", g.name, g.publisher, g.title_id);
            }
            ExitCode::SUCCESS
        }
        Err(e) => auth_fail(e),
    }
}

fn friends() -> ExitCode {
    match account::friends() {
        Ok(list) => {
            for f in list {
                let state = if f.online { "online" } else { "offline" };
                println!("{}\t{state}\t{}", f.name, f.activity);
            }
            ExitCode::SUCCESS
        }
        Err(e) => auth_fail(e),
    }
}

/// `--keyboard` and `--region NAME` are ours; the rest are `ping stream`'s.
fn split_flags(flags: &[String]) -> (bool, Option<String>, Vec<String>) {
    let (mut keyboard, mut region, mut rest) = (false, None, Vec::new());
    let mut it = flags.iter();
    while let Some(f) = it.next() {
        match f.as_str() {
            "--keyboard" => keyboard = true,
            "--region" => region = it.next().cloned(),
            _ => rest.push(f.clone()),
        }
    }
    (keyboard, region, rest)
}

fn stream_console(name: &str, flags: &[String]) -> ExitCode {
    let console = match find_console(name) {
        Ok(c) => c,
        Err(code) => return code,
    };
    let (keyboard, region, rest) = split_flags(flags);
    stream::run(
        What::Xbox(XboxSource {
            target: Target::Console {
                id: console.id,
                name: console.name,
            },
            keyboard_as_controller: !keyboard,
            region,
        }),
        stream::request(&rest),
    )
}

fn play(name: &str, flags: &[String]) -> ExitCode {
    let library = match account::cloud_library(false) {
        Ok(Some(l)) => l,
        Ok(None) => return fail("Xbox Cloud Gaming is not offered to this account here."),
        Err(e) => return auth_fail(e),
    };
    let wanted = name.to_lowercase();
    let game = library
        .games
        .iter()
        .find(|g| g.name.to_lowercase() == wanted || g.title_id.eq_ignore_ascii_case(name))
        .or_else(|| {
            library
                .games
                .iter()
                .find(|g| g.name.to_lowercase().contains(&wanted))
        });
    let Some(game) = game else {
        return fail(format!(
            "No cloud game named {name}; see `ping xbox games`."
        ));
    };
    let (keyboard, region, rest) = split_flags(flags);
    stream::run(
        What::Xbox(XboxSource {
            target: Target::Cloud {
                title_id: game.title_id.clone(),
                name: game.name.clone(),
            },
            keyboard_as_controller: !keyboard,
            region,
        }),
        stream::request(&rest),
    )
}
