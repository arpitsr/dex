use std::env;
use std::path::PathBuf;

use crate::core::types::PermissionMode;

#[derive(Clone)]
pub(crate) struct Args {
    pub base_url: Option<String>,
    pub model: Option<String>,
    pub session_path: Option<PathBuf>,
    pub no_session: bool,
    pub new_session: bool,
    pub session_name: Option<String>,
    pub skill_dirs: Vec<PathBuf>,
    pub permission: Option<PermissionMode>,
    pub rest: Vec<String>,
}

/// The mode in which the binary was invoked.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Mode {
    /// Start a headless HTTP server (`oye serve [host:port|port]`).
    Serve { bind: String },
    /// Start the TUI connected to a remote daemon (`oye connect <url>`).
    Connect { url: String },
    /// Start both server + TUI in the same process (default `oye`).
    Default,
    /// One-shot prompt (`oye "prompt"`).
    OneShot { prompt: String },
    /// Raw tool mode (`oye --tool`).
    Tool,
}

pub(crate) fn parse_args() -> Args {
    let mut base_url = None;
    let mut model = None;
    let mut session_path = None;
    let mut no_session = false;
    let mut new_session = false;
    let mut session_name = None;
    let mut skill_dirs = Vec::new();
    let mut permission = None;
    let mut rest = Vec::new();
    let mut input = env::args().skip(1);
    while let Some(arg) = input.next() {
        match arg.as_str() {
            "--base-url" => base_url = Some(required(&mut input, "--base-url")),
            "--model" => model = Some(required(&mut input, "--model")),
            "--session" | "-s" => {
                session_path = Some(PathBuf::from(required(&mut input, "--session")))
            }
            "--no-session" => no_session = true,
            "--new" | "-n" => new_session = true,
            "--name" => session_name = Some(required(&mut input, "--name")),
            "--permission" => {
                permission = Some(
                    PermissionMode::parse(&required(&mut input, "--permission"))
                        .unwrap_or_else(|error| fail(&error)),
                );
            }
            "--skill" => skill_dirs.push(PathBuf::from(required(&mut input, "--skill"))),
            _ => rest.push(arg),
        }
    }
    Args {
        base_url,
        model,
        session_path,
        no_session,
        new_session,
        session_name,
        skill_dirs,
        permission,
        rest,
    }
}

/// Determine the invocation mode from parsed args.
pub(crate) fn resolve_mode(args: &Args) -> Mode {
    match args.rest.first().map(|s| s.as_str()) {
        Some("serve") => {
            let bind = args
                .rest
                .get(1)
                .cloned()
                .unwrap_or_else(|| "127.0.0.1:8420".to_string());
            Mode::Serve { bind }
        }
        Some("connect") => {
            let url = args
                .rest
                .get(1)
                .cloned()
                .unwrap_or_else(|| "http://127.0.0.1:8420".to_string());
            Mode::Connect { url }
        }
        Some("--tool") => Mode::Tool,
        Some(prompt) if !prompt.starts_with('-') => Mode::OneShot {
            prompt: args.rest.join(" "),
        },
        None => Mode::Default,
        Some(_unknown) => {
            // Treat unknown flags as part of the prompt for backwards compat.
            Mode::OneShot {
                prompt: args.rest.join(" "),
            }
        }
    }
}

fn required(input: &mut impl Iterator<Item = String>, flag: &str) -> String {
    input
        .next()
        .unwrap_or_else(|| fail(&format!("{} requires a value", flag)))
}

fn fail(message: &str) -> ! {
    eprintln!("error: {}", message);
    std::process::exit(1)
}
