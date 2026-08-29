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
use crate::agent::state::ToolState;
use crate::core::console::install_sigint_handler;
use crate::core::types::{ChatMessage, PermissionMode};
use crate::llm::config::{load_file_config, permission_from_env_or_file, LlmConfig};
use crate::llm::prompt::system_prompt;
use crate::skills::{discover_skills, skill_dirs};

use serde_json::{json, Map, Value};
use std::env;
use std::io::{self, Write};

fn run_one_shot(prompt: &str, args: &Args) -> Result<(), Box<dyn std::error::Error>> {
    let config = LlmConfig::from_env(args.base_url.clone(), args.model.clone(), args.permission)?;
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
    eprintln!("ak raw tool mode");
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
            match execute(name, &args) {
                Ok(out) => json!({"ok": out}),
                Err(e) => json!({"err": e.to_string()}),
            }
        };
        println!("{}", result);
        let _ = stdout.flush();
    }
}

fn main() {
    install_sigint_handler();
    let args = cli::parse_args();

    // Check for --daemon flag.
    if args.rest.first().map(|s| s.as_str()) == Some("--daemon") {
        let addr: std::net::SocketAddr = args
            .rest
            .get(1)
            .and_then(|s| s.parse().ok())
            .unwrap_or(([127, 0, 0, 1], 8420).into());
        let rt = tokio::runtime::Runtime::new().expect("failed to create tokio runtime");
        rt.block_on(async {
            if let Err(e) = daemon::run_daemon(addr).await {
                eprintln!("daemon error: {e}");
                std::process::exit(1);
            }
        });
        return;
    }

    // Check for --connect flag.
    if args.rest.first().map(|s| s.as_str()) == Some("--connect") {
        let daemon_url = args
            .rest
            .get(1)
            .cloned()
            .unwrap_or_else(|| "http://127.0.0.1:8420".to_string());
        // Re-parse args without the --connect and url from rest.
        let mut client_args = args.clone();
        client_args.rest = args.rest[2..].to_vec();
        if let Err(e) = client::run_client(&daemon_url, &client_args) {
            eprintln!("client error: {e}");
            std::process::exit(1);
        }
        return;
    }

    if args.rest.len() == 1 && args.rest[0] == "--tool" {
        run_interactive();
    } else if !args.rest.is_empty() {
        let prompt = args.rest.join(" ");
        if let Err(e) = run_one_shot(&prompt, &args) {
            eprintln!("agent error: {}", e);
            std::process::exit(1);
        }
    } else if let Err(e) = ui::run_ratatui_repl(&args) {
        eprintln!("ui error: {}", e);
        std::process::exit(1);
    }
}
