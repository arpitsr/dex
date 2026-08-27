# ak

A terminal-based coding agent written in Rust. `ak` talks to an OpenAI-compatible
chat-completions API, calls tools (`read`, `bash`, `write`, `edit`, `grep`,
`find`) to operate on your local files, and offers an interactive TUI, a
one-shot prompt mode, and a raw JSON tool mode. Conversations are persisted as
sessions and can be resumed.

## Features

- **OpenAI-compatible backend** — works with any `/chat/completions` endpoint
  (OpenAI, Moonshot/Kimi, etc.). Streaming responses, automatic retries with
  exponential backoff, and configurable reasoning effort.
- **Agentic tool use** — the model can read files, run shell commands, write
  and edit files, and search the filesystem. Multiple tool calls in a single
  assistant turn run in parallel.
- **Interactive TUI** — a `ratatui` REPL with a streaming markdown transcript
  (via `ratatui-markdown`), a custom multi-line input editor with an inline
  block cursor and soft-wrapping (no `tui-textarea` underline / horizontal
  overflow), a solid-bordered input box, autoscroll, a status bar, and a
  visible steering/follow-up queue while the agent is working.
- **Session persistence** — each conversation is saved as a JSONL log. A fresh
  session starts by default; use `--session` to explicitly continue one.
- **Skills** — lightweight, discoverable agent skills (directories with a
  `SKILL.md` frontmatter) can be injected into the system prompt or loaded on
  demand via `/skill:<name>`.
- **History compaction** — when the context window is exceeded, older turns are
  summarized (or truncated as a fallback) to keep requests bounded.
- **Project instructions** — a repo-level `AGENTS.md`/`CLAUDE.md` is appended to
  the system prompt automatically.

## Building

Requires a Rust toolchain (edition 2021):

```sh
cargo build --release
# binary: target/release/ak
```

## Configuration

Configuration is read from a JSON file. The path is resolved in this order:

1. `$RUSTY_PI_CONFIG` (if set)
2. `$XDG_CONFIG_HOME/ak/config.json`
3. `~/.config/ak/config.json`

Copy the sample to get started:

```sh
mkdir -p ~/.config/ak
cp config.sample.json ~/.config/ak/config.json
```

`config.json` fields:

| Field            | Type     | Description                                                        |
| ---------------- | -------- | ------------------------------------------------------------------ |
| `api_key`        | string   | Default API key (overridable by `OPENAI_API_KEY`).                 |
| `base_url`       | string   | Default API base URL (overridable by `OPENAI_BASE_URL`).           |
| `model`          | string   | Default model (overridable by `OPENAI_MODEL`).                     |
| `thinking_effort`| string   | Optional reasoning effort passed to the API (e.g. `"medium"`).     |
| `context_window` | integer  | Token context window used for compaction/status (overridable by `AK_CONTEXT_WINDOW`). |

> Note: the `providers` array in `config.sample.json` is illustrative — the
> code reads a single flat config (`api_key`/`base_url`/`model`), not a provider list.

A minimal example:

```json
{
  "api_key": "sk-...",
  "base_url": "https://api.openai.com/v1",
  "model": "gpt-5.6-luna"
}
```

## Usage

Run `ak` with no arguments to launch the interactive TUI:

```sh
ak
```

Ask it to do something:

```
> read src/main.rs and summarize what it does
```

### One-shot mode

Pass a prompt as arguments to get a single answer (no TUI):

```sh
ak "explain the Cargo.toml dependencies"
```

### Raw tool mode

`ak --tool` reads JSON tool-invocation lines from stdin and prints JSON
results. Useful for piping tool calls from another process:

```sh
echo '{"name":"read","args":{"path":"Cargo.toml"}}' | ak --tool
# => {"ok":"[package]\nname = \"ak\"\n..."}
```

An empty line quits raw tool mode.

## Command-line flags

| Flag               | Description                                              |
| ------------------ | -------------------------------------------------------- |
| `--base-url <url>` | Override the API base URL for this run.                  |
| `--model <name>`   | Override the model for this run.                         |
| `-s`, `--session <path>` | Open/continue a specific session file.             |
| `--no-session`     | Disable session persistence for this run.                |
| `-n`, `--new`      | Start a new session (the default).                       |
| `--name <name>`    | Name the session.                                        |
| `--skill <dir>`    | Add an extra skill directory to discover skills from.    |
| `--tool`           | Run raw JSON tool mode (read JSON lines from stdin).     |
| `--tool`           | Run raw JSON tool mode (read JSON lines from stdin).     |

Any other arguments are treated as a one-shot prompt.

## TUI slash commands

| Command             | Description                                          |
| ------------------- | ---------------------------------------------------- |
| `/quit`             | Exit the REPL.                                       |
| `/clear`            | Clear the conversation history (keeps the system prompt). |
| `/new`              | Start a new session and clear history.               |
| `/session`          | Show the current session id, path, and turn count.   |
| `/resume`           | List saved sessions for this directory.              |
| `/name <name>`      | Rename the current session.                          |
| `/skill:<name>`     | Load a skill's full content into the conversation.    |
| `/model`           | Show the current model.                               |
| `/model <name>`     | Switch the model for the rest of the session.         |
| `/help` (unknown)   | Unknown commands print a hint.                        |

### Keyboard controls

- **Enter** — submit the current input.
- **Shift+Enter** — insert a newline (multi-line input).
- **Enter while working** — queue a steering message for the next model boundary.
- **Alt+Enter while working** — queue a follow-up for after the current task.
- **Esc** or **Ctrl+C** — cancel the active turn and restore queued messages.
- **PageUp/PageDown**, **Shift+Up/Down**, or **mouse wheel** — scroll the transcript.
- **Paste** — pasted text is inserted at the cursor.

### Steering and follow-ups

While `ak` is working, the input remains available. Submitted steering and
follow-up messages stay visible in the queue directly above the input box until
the worker accepts them. Steering is delivered before the next model call;
follow-ups wait until the current task has finished. The queue is kept separate
from the transcript so pending messages do not scroll away.

## Sessions

Sessions are stored as JSONL files under:

- `$XDG_DATA_HOME/ak/sessions` (or `~/.local/share/ak/sessions`),
- organized in subdirectories by a slug of the current working directory.

Each file starts with a `session` header line followed by `message` entries and
optional `session_info` (rename) entries. Entries are appended after every
turn, so a crash or Ctrl+C loses at most the in-progress turn. Starting `ak` in
a directory creates a fresh session; use `--session <path>` to continue a saved
one.

## Skills

Skills are discovered from these directories (first match wins per directory):

- `<cwd>/.ak/skills`
- `<cwd>/.agents/skills`
- `$XDG_CONFIG_HOME/ak/skills` (or `~/.config/ak/skills`)

A skill is a directory containing a `SKILL.md` file with YAML frontmatter:

```markdown
---
name: my-skill
description: Short description surfaced to the model.
---

Detailed instructions / reference content...
```

Only the `name` and `description` are included in the system prompt; the full
body is loaded on demand via `/skill:<name>` or when the conversation
references it.

## Tools

The agent can call the following tools (each maps to a function in the API
schema):

| Tool    | Purpose                                                          |
| ------- | -----------------------------------------------------------------|
| `read`  | Read a file (`path`).                                            |
| `bash`  | Run a shell command via `sh -c` (`command`).                     |
| `write` | Write/overwrite a file (`path`, `content`).                      |
| `edit`  | Replace exactly one occurrence of text (`path`, `oldText`, `newText`). |
| `grep`  | Search file contents recursively for a literal pattern (`pattern`, `path`). |
| `find`  | Find file paths matching a pattern (`pattern`, `path`).          |

Tool results are truncated before being sent back to the model, and a result
cache (`ak-tool-cache.json`) is kept across runs to reduce redundant work.

## Environment variables

| Variable             | Description                                              |
| -------------------- | -------------------------------------------------------- |
| `OPENAI_API_KEY`     | API key (takes precedence over `config.json`).           |
| `OPENAI_BASE_URL`    | API base URL override (non-empty).                       |
| `OPENAI_MODEL`       | Model override.                                          |
| `AK_CONTEXT_WINDOW`  | Token context window for compaction/status (overrides `config.json`). |
| `RUSTY_PI_CONFIG`    | Explicit path to `config.json`.                          |
| `XDG_CONFIG_HOME` / `XDG_DATA_HOME` / `XDG_CACHE_HOME` | XDG base dirs for config/data/cache. |
| `HOME`               | Fallback when XDG vars are unset.                        |

## Project structure

```
ak/
├── .ak/
│   └── skills/           # (optional) project-level agent skills
├── target/
├── Cargo.lock
├── Cargo.toml
├── README.md
├── config.sample.json
└── src/
    ├── main.rs           # agent core: LLM client, tool loop, sessions, CLI
    ├── session.rs        # JSONL session persistence
    ├── tools.rs          # builtin tools: read, bash, write, edit, grep, find
    └── ui.rs             # ratatui interactive REPL
```

## How it works

`src/main.rs` contains the agent core: configuration loading, the OpenAI
client + streaming parser (`read_stream`/`call_llm`), the tool implementations
(`execute`), session persistence (`Session`), skills discovery, history
compaction, and the headless one-shot / raw-tool entry points. `src/ui.rs`
implements the `ratatui` REPL; it runs `process_turn` on a worker thread and
routes streamed output into the UI through a channel (`SinkLine`). `src/tools.rs`
holds the six builtin tool implementations, and `src/session.rs` manages the
JSONL conversation logs.

The turn loop (`process_turn`) repeatedly calls the model with tools enabled,
executes any requested tool calls in parallel, feeds results back, and compacts
history once the message count or estimated token budget is exceeded. A
wrap-up nudge is injected when few tool-call iterations remain.

## License

See the repository for license information.
