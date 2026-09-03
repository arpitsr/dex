use serde_json::{json, Value};

use crate::core::types::*;

pub(crate) fn merge_chat_tool_call(calls: &mut Vec<LlmToolCall>, delta: StreamToolCall) {
    while calls.len() <= delta.index {
        calls.push(LlmToolCall {
            id: String::new(),
            call_type: "function".to_string(),
            function: FunctionCall {
                name: String::new(),
                arguments: String::new(),
            },
        });
    }
    let call = &mut calls[delta.index];
    if let Some(id) = delta.id {
        call.id = id;
    }
    if let Some(function) = delta.function {
        if let Some(name) = function.name {
            call.function.name.push_str(&name);
        }
        if let Some(arguments) = function.arguments {
            call.function.arguments.push_str(&arguments);
        }
    }
}

pub(crate) fn tools_schema() -> Vec<ToolDefinition> {
    vec![
        ToolDefinition {
            tool_type: "function".to_string(),
            function: FunctionDef {
                name: "read".to_string(),
                description: "Read file(s) with line numbers (right-aligned number + two spaces + expanded content, tabs expanded per .editorconfig/language), which you can reference in edits. Returns at most 2000 lines; paginate with offset/limit. To avoid extra round trips, pass `paths` (up to 10 files) or `glob` to read several files in ONE call; sections are returned per file.".to_string(),
                parameters: json!({
                    "type": "object",
                    "properties": {
                        "path": { "type": "string", "description": "relative path to a single file" },
                        "paths": { "type": "array", "items": { "type": "string" }, "description": "several files to read in one call (max 10)" },
                        "glob": { "type": "string", "description": "glob fan-out, e.g. 'src/tools/*.rs' or '*.rs' (max 8 files, sorted)" },
                        "offset": { "type": "integer", "description": "1-based line to start from (default 1)" },
                        "limit": { "type": "integer", "description": "maximum lines per file (default 2000 single-file, 200 multi-file)" }
                    },
                    "required": []
                }),
            },
        },
        ToolDefinition {
            tool_type: "function".to_string(),
            function: FunctionDef {
                name: "bash".to_string(),
                description: "Run a shell command. Output is capped (head and tail kept, middle elided). Prefer the read/ffgrep/fffind tools over cat/grep/find here, and prefer targeted commands (grep -n, tail -N, wc) over dumping whole files. For a distilled result (matched files, counts, short excerpts, an aggregate), run the whole pipeline in ONE call: the dex binary is available as \"$DEX_BIN\" and `\"$DEX_BIN\" run <tool> <key>=<value>...` executes read/ffgrep/fffind/git locally with raw output on stdout (exit 1 on error); only what it prints enters the conversation. Example: `\"$DEX_BIN\" run ffgrep pattern=TODO output_mode=files | while IFS= read -r f; do \"$DEX_BIN\" run read \"path=$f\" limit=3; done` (read-only; use dedicated tools when you need to see full output yourself).".to_string(),
                parameters: json!({
                    "type": "object",
                    "properties": { "command": { "type": "string", "description": "shell command to run" } },
                    "required": ["command"]
                }),
            },
        },
        ToolDefinition {
            tool_type: "function".to_string(),
            function: FunctionDef {
                name: "write".to_string(),
                description: "Write or overwrite a file (parent directories are created). For surgical changes to existing files, prefer `edit`.".to_string(),
                parameters: json!({
                    "type": "object",
                    "properties": {
                        "path": { "type": "string" },
                        "content": { "type": "string" }
                    },
                    "required": ["path", "content"]
                }),
            },
        },
        ToolDefinition {
            tool_type: "function".to_string(),
            function: FunctionDef {
                name: "edit".to_string(),
                description: "Replace text in a file. oldText must match exactly one location — include 2-3 surrounding lines to make it unique, or pass replaceAll for every occurrence. Whitespace-only mismatches are retried line-wise. On failure, read the file and retry with exact text.".to_string(),
                parameters: json!({
                    "type": "object",
                    "properties": {
                        "path": { "type": "string" },
                        "oldText": { "type": "string", "description": "exact existing text to replace" },
                        "newText": { "type": "string", "description": "replacement text" },
                        "replaceAll": { "type": "boolean", "description": "replace every occurrence instead of requiring exactly one (default false)" }
                    },
                    "required": ["path", "oldText", "newText"]
                }),
            },
        },
        ToolDefinition {
            tool_type: "function".to_string(),
            function: FunctionDef {
                name: "ffgrep".to_string(),
                description: "Fast frecency-ranked content search (respects .gitignore, git-aware). Regex when the pattern has metacharacters, plain text otherwise; a zero-match query is automatically retried as fuzzy, so typos still hit. Default output is matching file paths; content mode returns path:line:text (with optional context lines) so a follow-up read is often unnecessary. Keep queries SHORT — one term or one regex; multiple words narrow the search (AND), not OR. Narrow with a path prefix ('src/ TODO') or an exclude ('TODO !test/').".to_string(),
                parameters: json!({
                    "type": "object",
                    "properties": {
                        "pattern": { "type": "string", "description": "text or regex to search; may include path prefixes ('src/') and excludes ('!tests/')" },
                        "output_mode": { "type": "string", "enum": ["files", "content"], "description": "files (default): paths only; content: path:line:text" },
                        "head_limit": { "type": "integer", "description": "maximum results (default 50)" },
                        "context": { "type": "integer", "description": "lines of context around each match in content mode (0-10, default 0)" }
                    },
                    "required": ["pattern"]
                }),
            },
        },
        ToolDefinition {
            tool_type: "function".to_string(),
            function: FunctionDef {
                name: "fffind".to_string(),
                description: "Fuzzy file-path search (frecency-ranked, git-aware, typo-tolerant). Matches the whole workspace-relative path, not just the filename; supports path prefixes ('src/') and globs ('**/*.rs'). Keep queries SHORT — 1-2 terms; multiple words narrow (AND), not OR. Start broad with one term and refine.".to_string(),
                parameters: json!({
                    "type": "object",
                    "properties": {
                        "pattern": { "type": "string", "description": "fuzzy path query such as 'main', 'tools fff', or 'src/**/*.rs'; never empty or '*' alone" },
                        "limit": { "type": "integer", "description": "maximum paths returned (default 20)" }
                    },
                    "required": ["pattern"]
                }),
            },
        },
        ToolDefinition {
            tool_type: "function".to_string(),
            function: FunctionDef {
                name: "git".to_string(),
                description: "Inspect repository status or diff (read-only).".to_string(),
                parameters: json!({"type":"object","properties":{"mode":{"type":"string","enum":["status","diff"]}}}),
            },
        },
        ToolDefinition {
            tool_type: "function".to_string(),
            function: FunctionDef {
                name: "chain".to_string(),
                description: "Run a bounded read-only sequence in ONE round trip: a search step (ffgrep files-mode or fffind) followed by read steps that consume the matched files via from/take. Use when later steps depend on earlier output; for independent calls, batch them as parallel calls instead. Mutating and shell tools are not allowed in chains.".to_string(),
                parameters: json!({
                    "type": "object",
                    "properties": {
                        "steps": {
                            "type": "array",
                            "maxItems": 4,
                            "items": {
                                "type": "object",
                                "properties": {
                                    "tool": { "type": "string", "description": "read, ffgrep, fffind, or git" },
                                    "args": { "type": "object", "description": "arguments passed to that tool" },
                                    "from": { "type": "integer", "description": "index of an earlier step whose matched files this read consumes" },
                                    "take": { "type": "string", "enum": ["paths"], "description": "route the referenced step's file paths into this read" },
                                    "max_files": { "type": "integer", "description": "cap on files read when routing from a search step (default 5, max 10)" }
                                },
                                "required": ["tool"]
                            }
                        }
                    },
                    "required": ["steps"]
                }),
            },
        },
    ]
}

pub(crate) fn responses_input(messages: &[ChatMessage]) -> (Option<String>, Vec<Value>) {
    let mut instructions = Vec::new();
    let mut input = Vec::new();
    for message in messages {
        if message.role == "system" {
            if let Some(content) = &message.content {
                instructions.push(content.clone());
            }
            continue;
        }
        if message.role == "tool" {
            input.push(json!({
                "type": "function_call_output",
                "call_id": message.tool_call_id.clone().unwrap_or_default(),
                "output": message.content.clone().unwrap_or_default(),
            }));
            continue;
        }
        if message.role == "assistant" {
            if let Some(content) = &message.content {
                if !content.is_empty() {
                    input.push(json!({ "role": "assistant", "content": content }));
                }
            }
            for call in message.tool_calls.as_deref().unwrap_or_default() {
                input.push(json!({
                    "type": "function_call",
                    "call_id": call.id,
                    "name": call.function.name,
                    "arguments": call.function.arguments,
                }));
            }
            continue;
        }
        input.push(json!({
            "role": message.role,
            "content": message.content.clone().unwrap_or_default(),
        }));
    }
    (
        if instructions.is_empty() {
            None
        } else {
            Some(instructions.join("\n\n"))
        },
        input,
    )
}

pub(crate) fn responses_tools() -> Vec<Value> {
    tools_schema()
        .into_iter()
        .map(|tool| {
            json!({
                "type": "function",
                "name": tool.function.name,
                "description": tool.function.description,
                "parameters": tool.function.parameters,
            })
        })
        .collect()
}

pub(crate) fn response_tool_call(calls: &mut Vec<LlmToolCall>, index: usize, item: &Value) {
    while calls.len() <= index {
        calls.push(LlmToolCall {
            id: String::new(),
            call_type: "function".to_string(),
            function: FunctionCall {
                name: String::new(),
                arguments: String::new(),
            },
        });
    }
    let call = &mut calls[index];
    if let Some(id) = item
        .get("call_id")
        .or_else(|| item.get("id"))
        .and_then(Value::as_str)
    {
        call.id = id.to_string();
    }
    if let Some(name) = item.get("name").and_then(Value::as_str) {
        call.function.name = name.to_string();
    }
    if let Some(arguments) = item.get("arguments").and_then(Value::as_str) {
        call.function.arguments = arguments.to_string();
    }
}

pub(crate) fn response_call_index(calls: &[LlmToolCall], index: usize, item: &Value) -> usize {
    item.get("call_id")
        .or_else(|| item.get("id"))
        .and_then(Value::as_str)
        .and_then(|id| calls.iter().position(|call| call.id == id))
        .unwrap_or(index)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn merge_chat_tool_call_assembles_fragmented_deltas() {
        let mut calls = Vec::new();
        merge_chat_tool_call(
            &mut calls,
            StreamToolCall {
                index: 0,
                id: Some("call_1".into()),
                function: Some(StreamFunctionCall {
                    name: Some("read".into()),
                    arguments: Some("{\"path\":\"".into()),
                }),
            },
        );
        merge_chat_tool_call(
            &mut calls,
            StreamToolCall {
                index: 0,
                id: None,
                function: Some(StreamFunctionCall {
                    name: None,
                    arguments: Some("a.rs\"}".into()),
                }),
            },
        );
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].id, "call_1");
        assert_eq!(calls[0].function.name, "read");
        assert_eq!(calls[0].function.arguments, r#"{"path":"a.rs"}"#);
    }

    #[test]
    fn merge_chat_tool_call_grows_sparse_indices() {
        let mut calls = Vec::new();
        merge_chat_tool_call(
            &mut calls,
            StreamToolCall {
                index: 2,
                id: Some("c".into()),
                function: Some(StreamFunctionCall {
                    name: Some("bash".into()),
                    arguments: Some("{}".into()),
                }),
            },
        );
        assert_eq!(calls.len(), 3);
        assert_eq!(calls[2].id, "c");
        assert!(calls[0].id.is_empty());
    }

    #[test]
    fn tools_schema_contains_all_tools() {
        let schema = tools_schema();
        let names: Vec<_> = schema.iter().map(|t| t.function.name.as_str()).collect();
        assert_eq!(names, ["read", "bash", "write", "edit", "ffgrep", "fffind", "git", "chain"]);
    }

    #[test]
    fn responses_input_splits_system_and_tool_output() {
        let msgs = vec![
            ChatMessage {
                role: "system".into(),
                content: Some("sys1".into()),
                tool_calls: None,
                tool_call_id: None,
                name: None,
            },
            ChatMessage {
                role: "system".into(),
                content: Some("sys2".into()),
                tool_calls: None,
                tool_call_id: None,
                name: None,
            },
            ChatMessage {
                role: "user".into(),
                content: Some("hi".into()),
                tool_calls: None,
                tool_call_id: None,
                name: None,
            },
            ChatMessage {
                role: "tool".into(),
                content: Some("out".into()),
                tool_calls: None,
                tool_call_id: Some("call_1".into()),
                name: None,
            },
        ];
        let (instructions, input) = responses_input(&msgs);
        assert_eq!(instructions.unwrap(), "sys1\n\nsys2");
        assert_eq!(input[0]["role"], "user");
        assert_eq!(input[1]["type"], "function_call_output");
        assert_eq!(input[1]["call_id"], "call_1");
    }

    #[test]
    fn responses_input_encodes_assistant_tool_calls() {
        let msgs = vec![ChatMessage {
            role: "assistant".into(),
            content: Some("thinking".into()),
            tool_calls: Some(vec![LlmToolCall {
                id: "c1".into(),
                call_type: "function".into(),
                function: FunctionCall { name: "read".into(), arguments: "{}".into() },
            }]),
            tool_call_id: None,
            name: None,
        }];
        let (instructions, input) = responses_input(&msgs);
        assert!(instructions.is_none());
        assert_eq!(input[0]["role"], "assistant");
        assert_eq!(input[1]["type"], "function_call");
        assert_eq!(input[1]["call_id"], "c1");
    }

    #[test]
    fn response_tool_call_and_index_resolve_by_id() {
        let mut calls = vec![LlmToolCall {
            id: "a".into(),
            call_type: "function".into(),
            function: FunctionCall { name: "old".into(), arguments: String::new() },
        }];
        // Resolve existing id to index 0 even when suggested index is 5.
        let idx = response_call_index(&calls, 5, &json!({"call_id":"a"}));
        assert_eq!(idx, 0);
        // Unknown id falls back to suggested index.
        assert_eq!(response_call_index(&calls, 5, &json!({"call_id":"miss"})), 5);
        // Append a new call via response_tool_call.
        response_tool_call(&mut calls, 1, &json!({"call_id":"b","name":"write","arguments":"{}"}));
        assert_eq!(calls[1].id, "b");
        assert_eq!(calls[1].function.name, "write");
    }
}
