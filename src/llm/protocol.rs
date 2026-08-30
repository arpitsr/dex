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
                description: "Read file(s) with line numbers (line\\tcontent), which you can reference in edits. Returns at most 2000 lines; paginate with offset/limit. To avoid extra round trips, pass `paths` (up to 10 files) or `glob` to read several files in ONE call; sections are returned per file.".to_string(),
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
                description: "Run a shell command. Output is capped (head and tail kept, middle elided). Prefer the read/grep/find tools over cat/grep/find here, and prefer targeted commands (grep -n, tail -N, wc) over dumping whole files.".to_string(),
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
                name: "grep".to_string(),
                description: "Search file contents (ripgrep when available; regex, smart-case). Default returns matching file paths only — read the files it names, or switch to content/count mode for matches.".to_string(),
                parameters: json!({
                    "type": "object",
                    "properties": {
                        "pattern": { "type": "string", "description": "regular expression to search for" },
                        "path": { "type": "string", "description": "directory or file to search (default: current directory)" },
                        "output_mode": { "type": "string", "enum": ["files", "content", "count"], "description": "files (default): paths only; content: path:line:text; count: matches per file" },
                        "head_limit": { "type": "integer", "description": "maximum lines returned (default 100 files / 200 content lines / 50 counts)" },
                        "context": { "type": "integer", "description": "lines of context around each match in content mode (0-10, default 0); avoids a follow-up read" }
                    },
                    "required": ["pattern"]
                }),
            },
        },
        ToolDefinition {
            tool_type: "function".to_string(),
            function: FunctionDef {
                name: "find".to_string(),
                description: "Find file paths by substring, sorted and capped; do not use an empty path or the pattern `*` (use `grep` or a narrow path/pattern instead).".to_string(),
                parameters: json!({
                    "type": "object",
                    "properties": {
                        "pattern": { "type": "string", "description": "Targeted filename/path substring such as `.rs` or `src/main`; never `*` alone." },
                        "path": { "type": "string", "description": "workspace-relative directory or file; use a narrow directory such as `src`" },
                        "limit": { "type": "integer", "description": "maximum paths returned (default 100)" }
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
                description: "Run a bounded read-only sequence in ONE round trip: a search step (grep files-mode or find) followed by read steps that consume the matched files via from/take. Use when later steps depend on earlier output; for independent calls, batch them as parallel calls instead. Mutating and shell tools are not allowed in chains.".to_string(),
                parameters: json!({
                    "type": "object",
                    "properties": {
                        "steps": {
                            "type": "array",
                            "maxItems": 4,
                            "items": {
                                "type": "object",
                                "properties": {
                                    "tool": { "type": "string", "description": "read, grep, find, or git" },
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
