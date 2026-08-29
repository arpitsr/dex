use std::env;
use std::path::PathBuf;

use crate::core::types::PermissionMode;

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

fn required(input: &mut impl Iterator<Item = String>, flag: &str) -> String {
    input
        .next()
        .unwrap_or_else(|| fail(&format!("{} requires a value", flag)))
}

fn fail(message: &str) -> ! {
    eprintln!("error: {}", message);
    std::process::exit(1)
}
