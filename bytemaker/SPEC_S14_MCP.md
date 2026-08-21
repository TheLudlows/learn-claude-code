# bytemaker s14: MCP Plugin 设计规格

> **状态**: 设计稿  
> **对应教学章节**: s14_mcp_plugin  
> **依赖**: bytemaker SPEC.md (s04–s13 已实现)

---

## 1. 目标

为 bytemaker 增加 MCP (Model Context Protocol) 工具发现与调用能力：

- 运行时连接 MCP server，发现其提供的工具
- 将外部工具动态加入 agent 的工具池
- 通过 `mcp__{server}__{tool}` 命名空间避免冲突
- 宿主侧权限策略控制外部工具调用
- 子 agent / teammate 可见已连接的 MCP 工具

**对齐**：Python s14 的语义（connect → discover → dispatch），用 Rust 生产级实现。

---

## 2. 当前架构约束

### 2.1 ToolRegistry 不可变

```rust
// 现状：build_registry() 在 Agent::new() 时调用一次
pub fn build_registry() -> ToolRegistry { ... }

// Agent 持有 Arc，子 agent 共享
pub(crate) registry: Arc<ToolRegistry>,
```

`ToolRegistry` 内部是 `BTreeMap<String, Box<dyn Tool>>`，无 interior mutability。MCP 工具需要在运行时动态添加。

### 2.2 每轮组装工具定义

```rust
// run_loop 每轮开头
let defs = self.registry.definitions_for(self.kind);
```

工具定义是**每轮重新获取**的——如果 registry 支持动态添加，新工具自动在下一轮可见。

### 2.3 Tool trait 要求 Send + Sync

```rust
pub trait Tool: Send + Sync {
    async fn execute(&self, ctx: &ToolContext<'_>, input: &Value) -> String;
}
```

MCP client 必须满足 `Send + Sync`（跨 async 边界安全）。

### 2.4 权限管线已有三道闸门

```rust
// builtins.rs PermissionHook
on_pre_tool:
  1. deny_patterns (command only)
  2. approval_patterns (command only)
  3. registry.check_permission(name, input)  ← MCP 工具在此接入
```

`check_permission()` 已支持 per-tool 权限检查，MCP 工具只需在 `Tool` 实现中返回正确的 `PermissionCheck`。

---

## 3. 设计决策

### 3.1 ToolRegistry 改为 interior mutability + Arc

**决策**：`ToolRegistry` 内部加 `RwLock` + `Box` → `Arc`，支持运行时注册/注销。

**为什么 Box → Arc 是必要的**：

```rust
// 如果只加 RwLock 不改 Box：
// RwLockReadGuard 是 !Send 的，不能跨 .await 持有
// → dispatch 必须在读锁内 await execute() → 编译错误

// 解决方案：Box → Arc，clone 引用后释放锁，再 await
```

```rust
pub struct ToolRegistry {
    tools: RwLock<BTreeMap<String, Arc<dyn Tool>>>,  // Box → Arc
}

impl ToolRegistry {
    // 新增：运行时注册工具（MCP connect 时调用）
    pub fn register_dynamic(&self, tool: Arc<dyn Tool>) {
        let name = tool.name().to_string();
        self.tools.write().unwrap().insert(name, tool);
    }

    // 新增：运行时注销工具（MCP disconnect 时调用）
    pub fn unregister(&self, name: &str) -> bool {
        self.tools.write().unwrap().remove(name).is_some()
    }

    // 现有方法改为读锁
    pub fn definitions_for(&self, kind: AgentKind) -> Vec<ToolDefinition> {
        self.tools.read().unwrap()
            .values()
            .filter(|tool| tool.available_for(kind))
            .map(|tool| ToolDefinition { ... })
            .collect()
    }

    pub async fn dispatch(&self, name: &str, ctx: &ToolContext<'_>, input: &Value, kind: AgentKind) -> ToolResult {
        // 在读锁内 clone Arc，释放锁后再 await execute
        let tool = {
            let guard = self.tools.read().unwrap();
            match guard.get(name) {
                Some(tool) => Arc::clone(tool),
                None => return ToolResult::NotFound { ... },
            }
        }; // 读锁在此释放
        tool.execute(ctx, input).await
    }
}
```

**迁移**：`build_registry()` 改为 `register(Arc::new(tool))`，所有 23 个工具调用点统一修改。

### 3.2 新增 mcp 模块

```
bytemaker/src/
├── mcp/
│   ├── mod.rs           # 模块导出 + McpManager
│   ├── client.rs        # MCP 协议客户端 (stdio transport)
│   ├── tool.rs          # McpTool: impl Tool 的适配器
│   └── tools.rs         # connect_mcp / disconnect_mcp / list_mcp 工具
```

### 3.3 MCP transport: stdio 优先

**决策**：v1 只实现 stdio transport（最常见），SSE/HTTP 留 v2。

**原因**：
- Claude Desktop、Cursor、VS Code 的 MCP server 90%+ 用 stdio
- stdio 实现简单（`tokio::process::Command` + stdin/stdout JSON-RPC）
- SSE 需要 HTTP server 管理，复杂度高

---

## 4. 核心组件

### 4.1 McpManager (mcp/mod.rs)

**职责**：管理所有 MCP 连接的生命周期。

```rust
pub struct McpManager {
    /// server_name → McpConnection
    connections: RwLock<HashMap<String, McpConnection>>,
    /// 工具名冲突检测（规范化后的名字 → 来源）
    origins: RwLock<HashMap<String, String>>,
    /// 引用 ToolRegistry，连接时动态注册工具
    registry: Arc<ToolRegistry>,
}

pub struct McpConnection {
    pub server_name: String,
    pub client: McpClient,
    pub tools: Vec<McpToolDef>,
}

impl McpManager {
    /// 连接 MCP server，发现工具，注册到 registry。
    pub async fn connect(&self, server_name: &str, command: &str, args: &[&str]) -> Result<Vec<String>, AgentError>;

    /// 断开 MCP server，从 registry 注销工具。
    pub async fn disconnect(&self, server_name: &str) -> Result<(), AgentError>;

    /// 列出已连接的 server 和工具。
    pub fn list(&self) -> Vec<McpServerInfo>;

    /// 生成 system prompt 附加段（已连接的 server 列表）。
    pub fn system_prompt_suffix(&self) -> String;
}
```

**生命周期**：

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

disconnect("docs")
  → for each tool in connection.tools:
      registry.unregister("mcp__docs__search")
      origins.remove("mcp__docs__search")
  → kill child process
  → connections.remove("docs")
```

### 4.2 McpClient (mcp/client.rs)

**职责**：MCP 协议客户端，stdio JSON-RPC transport。

```rust
pub struct McpClient {
    stdin: tokio::process::ChildStdin,
    stdout_lines: tokio::io::Lines<tokio::io::BufReader<tokio::process::ChildStdout>>,
    next_id: AtomicU64,
    pending: Arc<Mutex<HashMap<u64, oneshot::Sender<JsonValue>>>>,
}

impl McpClient {
    /// 启动子进程并建立连接。
    pub async fn spawn(command: &str, args: &[&str]) -> Result<Self, AgentError>;

    /// 发送 initialize 请求。
    pub async fn initialize(&self) -> Result<InitResult, AgentError>;

    /// 发现工具（tools/list）。
    pub async fn list_tools(&self) -> Result<Vec<McpToolDef>, AgentError>;

    /// 调用工具（tools/call）。
    pub async fn call_tool(&self, name: &str, args: &JsonValue) -> Result<String, AgentError>;

    /// 关闭连接，kill 子进程。
    pub async fn shutdown(self) -> Result<(), AgentError>;
}
```

**JSON-RPC 消息格式**：

```json
// 请求
{"jsonrpc": "2.0", "id": 1, "method": "tools/list", "params": {}}

// 响应
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

// 调用
{"jsonrpc": "2.0", "id": 2, "method": "tools/call", "params": {
    "name": "search",
    "arguments": {"query": "agent hooks"}
}}

// 调用结果
{"jsonrpc": "2.0", "id": 2, "result": {
    "content": [
        {"type": "text", "text": "Found 3 results for 'agent hooks'"}
    ]
}}
```

**后台 reader 任务**：

```rust
// spawn 后启动后台 task 读取 stdout
tokio::spawn(async move {
    while let Some(line) = stdout_lines.next_line().await? {
        let msg: JsonRpcResponse = serde_json::from_str(&line)?;
        if let Some(sender) = pending.lock().unwrap().remove(&msg.id) {
            let _ = sender.send(msg.result);
        }
        // notifications (无 id) 记录日志
    }
});
```

### 4.3 McpTool (mcp/tool.rs)

**职责**：把 MCP server 的工具适配为 bytemaker 的 `Tool` trait。

```rust
pub struct McpTool {
    /// 规范化后的工具名（mcp__{server}__{tool}）
    prefixed_name: String,
    /// server 原始工具名
    raw_name: String,
    /// server 名称
    server_name: String,
    /// 工具描述
    description: String,
    /// 输入 JSON Schema
    input_schema: Value,
    /// MCP 客户端引用（用于调用 tools/call）
    client: Arc<McpClient>,
}

#[async_trait]
impl Tool for McpTool {
    fn name(&self) -> &str {
        &self.prefixed_name
    }

    fn description(&self) -> &str {
        &self.description
    }

    fn input_schema(&self) -> Value {
        self.input_schema.clone()
    }

    fn check_permission(&self, _input: &Value) -> PermissionCheck {
        // v1: 所有 MCP 工具需要用户确认
        PermissionCheck::NeedsApproval("External MCP tool call requires confirmation")
    }

    async fn execute(&self, _ctx: &ToolContext<'_>, input: &Value) -> String {
        match self.client.call_tool(&self.raw_name, input).await {
            Ok(result) => result,
            Err(e) => format!("MCP error: {}", e),
        }
    }

    fn available_for(&self, kind: AgentKind) -> bool {
        // MCP 工具对 Lead / Subagent / Teammate 都可见
        true
    }
}
```

**关键设计**：
- `McpTool` 持有 `Arc<McpClient>` 而非 `&McpClient`，满足 `Send + Sync`
- `check_permission()` 硬编码为 `NeedsApproval`（v1 简化：所有外部工具需确认）
- `execute()` 使用 `raw_name`（server 原始名）调用，而非 `prefixed_name`

### 4.4 名称规范化 (mcp/mod.rs)

```rust
const DISALLOWED_CHARS: &str = r"[^a-zA-Z0-9_-]";
const MAX_TOOL_NAME_LEN: usize = 64;

/// 替换不在 [a-zA-Z0-9_-] 中的字符为 _
fn normalize_mcp_name(name: &str) -> Result<String, AgentError> {
    let re = regex::Regex::new(DISALLOWED_CHARS).unwrap();
    let normalized = re.replace_all(name, "_").to_string();
    if normalized.is_empty() {
        return Err(AgentError::Validation("MCP name cannot normalize to empty".into()));
    }
    Ok(normalized)
}

/// 构造带前缀的工具名：mcp__{server}__{tool}
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

**冲突检测**：

```rust
// connect 时检查
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

### 4.5 权限策略（v1 简化）

**v1 设计**：所有 MCP 工具硬编码 `NeedsApproval`，无需配置。

```rust
// McpTool::check_permission() 直接返回：
PermissionCheck::NeedsApproval("External MCP tool call requires confirmation")
```

**未来扩展**（v2+）：可加 per-tool allowlist，从 `.claude/settings.json` 读取。

### 4.6 connect_mcp / disconnect_mcp / list_mcp 工具 (mcp/tools.rs)

```rust
/// connect_mcp: 连接 MCP server 并发现工具
pub struct ConnectMcpTool {
    manager: Arc<McpManager>,
}

#[async_trait]
impl Tool for ConnectMcpTool {
    fn name(&self) -> &str { "connect_mcp" }

    fn description(&self) -> &str {
        "Connect to an MCP server and discover its tools. Discovered tools become available as mcp__{server}__{tool}."
    }

    fn input_schema(&self) -> Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "name": {
                    "type": "string",
                    "description": "Server name (used for tool namespacing)"
                },
                "command": {
                    "type": "string",
                    "description": "Command to start the MCP server (e.g., 'npx')"
                },
                "args": {
                    "type": "array",
                    "items": {"type": "string"},
                    "description": "Arguments to the command"
                }
            },
            "required": ["name", "command"]
        })
    }

    fn available_for(&self, kind: AgentKind) -> bool {
        kind == AgentKind::Lead  // 仅 Lead 可连接/断开
    }

    async fn execute(&self, _ctx: &ToolContext<'_>, input: &Value) -> String {
        let name = input.get("name").and_then(|v| v.as_str()).unwrap_or("");
        let command = input.get("command").and_then(|v| v.as_str()).unwrap_or("");
        let args = input.get("args")
            .and_then(|v| v.as_array())
            .map(|arr| arr.iter().filter_map(|v| v.as_str()).collect::<Vec<_>>())
            .unwrap_or_default();

        match self.manager.connect(name, command, &args).await {
            Ok(tools) => {
                format!(
                    "Connected to MCP server '{}'. Discovered {} tools: {}",
                    name,
                    tools.len(),
                    tools.join(", ")
                )
            }
            Err(e) => format!("Error connecting to MCP server '{}': {}", name, e),
        }
    }
}

/// disconnect_mcp: 断开 MCP server
pub struct DisconnectMcpTool {
    manager: Arc<McpManager>,
}

#[async_trait]
impl Tool for DisconnectMcpTool {
    fn name(&self) -> &str { "disconnect_mcp" }

    fn description(&self) -> &str {
        "Disconnect from an MCP server and remove its tools."
    }

    fn input_schema(&self) -> Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "name": { "type": "string" }
            },
            "required": ["name"]
        })
    }

    fn available_for(&self, kind: AgentKind) -> bool {
        kind == AgentKind::Lead
    }

    async fn execute(&self, _ctx: &ToolContext<'_>, input: &Value) -> String {
        let name = input.get("name").and_then(|v| v.as_str()).unwrap_or("");
        match self.manager.disconnect(name).await {
            Ok(()) => format!("Disconnected from MCP server '{}'", name),
            Err(e) => format!("Error disconnecting: {}", e),
        }
    }
}

/// list_mcp: 列出已连接的 MCP server 和工具
pub struct ListMcpTool {
    manager: Arc<McpManager>,
}

#[async_trait]
impl Tool for ListMcpTool {
    fn name(&self) -> &str { "list_mcp" }

    fn description(&self) -> &str {
        "List all connected MCP servers and their discovered tools."
    }

    fn input_schema(&self) -> Value {
        serde_json::json!({"type": "object", "properties": {}})
    }

    async fn execute(&self, _ctx: &ToolContext<'_>, _input: &Value) -> String {
        let servers = self.manager.list();
        if servers.is_empty() {
            return "No MCP servers connected. Use connect_mcp to connect one.".to_string();
        }
        let mut lines = Vec::new();
        for server in servers {
            lines.push(format!("{}: {} tools", server.name, server.tools.len()));
            for tool in &server.tools {
                lines.push(format!("  - mcp__{}__{}", server.name, tool.name));
            }
        }
        lines.join("\n")
    }
}
```

---

## 5. 集成点

### 5.1 Agent 新增 mcp_manager 字段

```rust
pub struct Agent {
    // ... 现有字段 ...

    // s14: MCP 工具管理
    pub(crate) mcp_manager: Arc<McpManager>,
}
```

### 5.2 Agent::new() 初始化

```rust
pub async fn new(cfg: AgentConfig) -> Result<Self, AgentError> {
    let registry = Arc::new(ToolRegistry::new());

    // 注册基础工具（23 个，改为 Arc::new(...)）
    register_builtin_tools(&registry);

    // s14: 初始化 MCP manager（引用 registry，可动态注册工具）
    let mcp_manager = Arc::new(McpManager::new(Arc::clone(&registry)));

    // 注册 MCP 管理工具
    registry.register_dynamic(Arc::new(ConnectMcpTool::new(Arc::clone(&mcp_manager))));
    registry.register_dynamic(Arc::new(DisconnectMcpTool::new(Arc::clone(&mcp_manager))));
    registry.register_dynamic(Arc::new(ListMcpTool::new(Arc::clone(&mcp_manager))));

    // ... 其余初始化 ...
}
```

### 5.3 child_agent() 共享 mcp_manager

```rust
pub fn child_agent(&self, max_turns: usize, sub_system: &str) -> Agent {
    Agent {
        // ... 现有 Arc clone ...
        registry: Arc::clone(&self.registry),       // 共享 registry（含已注册的 MCP 工具）
        mcp_manager: Arc::clone(&self.mcp_manager), // 共享 MCP manager

        // 子 agent 不可 connect/disconnect（Lead-only），但可使用已连接的工具
        // available_for(Subagent) = false 在 ConnectMcpTool / DisconnectMcpTool 中控制
        // ...
    }
}
```

### 5.4 run_loop 中的 system prompt

```rust
pub async fn run_loop(&self, messages: &mut Vec<Message>, active_request: &str) -> Result<...> {
    // 召回记忆
    let recalled = self.memory.load_memories(...).await;
    let index = self.memory.read_memory_index();

    // s14: 附加已连接 MCP server 信息
    let mcp_suffix = self.mcp_manager.system_prompt_suffix();

    let system = build_system(&self.base_system, &index, &recalled);
    let system = if mcp_suffix.is_empty() {
        system
    } else {
        format!("{}\n\n{}", system, mcp_suffix)
    };

    // ... 其余循环不变 ...
}
```

### 5.5 权限管线集成

MCP 工具的权限检查已嵌入 `McpTool::check_permission()`，现有 `PermissionHook` 的闸门 3 自动覆盖：

```rust
// builtins.rs PermissionHook::on_pre_tool
// 闸门 4: registry.check_permission(name, input)
// → 找到 McpTool → McpTool::check_permission()
// → 返回 NeedsApproval（v1 硬编码）
// → ask_via_input() 询问用户
```

**无需修改 builtins.rs**。

### 5.6 Agent 退出时清理

```rust
// main.rs 退出前
agent.mcp_manager.shutdown_all().await;
```

---

## 6. 工具可见性矩阵（更新）

| Tool | Lead | Subagent | Teammate |
|------|------|----------|----------|
| connect_mcp | ✅ | ❌ | ❌ |
| disconnect_mcp | ✅ | ❌ | ❌ |
| list_mcp | ✅ | ✅ | ✅ |
| mcp__X__Y (已连接的工具) | ✅ | ✅ | ✅ |

**设计理由**：
- `connect_mcp` / `disconnect_mcp` 是管理操作，仅 Lead 可执行（避免子 agent 随意改变工具池）
- 已连接的 MCP 工具对所有 agent 可见（与基础工具一致）
- `list_mcp` 是只读查询，所有 agent 可用

---

## 7. 数据流

### 7.1 connect_mcp 完整流程

```
用户: "连接 docs MCP server"
  ↓
模型返回 tool_use: {name: "connect_mcp", input: {name: "docs", command: "npx", args: ["-y", "docs-server"]}}
  ↓
registry.dispatch("connect_mcp", ctx, input, Lead)
  ↓
ConnectMcpTool.execute(ctx, input)
  ↓
McpManager.connect("docs", "npx", ["-y", "docs-server"])
  ├─ McpClient::spawn("npx", ["-y", "docs-server"])
  │    ├─ tokio::process::Command 启动子进程
  │    ├─ 后台 task 读取 stdout (JSON-RPC responses)
  │    └─ 返回 McpClient { stdin, pending, ... }
  ├─ client.initialize() → JSON-RPC: initialize
  ├─ client.list_tools() → JSON-RPC: tools/list
  │    └─ 返回 [McpToolDef { name: "search", ... }, ...]
  └─ for each tool_def:
       ├─ prefixed = prefixed_tool_name("docs", "search") → "mcp__docs__search"
       ├─ collision check → origins.insert(...)
       ├─ mcp_tool = McpTool { prefixed_name, raw_name, client: Arc::clone(client), ... }
       └─ registry.register_dynamic(Arc::new(mcp_tool))
  ↓
返回 "Connected to MCP server 'docs'. Discovered 2 tools: mcp__docs__search, mcp__docs__get_version"
  ↓
tool_result 追加到 messages → 下一轮循环
  ↓
registry.definitions_for(Lead) → 包含 mcp__docs__search, mcp__docs__get_version
  ↓
模型看到新工具，可以调用
```

### 7.2 MCP 工具调用流程

```
模型返回 tool_use: {name: "mcp__docs__search", input: {query: "agent hooks"}}
  ↓
hooks.trigger_pre_tool:
  → PermissionHook.on_pre_tool("mcp__docs__search", input)
  → registry.check_permission("mcp__docs__search", input)
  → McpTool.check_permission(input) → NeedsApproval("External MCP tool call requires confirmation")
  → ask_via_input() 询问用户 → 用户确认
  → 放行
  ↓
registry.dispatch("mcp__docs__search", ctx, input, Lead)
  ↓
McpTool.execute(ctx, input)
  ↓
McpClient.call_tool("search", {query: "agent hooks"})
  ├─ 生成 JSON-RPC: {"jsonrpc": "2.0", "id": 3, "method": "tools/call", "params": {...}}
  ├─ 写入子进程 stdin
  ├─ oneshot::channel() 等待响应
  └─ 后台 reader task 收到响应 → sender.send(result)
  ↓
返回 "[docs] Found 3 results for 'agent hooks'"
  ↓
tool_result 追加到 messages
```

---

## 8. 新增依赖

```toml
[dependencies]
# 新增
tokio-process = { version = "0.2" }   # 或用 tokio::process（已有）
jsonrpc-core = "18"                    # JSON-RPC 协议（或手写，更轻量）
```

**评估**：
- `tokio::process` 已在 tokio 的 `full` features 中，无需额外 crate
- JSON-RPC 协议简单（request/response/notification 三种消息），手写更可控，约 100 行

**实际依赖变更**：**无新增 crate**。用 `tokio::process::Command` + `serde_json` 手写 JSON-RPC。

---

## 9. 错误处理

| 场景 | 处理 |
|------|------|
| server 进程启动失败 | `connect_mcp` 返回错误字符串，模型可重试或报告 |
| JSON-RPC 解析失败 | `call_tool` 返回 `"MCP error: ..."` |
| server 进程意外退出 | 后台 reader 检测到 EOF → 标记连接断开 → `call_tool` 返回错误 |
| 工具调用超时 | 可配置 timeout（默认 30s），超时返回 `"MCP error: timeout"` |
| 名称冲突 | `connect_mcp` 失败，返回冲突详情 |
| server 返回非标准 JSON | `extract_json_array` 风格的容错解析 |

---

## 10. 文件系统布局

无新增目录。server 的 stderr 通过 `tracing::warn!` 输出。

（v2 可加 `.claude/settings.json` 的 `mcp.servers` 配置段用于预配置 server。）

---

## 11. 测试计划

### 11.1 单元测试

```rust
// 名称规范化
#[test] fn normalize_strips_dots_and_slashes()
#[test] fn normalize_preserves_underscores_and_dashes()
#[test] fn normalize_rejects_empty()

// 前缀构造
#[test] fn prefixed_name_format()
#[test] fn prefixed_name_exceeds_64_chars()

// 冲突检测
#[test] fn collision_after_normalization_detected()
#[test] fn no_collision_for_distinct_servers()

// McpTool
#[test] fn mcp_tool_available_for_all_kinds()
#[test] fn connect_mcp_tool_lead_only()

// ToolRegistry 动态注册
#[test] fn register_dynamic_adds_tool()
#[test] fn unregister_removes_tool()
#[test] fn definitions_for_picks_up_dynamic_tool()
#[tokio::test] async fn dispatch_finds_dynamic_tool()
```

### 11.2 集成测试

```rust
// mock MCP server（echo server）
#[tokio::test] async fn connect_and_call_mock_server()
#[tokio::test] async fn disconnect_removes_tools_from_registry()
#[tokio::test] async fn multiple_servers_no_collision()
#[tokio::test] async fn server_crash_returns_error_not_panic()
```

### 11.3 Mock Server

```rust
/// 测试用 mock MCP server：读 stdin JSON-RPC，回固定响应
/// 用于验证 connect → list → call → disconnect 完整流程
struct MockMcpServer {
    tools: Vec<McpToolDef>,
    responses: HashMap<String, String>,
}
```

---

## 12. 与 Python s14 的差异

| 方面 | Python s14 | bytemaker s14 |
|------|-----------|---------------|
| MCP transport | 进程内模拟（无真实协议） | 真实 stdio JSON-RPC |
| 工具注册 | 每轮 `assemble_tool_pool()` 重建 | `RwLock<BTreeMap>` 动态注册，每轮 `definitions_for` 自动包含 |
| 名称冲突 | 抛 ValueError | 返回 AgentError::Validation |
| 权限策略 | 硬编码 dict | 硬编码 `NeedsApproval`（v1） |
| server 生命周期 | 无断开机制 | `disconnect_mcp` + `shutdown_all` |
| server 配置 | 代码硬编码 MOCK_SERVERS | `connect_mcp` 参数直传 |
| 子 agent 可见性 | MCP 工具全可见 | 同上 + `connect_mcp` Lead-only |
| 错误处理 | 返回错误字符串 | AgentError 类型化 + 降级字符串 |

---

## 13. 实现顺序

### Phase 1: 基础设施（无 transport）

1. `ToolRegistry` 改为 `RwLock<BTreeMap<String, Arc<dyn Tool>>>`
2. 新增 `register_dynamic()` / `unregister()`
3. 验证现有 23 个工具不受影响

### Phase 2: MCP 模块骨架

4. `mcp/mod.rs` — McpManager + 名称规范化 + 冲突检测
5. `mcp/tool.rs` — McpTool (impl Tool, check_permission 硬编码 NeedsApproval)

### Phase 3: stdio transport

6. `mcp/client.rs` — McpClient (stdio JSON-RPC)
7. 后台 reader task
8. `tools/call` + `tools/list` 实现

### Phase 4: 管理工具

9. `mcp/tools.rs` — connect_mcp / disconnect_mcp / list_mcp

### Phase 5: 集成

10. Agent 新增 mcp_manager 字段
11. child_agent / child_teammate 共享
12. system prompt 附加段
13. shutdown_all 清理

---

## 14. 关键风险

| 风险 | 影响 | 缓解 |
|------|------|------|
| `Box<dyn Tool>` → `Arc<dyn Tool>` 改动范围 | 所有 register 调用点 | Phase 1 单独做，充分测试 |
| MCP server 进程泄漏 | 僵尸进程 | `Drop` impl kill child + shutdown_all |
| RwLock 读锁内 await | 死锁 | dispatch 中先 clone Arc，释放锁后再 await |
| 大量 MCP 工具膨胀工具池 | 模型选择困难 | 不主动限制，但 system prompt 分类提示 |

---

**文档版本**: v1  
**创建时间**: 2026-08-21  
**基于**: bytemaker/src/ 全量代码分析 + s14_mcp_plugin 教学设计
