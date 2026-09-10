//! `ira set`, `ira mcp`, `ira skill` -- everything the settings window does,
//! from a terminal.
//!
//! Not a second way to configure IRA: the same two stores, through the same
//! functions. It exists because a window needs a window. Provisioning a machine
//! from a script, filling in a key before the first start (the fatal check for
//! a missing one fires long before there is anything to click), and looking at
//! what is configured over SSH all want a command.
//!
//! **These do not connect anything.** A server added here is connected at the
//! next start, or immediately if you add it in the window instead. Writing to
//! the database is a thing a second process can do safely; reaching into a
//! running IRA's tool registry is not.
//!
//! **And they do not ask.** The window speaks a stdio server's command aloud
//! and waits for a yes, because that route is reachable from any page the
//! browser will post from. A terminal on this machine already *is* the
//! authorisation -- someone who can run `ira mcp add` can run the command
//! directly, so a confirmation would guard nothing.

use crate::{db, settings, skills};

/// The two questions that must not start anything.
///
/// Answered before the logger, the settings and the skill index, because
/// `ira --version` printing three lines of start-up first is not a version, and
/// because either of those can fail on a machine where the question is still a
/// fair one to ask.
pub fn early(args: &[String]) -> Option<i32> {
    match args.first().map(String::as_str)? {
        "--version" | "-V" => {
            println!("ira {}", env!("CARGO_PKG_VERSION"));
            Some(0)
        }
        "--help" | "-h" | "help" => Some(help()),
        _ => None,
    }
}


/// Dispatches a subcommand. `None` means this was not one, and IRA should
/// start normally.
pub fn run(args: &[String]) -> Option<i32> {
    match args.first().map(String::as_str)? {
        "set" => Some(settings::set_from_cli(&args[1..])),
        "mcp" => Some(mcp(&args[1..])),
        "skill" => Some(skill(&args[1..])),
        _ => None,
    }
}

/// What she can be told to do, for someone who installed her and typed `ira
/// --help` because that is what you type.
///
/// Deliberately short. The full surface is in the README and in SPEC.md, and a
/// terminal is a bad place to read either.
fn help() -> i32 {
    println!("ira -- a voice assistant you can interrupt");
    println!();
    println!("  ira                          start her");
    println!("  ira doctor                   check models, microphone and keys");
    println!("  ira fetch                    download the models, the voice and piper");
    println!("  ira fetch --whisper          and offline speech-to-text");
    println!("  ira set <NAME> [value]       a key or a URL; no value clears it");
    println!("  ira mcp ls|add|env|rm        MCP servers");
    println!("  ira skill ls|add|on|off|rm   the instructions she is given");
    println!("  ira --version");
    println!();
    println!("Say \"hey Jarvis\", wait for the chirp, and talk. Interrupt any time.");
    0
}


/// Prints a problem and returns the exit code, so every arm reads as one line.
fn bad(what: impl std::fmt::Display) -> i32 {
    eprintln!("{what}");
    1
}

fn usage(lines: &[&str]) -> i32 {
    for l in lines {
        eprintln!("{l}");
    }
    2
}

fn mcp(args: &[String]) -> i32 {
    let rest: Vec<&str> = args.iter().skip(1).map(String::as_str).collect();
    match args.first().map(String::as_str) {
        Some("ls") => match db::servers() {
            Ok(servers) if servers.is_empty() => {
                println!("no servers configured");
                0
            }
            Ok(servers) => {
                for s in servers {
                    // The arguments are part of what will be run, so they are
                    // part of what `ls` has to show.
                    let where_ = match s.transport.as_str() {
                        "stdio" => [s.command.clone().unwrap_or_default()]
                            .into_iter()
                            .chain(s.args.clone())
                            .collect::<Vec<_>>()
                            .join(" "),
                        _ => s.url.clone().unwrap_or_default(),
                    };
                    println!(
                        "{:<16} {:<6} {:<4} {}",
                        s.name,
                        s.transport,
                        if s.enabled { "on" } else { "off" },
                        where_
                    );
                    for name in db::env_names(&s.name).unwrap_or_default() {
                        // Whether it is set, never what it is.
                        let set = settings::secret::read(&settings::secret::env_key(&s.name, &name))
                            .ok()
                            .flatten()
                            .is_some();
                        println!("                 env {name} = {}", if set { "set" } else { "EMPTY" });
                    }
                }
                0
            }
            Err(e) => bad(format!("{e:#}")),
        },

        Some("add") => match rest.as_slice() {
            [name, transport @ ("stdio" | "http"), target, args @ ..] => {
                let server = db::Server {
                    name: (*name).to_string(),
                    transport: (*transport).to_string(),
                    command: (*transport == "stdio").then(|| (*target).to_string()),
                    args: args.iter().map(|a| (*a).to_string()).collect(),
                    url: (*transport == "http").then(|| (*target).to_string()),
                    headers: Default::default(),
                    enabled: true,
                    tools: Default::default(),
                };
                match db::server_set(&server) {
                    Ok(()) => {
                        println!("{name} saved. It connects at the next start.");
                        0
                    }
                    Err(e) => bad(format!("{e:#}")),
                }
            }
            _ => usage(&["usage: ira mcp add <name> stdio <command> [args...]",
                         "       ira mcp add <name> http  <url>"]),
        },

        Some("rm") => match rest.as_slice() {
            [name] => match crate::mcp::forget(name) {
                Ok(()) => {
                    println!("{name} removed.");
                    0
                }
                Err(e) => bad(format!("{e:#}")),
            },
            _ => usage(&["usage: ira mcp rm <name>"]),
        },

        Some("env") => match rest.as_slice() {
            // The value is optional so a variable can be recorded now and
            // filled in from the window later, and so it can be typed at a
            // prompt rather than left in shell history.
            [server, name, value @ ..] => {
                let value = value.first().copied().unwrap_or("");
                if let Err(e) = db::env_add(server, name) {
                    return bad(format!("{e:#}"));
                }
                let key = settings::secret::env_key(server, name);
                if value.is_empty() {
                    println!("{name} recorded for {server}, with no value yet.");
                    return 0;
                }
                match settings::secret::write(&key, value) {
                    // Never the value: this is a terminal, and terminals are
                    // recorded.
                    Ok(()) => {
                        println!("{name} saved to the keyring for {server}.");
                        0
                    }
                    Err(e) => bad(format!("{e:#}")),
                }
            }
            _ => usage(&["usage: ira mcp env <server> <NAME> [VALUE]",
                         "       no value records the name and leaves it empty"]),
        },

        _ => usage(&["usage: ira mcp ls",
                     "       ira mcp add <name> <stdio|http> <command-or-url> [args...]",
                     "       ira mcp env <server> <NAME> [VALUE]",
                     "       ira mcp rm  <name>"]),
    }
}

fn skill(args: &[String]) -> i32 {
    let rest: Vec<&str> = args.iter().skip(1).map(String::as_str).collect();
    match args.first().map(String::as_str) {
        Some("ls") => {
            let rows = skills::index();
            if rows.is_empty() {
                println!("no skills");
                return 0;
            }
            for s in rows {
                println!(
                    "{:<16} {:<4} {}",
                    s.name,
                    if s.enabled { "on" } else { "off" },
                    s.description
                );
            }
            0
        }

        // From a file rather than an argument: a skill is prose with newlines
        // in it, and prose does not survive a shell.
        Some("add") => match rest.as_slice() {
            [name, path] => match std::fs::read_to_string(path) {
                Ok(text) => {
                    // Whatever front matter the source file had is kept, so
                    // adding a skill someone else wrote does not strip its
                    // description.
                    match skills::write_raw(name, &text) {
                        Ok(()) => {
                            println!("{name} written.");
                            0
                        }
                        Err(e) => bad(format!("{e:#}")),
                    }
                }
                Err(e) => bad(format!("could not read {path}: {e}")),
            },
            _ => usage(&["usage: ira skill add <name> <file.md>"]),
        },

        Some(on @ ("on" | "off")) => match rest.as_slice() {
            [name] => match skills::enable(name, on == "on") {
                Ok(()) => {
                    println!("{name} is {on}.");
                    0
                }
                Err(e) => bad(format!("{e:#}")),
            },
            _ => usage(&["usage: ira skill on|off <name>"]),
        },

        Some("rm") => match rest.as_slice() {
            [name] => match skills::delete(name) {
                Ok(()) => {
                    println!("{name} deleted.");
                    0
                }
                Err(e) => bad(format!("{e:#}")),
            },
            _ => usage(&["usage: ira skill rm <name>"]),
        },

        _ => usage(&["usage: ira skill ls",
                     "       ira skill add <name> <file.md>",
                     "       ira skill on|off <name>",
                     "       ira skill rm <name>"]),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Anything that is not a subcommand must fall through to starting IRA,
    /// or a typo would silently become "run the assistant" or vice versa.
    #[test]
    fn only_the_known_subcommands_are_handled() {
        assert!(run(&[]).is_none());
        assert!(run(&["--help".into()]).is_none());
        assert!(run(&["doctor".into()]).is_none(), "doctor is handled in main");

        // Present, and each refuses an empty argument list rather than doing
        // something surprising with it.
        assert_eq!(run(&["mcp".into()]), Some(2));
        assert_eq!(run(&["skill".into()]), Some(2));
    }
}
