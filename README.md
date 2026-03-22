# Tightbeam

[![made-with-rust](https://img.shields.io/badge/Made%20with-Rust-1f425f.svg)](https://www.rust-lang.org/)

LLM proxy for containerized AI agents. Proxies API calls, human messages, and remote MCP server requests through a unix socket — without mounting credentials into the container.

## How It Works

Three components:

1. **Container-side shim** — a single static binary inside the container. Reads JSON-RPC requests from stdin, writes them to a unix socket, and streams responses to stdout. Agent runtimes in any language pipe JSON through it.

2. **Host-side daemon** — a long-running process on the host. Listens on per-agent unix sockets, receives requests from the shim, attaches API credentials, manages conversation history, and forwards requests to the LLM provider.

3. **Per-agent profiles** — TOML files defining which LLM provider, model, and credentials each agent uses. Each profile gets its own unix socket. The agent never sees which model it talks to.

The agent sends new messages. Tightbeam attaches the API key from the host environment, forwards the request to the LLM provider, and streams the response back. Every exchange is logged to per-agent NDJSON files on the host. Credentials stay on the host — they never cross the socket boundary.

## Why Tightbeam?

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

This downloads the daemon, installs it to `~/.local/bin/`, and runs `tightbeam init` to set up the system service.

### Container Setup

Download the shim from [releases](https://github.com/calebfaruki/tightbeam/releases) and add it to your Dockerfile:

```dockerfile
COPY tightbeam /usr/local/bin/tightbeam
```

No symlinks needed — the shim is one binary with one purpose.

### Create an Agent Profile

```sh
cat > ~/.config/tightbeam/agents/my-agent.toml << 'EOF'
[provider]
name = "claude"
model = "claude-sonnet-4-20250514"
api_key_env = "ANTHROPIC_API_KEY"
max_tokens = 8192

[socket]
path = "my-agent.sock"

[log]
path = "my-agent/"
EOF
```

The `api_key_env` field names an environment variable on the host — the key itself never appears in the profile.

### Docker Run

Mount the agent's socket into the container. The shim always connects to `/run/docker-tightbeam.sock` inside the container.

```sh
docker run \
    -v ~/.config/tightbeam/sockets/my-agent.sock:/run/docker-tightbeam.sock \
    your-image
```

Conversation logs are written to `~/.local/share/tightbeam/logs/<agent>/`. Mount them read-only into the container if the agent needs prior context on restart:

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
tightbeam-daemon version           # Print version
```

### Shim

The shim reads JSON-RPC from stdin and streams responses to stdout. One connection per session — the tool loop flows as sequential requests on a single socket.

```bash
echo '{"jsonrpc":"2.0","id":"1","method":"llm_call","params":{"messages":[{"role":"user","content":"Hello"}],"tools":[]}}' \
  | tightbeam
```

From Python:

```python
import subprocess, json

proc = subprocess.Popen(
    ["tightbeam"],
    stdin=subprocess.PIPE, stdout=subprocess.PIPE
)

# Send request
req = {"jsonrpc": "2.0", "id": "1", "method": "llm_call", "params": {
    "messages": [{"role": "user", "content": "Hello"}],
    "tools": []
}}
proc.stdin.write(json.dumps(req).encode() + b"\n")
proc.stdin.flush()

# Read streaming notifications and final response
for line in proc.stdout:
    msg = json.loads(line)
    if "id" in msg:  # Final response
        break
    # Notification: streamed content
    print(msg["params"]["data"].get("text", ""), end="", flush=True)
```

## Socket Protocol

JSON-RPC 2.0 over NDJSON. Each line is one complete JSON object.

### Request: `llm_call`

Agent sends new messages. Tightbeam attaches credentials and forwards to the LLM provider.

```json
{
  "jsonrpc": "2.0",
  "id": 1,
  "method": "llm_call",
  "params": {
    "messages": [{"role": "user", "content": "What files are in src?"}],
    "tools": [{"name": "bash", "description": "Run a command", "input_schema": {"type": "object"}}],
    "system": "You are a coding assistant."
  }
}
```

### Request: `tool_result`

After executing a tool call locally, the agent sends the result back. Tightbeam appends it to history and immediately calls the LLM again.

```json
{
  "jsonrpc": "2.0",
  "id": 2,
  "method": "tool_result",
  "params": {
    "tool_call_id": "tc-001",
    "result": "main.rs\nlib.rs\n"
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

Has `id` — signals completion of this request.

```json
{
  "jsonrpc": "2.0",
  "id": 1,
  "result": {
    "stop_reason": "end_turn",
    "text": "The src directory contains main.rs and lib.rs."
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

## Agent Profile Schema

```toml
[provider]
name = "claude"                          # Provider name (matches daemon's provider registry)
model = "claude-sonnet-4-20250514"       # Model identifier
api_key_env = "ANTHROPIC_API_KEY"        # Host env var containing the API key
max_tokens = 8192                        # Max tokens per response (default: 8192)

[socket]
path = "my-agent.sock"                   # Relative to ~/.config/tightbeam/sockets/

[log]
path = "my-agent/"                       # Relative to ~/.local/share/tightbeam/logs/
```

## Conversation Ownership

Tightbeam owns the conversation. The agent is stateless.

1. Agent sends `llm_call` with new message(s)
2. Tightbeam logs messages to the NDJSON log file
3. Tightbeam attaches credentials, forwards to LLM provider
4. Response streams back — tightbeam logs it, forwards to agent
5. Agent sends `tool_result` — tightbeam logs it, calls LLM again
6. Loop continues

Conversation logs are written to `~/.local/share/tightbeam/logs/<agent>/` and mounted read-only into the container. The agent reads prior context directly from the log file on restart.

## Idle Lifecycle

Tightbeam tracks last message time per agent. When an agent exceeds the idle timeout, tightbeam stops the container service via `systemctl --user stop`. When a new message arrives for a stopped agent, tightbeam starts it via `systemctl --user start`.

## Host Filesystem Layout

```
~/.config/tightbeam/
  agents/
    my-agent.toml              # Per-agent profile
  sockets/
    my-agent.sock              # Per-agent unix socket

~/.local/share/tightbeam/
  logs/
    my-agent/
      conversation.ndjson      # Full message log
```

## Security Model

- Credentials stay on the host. API keys never cross the socket boundary.
- The agent does not know which model or provider it talks to.
- LLM provider is swappable at the profile layer without agent changes.
- All messages (user, agent, LLM, tool results) are logged to NDJSON files on the host.
- Errors are forwarded as-is. Tightbeam does not retry or modify API responses.
