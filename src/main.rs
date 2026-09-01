mod agent;
mod cli;
mod client;
mod config;
mod core;
mod daemon;
mod llm;
mod protocol;
mod session;
mod skills;
mod tools;
mod ui;

use cli::*;
use session::*;
use tools::*;

use crate::agent::r#loop::{approve_tool, process_turn};
use crate::agent::state::{GlobalCancellation, ToolState};
use crate::core::console::install_sigint_handler;
use crate::core::types::{ChatMessage, PermissionMode};
use crate::llm::config::{load_file_config, permission_from_env_or_file, LlmConfig};
use crate::llm::prompt::system_prompt;
use crate::skills::{discover_skills, skill_dirs};

use serde_json::{json, Map, Value};
use std::env;
use std::io::{self, Write};

fn run_one_shot(prompt: &str, args: &Args) -> Result<(), Box<dyn std::error::Error>> {
    let mut config =
        LlmConfig::from_env(args.base_url.clone(), args.model.clone(), args.permission)?;
    // P9: auto-detect verification at the one-shot boundary too.
    if config.verify_command.is_none() {
        config.verify_command = crate::llm::config::detect_verify_command();
    }
    let mut skill_dirs = skill_dirs();
    skill_dirs.extend(args.skill_dirs.iter().cloned());
    let skills = discover_skills(&skill_dirs);
    let cwd = env::current_dir()
        .map(|p| p.to_string_lossy().to_string())
        .unwrap_or_default();
    let mut session = if args.no_session {
        None
    } else {
        match if args.new_session {
            Session::new(cwd.clone(), args.session_name.clone())
        } else {
            Session::open_or_continue(cwd.clone(), args.session_path.as_deref(), false)
        } {
            Ok(mut session) => {
                if let Some(name) = &args.session_name {
                    let _ = session.set_name(name.clone());
                }
                Some(session)
            }
            Err(error) => {
                eprintln!(
                    "[session] could not open session ({}); continuing without persistence",
                    error
                );
                None
            }
        }
    };
    let mut messages = vec![ChatMessage {
        role: "system".to_string(),
        content: Some(system_prompt(&skills)),
        tool_calls: None,
        tool_call_id: None,
        name: None,
    }];
    if let Some(existing) = session.as_ref().and_then(|s| s.path()) {
        messages.extend(load_messages_from_session(existing).unwrap_or_default());
    }
    let user = ChatMessage {
        role: "user".into(),
        content: Some(prompt.into()),
        tool_calls: None,
        tool_call_id: None,
        name: None,
    };
    if let Some(session) = session.as_mut() {
        let _ = session.turn_event("turn_start");
        let _ = session.append_message(user.clone());
    }
    messages.push(user);
    let mut state = ToolState::load();
    let console = crate::core::console::Console::none();
    let result = process_turn(
        &config,
        &mut messages,
        &mut state,
        None,
        None,
        session.as_mut(),
        &config,
        &crate::agent::state::GlobalCancellation,
        &console,
    );
    if let Some(session) = session.as_mut() {
        let _ = session.turn_event(if result.is_ok() {
            "turn_complete"
        } else {
            "turn_failed"
        });
    }
    println!();
    result.map(|_| ())
}

fn run_interactive() {
    eprintln!("dex raw tool mode");
    eprintln!("tools: read, bash, write, edit, grep, find, git");
    eprintln!("send JSON lines like: {{\"name\":\"read\",\"args\":{{\"path\":\"Cargo.toml\"}}}}");
    eprintln!("empty line quits");

    let permission = load_file_config()
        .and_then(|file| permission_from_env_or_file(&file))
        .unwrap_or(PermissionMode::ReadOnly);

    let stdin = io::stdin();
    let mut stdout = io::stdout();
    for line in stdin.lines() {
        let line = match line {
            Ok(l) => l,
            Err(e) => {
                println!("{}", json!({"err": format!("read error: {}", e)}));
                continue;
            }
        };
        if line.trim().is_empty() {
            break;
        }
        let parsed: Value = match serde_json::from_str(&line) {
            Ok(v) => v,
            Err(e) => {
                println!("{}", json!({"err": format!("json error: {}", e)}));
                continue;
            }
        };
        let name = match parsed.get("name").and_then(Value::as_str) {
            Some(n) => n,
            None => {
                println!("{}", json!({"err": "missing 'name'"}));
                continue;
            }
        };
        let args = match parsed.get("args").and_then(Value::as_object) {
            Some(a) => a.clone(),
            None => Map::new(),
        };
        let input = serde_json::to_string(&args).unwrap_or_default();
        let result = if !approve_tool(
            permission,
            name,
            &input,
            &crate::core::console::Console::none(),
        ) {
            json!({"err": format!("permission denied for tool '{}'", name)})
        } else {
            match execute(name, &args, &GlobalCancellation) {
                Ok(out) => json!({"ok": out}),
                Err(e) => json!({"err": e.to_string()}),
            }
        };
        println!("{}", result);
        let _ = stdout.flush();
    }
}

/// Start the daemon server on a background thread with a pre-bound listener
/// (no window for another process to steal the port), and wait until it is
/// ready to serve. Returns the address it listens on.
fn start_daemon_background() -> std::io::Result<std::net::SocketAddr> {
    let listener = std::net::TcpListener::bind("127.0.0.1:0")?;
    let addr = listener.local_addr()?;

    std::thread::spawn(move || {
        let rt = tokio::runtime::Runtime::new().expect("failed to create tokio runtime");
        rt.block_on(async {
            if let Err(e) = daemon::run_daemon(listener).await {
                eprintln!("daemon error: {e}");
            }
        });
    });

    // Block until /health answers so the TUI never races server startup.
    let url = format!("http://{addr}");
    let client = client::http::DaemonClient::new(&url)
        .map_err(|e| std::io::Error::other(format!("failed to reach daemon: {e}")))?;
    client
        .wait_until_ready(std::time::Duration::from_secs(10))
        .map_err(|e| std::io::Error::other(format!("daemon startup failed: {e}")))?;
    Ok(addr)
}

fn main() {
    install_sigint_handler();
    let args = cli::parse_args();
    let mode = cli::resolve_mode(&args);

    match mode {
        Mode::Serve { bind } => {
            let addr: std::net::SocketAddr = if bind.contains(':') {
                bind.parse().unwrap_or_else(|_| {
                    eprintln!("error: invalid bind address '{bind}' (use [host:]port)");
                    std::process::exit(1);
                })
            } else {
                ([127, 0, 0, 1], bind.parse().unwrap_or(8420)).into()
            };
            if addr.ip().is_unspecified() {
                eprintln!(
                    "warning: daemon listening on {addr} is exposed on all interfaces and has no authentication — prefer 127.0.0.1 for local use"
                );
            }
            let listener = match std::net::TcpListener::bind(addr) {
                Ok(listener) => listener,
                Err(e) => {
                    eprintln!("daemon error: cannot bind {addr}: {e}");
                    std::process::exit(1);
                }
            };
            let rt = tokio::runtime::Runtime::new().expect("failed to create tokio runtime");
            rt.block_on(async {
                if let Err(e) = daemon::run_daemon(listener).await {
                    eprintln!("daemon error: {e}");
                    std::process::exit(1);
                }
            });
        }
        Mode::Connect { url } => {
            // `dex connect <url>` opens the TUI; `dex connect <url> "prompt"`
            // runs a one-shot turn against the daemon.
            let prompt = args
                .rest
                .get(2..)
                .map(|rest| rest.join(" "))
                .filter(|p| !p.trim().is_empty());
            let result = match prompt {
                Some(prompt) => client::http::DaemonClient::new(&url).and_then(|client| {
                    client.wait_until_ready(std::time::Duration::from_secs(10))?;
                    client::repl::one_shot(&client, &prompt)
                }),
                None => ui::run_ratatui_repl_with_remote(&args, &url).map_err(Into::into),
            };
            if let Err(e) = result {
                eprintln!("client error: {}", e);
                std::process::exit(1);
            }
        }
        Mode::Default => {
            // Start server in background, then launch TUI connected to it.
            let addr = match start_daemon_background() {
                Ok(addr) => addr,
                Err(e) => {
                    eprintln!("daemon error: {}", e);
                    std::process::exit(1);
                }
            };
            let url = format!("http://{addr}");
            if let Err(e) = ui::run_ratatui_repl_with_remote(&args, &url) {
                eprintln!("ui error: {}", e);
                std::process::exit(1);
            }
        }
        Mode::OneShot { prompt } => {
            if let Err(e) = run_one_shot(&prompt, &args) {
                eprintln!("agent error: {}", e);
                std::process::exit(1);
            }
        }
        Mode::Tool => {
            run_interactive();
        }
        Mode::RunTool { name, args } => {
            let parsed = match cli::parse_tool_args(&args) {
                Ok(parsed) => parsed,
                Err(error) => {
                    eprintln!("error: {error}");
                    std::process::exit(1);
                }
            };
            // Read-only tools pass unconditionally (approve_tool never asks
            // for them), so stitching works from non-interactive scripts;
            // write/shell still require interactive approval or trusted env.
            let permission = load_file_config()
                .and_then(|file| permission_from_env_or_file(&file))
                .unwrap_or(PermissionMode::ReadOnly);
            let input = serde_json::to_string(&parsed).unwrap_or_default();
            if !approve_tool(
                permission,
                &name,
                &input,
                &crate::core::console::Console::none(),
            ) {
                eprintln!("Error: permission denied for tool '{name}'");
                std::process::exit(1);
            }
            match execute(&name, &parsed, &GlobalCancellation) {
                Ok(out) => print!("{out}"),
                Err(e) => {
                    eprintln!("Error: {e}");
                    std::process::exit(1);
                }
            }
        }
    }
}
