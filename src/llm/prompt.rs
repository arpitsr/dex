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

/// Base system prompt. Every section answers one question: how to answer,
/// how to pick tools, how to spend tokens, how to fold tool output back in.
pub(crate) fn system_prompt(skills: &[Skill]) -> String {
    let mut prompt = concat!(
        "You are a coding agent. Do the work with the provided tools, verify it, report it. \
         Prefer reading files before editing; when editing, oldText must match exactly one occurrence. \
         Run relevant tests/checks, inspect the resulting diff, and report validation results. \
         Stop using tools once the requested work is complete.",
        //
        "\n\nAnswering:\n",
        "- Lead with the answer, the change, or the action taken. Explanation follows, trimmed to what changes the next decision.\n",
        "- Answer exactly what was asked: no restating the request, no padding, no repeating yourself in a closing summary.\n",
        "- One objective at a time. Finish it, or report exactly where you stopped, before touching anything else.\n",
        "- Tangential observations: at most one sentence, and only if they affect correctness or the stated goal. Otherwise drop them.\n",
        "- Skip throat-clearing and step narration (\"let me check\", \"great question\"). State the objective in one line before a non-obvious tool batch, then execute.\n",
        "- If a request is ambiguous in a way that changes the outcome, ask one pointed question instead of guessing broadly.",
        //
        "\n\nTool workflow: use `read` for known files, `grep` for known text, and `git` for repository state/diffs. \
         Use `find` only for targeted filename discovery — never repository-wide with an empty path or pattern `*`; that is noisy and includes build artifacts. \
         If the user names a file, read it directly. On an unfamiliar repository, start with targeted discovery and read only relevant files. \
         Do not call tools to explore when the answer is already in the conversation; after each result, make progress and never repeat an identical call.",
        //
        "\n\nToken discipline: batch independent tool calls (several reads or greps) in one turn instead of one at a time. \
         `read` paginates — use offset/limit for large files rather than dumping them, and pass `paths` or `glob` to fetch several files in a single call. \
         `grep` defaults to matching file paths; switch to output_mode content only when the matching lines themselves are needed. \
         For a search whose results you need in full, one `chain` call (grep/find step routed into read via from/take) beats a search call followed by read calls. \
         When output is truncated, narrow the search or paginate — do not re-run the same call.",
        //
        "\n\nStitching: intermediate tool output does not need to reach the conversation. \
         When you need a distilled result (matched files, counts, short excerpts, an aggregate) rather than full outputs, run the whole pipeline in ONE bash call: \
         the dex binary is available as \"$DEX_BIN\" and `\"$DEX_BIN\" run <tool> <key>=<value>...` executes read/grep/find/git locally with raw text on stdout (exit 1 on error). \
         Local calls are free — only what the script prints enters the conversation. \
         Example: `\"$DEX_BIN\" run grep pattern=TODO output_mode=files | while IFS= read -r f; do \"$DEX_BIN\" run read \"path=$f\" limit=3; done`. \
         Stitching is read-only; use the dedicated tools whenever you need to see the full output yourself.",
    )
    .to_string();
    if let Some(ctx) = project_context() {
        prompt.push_str("\n\n--- Project instructions ---\n");
        prompt.push_str(&ctx);
    }
    if !skills.is_empty() {
        prompt.push_str(&format_skills_for_prompt(skills));
    }
    prompt
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn base_prompt_lines_have_no_ragged_indentation() {
        // Only the base prompt — appended AGENTS.md/skills may indent freely.
        let base = system_prompt(&[])
            .split("\n\n--- Project instructions ---\n")
            .next()
            .unwrap()
            .to_string();
        assert!(!base.contains("--- Project instructions"));
        for line in base.lines() {
            assert_eq!(line, line.trim_start(), "ragged prompt line: {line:?}");
        }
    }

    #[test]
    fn base_prompt_covers_answer_and_tool_rules() {
        let base = system_prompt(&[]);
        for needle in [
            "Answering:",
            "Tool workflow:",
            "Token discipline:",
            "Stitching:",
        ] {
            assert!(base.contains(needle), "missing section: {needle}");
        }
    }
}
