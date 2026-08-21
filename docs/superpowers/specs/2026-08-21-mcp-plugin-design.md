# bytemaker s14: MCP Plugin Design

**Created**: 2026-08-21
**Status**: Design Approved
**Based on**: SPEC_S14_MCP.md

---

## 1. Overview

Add MCP (Model Context Protocol) tool discovery and invocation capabilities to bytemaker:

- Runtime connection to MCP servers, discovering their tools
- Dynamic registration of external tools into the agent's tool pool
- Tool namespacing via `mcp__{server}__{tool}` to avoid conflicts
- Host-side permission policy for external tool calls
- MCP tools visible to sub-agents and teammates

**Alignment**: Matches Python s14 semantics (connect → discover → dispatch), implemented with production-grade Rust.

---

## 2. Architecture

### 2.1 Component Diagram

```
┌─────────────────────────────────────────────────────────────┐
│                           Agent                               │
├─────────────────────────────────────────────────────────────┤
│  registry: Arc<ToolRegistry>                                 │
│    └── RwLock<BTreeMap<String, Arc<dyn Tool>>>               │
│        ├── Built-in tools (23)                               │
│        ├── connect_mcp / disconnect_mcp / list_mcp           │
│        └── mcp__*__* (dynamically registered)               │
├─────────────────────────────────────────────────────────────┤
│  mcp_manager: Arc<McpManager>                                │
│    ├── connections: RwLock<HashMap<String, McpConnection>>  │
│    ├── origins: RwLock<HashMap<String, String>> (collision) │
│    └── registry: Arc<ToolRegistry> (reference)              │
└─────────────────────────────────────────────────────────────┘
                            │
                            │ Arc::clone
                            │
         ┌──────────────────┴──────────────────┐
         │                                      │
    child_agent()                       child_teammate()
    (shares registry + mcp_manager)       (shares registry + mcp_manager)

┌─────────────────────────────────────────────────────────────┐
│                      McpManager                               │
├─────────────────────────────────────────────────────────────┤
│  connect(server_name, command, args) -> Vec<String>          │
│  disconnect(server_name) -> Result<(), AgentError>          │
│  list() -> Vec<McpServerInfo>                                │
│  system_prompt_suffix() -> String                            │
│  shutdown_all() (force kill)                                │
└─────────────────────────────────────────────────────────────┘

┌─────────────────────────────────────────────────────────────┐
│                     McpConnection                             │
├─────────────────────────────────────────────────────────────┤
│  server_name: String                                         │
│  client: Arc<McpClient>                                      │
│  tools: Vec<McpToolDef>                                      │
└─────────────────────────────────────────────────────────────┘

┌─────────────────────────────────────────────────────────────┐
│                      McpClient                                │
├─────────────────────────────────────────────────────────────┤
│  child: Option<tokio::process::Child>                       │
│  stdin: ChildStdin                                           │
│  stdout: Lines<BufReader<ChildStdout>>                      │
│  next_id: AtomicU64                                          │
│  pending: Arc<Mutex<HashMap<u64, oneshot::Sender<JsonValue>>>>│
├─────────────────────────────────────────────────────────────┤
│  spawn(command, args) -> Result<Self, AgentError>            │
│  initialize() -> Result<InitResult, AgentError>              │
│  list_tools() -> Result<Vec<McpToolDef>, AgentError>         │
│  call_tool(name, args) -> Result<String, AgentError>         │
│  shutdown(self) -> Result<(), AgentError>                    │
└─────────────────────────────────────────────────────────────┘

┌─────────────────────────────────────────────────────────────┐
│                       McpTool                                 │
├─────────────────────────────────────────────────────────────┤
│  prefixed_name: String (mcp__{server}__{tool})               │
│  raw_name: String (server's original name)                   │
│  server_name: String                                         │
│  description: String                                         │
│  input_schema: Value                                         │
│  client: Arc<McpClient>                                      │
├─────────────────────────────────────────────────────────────┤
│  impl Tool:                                                   │
│    name() -> &str                                            │
│    description() -> &str                                      │
│    input_schema() -> Value                                    │
│    check_permission(input) -> NeedsApproval (v1 hardcoded)   │
│    execute(ctx, input) -> String (calls client.call_tool)    │
│    available_for(kind) -> true (all agents)                  │
└─────────────────────────────────────────────────────────────┘
```

---

## 3. Core Components

### 3.1 ToolRegistry Refactoring

**Current State**: `BTreeMap<String, Box<dyn Tool>>` (immutable after build)

**New State**: `RwLock<BTreeMap<String, Arc<dyn Tool>>>` (runtime mutable)

```rust
pub struct ToolRegistry {
    tools: RwLock<BTreeMap<String, Arc<dyn Tool>>>,
}

impl ToolRegistry {
    // New: Runtime registration (called during MCP connect)
    pub fn register_dynamic(&self, tool: Arc<dyn Tool>) {
        let name = tool.name().to_string();
        self.tools.write().unwrap().insert(name, tool);
    }

    // New: Runtime unregistration (called during MCP disconnect)
    pub fn unregister(&self, name: &str) -> bool {
        self.tools.write().unwrap().remove(name).is_some()
    }

    // Modified: Use read lock
    pub fn definitions_for(&self, kind: AgentKind) -> Vec<ToolDefinition> {
        self.tools.read().unwrap()
            .values()
            .filter(|tool| tool.available_for(kind))
            .map(|tool| ToolDefinition { ... })
            .collect()
    }

    // Modified: Clone Arc before await to release lock
    pub async fn dispatch(&self, name: &str, ctx: &ToolContext<'_>, input: &Value, kind: AgentKind) -> ToolResult {
        let tool = {
            let guard = self.tools.read().unwrap();
            match guard.get(name) {
                Some(tool) => Arc::clone(tool),
                None => return ToolResult::NotFound { ... },
            }
        }; // Read lock released here
        tool.execute(ctx, input).await
    }
}
```

**Migration**: All 23 `register()` calls in `build_registry()` change from `Box::new()` to `Arc::new()`.

### 3.2 McpManager (mcp/mod.rs)

**Responsibilities**: Lifecycle management of all MCP connections.

```rust
pub struct McpManager {
    connections: RwLock<HashMap<String, McpConnection>>,
    origins: RwLock<HashMap<String, String>>, // Normalized name → origin
    registry: Arc<ToolRegistry>,
}

pub struct McpConnection {
    pub server_name: String,
    pub client: Arc<McpClient>,
    pub tools: Vec<McpToolDef>,
}

impl McpManager {
    pub fn new(registry: Arc<ToolRegistry>) -> Self;

    /// Connect to MCP server, discover tools, register to registry.
    pub async fn connect(&self, server_name: &str, command: &str, args: &[&str])
        -> Result<Vec<String>, AgentError>;

    /// Disconnect MCP server, unregister tools.
    pub async fn disconnect(&self, server_name: &str) -> Result<(), AgentError>;

    /// List connected servers and tools.
    pub fn list(&self) -> Vec<McpServerInfo>;

    /// Generate system prompt suffix (connected server list).
    pub fn system_prompt_suffix(&self) -> String;

    /// Force kill all MCP server processes.
    pub async fn shutdown_all(&self);
}
```

**Connect Lifecycle**:
```
connect("docs", "npx", ["-y", "@modelcontextprotocol/server-docs"])
  → spawn child process (stdio)
  → send initialize request
  → send tools/list request
  → for each tool:
      normalize name → mcp__docs__search
      check collision
      create McpTool { server_name, tool_name, schema, ... }
      registry.register_dynamic(Arc::new(mcp_tool))
      origins.insert("mcp__docs__search", "docs/search")
  → return tool names
```

**Disconnect Lifecycle**:
```
disconnect("docs")
  → for each tool in connection.tools:
      registry.unregister("mcp__docs__search")
      origins.remove("mcp__docs__search")
  → kill child process (force)
  → connections.remove("docs")
```

### 3.3 McpClient (mcp/client.rs)

**Responsibilities**: MCP protocol client, stdio JSON-RPC transport.

```rust
pub struct McpClient {
    child: Option<tokio::process::Child>,
    stdin: tokio::process::ChildStdin,
    stdout_lines: tokio::io::Lines<tokio::io::BufReader<tokio::process::ChildStdout>>,
    next_id: AtomicU64,
    pending: Arc<Mutex<HashMap<u64, oneshot::Sender<JsonValue>>>>,
}

impl McpClient {
    /// Spawn child process and establish connection.
    pub async fn spawn(command: &str, args: &[&str]) -> Result<Self, AgentError>;

    /// Send initialize request.
    pub async fn initialize(&self) -> Result<InitResult, AgentError>;

    /// Discover tools (tools/list).
    pub async fn list_tools(&self) -> Result<Vec<McpToolDef>, AgentError>;

    /// Call tool (tools/call).
    pub async fn call_tool(&self, name: &str, args: &JsonValue) -> Result<String, AgentError>;

    /// Close connection, kill child process.
    pub async fn shutdown(mut self) -> Result<(), AgentError>;
}
```

**JSON-RPC Message Format**:
```json
// Request
{"jsonrpc": "2.0", "id": 1, "method": "tools/list", "params": {}}

// Response
{"jsonrpc": "2.0", "id": 1, "result": {
    "tools": [
        {
            "name": "search",
            "description": "Search the docs",
            "inputSchema": {
                "type": "object",
                "properties": {"query": {"type": "string"}},
                "required": ["query"]
            }
        }
    ]
}}

// Call
{"jsonrpc": "2.0", "id": 2, "method": "tools/call", "params": {
    "name": "search",
    "arguments": {"query": "agent hooks"}
}}

// Call result
{"jsonrpc": "2.0", "id": 2, "result": {
    "content": [
        {"type": "text", "text": "Found 3 results for 'agent hooks'"}
    ]
}}
```

**Background Reader Task**:
```rust
tokio::spawn(async move {
    while let Some(line) = stdout_lines.next_line().await? {
        let msg: JsonRpcResponse = serde_json::from_str(&line)?;
        if let Some(sender) = pending.lock().unwrap().remove(&msg.id) {
            let _ = sender.send(msg.result);
        }
        // Notifications (no id) are logged
    }
});
```

### 3.4 McpTool (mcp/tool.rs)

**Responsibilities**: Adapt MCP server tools to bytemaker's `Tool` trait.

```rust
pub struct McpTool {
    prefixed_name: String,    // mcp__{server}__{tool}
    raw_name: String,          // server's original name
    server_name: String,
    description: String,
    input_schema: Value,
    client: Arc<McpClient>,
}

#[async_trait]
impl Tool for McpTool {
    fn name(&self) -> &str { &self.prefixed_name }
    fn description(&self) -> &str { &self.description }
    fn input_schema(&self) -> Value { self.input_schema.clone() }

    fn check_permission(&self, _input: &Value) -> PermissionCheck {
        // v1: All MCP tools require user confirmation
        PermissionCheck::NeedsApproval("External MCP tool call requires confirmation")
    }

    async fn execute(&self, _ctx: &ToolContext<'_>, input: &Value) -> String {
        match self.client.call_tool(&self.raw_name, input).await {
            Ok(result) => result,
            Err(e) => format!("MCP error: {}", e),
        }
    }

    fn available_for(&self, _kind: AgentKind) -> bool {
        // MCP tools visible to Lead / Subagent / Teammate
        true
    }
}
```

### 3.5 Name Normalization (mcp/mod.rs)

```rust
const DISALLOWED_CHARS: &str = r"[^a-zA-Z0-9_-]";
const MAX_TOOL_NAME_LEN: usize = 64;

/// Replace characters not in [a-zA-Z0-9_-] with _
fn normalize_mcp_name(name: &str) -> Result<String, AgentError> {
    let re = regex::Regex::new(DISALLOWED_CHARS).unwrap();
    let normalized = re.replace_all(name, "_").to_string();
    if normalized.is_empty() {
        return Err(AgentError::Validation("MCP name cannot normalize to empty".into()));
    }
    Ok(normalized)
}

/// Construct prefixed tool name: mcp__{server}__{tool}
fn prefixed_tool_name(server: &str, tool: &str) -> Result<String, AgentError> {
    let safe_server = normalize_mcp_name(server)?;
    let safe_tool = normalize_mcp_name(tool)?;
    let prefixed = format!("mcp__{}__{}", safe_server, safe_tool);
    if prefixed.len() > MAX_TOOL_NAME_LEN {
        return Err(AgentError::Validation(
            format!("MCP tool name exceeds {} chars: {}", MAX_TOOL_NAME_LEN, prefixed)
        ));
    }
    Ok(prefixed)
}
```

**Collision Detection**:
```rust
// Check during connect
let prefixed = prefixed_tool_name(server_name, &tool.name)?;
let origin = format!("{}/{}", server_name, tool.name);

let mut origins = self.origins.write().unwrap();
if let Some(existing) = origins.get(&prefixed) {
    return Err(AgentError::Validation(format!(
        "MCP tool name collision: '{}' maps to both {} and {}",
        prefixed, existing, origin
    )));
}
origins.insert(prefixed.clone(), origin);
```

### 3.6 Permission Policy (v1 Simplified)

**v1 Design**: All MCP tools hardcoded to `NeedsApproval`, no configuration.

```rust
// McpTool::check_permission() directly returns:
PermissionCheck::NeedsApproval("External MCP tool call requires confirmation")
```

**Future Extension** (v2+): Per-tool allowlist from `.claude/settings.json`.

### 3.7 Management Tools (mcp/tools.rs)

#### connect_mcp
```rust
pub struct ConnectMcpTool {
    manager: Arc<McpManager>,
}

impl Tool for ConnectMcpTool {
    fn name(&self) -> &str { "connect_mcp" }
    fn description(&self) -> &str {
        "Connect to an MCP server and discover its tools. Discovered tools become available as mcp__{server}__{tool}."
    }
    fn input_schema(&self) -> Value { /* name, command, args */ }
    fn available_for(&self, kind: AgentKind) -> bool { kind == AgentKind::Lead }
    async fn execute(&self, _ctx: &ToolContext<'_>, input: &Value) -> String { /* ... */ }
}
```

#### disconnect_mcp
```rust
pub struct DisconnectMcpTool {
    manager: Arc<McpManager>,
}

impl Tool for DisconnectMcpTool {
    fn name(&self) -> &str { "disconnect_mcp" }
    fn description(&self) -> &str { "Disconnect from an MCP server and remove its tools." }
    fn input_schema(&self) -> Value { /* name */ }
    fn available_for(&self, kind: AgentKind) -> bool { kind == AgentKind::Lead }
    async fn execute(&self, _ctx: &ToolContext<'_>, input: &Value) -> String { /* ... */ }
}
```

#### list_mcp
```rust
pub struct ListMcpTool {
    manager: Arc<McpManager>,
}

impl Tool for ListMcpTool {
    fn name(&self) -> &str { "list_mcp" }
    fn description(&self) -> &str { "List all connected MCP servers and their discovered tools." }
    fn input_schema(&self) -> Value { /* {} */ }
    fn available_for(&self, _kind: AgentKind) -> bool { true } // All agents
    async fn execute(&self, _ctx: &ToolContext<'_>, _input: &Value) -> String { /* ... */ }
}
```

---

## 4. Integration Points

### 4.1 Agent: Add mcp_manager Field

```rust
pub struct Agent {
    // ... existing fields ...

    // s14: MCP tool management
    pub(crate) mcp_manager: Arc<mcp::McpManager>,
}
```

### 4.2 Agent::new() Initialization

```rust
pub async fn new(cfg: AgentConfig) -> Result<Self, AgentError> {
    let registry = Arc::new(ToolRegistry::new());

    // Register built-in tools (23 tools, changed to Arc::new(...))
    register_builtin_tools(&registry);

    // s14: Initialize MCP manager (references registry for dynamic registration)
    let mcp_manager = Arc::new(mcp::McpManager::new(Arc::clone(&registry)));

    // Register MCP management tools
    registry.register_dynamic(Arc::new(mcp::ConnectMcpTool::new(Arc::clone(&mcp_manager))));
    registry.register_dynamic(Arc::new(mcp::DisconnectMcpTool::new(Arc::clone(&mcp_manager))));
    registry.register_dynamic(Arc::new(mcp::ListMcpTool::new(Arc::clone(&mcp_manager))));

    // ... remaining initialization ...
}
```

### 4.3 child_agent() / child_teammate() Share mcp_manager

```rust
pub fn child_agent(&self, max_turns: usize, sub_system: &str) -> Agent {
    Agent {
        // ... existing Arc clones ...
        registry: Arc::clone(&self.registry),       // Shared registry (includes registered MCP tools)
        mcp_manager: Arc::clone(&self.mcp_manager), // Shared MCP manager

        // Sub-agents cannot connect/disconnect (Lead-only), but can use connected tools
        // Controlled by ConnectMcpTool/DisconnectMcpTool's available_for()
        // ...
    }
}
```

### 4.4 run_loop: System Prompt Suffix

```rust
pub async fn run_loop(&self, messages: &mut Vec<Message>, active_request: &str) -> Result<...> {
    // Recall memories
    let recalled = self.memory.load_memories(...).await;
    let index = self.memory.read_memory_index();

    // s14: Append connected MCP server information
    let mcp_suffix = self.mcp_manager.system_prompt_suffix();

    let system = build_system(&self.base_system, &index, &recalled);
    let system = if mcp_suffix.is_empty() {
        system
    } else {
        format!("{}\n\n{}", system, mcp_suffix)
    };

    // ... rest of loop unchanged ...
}
```

### 4.5 Permission Pipeline Integration

MCP tool permission checks are embedded in `McpTool::check_permission()`, existing `PermissionHook` gate 3 automatically covers this:

```rust
// builtins.rs PermissionHook::on_pre_tool
// Gate 4: registry.check_permission(name, input)
// → Find McpTool → McpTool::check_permission()
// → Returns NeedsApproval (v1 hardcoded)
// → ask_via_input() asks user
```

**No changes needed to builtins.rs**.

### 4.6 Agent Exit Cleanup

```rust
// main.rs: Before exit
agent.mcp_manager.shutdown_all().await;
```

---

## 5. Tool Visibility Matrix

| Tool | Lead | Subagent | Teammate |
|------|------|----------|----------|
| connect_mcp | ✅ | ❌ | ❌ |
| disconnect_mcp | ✅ | ❌ | ❌ |
| list_mcp | ✅ | ✅ | ✅ |
| mcp__X__Y (connected tools) | ✅ | ✅ | ✅ |

**Rationale**:
- `connect_mcp` / `disconnect_mcp` are management operations, Lead-only (prevents sub-agents from arbitrarily changing the tool pool)
- Connected MCP tools are visible to all agents (consistent with built-in tools)
- `list_mcp` is a read-only query, available to all agents

---

## 6. Data Flow

### 6.1 connect_mcp Complete Flow

```
User: "Connect to docs MCP server"
  ↓
Model returns tool_use: {name: "connect_mcp", input: {name: "docs", command: "npx", args: ["-y", "docs-server"]}}
  ↓
registry.dispatch("connect_mcp", ctx, input, Lead)
  ↓
ConnectMcpTool.execute(ctx, input)
  ↓
McpManager.connect("docs", "npx", ["-y", "docs-server"])
  ├─ McpClient::spawn("npx", ["-y", "docs-server"])
  │    ├─ tokio::process::Command spawns child process
  │    ├─ Background task reads stdout (JSON-RPC responses)
  │    └─ Returns McpClient { stdin, pending, ... }
  ├─ client.initialize() → JSON-RPC: initialize
  ├─ client.list_tools() → JSON-RPC: tools/list
  │    └─ Returns [McpToolDef { name: "search", ... }, ...]
  └─ for each tool_def:
       ├─ prefixed = prefixed_tool_name("docs", "search") → "mcp__docs__search"
       ├─ collision check → origins.insert(...)
       ├─ mcp_tool = McpTool { prefixed_name, raw_name, client: Arc::clone(client), ... }
       └─ registry.register_dynamic(Arc::new(mcp_tool))
  ↓
Returns "Connected to MCP server 'docs'. Discovered 2 tools: mcp__docs__search, mcp__docs__get_version"
  ↓
tool_result appended to messages → next loop iteration
  ↓
registry.definitions_for(Lead) → includes mcp__docs__search, mcp__docs__get_version
  ↓
Model sees new tools, can invoke them
```

### 6.2 MCP Tool Call Flow

```
Model returns tool_use: {name: "mcp__docs__search", input: {query: "agent hooks"}}
  ↓
hooks.trigger_pre_tool:
  → PermissionHook.on_pre_tool("mcp__docs__search", input)
  → registry.check_permission("mcp__docs__search", input)
  → McpTool.check_permission(input) → NeedsApproval("External MCP tool call requires confirmation")
  → ask_via_input() asks user → user confirms
  → Allow
  ↓
registry.dispatch("mcp__docs__search", ctx, input, Lead)
  ↓
McpTool.execute(ctx, input)
  ↓
McpClient.call_tool("search", {query: "agent hooks"})
  ├─ Generate JSON-RPC: {"jsonrpc": "2.0", "id": 3, "method": "tools/call", "params": {...}}
  ├─ Write to child process stdin
  ├─ oneshot::channel() waits for response
  └─ Background reader task receives response → sender.send(result)
  ↓
Returns "[docs] Found 3 results for 'agent hooks'"
  ↓
tool_result appended to messages
```

---

## 7. Error Handling

| Scenario | Handling |
|----------|----------|
| Server process start fails | `connect_mcp` returns error string, model can retry or report |
| JSON-RPC parse fails | `call_tool` returns `"MCP error: ..."` |
| Server process exits unexpectedly | Background reader detects EOF → marks disconnected → `call_tool` returns error |
| Tool call timeout | Configurable timeout (default 30s), returns `"MCP error: timeout"` |
| Name collision | `connect_mcp` fails, returns collision details |
| Server returns non-standard JSON | Fault-tolerant parsing (extract JSON array style) |

---

## 8. Testing Strategy

### 8.1 Unit Tests

```rust
// Name normalization
#[test] fn normalize_strips_dots_and_slashes()
#[test] fn normalize_preserves_underscores_and_dashes()
#[test] fn normalize_rejects_empty()

// Prefix construction
#[test] fn prefixed_name_format()
#[test] fn prefixed_name_exceeds_64_chars()

// Collision detection
#[test] fn collision_after_normalization_detected()
#[test] fn no_collision_for_distinct_servers()

// McpTool
#[test] fn mcp_tool_available_for_all_kinds()
#[test] fn connect_mcp_tool_lead_only()

// ToolRegistry dynamic registration
#[test] fn register_dynamic_adds_tool()
#[test] fn unregister_removes_tool()
#[test] fn definitions_for_picks_up_dynamic_tool()
#[tokio::test] async fn dispatch_finds_dynamic_tool()
```

### 8.2 Integration Tests (Mock Server)

```rust
// Mock MCP server (echo server for testing)
#[tokio::test] async fn connect_and_call_mock_server()
#[tokio::test] async fn disconnect_removes_tools_from_registry()
#[tokio::test] async fn multiple_servers_no_collision()
#[tokio::test] async fn server_crash_returns_error_not_panic()
```

### 8.3 Mock Server

Test-only mock MCP server that:
- Reads stdin JSON-RPC, returns fixed responses
- Validates complete connect → list → call → disconnect flow
- Does not depend on external MCP servers (no node/npx required)

---

## 9. Dependencies

**No new crates needed.** Using existing dependencies:
- `tokio::process` (already in `tokio` full features)
- `serde_json` (already present)
- `regex` (already present)

---

## 10. Implementation Phases

### Phase 1: Infrastructure (No transport)
1. `ToolRegistry` → `RwLock<BTreeMap<String, Arc<dyn Tool>>>`
2. Add `register_dynamic()` / `unregister()`
3. Verify existing 23 tools unaffected

### Phase 2: MCP Module Skeleton
4. `mcp/mod.rs` — McpManager + name normalization + collision detection
5. `mcp/tool.rs` — McpTool (impl Tool, check_permission hardcoded NeedsApproval)

### Phase 3: stdio Transport
6. `mcp/client.rs` — McpClient (stdio JSON-RPC)
7. Background reader task
8. `tools/call` + `tools/list` implementation

### Phase 4: Management Tools
9. `mcp/tools.rs` — connect_mcp / disconnect_mcp / list_mcp

### Phase 5: Integration
10. Agent: Add mcp_manager field
11. child_agent / child_teammate: Share mcp_manager
12. run_loop: Add system prompt suffix
13. shutdown_all: Force kill cleanup

---

## 11. Key Risks

| Risk | Impact | Mitigation |
|------|--------|------------|
| `Box<dyn Tool>` → `Arc<dyn Tool>` change scope | All register call sites | Phase 1 done separately, fully tested |
| MCP server process leaks | Zombie processes | `Drop` impl kills child + shutdown_all |
| RwLock read lock held across await | Deadlock | Clone Arc in dispatch before releasing lock |
| Large number of MCP tools bloats tool pool | Model selection difficulty | No active limit, but system prompt categorizes |

---

## 12. Differences from Python s14

| Aspect | Python s14 | bytemaker s14 |
|--------|-----------|---------------|
| MCP transport | In-process simulation (no real protocol) | Real stdio JSON-RPC |
| Tool registration | Rebuild tool pool each round via `assemble_tool_pool()` | `RwLock<BTreeMap>` dynamic registration, `definitions_for` automatically includes each round |
| Name collision | Raises ValueError | Returns AgentError::Validation |
| Permission policy | Hardcoded dict | Hardcoded `NeedsApproval` (v1) |
| Server lifecycle | No disconnect mechanism | `disconnect_mcp` + `shutdown_all` |
| Server configuration | Code-hardcoded MOCK_SERVERS | `connect_mcp` parameters passed directly |
| Sub-agent visibility | MCP tools fully visible | Same + `connect_mcp` Lead-only |
| Error handling | Returns error strings | AgentError typed + degrade to string |

---

**Design Status**: Approved
**Next Step**: Create implementation plan using writing-plans skill
