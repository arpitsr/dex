use serde_json::Value;
use std::collections::HashMap;
use std::io::{self, BufRead, BufReader, Write};
use std::sync::mpsc;

use crate::core::console::*;
use crate::core::highlight::*;
use crate::core::types::*;
use crate::llm::protocol::*;

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
                termimad::print_text(&format!("{}\n", line));
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

pub(crate) fn read_stream(
    response: reqwest::blocking::Response,
    sink: Option<mpsc::Sender<SinkLine>>,
    cancel: &dyn crate::agent::state::CancellationSource,
) -> Result<(ChatMessage, Option<u64>), Box<dyn std::error::Error>> {
    let mut reader = BufReader::new(response);
    let mut line = String::new();
    let mut content = String::new();
    let mut pending = String::new(); // partial line not yet printed
    let mut printer = StreamPrinter::new(sink.clone());
    let mut tool_calls: Vec<LlmToolCall> = Vec::new();
    let mut usage_tokens: Option<u64> = None;

    loop {
        if cancel.take_cancelled() {
            // Cancellation during generation: stop consuming the stream and
            // unwind so control returns to the prompt.
            with_console(sink.is_some(), || println!());
            io::stdout().flush()?;
            return Err("cancelled".into());
        }
        line.clear();
        if reader.read_line(&mut line)? == 0 {
            break;
        }
        let Some(data) = line.strip_prefix("data: ") else {
            continue;
        };
        let data = data.trim();
        if data == "[DONE]" {
            break;
        }
        // Providers interleave non-chunk payloads (keep-alives, error
        // notices); skipping one unshapely line beats aborting a
        // multi-minute generation.
        let Ok(chunk) = serde_json::from_str::<StreamChunk>(data) else {
            continue;
        };
        if let Some(usage) = &chunk.usage {
            usage_tokens = Some(usage.prompt_tokens);
        }
        for choice in chunk.choices {
            if let Some(text) = choice.delta.content {
                content.push_str(&text);
                // Print complete lines live; keep any partial tail buffered.
                pending.push_str(&text);
                while let Some(pos) = pending.find('\n') {
                    let complete: String = pending.drain(..=pos).collect();
                    printer.feed_line(complete.trim_end_matches('\n'));
                }
                io::stdout().flush()?;
            }
            for delta in choice.delta.tool_calls.unwrap_or_default() {
                merge_chat_tool_call(&mut tool_calls, delta);
            }
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
    Ok((
        ChatMessage {
            role: "assistant".to_string(),
            content: (!content.is_empty()).then_some(content),
            tool_calls: (!tool_calls.is_empty()).then_some(tool_calls),
            tool_call_id: None,
            name: None,
        },
        usage_tokens,
    ))
}

pub(crate) fn read_responses_stream(
    response: reqwest::blocking::Response,
    sink: Option<mpsc::Sender<SinkLine>>,
    cancel: &dyn crate::agent::state::CancellationSource,
) -> Result<(ChatMessage, Option<u64>), Box<dyn std::error::Error>> {
    let mut reader = BufReader::new(response);
    let mut line = String::new();
    let mut content = String::new();
    let mut pending = String::new();
    let mut printer = StreamPrinter::new(sink.clone());
    let mut tool_calls = Vec::new();
    let mut response_items: HashMap<String, usize> = HashMap::new();
    let mut pending_arguments: HashMap<String, String> = HashMap::new();
    let mut usage_tokens = None;

    loop {
        if cancel.take_cancelled() {
            with_console(sink.is_some(), || println!());
            io::stdout().flush()?;
            return Err("cancelled".into());
        }
        line.clear();
        if reader.read_line(&mut line)? == 0 {
            break;
        }
        let Some(data) = line.strip_prefix("data:") else {
            continue;
        };
        let data = data.trim();
        if data == "[DONE]" || data.is_empty() {
            continue;
        }
        let event: Value = match serde_json::from_str(data) {
            Ok(event) => event,
            Err(_) => continue,
        };
        let event_type = event
            .get("type")
            .and_then(Value::as_str)
            .unwrap_or_default();
        match event_type {
            "response.output_text.delta" => {
                if let Some(delta) = event.get("delta").and_then(Value::as_str) {
                    content.push_str(delta);
                    pending.push_str(delta);
                    while let Some(pos) = pending.find('\n') {
                        let complete: String = pending.drain(..=pos).collect();
                        printer.feed_line(complete.trim_end_matches('\n'));
                    }
                    io::stdout().flush()?;
                }
            }
            "response.output_item.added" => {
                if event.pointer("/item/type").and_then(Value::as_str) == Some("function_call") {
                    let item = event.get("item").unwrap_or(&Value::Null);
                    let index = response_call_index(
                        &tool_calls,
                        event
                            .get("output_index")
                            .and_then(Value::as_u64)
                            .unwrap_or(tool_calls.len() as u64) as usize,
                        item,
                    );
                    response_tool_call(&mut tool_calls, index, item);
                    if let Some(item_id) = item.get("id").and_then(Value::as_str) {
                        response_items.insert(item_id.to_string(), index);
                        if let Some(arguments) = pending_arguments.remove(item_id) {
                            if let Some(call) = tool_calls.get_mut(index) {
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
                        .and_then(|id| response_items.get(id).copied())
                    {
                        if let Some(call) = tool_calls.get_mut(index) {
                            call.function.arguments.push_str(delta);
                        }
                    } else {
                        pending_arguments.entry(key).or_default().push_str(delta);
                    }
                }
            }
            "response.output_item.done" => {
                if event.pointer("/item/type").and_then(Value::as_str) == Some("function_call") {
                    let item = event.get("item").unwrap_or(&Value::Null);
                    let index = response_call_index(
                        &tool_calls,
                        event
                            .get("output_index")
                            .and_then(Value::as_u64)
                            .unwrap_or(tool_calls.len() as u64) as usize,
                        item,
                    );
                    response_tool_call(&mut tool_calls, index, item);
                    if let Some(item_id) = item.get("id").and_then(Value::as_str) {
                        response_items.insert(item_id.to_string(), index);
                        if let Some(arguments) = pending_arguments.remove(item_id) {
                            if let Some(call) = tool_calls.get_mut(index) {
                                call.function.arguments.push_str(&arguments);
                            }
                        }
                    }
                }
            }
            "response.completed" | "response.done" => {
                if let Some(usage) = event.pointer("/response/usage") {
                    usage_tokens = usage.get("input_tokens").and_then(Value::as_u64);
                }
            }
            _ => {}
        }
    }

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
    tool_calls.retain(|call| !call.id.is_empty() && !call.function.name.is_empty());
    Ok((
        ChatMessage {
            role: "assistant".to_string(),
            content: (!content.is_empty()).then_some(content),
            tool_calls: (!tool_calls.is_empty()).then_some(tool_calls),
            tool_call_id: None,
            name: None,
        },
        usage_tokens,
    ))
}
