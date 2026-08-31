use std::env;
use std::io::{self, Write};
use std::path::PathBuf;
use std::process::{Command, Stdio};

use crate::core::console::*;

/// Render an assistant message to the terminal as markdown, with
/// fenced code blocks highlighted via `bat` when available.
pub(crate) fn print_code_block(lang: &str, body: &str) {
    // Try `bat` first (supports language tags + line numbers + theme).
    let bat = ["bat", "batcat"]
        .iter()
        .find_map(|b| which(b).ok().map(|p| (b.to_string(), p)));
    if let Some((bin, path)) = bat {
        let mut cmd = Command::new(&path);
        cmd.args([
            "--color=always",
            "--style=plain,header=fault",
            "--paging=never",
        ]);
        if !lang.is_empty() {
            cmd.args(["-l", lang]);
        }
        if let Ok(mut child) = cmd.arg("-").stdin(Stdio::piped()).spawn() {
            if let Some(stdin) = child.stdin.as_mut() {
                let _ = stdin.write_all(body.as_bytes());
            }
            drop(child.stdin.take());
            if let Ok(out) = child.wait_with_output() {
                if out.status.success() {
                    print!("{}", String::from_utf8_lossy(&out.stdout));
                    return;
                }
            }
        }
        let _ = bin;
    }
    // Fallback: use a small lexer so terminals without bat still get useful
    // syntax colours (strings/comments are consumed before keywords).
    print_ansi_highlighted_code(lang, body);
}

pub(crate) fn print_ansi_highlighted_code(lang: &str, body: &str) {
    const KEYWORD: &str = "\x1b[1;35m";
    const STRING: &str = "\x1b[0;32m";
    const NUMBER: &str = "\x1b[0;33m";
    const COMMENT: &str = "\x1b[0;90m";
    const PUNCT: &str = "\x1b[0;36m";
    let lang = lang.to_ascii_lowercase();
    let hash_comments = matches!(
        lang.as_str(),
        "python" | "py" | "ruby" | "rb" | "bash" | "sh" | "yaml" | "yml" | "toml" | "perl"
    );
    let keywords = match lang.as_str() {
        "rust" | "rs" => "as break const continue crate else enum extern false fn for if impl in let loop match mod move mut pub ref return self Self static struct super trait true type unsafe use where while async await dyn",
        "python" | "py" => "and as assert async await break class continue def del elif else except False finally for from global if import in is lambda None not or pass raise return True try while with yield",
        "javascript" | "js" | "typescript" | "ts" => "as async await break case catch class const continue default delete else export extends false finally for function if import in let new null of return static super this throw true try typeof var while with yield",
        "go" | "golang" => "break case const continue default defer else fallthrough for func go goto if import interface map package range return select struct switch type var",
        _ => "class const def else false fn for function if import let match mut new null pub return static struct true try type while async await",
    };
    let is_keyword = |word: &str| keywords.split_whitespace().any(|k| k == word);

    for line in body.split_inclusive('\n') {
        let chars: Vec<char> = line.chars().collect();
        let mut i = 0;
        while i < chars.len() {
            let c = chars[i];
            if (c == '/' && i + 1 < chars.len() && chars[i + 1] == '/')
                || (c == '#' && hash_comments)
                || (c == '-' && i + 1 < chars.len() && chars[i + 1] == '-')
            {
                print!(
                    "{}{}{}",
                    COMMENT,
                    chars[i..].iter().collect::<String>(),
                    RESET
                );
                break;
            } else if matches!(c, '\"' | '\'' | '`') {
                let quote = c;
                let start = i;
                i += 1;
                while i < chars.len() {
                    if chars[i] == '\\' {
                        i += 2;
                        continue;
                    }
                    let closed = chars[i] == quote;
                    i += 1;
                    if closed {
                        break;
                    }
                }
                print!(
                    "{}{}{}",
                    STRING,
                    chars[start..i.min(chars.len())].iter().collect::<String>(),
                    RESET
                );
            } else if c.is_ascii_digit() {
                let start = i;
                while i < chars.len()
                    && (chars[i].is_ascii_alphanumeric() || matches!(chars[i], '.' | '_'))
                {
                    i += 1;
                }
                print!(
                    "{}{}{}",
                    NUMBER,
                    chars[start..i].iter().collect::<String>(),
                    RESET
                );
            } else if c.is_ascii_alphabetic() || c == '_' {
                let start = i;
                while i < chars.len() && (chars[i].is_ascii_alphanumeric() || chars[i] == '_') {
                    i += 1;
                }
                let word: String = chars[start..i].iter().collect();
                if is_keyword(&word) {
                    print!("{}{}{}", KEYWORD, word, RESET);
                } else {
                    print!("{}", word);
                }
            } else {
                i += 1;
                if "{}[]()<>;:,.=+-*/%!&|?".contains(c) {
                    print!("{}{}{}", PUNCT, c, RESET);
                } else {
                    print!("{}", c);
                }
            }
        }
    }
}

pub(crate) fn which(bin: &str) -> Result<PathBuf, io::Error> {
    let path_var = env::var("PATH").unwrap_or_default();
    for dir in env::split_paths(&path_var) {
        let candidate = dir.join(bin);
        if candidate.is_file() {
            return Ok(candidate);
        }
    }
    Err(io::Error::new(
        io::ErrorKind::NotFound,
        format!("{} not found", bin),
    ))
}
