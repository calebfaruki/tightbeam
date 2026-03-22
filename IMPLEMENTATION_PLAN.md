## Tightbeam Runtime Implementation

### Stage 1: tightbeam-protocol crate
**Goal**: Extract shared types into a new crate used by both binaries
**Types**: Message, ToolDefinition, ToolCall, StopReason, TurnRequest, TurnResponse, StreamData, HumanMessage
**Status**: Complete

### Stage 2: Daemon protocol evolution
**Goal**: Update daemon to use tightbeam-protocol and support `turn` method
**Status**: Complete

### Stage 3: Remove tightbeam-shim
**Goal**: Remove old shim to avoid binary name collision
**Status**: Complete

### Stage 4: tightbeam-runtime
**Goal**: Container-side binary with socket connection, CLI parsing, and agent tool loop
**Status**: Complete
