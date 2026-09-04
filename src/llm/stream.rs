//! SSE stream readers. A shared driver owns framing, printing, cancellation,
//! and usage threading; one thin parser per wire protocol translates raw
//! `data:` lines into protocol-agnostic [`StreamEvent`]s. Protocol state
//! (tool-call merging, reasoning replay) stays parser-local, so the driver
//! is written once and a new protocol plugs in as another [`StreamParser`].

use serde_json::Value;
use std::collections::HashMap;
use std::io::{self, BufRead, BufReader, Write};
use std::sync::mpsc;

use crate::core::console::with_console;
use crate::core::highlight::{print_code_block, print_markdown_text};
use crate::core::types::{
    ChatMessage, LlmToolCall, Role, SinkLine, StopReason, StreamChunk, StreamDelta, Usage,
};
use crate::llm::protocol::{merge_chat_tool_call, response_call_index, response_tool_call};

/// Incremental markdown printer: prose is flushed as soon as a full line
/// arrives; code fences are buffered until closed so they can be highlighted
/// as one block. If a fence is still open when the stream ends, it is
/// flushed as-is.
pub(crate) struct StreamPrinter {
    in_code: bool,
    code_lang: String,
    code_body: String,
    sink: Option<mpsc::Sender<SinkLine>>,
}

impl StreamPrinter {
    pub(crate) fn new(sink: Option<mpsc::Sender<SinkLine>>) -> Self {
        Self {
            in_code: false,
            code_lang: String::new(),
            code_body: String::new(),
            sink,
        }
    }

    pub(crate) fn feed_line(&mut self, line: &str) {
        if self.sink.is_some() {
            // Sink mode: no spinner to erase; stream directly.
            self.feed_line_inner(line);
        } else {
            with_console(self.sink.is_some(), || self.feed_line_inner(line));
        }
    }

    pub(crate) fn feed_line_inner(&mut self, line: &str) {
        let trimmed = line.trim_start();
        if trimmed.starts_with("```") {
            if self.in_code {
                if let Some(sink) = &self.sink {
                    sink.send(SinkLine::Assistant(format!(
                        "```{}:\n{}\n```",
                        self.code_lang, self.code_body
                    )))
                    .ok();
                } else {
                    print_code_block(&self.code_lang, &self.code_body);
                }
                self.code_body.clear();
                self.code_lang.clear();
                self.in_code = false;
            } else {
                self.in_code = true;
                self.code_lang = trimmed.trim_start_matches('`').trim().to_string();
            }
        } else if self.in_code {
            self.code_body.push_str(line);
            self.code_body.push('\n');
        } else {
            if let Some(sink) = &self.sink {
                sink.send(SinkLine::Assistant(line.to_string())).ok();
            } else {
                print_markdown_text(line);
            }
        }
    }

    pub(crate) fn finish(self) {
        if self.in_code && !self.code_body.is_empty() {
            if let Some(sink) = &self.sink {
                sink.send(SinkLine::Assistant(format!(
                    "```{}:\n{}\n```",
                    self.code_lang, self.code_body
                )))
                .ok();
            } else {
                with_console(self.sink.is_some(), || {
                    print_code_block(&self.code_lang, &self.code_body)
                });
            }
        }
    }
}

/// Reasoning delta under the provider-specific chat-completions key, as a
/// string; non-string shapes (some providers send arrays) yield None.
pub(crate) fn delta_thought(delta: &StreamDelta) -> Option<&str> {
    delta
        .reasoning
        .as_ref()
        .and_then(Value::as_str)
        .or_else(|| delta.reasoning_content.as_ref().and_then(Value::as_str))
}

/// A failure raised after output has already been emitted to the user (text
/// or reasoning streamed live); a retried call would duplicate that partial
/// transcript. Purely a type-level marker: `Display` passes the inner message
/// through untouched so callers matching on `"cancelled"` etc. keep working.
#[derive(Debug)]
pub(crate) struct MidStreamError(pub String);

impl std::fmt::Display for MidStreamError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for MidStreamError {}

/// Wrap a stream failure as [`MidStreamError`] only once output has flowed;
/// a failure before any output is retryable (protocol fallback may re-run
/// the turn without duplicating anything).
fn stream_err(message: &str, output_flowed: bool) -> Box<dyn std::error::Error> {
    if output_flowed {
        Box::new(MidStreamError(message.to_string()))
    } else {
        message.into()
    }
}

/// Mid-stream events a parser emits per SSE line; the driver owns what
/// happens to them (printing, accumulation, usage threading, lifecycle).
enum StreamEvent {
    /// Reasoning delta; UI-only, routed to the sink, never persisted.
    Thinking(String),
    /// Content delta; accumulated and printed live by the driver.
    Text(String),
    /// Provider-reported token usage (the last one wins).
    Usage(Usage),
    /// Terminal condition reported by the provider.
    Stop(StopReason),
    /// Protocol-level end of stream (chat-completions `[DONE]`); the driver
    /// stops reading. The responses API ends at EOF instead.
    Done,
}

/// Parser-owned parts of the final assistant message; the driver owns
/// `content` and assembles the message itself.
struct ParsedMessage {
    tool_calls: Vec<LlmToolCall>,
    reasoning_items: Option<Vec<Value>>,
    reasoning_content: Option<String>,
}

/// One protocol parser, fed raw SSE lines. All protocol-specific state
/// (tool-call merging, reasoning replay) lives in the implementation.
trait StreamParser {
    fn feed(&mut self, line: &str) -> Vec<StreamEvent>;
    fn finish(self) -> ParsedMessage;
}

/// One model turn read off the wire: the assembled assistant message, the
/// last reported usage, and the provider-reported terminal condition (`None`
/// when the stream ended without one — mid-stream drop, or a provider that
/// omits `finish_reason`).
#[derive(Debug)]
pub(crate) struct Turn {
    pub(crate) message: ChatMessage,
    pub(crate) usage: Option<Usage>,
    pub(crate) stop_reason: Option<StopReason>,
}

/// Shared SSE driver: feeds each line to the parser, prints text/reasoning
/// as it arrives, tracks usage and the terminal condition, and assembles the
/// final turn. Fails with [`MidStreamError`] once any output has been
/// emitted — a retry would duplicate it.
fn run_sse<P: StreamParser>(
    response: reqwest::blocking::Response,
    sink: Option<mpsc::Sender<SinkLine>>,
    cancel: &dyn crate::agent::state::CancellationSource,
    mut parser: P,
) -> Result<Turn, Box<dyn std::error::Error>> {
    let mut reader = BufReader::new(response);
    let mut line = String::new();
    let mut content = String::new();
    let mut pending = String::new(); // partial line not yet printed
    let mut printer = StreamPrinter::new(sink.clone());
    let mut usage: Option<Usage> = None;
    let mut stop_reason: Option<StopReason> = None;
    let mut output_flowed = false;

    loop {
        if cancel.take_cancelled() {
            // Cancellation during generation: stop consuming the stream and
            // unwind so control returns to the prompt.
            with_console(sink.is_some(), || println!());
            io::stdout().flush()?;
            return Err(stream_err("cancelled", output_flowed));
        }
        line.clear();
        if reader.read_line(&mut line)? == 0 {
            break;
        }
        let mut done = false;
        for event in parser.feed(&line) {
            match event {
                StreamEvent::Thinking(thought) => {
                    output_flowed = true;
                    if let Some(sink) = &sink {
                        sink.send(SinkLine::Thinking(thought)).ok();
                    }
                }
                StreamEvent::Text(text) => {
                    output_flowed = true;
                    content.push_str(&text);
                    // Print complete lines live; keep any partial tail buffered.
                    pending.push_str(&text);
                    while let Some(pos) = pending.find('\n') {
                        // Memmove the remainder to the front instead of
                        // collecting a new String per completed line.
                        let complete = &pending[..=pos];
                        printer.feed_line(complete.trim_end_matches('\n'));
                        pending.replace_range(..=pos, "");
                    }
                    // One flush per delta is a syscall per token; sink consumers
                    // get their liveness from the channel (console printing needs
                    // the flush).
                    if sink.is_none() {
                        io::stdout().flush()?;
                    }
                }
                StreamEvent::Usage(u) => usage = Some(u),
                StreamEvent::Stop(reason) => stop_reason = Some(reason),
                StreamEvent::Done => done = true,
            }
        }
        if done {
            break;
        }
    }

    // Flush any trailing partial line, then close out buffered code blocks.
    if !pending.is_empty() {
        printer.feed_line(&pending);
        io::stdout().flush()?;
    }
    printer.finish();
    io::stdout().flush()?;
    if !content.is_empty() {
        with_console(sink.is_some(), || println!());
        io::stdout().flush()?;
    }

    let parsed = parser.finish();
    Ok(Turn {
        message: ChatMessage {
            role: Role::Assistant,
            content: (!content.is_empty()).then_some(content),
            tool_calls: (!parsed.tool_calls.is_empty()).then_some(parsed.tool_calls),
            tool_call_id: None,
            name: None,
            reasoning_items: parsed.reasoning_items,
            reasoning_content: parsed.reasoning_content,
        },
        usage,
        stop_reason,
    })
}

/// Read a chat-completions SSE body into a turn.
pub(crate) fn read_stream(
    response: reqwest::blocking::Response,
    sink: Option<mpsc::Sender<SinkLine>>,
    cancel: &dyn crate::agent::state::CancellationSource,
) -> Result<Turn, Box<dyn std::error::Error>> {
    run_sse(response, sink, cancel, ChatCompletionsParser::default())
}

/// Read a responses-API SSE body into a turn.
pub(crate) fn read_responses_stream(
    response: reqwest::blocking::Response,
    sink: Option<mpsc::Sender<SinkLine>>,
    cancel: &dyn crate::agent::state::CancellationSource,
) -> Result<Turn, Box<dyn std::error::Error>> {
    run_sse(response, sink, cancel, ResponsesParser::default())
}

fn stop_reason_from_finish(finish: &str) -> Option<StopReason> {
    match finish {
        "stop" => Some(StopReason::Stop),
        "length" => Some(StopReason::Length),
        "tool_calls" | "function_call" => Some(StopReason::ToolUse),
        // Unrecognized provider reason — leave unset rather than guess.
        _ => None,
    }
}

/// `POST /chat/completions` stream: JSON chunks under `data: ` (note the
/// space), terminated by `[DONE]`. Tool-call deltas merge by `index`.
#[derive(Default)]
struct ChatCompletionsParser {
    tool_calls: Vec<LlmToolCall>,
    /// DeepSeek-style reasoning text, replayed on the assistant message.
    reasoning: String,
}

impl StreamParser for ChatCompletionsParser {
    fn feed(&mut self, line: &str) -> Vec<StreamEvent> {
        let Some(data) = line.strip_prefix("data: ") else {
            return Vec::new();
        };
        let data = data.trim();
        if data == "[DONE]" {
            return vec![StreamEvent::Done];
        }
        // Providers interleave non-chunk payloads (keep-alives, error
        // notices); skipping one unshapely line beats aborting a
        // multi-minute generation.
        let Ok(chunk) = serde_json::from_str::<StreamChunk>(data) else {
            return Vec::new();
        };
        let mut events = Vec::new();
        if let Some(usage) = &chunk.usage {
            events.push(StreamEvent::Usage(Usage {
                prompt_tokens: usage.prompt_tokens,
                completion_tokens: usage.completion_tokens,
                cached_tokens: usage.prompt_details.as_ref().map(|d| d.cached_tokens),
            }));
        }
        for choice in chunk.choices {
            if let Some(text) = delta_thought(&choice.delta) {
                events.push(StreamEvent::Thinking(text.to_string()));
            }
            // DeepSeek-style reasoning text: captured for replay on the
            // assistant message so the provider gets its own reasoning
            // thread back next request (self-gating: only set when the
            // provider streamed this exact field).
            if let Some(text) = choice
                .delta
                .reasoning_content
                .as_ref()
                .and_then(Value::as_str)
            {
                self.reasoning.push_str(text);
            }
            if let Some(text) = choice.delta.content {
                events.push(StreamEvent::Text(text));
            }
            for delta in choice.delta.tool_calls.unwrap_or_default() {
                merge_chat_tool_call(&mut self.tool_calls, delta);
            }
            if let Some(reason) = &choice.finish_reason {
                if let Some(stop) = stop_reason_from_finish(reason) {
                    events.push(StreamEvent::Stop(stop));
                }
            }
        }
        events
    }

    fn finish(self) -> ParsedMessage {
        ParsedMessage {
            tool_calls: self.tool_calls,
            reasoning_items: None,
            reasoning_content: (!self.reasoning.is_empty()).then_some(self.reasoning),
        }
    }
}

/// `POST /responses` stream: typed JSON events under `data:`; the body ends
/// at EOF. Tool calls arrive as items (added/done) plus argument deltas.
#[derive(Default)]
struct ResponsesParser {
    tool_calls: Vec<LlmToolCall>,
    response_items: HashMap<String, usize>,
    pending_arguments: HashMap<String, String>,
    reasoning_items: Vec<Value>,
}

impl ResponsesParser {
    /// Usage + terminal condition from a completed/incomplete response event.
    fn response_terminal(&self, response: &Value, events: &mut Vec<StreamEvent>) {
        if let Some(usage) = response.get("usage") {
            if let Some(input) = usage.get("input_tokens").and_then(Value::as_u64) {
                events.push(StreamEvent::Usage(Usage {
                    prompt_tokens: input,
                    completion_tokens: usage
                        .get("output_tokens")
                        .and_then(Value::as_u64)
                        .unwrap_or(0),
                    cached_tokens: usage
                        .pointer("/input_tokens_details/cached_tokens")
                        .and_then(Value::as_u64),
                }));
            }
        }
        let truncated = response.pointer("/incomplete_details/reason")
            == Some(&Value::String("max_output_tokens".to_string()));
        events.push(StreamEvent::Stop(if truncated {
            StopReason::Length
        } else {
            StopReason::Stop
        }));
    }
}

impl StreamParser for ResponsesParser {
    fn feed(&mut self, line: &str) -> Vec<StreamEvent> {
        let Some(data) = line.strip_prefix("data:") else {
            return Vec::new();
        };
        let data = data.trim();
        if data == "[DONE]" || data.is_empty() {
            return Vec::new();
        }
        let Ok(event) = serde_json::from_str::<Value>(data) else {
            return Vec::new();
        };
        let event_type = event
            .get("type")
            .and_then(Value::as_str)
            .unwrap_or_default();
        let mut events = Vec::new();
        match event_type {
            "response.reasoning_summary_text.delta" | "response.reasoning_text.delta" => {
                if let Some(delta) = event.get("delta").and_then(Value::as_str) {
                    events.push(StreamEvent::Thinking(delta.to_string()));
                }
            }
            "response.output_text.delta" => {
                if let Some(delta) = event.get("delta").and_then(Value::as_str) {
                    events.push(StreamEvent::Text(delta.to_string()));
                }
            }
            "response.output_item.added" => {
                if event.pointer("/item/type").and_then(Value::as_str) == Some("function_call") {
                    let item = event.get("item").unwrap_or(&Value::Null);
                    let index = response_call_index(
                        &self.tool_calls,
                        event
                            .get("output_index")
                            .and_then(Value::as_u64)
                            .unwrap_or(self.tool_calls.len() as u64)
                            as usize,
                        item,
                    );
                    response_tool_call(&mut self.tool_calls, index, item);
                    if let Some(item_id) = item.get("id").and_then(Value::as_str) {
                        self.response_items.insert(item_id.to_string(), index);
                        if let Some(arguments) = self.pending_arguments.remove(item_id) {
                            if let Some(call) = self.tool_calls.get_mut(index) {
                                call.function.arguments.push_str(&arguments);
                            }
                        }
                    }
                }
            }
            "response.function_call_arguments.delta" => {
                if let Some(delta) = event.get("delta").and_then(Value::as_str) {
                    let key = event
                        .get("item_id")
                        .and_then(Value::as_str)
                        .map(str::to_string)
                        .unwrap_or_else(|| {
                            format!(
                                "output:{}",
                                event
                                    .get("output_index")
                                    .and_then(Value::as_u64)
                                    .unwrap_or(0)
                            )
                        });
                    if let Some(index) = event
                        .get("item_id")
                        .and_then(Value::as_str)
                        .and_then(|id| self.response_items.get(id).copied())
                    {
                        if let Some(call) = self.tool_calls.get_mut(index) {
                            call.function.arguments.push_str(delta);
                        }
                    } else {
                        self.pending_arguments
                            .entry(key)
                            .or_default()
                            .push_str(delta);
                    }
                }
            }
            "response.output_item.done" => {
                if event.pointer("/item/type").and_then(Value::as_str) == Some("function_call") {
                    let item = event.get("item").unwrap_or(&Value::Null);
                    let index = response_call_index(
                        &self.tool_calls,
                        event
                            .get("output_index")
                            .and_then(Value::as_u64)
                            .unwrap_or(self.tool_calls.len() as u64)
                            as usize,
                        item,
                    );
                    response_tool_call(&mut self.tool_calls, index, item);
                    if let Some(item_id) = item.get("id").and_then(Value::as_str) {
                        self.response_items.insert(item_id.to_string(), index);
                        if let Some(arguments) = self.pending_arguments.remove(item_id) {
                            if let Some(call) = self.tool_calls.get_mut(index) {
                                call.function.arguments.push_str(&arguments);
                            }
                        }
                    }
                } else if event.pointer("/item/type").and_then(Value::as_str) == Some("reasoning") {
                    // Keep raw reasoning items for stateless replay next
                    // request; summary-only items (no encrypted_content)
                    // can't be replayed and would corrupt the thread.
                    let item = event.get("item").unwrap_or(&Value::Null);
                    if item.get("encrypted_content").is_some_and(|v| {
                        !v.is_null() && v.as_str().map(|s| !s.is_empty()).unwrap_or(true)
                    }) {
                        self.reasoning_items.push(item.clone());
                    }
                }
            }
            "response.completed" | "response.done" | "response.incomplete" => {
                if let Some(response) = event.get("response") {
                    self.response_terminal(response, &mut events);
                }
            }
            _ => {}
        }
        events
    }

    fn finish(self) -> ParsedMessage {
        // Completed calls without an id must be dropped, not executed.
        let mut tool_calls = self.tool_calls;
        tool_calls.retain(|call| !call.id.is_empty() && !call.function.name.is_empty());
        ParsedMessage {
            tool_calls,
            reasoning_items: (!self.reasoning_items.is_empty()).then_some(self.reasoning_items),
            reasoning_content: None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{
        delta_thought, read_responses_stream, read_stream, stream_err, SinkLine, StreamDelta,
        StreamPrinter, Usage,
    };
    use crate::core::console::CancellationToken;
    use crate::core::types::{StopReason, StreamUsage};
    use crate::llm::streaming::is_mid_stream;
    use std::sync::mpsc;

    /// The compaction summarizer (and any in-process caller) must be able to
    /// pass a sink and have ALL streamed output routed into the channel —
    /// never printed raw to stdout, which inside the TUI process is the
    /// alternate screen (ghost text until resize). With sink=None the raw
    /// print is intentional (plain one-shot CLI streaming); callers running
    /// beside a TUI must always pass a sink.
    #[test]
    fn stream_printer_with_sink_routes_lines_to_channel_not_stdout() {
        let (tx, rx) = mpsc::channel();
        let mut printer = StreamPrinter::new(Some(tx));
        printer.feed_line("Key facts: internal summary line");
        printer.feed_line("```rust");
        printer.feed_line("fn main() {}");
        printer.feed_line("```");
        printer.finish();

        let lines: Vec<SinkLine> = rx.try_iter().collect();
        assert!(lines
            .iter()
            .any(|l| matches!(l, SinkLine::Assistant(s) if s.contains("Key facts"))));
        assert!(lines
            .iter()
            .any(|l| matches!(l, SinkLine::Assistant(s) if s.contains("fn main"))));
    }

    #[test]
    fn reasoning_deltas_extract_from_provider_specific_fields() {
        let deepseek: StreamDelta =
            serde_json::from_str(r#"{"reasoning_content":"step 1"}"#).unwrap();
        assert_eq!(delta_thought(&deepseek), Some("step 1"));
        let openrouter: StreamDelta = serde_json::from_str(r#"{"reasoning":"step 2"}"#).unwrap();
        assert_eq!(delta_thought(&openrouter), Some("step 2"));
        // Non-string shapes must not kill the chunk parse or yield text.
        let array: StreamDelta = serde_json::from_str(r#"{"reasoning":[{"a":1}]}"#).unwrap();
        assert_eq!(delta_thought(&array), None);
        let plain: StreamDelta = serde_json::from_str(r#"{"content":"hi"}"#).unwrap();
        assert_eq!(delta_thought(&plain), None);
    }

    /// Chat-completions nests cached tokens under `prompt_tokens_details`; a
    /// provider that omits the detail object must parse as None, not fail.
    #[test]
    fn stream_usage_parses_cached_tokens_with_and_without_detail() {
        let usage: StreamUsage = serde_json::from_str(
            r#"{"prompt_tokens":100,"completion_tokens":17,"prompt_tokens_details":{"cached_tokens":42}}"#,
        )
        .unwrap();
        assert_eq!(usage.prompt_tokens, 100);
        assert_eq!(usage.completion_tokens, 17);
        assert_eq!(usage.prompt_details.map(|d| d.cached_tokens), Some(42));

        let usage: StreamUsage = serde_json::from_str(r#"{"prompt_tokens":100}"#).unwrap();
        assert!(usage.prompt_details.is_none());
        // Providers that omit completion_tokens parse as 0, not fail.
        assert_eq!(usage.completion_tokens, 0);
    }

    // ---- SSE parser tests: real reqwest::blocking::Response built from an
    // http::Response with a raw SSE body, so both wire parsers are exercised
    // end to end without a server. ----

    fn sse_response(lines: &[&str]) -> reqwest::blocking::Response {
        let body = lines.join("\n\n") + "\n\n";
        http::Response::builder()
            .status(200)
            .body(body)
            .unwrap()
            .into()
    }

    /// A reasoning output item with encrypted_content is captured onto the
    /// message for stateless replay; summary-only items are not.
    #[test]
    fn responses_stream_captures_replayable_reasoning_items() {
        let (tx, _rx) = mpsc::channel();
        let resp = sse_response(&[
            r#"data: {"type":"response.output_item.done","output_index":0,"item":{"type":"reasoning","id":"r1","summary":[],"encrypted_content":"blob1"}}"#,
            r#"data: {"type":"response.output_item.done","output_index":1,"item":{"type":"reasoning","id":"r2","summary":[{"type":"summary_text","text":"visible"}]}}"#,
            r#"data: {"type":"response.output_item.added","output_index":2,"item":{"type":"function_call","id":"item_1","call_id":"call_1","name":"read","arguments":"{}"}}"#,
            "data: [DONE]",
        ]);
        let msg = read_responses_stream(resp, Some(tx), &CancellationToken::new())
            .unwrap()
            .message;
        let items = msg.reasoning_items.expect("replayable items captured");
        assert_eq!(items.len(), 1, "summary-only item must be skipped");
        assert_eq!(items[0]["encrypted_content"], "blob1");
        assert!(msg.reasoning_content.is_none());
    }

    /// Null AND empty-string `encrypted_content` are both unreplayable:
    /// an empty blob would corrupt the thread if sent back.
    #[test]
    fn responses_stream_skips_null_and_empty_encrypted_content() {
        let (tx, _rx) = mpsc::channel();
        let resp = sse_response(&[
            r#"data: {"type":"response.output_item.done","output_index":0,"item":{"type":"reasoning","id":"r1","summary":[],"encrypted_content":""}}"#,
            r#"data: {"type":"response.output_item.done","output_index":1,"item":{"type":"reasoning","id":"r2","summary":[],"encrypted_content":null}}"#,
            "data: [DONE]",
        ]);
        let msg = read_responses_stream(resp, Some(tx), &CancellationToken::new())
            .unwrap()
            .message;
        assert!(
            msg.reasoning_items.is_none(),
            "empty/null blobs must not be captured: {:?}",
            msg.reasoning_items
        );
    }

    /// DeepSeek-style chat-completions reasoning: `reasoning_content` deltas
    /// accumulate onto the message for replay; other reasoning keys are
    /// UI-only and must not leak into the replay field.
    #[test]
    fn chat_stream_captures_reasoning_content_for_replay() {
        let (tx, _rx) = mpsc::channel();
        let resp = sse_response(&[
            r#"data: {"choices":[{"delta":{"reasoning_content":"step 1"}}]}"#,
            r#"data: {"choices":[{"delta":{"reasoning":"openrouter-style"}}]}"#,
            r#"data: {"choices":[{"delta":{"reasoning_content":" step 2"}}]}"#,
            r#"data: {"choices":[{"delta":{"content":"done"}}]}"#,
            "data: [DONE]",
        ]);
        let msg = read_stream(resp, Some(tx), &CancellationToken::new())
            .unwrap()
            .message;
        assert_eq!(msg.reasoning_content.as_deref(), Some("step 1 step 2"));
        assert!(msg.reasoning_items.is_none());
    }

    /// Chat-completions stream: content accumulates across chunks and splits
    /// into complete sink lines; fragmented tool-call deltas merge into one
    /// call; the usage chunk (with cache detail) surfaces as `Some(Usage)`.
    #[test]
    fn chat_stream_assembles_content_tools_and_usage() {
        let (tx, rx) = mpsc::channel();
        let resp = sse_response(&[
            r#"data: {"choices":[{"delta":{"role":"assistant"}}]}"#,
            r#"data: {"choices":[{"delta":{"content":"hello "}}]}"#,
            r#"data: {"choices":[{"delta":{"content":"world\n"}}]}"#,
            r#"data: {"choices":[{"delta":{"content":"second line"}}]}"#,
            r#"data: {"choices":[{"delta":{"tool_calls":[{"index":0,"id":"c1","function":{"name":"read","arguments":"{\"pa"}}]}}]}"#,
            r#"data: {"choices":[{"delta":{"tool_calls":[{"index":0,"function":{"arguments":"th\":\"a.rs\"}"}}]}}]}"#,
            r#"data: {"choices":[],"usage":{"prompt_tokens":123,"completion_tokens":45,"prompt_tokens_details":{"cached_tokens":7}}}"#,
            "data: [DONE]",
        ]);
        let turn = read_stream(resp, Some(tx), &CancellationToken::new()).unwrap();
        let usage = turn.usage;
        let msg = turn.message;
        assert_eq!(msg.content.as_deref(), Some("hello world\nsecond line"));
        let calls = msg.tool_calls.unwrap();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].id, "c1");
        assert_eq!(calls[0].function.name, "read");
        assert_eq!(calls[0].function.arguments, r#"{"path":"a.rs"}"#);
        assert_eq!(
            usage,
            Some(Usage {
                prompt_tokens: 123,
                completion_tokens: 45,
                cached_tokens: Some(7)
            })
        );
        let lines: Vec<SinkLine> = rx.try_iter().collect();
        assert!(matches!(&lines[0], SinkLine::Assistant(s) if s == "hello world"));
        assert!(matches!(&lines[1], SinkLine::Assistant(s) if s == "second line"));
    }

    /// A code fence opened mid-stream is buffered and flushed as one block;
    /// prose before/after streams line by line.
    #[test]
    fn chat_stream_buffers_code_fences() {
        let (tx, rx) = mpsc::channel();
        let resp = sse_response(&[
            r#"data: {"choices":[{"delta":{"content":"```rust\n"}}]}"#,
            r#"data: {"choices":[{"delta":{"content":"fn main() {}\n"}}]}"#,
            r#"data: {"choices":[{"delta":{"content":"```\n"}}]}"#,
            r#"data: {"choices":[{"delta":{"content":"trailing prose"}}]}"#,
            "data: [DONE]",
        ]);
        let msg = read_stream(resp, Some(tx), &CancellationToken::new())
            .unwrap()
            .message;
        assert_eq!(
            msg.content.as_deref(),
            Some("```rust\nfn main() {}\n```\ntrailing prose")
        );
        let lines: Vec<SinkLine> = rx.try_iter().collect();
        // The fence body keeps its streamed trailing newline and the close
        // adds one more — current sink contract; consumers re-parse the block.
        assert!(
            matches!(&lines[0], SinkLine::Assistant(s)
            if s == "```rust:\nfn main() {}\n\n```"),
            "{lines:?}"
        );
        assert!(matches!(&lines[1], SinkLine::Assistant(s) if s == "trailing prose"));
    }

    /// `finish_reason` on the final chunk maps onto the normalized
    /// [`StopReason`] (length truncation, tool-call handoff, clean stop).
    #[test]
    fn chat_stream_maps_finish_reason_to_stop_reason() {
        for (finish, expected) in [
            ("stop", StopReason::Stop),
            ("length", StopReason::Length),
            ("tool_calls", StopReason::ToolUse),
            ("function_call", StopReason::ToolUse),
        ] {
            let (tx, _rx) = mpsc::channel();
            let resp = sse_response(&[
                r#"data: {"choices":[{"delta":{"content":"hi"}}]}"#,
                &format!(r#"data: {{"choices":[{{"delta":{{}},"finish_reason":"{finish}"}}]}}"#),
                "data: [DONE]",
            ]);
            let turn = read_stream(resp, Some(tx), &CancellationToken::new()).unwrap();
            assert_eq!(turn.stop_reason, Some(expected), "finish_reason={finish}");
            assert_eq!(turn.message.content.as_deref(), Some("hi"));
        }
        // Unrecognized reasons stay unset instead of being guessed.
        let (tx, _rx) = mpsc::channel();
        let resp = sse_response(&[
            r#"data: {"choices":[{"delta":{},"finish_reason":"content_filter"}]}"#,
            "data: [DONE]",
        ]);
        let turn = read_stream(resp, Some(tx), &CancellationToken::new()).unwrap();
        assert_eq!(turn.stop_reason, None);
    }

    /// A pre-cancelled token unwinds before reading anything.
    #[test]
    fn chat_stream_returns_cancelled_error_when_token_set() {
        let token = CancellationToken::new();
        token.cancel();
        let (tx, _rx) = mpsc::channel();
        let resp = sse_response(&[r#"data: {"choices":[{"delta":{"content":"x"}}]}"#]);
        let err = read_stream(resp, Some(tx), &token).unwrap_err();
        assert_eq!(err.to_string(), "cancelled");
    }

    /// Responses API: the function-call state machine must survive deltas
    /// that arrive BEFORE their item is added (buffered then flushed), and
    /// completed calls without an id must be dropped, not executed.
    #[test]
    fn responses_stream_reassembles_tool_calls_with_early_deltas() {
        let (tx, _rx) = mpsc::channel();
        let resp = sse_response(&[
            r#"data: {"type":"response.output_text.delta","delta":"hello\n"}"#,
            r#"data: {"type":"response.output_item.added","output_index":0,"item":{"type":"function_call","id":"item_1","call_id":"call_1","name":"read","arguments":""}}"#,
            r#"data: {"type":"response.function_call_arguments.delta","item_id":"item_2","delta":"EARLY"}"#,
            r#"data: {"type":"response.output_item.added","output_index":1,"item":{"type":"function_call","id":"item_2","call_id":"call_2","name":"bash","arguments":""}}"#,
            r#"data: {"type":"response.function_call_arguments.delta","item_id":"item_2","delta":"LATER"}"#,
            r#"data: {"type":"response.output_item.added","output_index":3,"item":{"type":"function_call","name":"ghost"}}"#,
            r#"data: {"type":"response.completed","response":{"usage":{"input_tokens":50,"output_tokens":9,"input_tokens_details":{"cached_tokens":5}}}}"#,
            "data: [DONE]",
        ]);
        let turn = read_responses_stream(resp, Some(tx), &CancellationToken::new()).unwrap();
        let usage = turn.usage;
        let msg = turn.message;
        assert_eq!(msg.content.as_deref(), Some("hello\n"));
        let calls = msg.tool_calls.unwrap();
        assert_eq!(
            calls.len(),
            2,
            "ghost call (no id) and padding must be filtered: {calls:?}"
        );
        assert_eq!(calls[0].id, "call_1");
        assert_eq!(calls[0].function.name, "read");
        assert_eq!(calls[1].id, "call_2");
        assert_eq!(calls[1].function.arguments, "EARLYLATER");
        assert_eq!(
            usage,
            Some(Usage {
                prompt_tokens: 50,
                completion_tokens: 9,
                cached_tokens: Some(5)
            })
        );
    }

    /// Garbage, empty, and keep-alive data lines are skipped; reasoning
    /// deltas (both provider keys) land as Thinking sink lines; a stream
    /// with no output yields an empty assistant message.
    #[test]
    fn responses_stream_tolerates_garbage_and_extracts_reasoning() {
        let (tx, rx) = mpsc::channel();
        let resp = sse_response(&[
            "data: not json at all",
            "data: ",
            r#"data: {"type":"response.reasoning_summary_text.delta","delta":"thinking"}"#,
            r#"data: {"type":"response.reasoning_text.delta","delta":" more"}"#,
            "data: [DONE]",
        ]);
        let turn = read_responses_stream(resp, Some(tx), &CancellationToken::new()).unwrap();
        assert_eq!(turn.message.content, None);
        assert!(turn.message.tool_calls.is_none());
        assert_eq!(turn.usage, None);
        let lines: Vec<SinkLine> = rx.try_iter().collect();
        assert!(matches!(&lines[0], SinkLine::Thinking(s) if s == "thinking"));
        assert!(matches!(&lines[1], SinkLine::Thinking(s) if s == " more"));
    }

    /// `response.completed` normalizes to a clean stop; `response.incomplete`
    /// with `max_output_tokens` normalizes to Length (truncated output).
    #[test]
    fn responses_stream_maps_terminal_events_to_stop_reasons() {
        let (tx, _rx) = mpsc::channel();
        let resp = sse_response(&[
            r#"data: {"type":"response.output_text.delta","delta":"partial"}"#,
            r#"data: {"type":"response.incomplete","response":{"usage":{"input_tokens":10,"output_tokens":99},"incomplete_details":{"reason":"max_output_tokens"}}}"#,
            "data: [DONE]",
        ]);
        let turn = read_responses_stream(resp, Some(tx), &CancellationToken::new()).unwrap();
        assert_eq!(turn.stop_reason, Some(StopReason::Length));
        assert_eq!(
            turn.usage,
            Some(Usage {
                prompt_tokens: 10,
                completion_tokens: 99,
                cached_tokens: None
            })
        );

        let (tx, _rx) = mpsc::channel();
        let resp = sse_response(&[
            r#"data: {"type":"response.completed","response":{"usage":{"input_tokens":3,"output_tokens":4}}}"#,
            "data: [DONE]",
        ]);
        let turn = read_responses_stream(resp, Some(tx), &CancellationToken::new()).unwrap();
        assert_eq!(turn.stop_reason, Some(StopReason::Stop));
    }

    /// The mid-stream marker applies only once output has flowed; before
    /// that a failure stays retryable. Display passes the message through so
    /// `"cancelled"` matching keeps working.
    #[test]
    fn stream_error_marking_follows_output_flow() {
        assert!(!is_mid_stream(&*stream_err("boom", false)));
        assert!(is_mid_stream(&*stream_err("boom", true)));
        assert_eq!(stream_err("cancelled", true).to_string(), "cancelled");
    }
}
