use std::env;
use std::fs;

use crate::core::types::*;
use crate::skills::*;

pub(crate) fn project_context() -> Option<String> {
    let mut dir = env::current_dir().ok()?;
    loop {
        for name in ["AGENTS.md", "CLAUDE.md"] {
            if let Ok(content) = fs::read_to_string(dir.join(name)) {
                return Some(content);
            }
        }
        if !dir.pop() {
            break;
        }
    }
    None
}

pub(crate) fn system_prompt(skills: &[Skill]) -> String {
    let mut prompt = "You are a coding agent. Use the provided tools to help the user. \
     Prefer reading files before editing. \
     When editing, oldText must match exactly one occurrence in the file. \
     Stop using tools once the requested work is complete. \
     Run relevant tests/checks, inspect the resulting diff, and report validation results. \
     Be concise."
        .to_string();
    prompt.push_str("\n\nTool workflow: use `read` for known files, `grep` for known text, and `git` for repository state/diffs. Use `find` only for targeted filename discovery. Never begin with a repository-wide find using an empty path or pattern `*`; it is noisy and commonly includes build artifacts. If the user names a file, read it directly. For an unfamiliar repository, start with targeted discovery and then read only relevant files. Do not call tools merely to explore when the request can be answered from the conversation; after each result, make progress and avoid repeating identical calls.");
    prompt.push_str("\n\nToken discipline: batch independent tool calls (several reads or greps) in one turn instead of one at a time. `read` paginates — use offset/limit for large files rather than dumping them, and pass `paths` or `glob` to fetch several files in a single call. `grep` defaults to matching file paths; read only the files it names, and use output_mode content only when the matching lines themselves are needed. For a search whose results you must read, one `chain` call (grep/find step routed into read via from/take) beats a search call followed by read calls. Truncation markers report what was elided: narrow the search or paginate instead of repeating the same call.");
    if let Some(ctx) = project_context() {
        prompt.push_str("\n\n--- Project instructions ---\n");
        prompt.push_str(&ctx);
    }
    if !skills.is_empty() {
        prompt.push_str(&format_skills_for_prompt(skills));
    }
    prompt
}
