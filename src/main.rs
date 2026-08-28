mod agent;
mod cli;
mod config;
mod core;
mod llm;
mod session;
mod skills;
mod tools;
mod ui;

pub(crate) use agent::compaction::*;
pub(crate) use agent::r#loop::*;
pub(crate) use agent::state::*;
pub(crate) use core::console::*;
pub(crate) use llm::client::*;
pub(crate) use llm::config::*;
pub(crate) use llm::prompt::*;
pub(crate) use skills::*;
pub(crate) use core::types::*;

use cli::*;
use session::*;
use tools::*;

use std::env;
use std::io::{self, Write};
use std::path::PathBuf;
use serde_json::{json, Map, Value};

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
    let result = process_turn(
        &config,
        &mut messages,
        &mut state,
        None,
        None,
        session.as_mut(),
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
        .and_then(|file| PermissionMode::from_env_or_file(&file))
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
        let result = if !approve_tool(permission, name, &input) {
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

#[allow(dead_code)]
struct LegacyArgs {
    base_url: Option<String>,
    model: Option<String>,
    session_path: Option<PathBuf>,
    no_session: bool,
    new_session: bool,
    session_name: Option<String>,
    skill_dirs: Vec<PathBuf>,
    permission: Option<PermissionMode>,
    rest: Vec<String>,
}

#[allow(dead_code)]
fn parse_args_from<I: Iterator<Item = String>>(input: I) -> LegacyArgs {
    let mut base_url: Option<String> = None;
    let mut model: Option<String> = None;
    let mut session_path: Option<PathBuf> = None;
    let mut no_session = false;
    let mut new_session = false;
    let mut session_name: Option<String> = None;
    let mut skill_dirs: Vec<PathBuf> = Vec::new();
    let mut permission: Option<PermissionMode> = None;
    let mut rest: Vec<String> = Vec::new();
    let mut args = input;
    while let Some(arg) = args.next() {
        if arg == "--base-url" {
            match args.next() {
                Some(url) => base_url = Some(url),
                None => {
                    eprintln!("error: --base-url requires a value");
                    std::process::exit(1);
                }
            }
        } else if arg == "--model" {
            match args.next() {
                Some(m) => model = Some(m),
                None => {
                    eprintln!("error: --model requires a value");
                    std::process::exit(1);
                }
            }
        } else if arg == "--session" || arg == "-s" {
            match args.next() {
                Some(p) => session_path = Some(PathBuf::from(p)),
                None => {
                    eprintln!("error: --session requires a value");
                    std::process::exit(1);
                }
            }
        } else if arg == "--no-session" {
            no_session = true;
        } else if arg == "--new" || arg == "-n" {
            new_session = true;
        } else if arg == "--name" {
            match args.next() {
                Some(n) => session_name = Some(n),
                None => {
                    eprintln!("error: --name requires a value");
                    std::process::exit(1);
                }
            }
        } else if arg == "--permission" {
            match args.next().and_then(|v| PermissionMode::parse(&v).ok()) {
                Some(mode) => permission = Some(mode),
                None => {
                    eprintln!(
                        "error: --permission requires read-only, ask-writes, ask-shell, or trusted"
                    );
                    std::process::exit(1);
                }
            }
        } else if arg == "--skill" {
            match args.next() {
                Some(p) => skill_dirs.push(PathBuf::from(p)),
                None => {
                    eprintln!("error: --skill requires a value");
                    std::process::exit(1);
                }
            }
        } else {
            rest.push(arg);
        }
    }
    LegacyArgs {
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

fn main() {
    install_sigint_handler();
    let args = cli::parse_args();
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
