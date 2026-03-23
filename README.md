# Tightbeam

[![made-with-rust](https://img.shields.io/badge/Made%20with-Rust-1f425f.svg)](https://www.rust-lang.org/)

LLM proxy for containerized AI agents. The daemon proxies LLM API calls and remote MCP tool calls through a unix socket. The runtime drives the agent loop inside the container. Credentials never cross the socket boundary.

## How It Works

Three components:

1. **Runtime** — a binary inside the container that runs the agent loop. Waits for human messages, sends turns to the daemon, executes tools locally (bash, file I/O), and sends results back. The agent logic lives here.

2. **Daemon** — a long-running process on the host. Listens on per-agent unix sockets, attaches API credentials, manages conversation history, forwards requests to LLM providers, and executes MCP tool calls on behalf of the agent.

3. **Registry + profiles** — a global registry defines available LLM providers and MCP servers. Per-agent TOML profiles reference registry entries by key, with optional overrides and MCP tool allowlists. Each agent gets its own unix socket. The agent never sees which model it talks to.

The runtime sends new messages via `turn` requests. The daemon attaches the API key from the host environment, forwards to the LLM provider, and streams the response back. When the LLM requests MCP tools (GitHub, web search, etc.), the daemon executes them directly. Every exchange is logged to per-agent NDJSON files on the host.

## Why Tightbeam

AI agents running in containers need to call LLM APIs, but giving them API keys means:

- **Credential exposure** — a compromised agent leaks your API key
- **No audit trail** — the agent calls whatever it wants with your credentials
- **No conversation control** — the agent manages its own context window

Tightbeam solves this by proxying LLM calls through the host. The container sends messages, the host attaches credentials and manages history. The agent is stateless — it doesn't know the API key, the model, or even the provider.

When the container has no network egress, tightbeam is the agent's sole communication gateway to the outside world.

Use [Airlock](https://github.com/calebfaruki/airlock) for CLI credential isolation. Use Tightbeam for LLM API isolation.

## Installation

### Quick Install (Linux/macOS)

```sh
curl -fsSL https://raw.githubusercontent.com/calebfaruki/tightbeam/main/install.sh | sh
```

This downloads the daemon, installs it to `~/.local/bin/`, and runs `tightbeam-daemon init` to set up the system service.

### Container Setup

Download the runtime from [releases](https://github.com/calebfaruki/tightbeam/releases) and add it to your Dockerfile:

```dockerfile
COPY tightbeam /usr/local/bin/tightbeam
```

The runtime drives the agent loop. It connects to the daemon socket, waits for human messages, calls the LLM, executes tools locally, and sends results back.

```sh
tightbeam \
  --system-prompt /etc/agent/prompt.md \
  --tools bash,read_file,write_file,list_directory \
  --socket /run/docker-tightbeam.sock
```

Flags:

| Flag | Required | Default | Description |
|------|----------|---------|-------------|
| `--system-prompt` | yes | — | Path to system prompt file |
| `--tools` | yes | — | Comma-separated tool list |
| `--socket` | yes | — | Path to daemon unix socket |
| `--max-iterations` | no | 100 | Max tool call rounds per human message |
| `--max-output-chars` | no | 30000 | Truncate tool output beyond this |

Available tools: `bash`, `read_file`, `write_file`, `list_directory`.

### Create a Registry

The registry defines LLM providers and MCP servers available to all agents. Create `~/.config/tightbeam/registry.toml`:

```toml
[llm.claude-sonnet]
provider = "anthropic"
model = "claude-sonnet-4-20250514"
api_key_env = "ANTHROPIC_API_KEY"
max_tokens = 8192

[mcp.github]
url = "https://mcp.github.com/sse"
auth_env = "GITHUB_TOKEN"
```

The `api_key_env` and `auth_env` fields name host environment variables — values never appear in config.

### Create an Agent Profile

Each agent gets a TOML file in `~/.config/tightbeam/agents/`. The filename becomes the agent name.

```sh
cat > ~/.config/tightbeam/agents/my-agent.toml << 'EOF'
[llm.claude-sonnet]

[mcp.github]
tools = ["create_pull_request", "list_issues"]
EOF
```

The `[llm.claude-sonnet]` section references the registry entry. An empty body uses all registry defaults. Fields present in the agent profile override the registry value for that field only.

Exactly one `[llm.*]` section is required. `[mcp.*]` sections are optional.

### Docker Run

Mount the agent's socket into the container:

```sh
docker run \
    -v ~/.config/tightbeam/sockets/my-agent.sock:/run/docker-tightbeam.sock \
    your-image
```

Conversation logs are written to `~/.local/share/tightbeam/logs/<agent>/`. Mount them read-only if the agent needs prior context on restart:

```sh
docker run \
    -v ~/.config/tightbeam/sockets/my-agent.sock:/run/docker-tightbeam.sock \
    -v ~/.local/share/tightbeam/logs/my-agent:/var/log/tightbeam:ro \
    your-image
```

## Usage

### Daemon

```sh
tightbeam-daemon start             # Run daemon in foreground
tightbeam-daemon init              # Install as system service (systemd/launchd)
tightbeam-daemon init --uninstall  # Remove system service
tightbeam-daemon status            # Show running state, connected agents
tightbeam-daemon show <agent>      # Print active profile for an agent
tightbeam-daemon logs <agent>      # Print conversation log
tightbeam-daemon stop              # Stop and uninstall service
tightbeam-daemon version           # Print version
```

### Runtime

The runtime runs inside the container. It connects to the daemon socket, loads a system prompt, and enters the agent loop:

1. Wait for a human message from the daemon
2. Send a `turn` with system prompt, tool definitions, and the message
3. If the LLM returns tool calls, execute them locally and send results in a new `turn`
4. Repeat until `end_turn` or `max_tokens`, then wait for the next human message

## Socket Protocol

JSON-RPC 2.0 over NDJSON. Each line is one complete JSON object.

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
    "messages": [{"role": "user", "content": "What files are in src?"}]
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
    "messages": [{"role": "tool", "tool_call_id": "tc-001", "content": "main.rs\nlib.rs\n"}]
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
    "content": "The src directory contains main.rs and lib.rs."
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

## Configuration

### Registry

`~/.config/tightbeam/registry.toml` defines LLM providers and MCP servers centrally. Optional — agents can override every field.

```toml
[llm.claude-sonnet]
provider = "anthropic"
model = "claude-sonnet-4-20250514"
api_key_env = "ANTHROPIC_API_KEY"
max_tokens = 8192                    # optional, defaults to 8192

[mcp.github]
url = "https://mcp.github.com/sse"
auth_env = "GITHUB_TOKEN"

[mcp.web-search]
url = "https://mcp.search.example.com/sse"
auth_env = "SEARCH_API_KEY"
```

### Agent Profile

`~/.config/tightbeam/agents/<name>.toml`. The filename stem is the agent name. Socket path (`sockets/<name>.sock`) and log path (`logs/<name>/`) are derived automatically.

```toml
[llm.claude-sonnet]
# Empty body = all registry defaults.
# Fields present override the registry value for that field only.
# Example: override just the API key:
# api_key_env = "AGENT_SPECIFIC_KEY"

[mcp.github]
tools = ["create_pull_request", "list_issues"]

[mcp.web-search]
# tools omitted = allow all tools from this server
# tools = [] would allow none (server effectively disabled)
```

Exactly one `[llm.*]` section required. Zero or more `[mcp.*]` sections.

### MCP Tool Allowlists

The `tools` field on `[mcp.*]` sections controls which tools the LLM can call:

| Value | Meaning |
|-------|---------|
| omitted | Allow all tools from the server |
| `tools = []` | Allow none (server disabled) |
| `tools = ["x", "y"]` | Allow only named tools |

## MCP Support

The daemon acts as an MCP client. It connects to remote MCP servers, discovers their tools, and merges them with the runtime's local tools. The LLM sees one flat tool list.

When the LLM returns tool calls, the daemon partitions them:

- **All local** — returned to the runtime for execution
- **All MCP** — daemon executes them, appends results to conversation, calls the LLM again. The runtime waits and receives the final response.
- **Mixed** — daemon executes MCP calls immediately, returns only the local calls to the runtime. When the runtime sends back local results, the daemon interleaves all results in the original call order and continues.

MCP connections are lazy (first turn, not startup) and cached for the session. Auth uses Bearer tokens from host environment variables. If a connection drops mid-session, the daemon retries once.

## Conversation Ownership

Tightbeam owns the conversation. The agent is stateless.

1. Runtime sends a `turn` with new messages
2. Daemon logs messages to NDJSON, attaches credentials, forwards to LLM
3. Response streams back — daemon logs it, forwards to runtime
4. Runtime executes tool calls locally, sends results in next `turn`
5. Daemon logs results, calls LLM again
6. Loop continues until `end_turn` or `max_tokens`

## Host Filesystem Layout

```
~/.config/tightbeam/
  registry.toml                    # Global LLM/MCP server definitions
  agents/
    my-agent.toml                  # Per-agent profile
  sockets/
    my-agent.sock                  # Per-agent unix socket (mode 0600)

~/.local/share/tightbeam/
  logs/
    my-agent/
      conversation.ndjson          # Full message log
```

## Security Model

- Credentials stay on the host. API keys and MCP auth tokens never cross the socket boundary.
- The agent does not know which model or provider it talks to.
- LLM provider is swappable at the profile layer without agent changes.
- MCP servers are configured on the host. The runtime has no knowledge of MCP.
- All messages (user, assistant, tool results) are logged to NDJSON files on the host.
- Errors are forwarded as-is. Tightbeam does not retry or modify API responses.
