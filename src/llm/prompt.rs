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

/// Base system prompt, pi-style: identity plus imperative working rules.
/// Tool-behavior detail lives in the tool descriptions (src/llm/protocol.rs),
/// where the model sees it at each tool decision — never duplicated here.
pub(crate) fn system_prompt(skills: &[Skill]) -> String {
    let mut prompt = concat!(
        "You are a coding agent. Work directly with the provided tools — read, search, edit, run, verify — and report the result. \
         Do not narrate a plan, ask permission for clearly requested work, or keep calling tools once the work is done.",
        //
        "\n\nWorking rules:\n",
        "- Batch every independent tool call (several reads, greps, searches) into ONE response — they run in parallel. \
         Sequence only calls that depend on earlier output. Most tasks should take a handful of tool rounds, not one call per file.\n",
        "- Never repeat an identical tool call, and do not explore when the answer is already in the conversation. \
         When output is truncated, narrow the search or paginate — do not re-run the same call.\n",
        "- Read a file before editing it; edit with exact oldText; verify the change (build, tests, diff) before reporting success.\n",
        "- If a request is ambiguous in a way that changes the outcome, ask one pointed question instead of guessing broadly. Otherwise decide and act.",
        //
        "\n\nAnswering: lead with the result, keep prose to what changes the next decision. \
         No restating the request, no closing summary.",
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
        for needle in ["Working rules:", "Answering:"] {
            assert!(base.contains(needle), "missing section: {needle}");
        }
    }

    #[test]
    fn base_prompt_does_not_duplicate_tool_schema_detail() {
        // Pagination/chain/stitching guidance lives in tool descriptions, not here.
        let base = system_prompt(&[]);
        for needle in [
            "offset/limit",
            "from/take",
            "`chain`",
            "$DEX_BIN",
            "output_mode",
        ] {
            assert!(
                !base.contains(needle),
                "tool-schema detail leaked: {needle}"
            );
        }
    }
}
