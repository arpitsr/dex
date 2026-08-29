use std::io::{self, Write};

use crate::protocol::{ApprovalDecision, StreamEvent};

use super::http::DaemonClient;

/// One-shot mode: send a single prompt and print the response.
pub(crate) fn one_shot(
    client: &DaemonClient,
    prompt: &str,
) -> Result<(), Box<dyn std::error::Error>> {
    let cwd = std::env::current_dir()
        .map(|p| p.to_string_lossy().into_owned())
        .unwrap_or_default();
    let session = client.create_session(&cwd, None)?;

    eprintln!("session: {}", session.session_id);

    let events = client.chat_with_approval(&session.session_id, prompt, |name, input| {
        eprintln!("\n  Approve {name}? ({input})");
        eprint!("  [y/N/s(session)] ");
        io::stderr().flush().ok();
        let mut answer = String::new();
        io::stdin().read_line(&mut answer).ok();
        match answer.trim().to_ascii_lowercase().as_str() {
            "y" | "yes" => ApprovalDecision::AllowOnce,
            "s" | "session" => ApprovalDecision::AllowSession,
            _ => ApprovalDecision::Deny,
        }
    })?;

    for event in &events {
        match event {
            StreamEvent::AssistantText(text) => {
                print!("{text}");
                io::stdout().flush()?;
            }
            StreamEvent::ToolCall { name, .. } => {
                eprintln!("\n  > {name}...");
            }
            StreamEvent::ToolResult {
                name,
                summary,
                success,
            } => {
                let icon = if *success { "✓" } else { "✗" };
                eprintln!("  {icon} {name}: {summary}");
            }
            StreamEvent::TurnFailed { error } => {
                eprintln!("\nerror: {error}");
            }
            StreamEvent::System(msg) => {
                eprintln!("[system] {msg}");
            }
            StreamEvent::Error(msg) => {
                eprintln!("[error] {msg}");
            }
            _ => {}
        }
    }

    println!();
    Ok(())
}

/// Interactive REPL mode.
pub(crate) fn run_repl(client: &DaemonClient) -> Result<(), Box<dyn std::error::Error>> {
    let cwd = std::env::current_dir()
        .map(|p| p.to_string_lossy().into_owned())
        .unwrap_or_default();
    let session = client.create_session(&cwd, None)?;

    println!("Connected to daemon. Session: {}", session.session_id);
    println!("Type your prompt and press Enter. Ctrl+C to quit.\n");

    let stdin = io::stdin();

    loop {
        print!("> ");
        io::stdout().flush()?;

        let mut input = String::new();
        match stdin.read_line(&mut input) {
            Ok(0) => break,
            Ok(_) => {}
            Err(e) => {
                eprintln!("read error: {e}");
                break;
            }
        }

        let input = input.trim();
        if input.is_empty() {
            continue;
        }
        if input == "/quit" || input == "/exit" {
            break;
        }
        if input == "/sessions" {
            match client.list_sessions() {
                Ok(sessions) => {
                    for s in &sessions {
                        println!(
                            "  {} {} ({})",
                            s.session_id,
                            s.name.as_deref().unwrap_or("(unnamed)"),
                            s.cwd,
                        );
                    }
                }
                Err(e) => eprintln!("error listing sessions: {e}"),
            }
            continue;
        }
        if input == "/cancel" {
            if let Err(e) = client.cancel(&session.session_id) {
                eprintln!("error cancelling: {e}");
            } else {
                println!("cancel sent");
            }
            continue;
        }

        let events = match client.chat_with_approval(&session.session_id, input, |name, input| {
            eprintln!("\n  Approve {name}? ({input})");
            eprint!("  [y/N/s(session)] ");
            io::stderr().flush().ok();
            let mut answer = String::new();
            io::stdin().read_line(&mut answer).ok();
            match answer.trim().to_ascii_lowercase().as_str() {
                "y" | "yes" => ApprovalDecision::AllowOnce,
                "s" | "session" => ApprovalDecision::AllowSession,
                _ => ApprovalDecision::Deny,
            }
        }) {
            Ok(e) => e,
            Err(e) => {
                eprintln!("error: {e}");
                continue;
            }
        };

        for event in &events {
            match event {
                StreamEvent::AssistantText(text) => {
                    print!("{text}");
                    io::stdout().flush()?;
                }
                StreamEvent::ToolCall { name, .. } => {
                    eprintln!("\n  > {name}...");
                }
                StreamEvent::ToolResult {
                    name,
                    summary,
                    success,
                } => {
                    let icon = if *success { "✓" } else { "✗" };
                    eprintln!("  {icon} {name}: {summary}");
                }
                StreamEvent::TurnFailed { error } => {
                    eprintln!("\nerror: {error}");
                }
                StreamEvent::System(msg) => {
                    eprintln!("[system] {msg}");
                }
                StreamEvent::Error(msg) => {
                    eprintln!("[error] {msg}");
                }
                _ => {}
            }
        }

        println!();
    }

    Ok(())
}
