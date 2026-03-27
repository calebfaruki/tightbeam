# Tightbeam

[![made-with-rust](https://img.shields.io/badge/Made%20with-Rust-1f425f.svg)](https://www.rust-lang.org/)

LLM and MCP proxy for agent workspaces. The daemon proxies LLM API calls and remote MCP tool calls through a unix socket. The runtime drives the agent loop inside the container. Credentials never cross the socket boundary.

## How It Works

Two components:

1. **Runtime** — a binary inside the agent container that runs the agent loop. Waits for human messages, sends turns to the daemon, executes tools locally (bash, file I/O), and sends results back.

2. **Daemon** — a sidecar process in the daemon container, one per agent. Listens on a unix socket, reads API credentials from mounted secret files, manages conversation history, forwards requests to the LLM provider, and executes MCP tool calls on behalf of the agent.

Each agent gets its own daemon instance. LLM config (provider, model, API key) comes from k8s Secret volume mounts at `/run/secrets/llm/`. MCP servers are declared as subdirectories of `/run/secrets/mcp/`.

The runtime sends new messages via `turn` requests over a length-prefixed binary framing protocol. The daemon reads the API key from a secret file, forwards to the LLM provider, and streams the response back. When the LLM requests MCP tools (GitHub, web search, etc.), the daemon executes them directly. Every exchange is logged to NDJSON files.

## Why Tightbeam

AI agents running in containers need to call LLM APIs, but giving them API keys means:

- **Credential exposure** — a compromised agent leaks your API key
- **No audit trail** — the agent calls whatever it wants with your credentials
- **No conversation control** — the agent manages its own context window

Tightbeam solves this by proxying LLM calls through the daemon. The container sends messages, the daemon attaches credentials and manages history. The runtime is stateless — it doesn't know the API key, the model, or even the provider.

When the container has no network egress, tightbeam is the agent's sole communication gateway to the outside world.

Use [Airlock](https://github.com/calebfaruki/airlock) for CLI credential isolation. Use Tightbeam for LLM API isolation.

## Installation

### Container Setup

Download the runtime from [releases](https://github.com/calebfaruki/tightbeam/releases) and add it to your Dockerfile:

```dockerfile
COPY tightbeam /usr/local/bin/tightbeam
```

The runtime drives the agent loop. It connects to the daemon socket, loads the system prompt from `/etc/agent/` (all `.md` files, sorted and concatenated), and enters the agent loop.

```sh
tightbeam --tools bash,read_file,write_file,list_directory
```

The runtime connects to the daemon socket at `/run/tightbeam/tightbeam.sock`.

Flags:

| Flag | Required | Default | Description |
|------|----------|---------|-------------|
| `--tools` | yes | — | Comma-separated tool list |
| `--max-iterations` | no | 100 | Max tool call rounds per human message |
| `--max-output-chars` | no | 30000 | Truncate tool output beyond this |

Available tools: `bash`, `read_file`, `write_file`, `list_directory`.

The system prompt is assembled automatically from all `.md` files in `/etc/agent/` inside the container. Files are sorted by path and concatenated, supporting both single-file and multi-file layouts.

### LLM Config (k8s Secret Mount)

LLM config is read from files mounted at `/run/secrets/llm/`:

```
/run/secrets/llm/provider     -> "anthropic"
/run/secrets/llm/model         -> "claude-sonnet-4-20250514"
/run/secrets/llm/api-key       -> "sk-ant-..."
/run/secrets/llm/max-tokens    -> "8192"          # optional, defaults to 8192
```

The daemon reads these files at startup. Missing `provider`, `model`, or `api-key` is a hard error. Values are trimmed of whitespace.

In k8s, these are populated by a Secret volume mount. The orchestrator creates the Secret and mounts it into the daemon container.

### MCP Config (Mounted Directory)

MCP servers are declared as subdirectories of `/run/secrets/mcp/`:

```
/run/secrets/mcp/
  github/
    url           # "https://mcp.github.com/sse" (required)
    auth_token    # "ghp_xxxx" (optional — absent means no auth)
    tools         # one tool name per line (optional — absent means all)
  filesystem/
    url           # "http://localhost:9100/sse"
```

Each subdirectory is one MCP server. The subdirectory name is the server's logical name. If the directory is empty or doesn't exist, the daemon starts with zero MCP servers.

### Docker Run

Mount the agent's socket into the container:

```sh
docker run \
    -v /run/sockets/my-agent.sock:/run/docker-tightbeam.sock \
    your-image
```

Conversation logs are written to `<logs-dir>/<name>/`. Mount them read-only if the agent needs prior context on restart:

```sh
docker run \
    -v /run/sockets/my-agent.sock:/run/docker-tightbeam.sock \
    -v /var/log/tightbeam/my-agent:/var/log/tightbeam:ro \
    your-image
```

## Usage

### Daemon

```sh
tightbeam-daemon start
tightbeam-daemon logs
tightbeam-daemon send <message>
tightbeam-daemon check
tightbeam-daemon version
```

No flags. All paths are hardcoded:
- LLM config: `/run/secrets/llm/`
- MCP config: `/run/secrets/mcp/`
- Socket: `/run/tightbeam/tightbeam.sock`
- Logs: `/var/log/tightbeam/conversation.ndjson`

The orchestrator mounts configs, secrets, and volumes at these paths.


### Runtime

The runtime runs inside the container. It connects to the daemon socket, loads the system prompt from `/etc/agent/`, and enters the agent loop:

1. Register with the daemon, then wait for a human message
2. Send a `turn` with the message (system prompt and tools are cached from the first turn)
3. If the LLM returns tool calls, execute them locally and send results in a new `turn`
4. Repeat until `end_turn` or `max_tokens`, then wait for the next human message

## Socket Protocol

JSON-RPC 2.0 with length-prefixed binary framing. Each message is preceded by a 4-byte big-endian `u32` payload length, followed by the UTF-8 JSON payload.

```
[4 bytes: u32 big-endian length][payload bytes]
```

All `content` fields are arrays of typed blocks:

```json
{"role": "user", "content": [{"type": "text", "text": "Hello"}]}
```

### Request: `turn`

The runtime sends new messages, tool definitions, and optionally a system prompt. System and tools are sent on the first turn and cached by the daemon.

```json
{
  "jsonrpc": "2.0",
  "id": 1,
  "method": "turn",
  "params": {
    "system": "You are a coding assistant.",
    "tools": [{"name": "bash", "description": "Run a command", "parameters": {"type": "object"}}],
    "messages": [{"role": "user", "content": [{"type": "text", "text": "What files are in src?"}]}]
  }
}
```

Tool results are sent as messages in a subsequent `turn`:

```json
{
  "jsonrpc": "2.0",
  "id": 2,
  "method": "turn",
  "params": {
    "messages": [{"role": "tool", "tool_call_id": "tc-001", "content": [{"type": "text", "text": "main.rs\nlib.rs\n"}]}]
  }
}
```

### Response: Streaming Notifications

No `id` field — these stream in real time as the LLM generates output.

```json
{"jsonrpc": "2.0", "method": "output", "params": {"stream": "content", "data": {"type": "text", "text": "The src"}}}
{"jsonrpc": "2.0", "method": "output", "params": {"stream": "content", "data": {"type": "text", "text": " directory contains"}}}
```

### Response: Final

Has `id` — signals completion of this turn.

```json
{
  "jsonrpc": "2.0",
  "id": 1,
  "result": {
    "stop_reason": "end_turn",
    "content": [{"type": "text", "text": "The src directory contains main.rs and lib.rs."}]
  }
}
```

When the LLM requests tool calls:

```json
{
  "jsonrpc": "2.0",
  "id": 1,
  "result": {
    "stop_reason": "tool_use",
    "tool_calls": [{"id": "tc-001", "name": "bash", "input": {"command": "ls src/"}}]
  }
}
```

### Response: Error

API errors forwarded as-is. Tightbeam does not retry. The agent decides what to do.

```json
{
  "jsonrpc": "2.0",
  "id": 1,
  "error": {"code": 429, "message": "Rate limit exceeded."}
}
```

### Connection Handshake

Every connection to an agent socket must identify itself with its first frame:

- **Runtime** sends a `register` request. The daemon enters the runtime handler (turn loop).
- **CLI / channel adapter** sends a `send` request. The daemon enters the subscriber handler.

Runtime registration:
```json
{"jsonrpc": "2.0", "method": "register", "params": {"role": "runtime"}}
```

No response. The daemon begins sending `human_message` notifications when messages arrive.

### Request: `send`

Inject a human message into the agent's conversation. Sent by CLI or channel adapters as the first frame on a new connection.

```json
{
  "jsonrpc": "2.0",
  "id": 1,
  "method": "send",
  "params": {
    "content": [{"type": "text", "text": "Create a hello world file"}]
  }
}
```

Response (agent idle, message delivered immediately):
```json
{"jsonrpc": "2.0", "id": 1, "result": {"status": "delivered"}}
```

Response (agent busy, message queued):
```json
{"jsonrpc": "2.0", "id": 1, "result": {"status": "queued"}}
```

After a queued message is delivered:
```json
{"jsonrpc": "2.0", "method": "delivered"}
```

Response (no runtime connected):
```json
{"jsonrpc": "2.0", "id": 1, "error": {"code": -32000, "message": "runtime not connected"}}
```

### File Transfer

The `send` request supports image delivery via a multi-frame protocol. The content array includes `file_incoming` blocks alongside text, followed by one raw-byte frame per file.

```json
{
  "jsonrpc": "2.0",
  "id": 1,
  "method": "send",
  "params": {
    "content": [
      {"type": "text", "text": "Describe this image"},
      {"type": "file_incoming", "filename": "photo.png", "mime_type": "image/png", "size": 102400}
    ]
  }
}
```

The daemon validates all `file_incoming` blocks, then writes the RPC response (`delivered`/`queued`) **before** reading any file frames. The CLI reads the response before sending file data. This ordering prevents deadlock.

Each file frame uses the same 4-byte BE u32 length prefix as JSON frames, but the payload is raw bytes (not JSON). Frames are sent in the same order as their `file_incoming` blocks in the content array.

The daemon base64-encodes each file and replaces the `file_incoming` block with an `image` block before delivering to the runtime:

```json
{"type": "image", "media_type": "image/png", "data": "<base64>"}
```

v1 supports: `image/png`, `image/jpeg`, `image/gif`, `image/webp`. Unsupported MIME types are rejected with an error response. Files larger than 4GB are rejected (framing limit).

### Subscriber Notifications

After sending a message, the connection becomes a subscriber. Subscribers receive copies of the agent's text output (not tool calls) and lifecycle events:

Text output (streamed):
```json
{"jsonrpc": "2.0", "method": "output", "params": {"stream": "content", "data": {"type": "text", "text": "Hello"}}}
```

Agent turn complete:
```json
{"jsonrpc": "2.0", "method": "end_turn"}
```

Runtime disconnected:
```json
{"jsonrpc": "2.0", "method": "error", "params": {"message": "agent disconnected"}}
```

Subscribers receive text output only. Tool use events are internal to the runtime and are not broadcast.

## Configuration

### Agent Config

Config comes from two sources:

**LLM** — k8s Secret volume mounted at `/run/secrets/llm/`:

```
/run/secrets/llm/provider     -> "anthropic"
/run/secrets/llm/model         -> "claude-sonnet-4-20250514"
/run/secrets/llm/api-key       -> "sk-ant-..."
/run/secrets/llm/max-tokens    -> "8192"          # optional, defaults to 8192
```

Missing `provider`, `model`, or `api-key` is a hard error. Values are trimmed of whitespace.

**MCP** — subdirectories of `/run/secrets/mcp/`:

```
/run/secrets/mcp/
  github/
    url           # "https://mcp.github.com/sse" (required)
    auth_token    # "ghp_xxxx" (optional — absent means no auth)
    tools         # one tool name per line (optional — absent means all)
  web-search/
    url           # "https://mcp.search.example.com/sse"
    auth_token    # "search-token-xxx"
```

Each subdirectory is one MCP server. The subdirectory name is the server's logical name. If the directory is empty or doesn't exist, zero MCP servers.

### MCP Tool Allowlists

The `tools` file in each MCP server subdirectory controls which tools the LLM can call:

| Value | Meaning |
|-------|---------|
| file absent | Allow all tools from the server |
| file present, one name per line | Allow only named tools |

## MCP Support

The daemon acts as an MCP client. It connects to remote MCP servers, discovers their tools, and merges them with the runtime's local tools. The LLM sees one flat tool list.

When the LLM returns tool calls, the daemon partitions them:

- **All local** — returned to the runtime for execution
- **All MCP** — daemon executes them, appends results to conversation, calls the LLM again. The runtime waits and receives the final response.
- **Mixed** — daemon executes MCP calls immediately, returns only the local calls to the runtime. When the runtime sends back local results, the daemon interleaves all results in the original call order and continues.

MCP connections are lazy (first turn, not startup) and cached for the session. Auth uses Bearer tokens read from the `auth_token` file. If a connection drops mid-session, the daemon retries once.

## Conversation Ownership

Tightbeam owns the conversation. The runtime is stateless.

1. Runtime sends a `turn` with new messages
2. Daemon logs messages to NDJSON, attaches credentials, forwards to LLM
3. Response streams back — daemon logs it, forwards to runtime
4. Runtime executes tool calls locally, sends results in next `turn`
5. Daemon logs results, calls LLM again
6. Loop continues until `end_turn` or `max_tokens`
7. External messages arrive via `send` — the daemon delivers them as `human_message` notifications to the runtime, and the loop resumes from step 1

## Filesystem Layout

Each daemon instance serves one agent. All paths are hardcoded:

```
/run/secrets/llm/                           # LLM config (k8s Secret mount)
/run/secrets/mcp/                         # MCP server configs (mounted directory)
/run/tightbeam/tightbeam.sock               # unix socket (mode 0600)
/var/log/tightbeam/conversation.ndjson      # conversation log
```

The orchestrator mounts secrets and configs at these locations.

## Security Model

- LLM credentials are read from k8s Secret volume mounts at `/run/secrets/llm/`. MCP credentials are read from `auth_token` files in `/run/secrets/mcp/<server>/`.
- API keys and MCP auth tokens never cross the socket boundary.
- The agent does not know which model or provider it talks to.
- LLM provider is swappable at the config layer without agent changes.
- MCP servers are configured by the daemon. The runtime has no knowledge of MCP.
- All messages (user, assistant, tool results) are logged to NDJSON files.
- Errors are forwarded as-is. Tightbeam does not retry or modify API responses.
- External message delivery (`send`) goes through the daemon. The daemon queues messages for a busy agent and tracks subscribers. Subscribers see agent text output only, never tool call internals.
