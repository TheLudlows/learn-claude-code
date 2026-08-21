# s14_mcp_plugin 技术规格文档

> **版本**: v9  
> **依赖**: s04_hooks  
> **下游**: s15_integrated_harness

---

## 1. 概述

s14 实现 **MCP (Model Context Protocol) Tools** 机制，使 Agent 能够动态发现并调用外部工具服务。核心能力：

- 运行时连接 MCP Server，发现其提供的工具
- 将外部工具加入 Agent 的动态工具池
- 通过命名空间前缀 (`mcp__{server}__{tool}`) 避免多 server 工具名冲突
- 宿主侧权限策略控制外部工具调用

**本质**：把"写死在代码里的工具"变成"运行时动态发现的工具"，实现工具系统的可扩展性。

---

## 2. 架构

```
用户输入
  ↓
┌─────────────────────────────────────────────────────────┐
│ Agent Loop (每轮循环)                                     │
│                                                         │
│  1. assemble_tool_pool()                                │
│     ├─ 基础工具 (bash/read/write/edit/glob)              │
│     ├─ connect_mcp 工具                                  │
│     └─ 已连接的 MCP 工具 (动态)                           │
│                                                         │
│  2. API call (tools=当前工具池)                           │
│     ↓                                                   │
│  3. 模型返回 tool_use                                    │
│     ├─ 基础工具 → 直接执行                                │
│     ├─ connect_mcp → 连接新 server，下轮生效              │
│     └─ mcp__X__Y → MCPClient.call_tool()                │
│                                                         │
│  4. tool_result 追加到 messages                          │
└─────────────────────────────────────────────────────────┘
         ↓                          ↑
    tools/list                tools/call
         ↓                          ↑
┌─────────────────────────────────────────────────────────┐
│ MCP Server (进程内模拟)                                   │
│  ├─ docs: search, get_version                           │
│  └─ deploy: trigger, status                             │
└─────────────────────────────────────────────────────────┘
```

---

## 3. 核心组件

### 3.1 MCPClient

**职责**：保存 MCP Server 的工具定义和调用入口。

```python
class MCPClient:
    def __init__(self, name: str):
        self.name = name
        self.tools: list[dict] = []          # 工具定义 (JSON Schema)
        self._handlers: dict[str, callable] = {}  # 工具名 → 调用函数

    def register(self, tool_defs: list[dict], handlers: dict[str, callable]):
        """注册工具定义和对应 handler（模拟 tools/list 结果）"""
        # 校验：名称非空、无重复、handler 齐全
        ...

    def call_tool(self, tool_name: str, args: dict) -> str:
        """调用工具（模拟 tools/call）"""
        handler = self._handlers.get(tool_name)
        if not handler:
            return f"MCP error: unknown tool '{tool_name}'"
        try:
            return str(handler(**args))
        except Exception as exc:
            return f"MCP error: {type(exc).__name__}: {exc}"
```

**设计要点**：
- `register()` 对应 MCP 协议的 `tools/list` 响应
- `call_tool()` 对应 MCP 协议的 `tools/call` 请求
- 错误返回字符串而非抛异常，保证 Agent Loop 不中断

---

### 3.2 connect_mcp 工具

**职责**：连接 MCP Server 并触发工具发现。

```python
def connect_mcp(name: str) -> str:
    if name in mcp_clients:
        return f"MCP server '{name}' already connected"
    factory = MOCK_SERVERS.get(name)
    if not factory:
        return f"Unknown server '{name}'. Available: {', '.join(MOCK_SERVERS)}"
    server = factory()
    mcp_clients[name] = server
    ...
    return f"Connected to MCP server '{name}'. Discovered {len(server.tools)} tools: ..."
```

**工具定义**：
```python
CONNECT_TOOL = {
    "name": "connect_mcp",
    "description": "Connect to an MCP server and discover its tools.",
    "input_schema": {
        "type": "object",
        "properties": {"name": {"type": "string", "enum": ["docs", "deploy"]}},
        "required": ["name"],
    },
}
```

**行为**：
- 连接是**延迟的**：模型调用 `connect_mcp` 时才真正连接
- 连接后工具**下一轮**才可见（因为 `assemble_tool_pool()` 在每轮开头调用）
- 重复连接返回已连接状态，不报错

---

### 3.3 assemble_tool_pool()

**职责**：每轮循环开头组装当前可用工具池。

```python
def assemble_tool_pool() -> tuple[list[dict], dict[str, callable]]:
    tools = list(BUILTIN_TOOLS)           # 基础工具 + connect_mcp
    handlers = dict(BUILTIN_HANDLERS)
    policies: dict[str, str] = {}
    origins = {...}  # 用于冲突检测

    for server_name, server in mcp_clients.items():
        safe_server = normalize_mcp_name(server_name)
        for tool_def in server.tools:
            raw_name = tool_def["name"]
            safe_tool = normalize_mcp_name(raw_name)
            prefixed = f"mcp__{safe_server}__{safe_tool}"

            # 校验：长度 ≤64、无冲突、schema 合法
            if len(prefixed) > 64:
                raise ValueError(...)
            if prefixed in origins:
                raise ValueError("MCP tool name collision after normalization")

            # 加入工具池
            tools.append({
                "name": prefixed,
                "description": tool_def.get("description", ""),
                "input_schema": schema,
            })

            # 创建 handler（闭包捕获当前 server 和 tool）
            handlers[prefixed] = (
                lambda *, client=server, tool=raw_name, **kwargs:
                client.call_tool(tool, kwargs)
            )

            # 记录权限策略
            policies[prefixed] = MCP_HOST_POLICY.get(
                (server_name, raw_name), "confirm"
            )

    mcp_tool_policies = policies
    return tools, handlers
```

**关键设计**：
1. **动态组装**：每轮循环重新组装，新连接的工具自动生效
2. **命名空间前缀**：`mcp__{server}__{tool}` 避免冲突
3. **名称规范化**：`normalize_mcp_name()` 替换非法字符为 `_`
4. **冲突检测**：规范化后检查重名（防止 `a.b/c` 和 `a_b_c` 冲突）
5. **长度限制**：≤64 字符（Anthropic API 工具名限制）
6. **闭包捕获**：`lambda *, client=server, tool=raw_name` 避免循环变量捕获问题

---

### 3.4 名称规范化

```python
_DISALLOWED_CHARS = re.compile(r"[^a-zA-Z0-9_-]")

def normalize_mcp_name(name: str) -> str:
    """替换不在 [a-zA-Z0-9_-] 中的字符为 _"""
    normalized = _DISALLOWED_CHARS.sub("_", name)
    if not normalized:
        raise ValueError("MCP names cannot normalize to an empty string")
    return normalized
```

**原因**：Anthropic API 工具名只允许 `[a-zA-Z0-9_-]`，MCP server 返回的名称可能包含 `.`、`/` 等字符。

---

## 4. 权限模型

### 4.1 宿主侧策略 (Host Policy)

```python
MCP_HOST_POLICY = {
    ("docs", "search"): "allow",           # 只读查询，自动放行
    ("docs", "get_version"): "allow",
    ("deploy", "status"): "allow",         # 只读状态，自动放行
    ("deploy", "trigger"): "confirm",      # 破坏性操作，需用户确认
}
```

**设计原则**：
- 权限由**宿主配置**决定，不信任 server 的 `readOnlyHint`/`destructiveHint`
- 未配置的工具默认 `"confirm"`（保守策略）
- 策略以 `(server_name, tool_name)` 为 key，粒度到单个工具

### 4.2 权限检查集成

```python
def permission_hook(block):
    ...
    if block.name.startswith("mcp__"):
        policy = mcp_tool_policies.get(block.name, "confirm")
        if policy != "allow":
            print(f"\n[permission] External tool {block.name}({block.input})")
            if input("Allow? [y/N] ").strip().lower() not in {"y", "yes"}:
                return "Permission denied by user"
    return None
```

**流程**：
1. `PreToolUse` hook 拦截所有工具调用
2. 检测工具名前缀 `mcp__` → 是外部工具
3. 查询 `mcp_tool_policies` 获取策略
4. `"allow"` → 放行；`"confirm"` → 提示用户确认

---

## 5. 错误处理

### 5.1 工具调用错误

```python
def call_tool(self, tool_name: str, args: dict) -> str:
    handler = self._handlers.get(tool_name)
    if not handler:
        return f"MCP error: unknown tool '{tool_name}'"
    try:
        return str(handler(**args))
    except Exception as exc:
        return f"MCP error: {type(exc).__name__}: {exc}"
```

**行为**：
- 未知工具 → 返回错误字符串
- 参数错误 → 捕获异常，返回错误字符串
- 错误作为 `tool_result` 返回给模型，模型可在下一轮修正

### 5.2 API 调用错误

```python
def agent_loop(messages: list):
    while True:
        try:
            tools, handlers = assemble_tool_pool()
            response = client.messages.create(...)
        except Exception as exc:
            messages.append({
                "role": "assistant",
                "content": [{"type": "text", "text": f"[Error] {type(exc).__name__}: {exc}"}],
            })
            trigger_hooks("Stop", messages)
            return
```

**行为**：API 错误（网络、限流等）直接终止循环，记录错误到 messages。

---

## 6. 模拟 Server

### 6.1 docs server

```python
def _mock_server_docs() -> MCPClient:
    server = MCPClient("docs")
    server.register(
        tool_defs=[
            {
                "name": "search",
                "description": "Search the documentation.",
                "inputSchema": {
                    "type": "object",
                    "properties": {"query": {"type": "string"}},
                    "required": ["query"],
                },
                "annotations": {"readOnlyHint": True},
            },
            {
                "name": "get_version",
                "description": "Get the documentation API version.",
                "inputSchema": {"type": "object", "properties": {}},
                "annotations": {"readOnlyHint": True},
            },
        ],
        handlers={
            "search": lambda query: f"[docs] Found 3 results for '{query}'",
            "get_version": lambda: "[docs] API v2.1.0",
        },
    )
    return server
```

### 6.2 deploy Server

```python
def _mock_server_deploy() -> MCPClient:
    server = MCPClient("deploy")
    server.register(
        tool_defs=[
            {
                "name": "trigger",
                "description": "Trigger a deployment.",
                "inputSchema": {
                    "type": "object",
                    "properties": {"service": {"type": "string"}},
                    "required": ["service"],
                },
                "annotations": {"destructiveHint": True},
            },
            {
                "name": "status",
                "description": "Check deployment status.",
                "inputSchema": {
                    "type": "object",
                    "properties": {"service": {"type": "string"}},
                    "required": ["service"],
                },
                "annotations": {"readOnlyHint": True},
            },
        ],
        handlers={
            "trigger": lambda service: f"[deploy] Triggered: {service}",
            "status": lambda service: f"[deploy] {service}: running (v1.4.2)",
        },
    )
    return server
```

**用途**：
- 展示 `tools/list` 和 `tools/call` 的协议边界
- 演示 `readOnlyHint` vs `destructiveHint` 的区别（但权限仍由宿主策略控制）
- 真实 MCP transport（stdio/SSE）不在本章实现

---

## 7. 与 s04 的差异

| 方面 | s04 (Hooks) | s14 (MCP) |
|------|-------------|-----------|
| 工具来源 | 硬编码在 `code.py` | 基础工具 + 动态发现的 MCP 工具 |
| 工具池 | 固定 `TOOLS` 列表 | 每轮 `assemble_tool_pool()` 动态组装 |
| 外部工具 | 无 | `mcp__{server}__{tool}` 命名空间 |
| 权限 | Shell 命令 + 路径检查 | 增加 MCP 宿主策略 (`MCP_HOST_POLICY`) |
| 工具发现 | 无 | `connect_mcp` → `MCPClient.register()` |
| MCP transport | 无 | 进程内模拟（无真实 stdio/SSE） |

---

## 8. 集成边界

s14 是一个**独立模块**，不带入以下系统（它们在 s15 合并）：

- ❌ s06 Subagent（任务委派）
- ❌ s10 Task System（任务队列）
- ❌ s11 Background Tasks（后台任务）
- ❌ s12 Cron Scheduler（定时任务）
- ❌ s13 Agent Teams（多 agent 协作）
- ❌ s07 Skill Loading（技能加载）
- ❌ s08 Memory（记忆系统）
- ❌ s09 Context Compaction（上下文压缩）

**输入**：s04 的基础工具 + Hooks  
**输出**：MCP 工具发现与调用机制，供 s15 集成

---

## 9. 设计决策

### 9.1 为什么延迟连接？

**决策**：模型调用 `connect_mcp` 时才连接，而非启动时连接所有 server。

**原因**：
- 减少启动开销（只连接需要的 server）
- 模型可根据任务自主决定连接哪些 server
- 避免暴露不必要的工具（安全考量）

### 9.2 为什么每轮重新组装工具池？

**决策**：`assemble_tool_pool()` 在每轮循环开头调用。

**原因**：
- 新连接的工具需要在下一轮立即可用
- 避免在循环中间修改工具列表（复杂性高）
- 工具池组装开销低（只是列表拼接）

### 9.3 为什么用前缀命名空间？

**决策**：`mcp__{server}__{tool}` 格式。

**原因**：
- 多 server 可能提供同名工具（如 `search`、`status`）
- 前缀清晰标识工具来源
- `__` 双下划线在视觉上分隔明确

**替代方案**：
- 用 server 名作为工具名前缀（如 `docs_search`）→ 可能和基础工具冲突
- 用 UUID → 不可读

### 9.4 为什么不信任 server 的 annotations？

**决策**：`readOnlyHint`/`destructiveHint` 仅作为参考，权限由 `MCP_HOST_POLICY` 控制。

**原因**：
- Server 可能被恶意篡改，提供误导性 annotations
- 权限是安全边界，必须由可信方（宿主）控制
- 类似浏览器不信任服务器的 CORS 头，由浏览器策略决定

---

## 10. 限制与未来工作

### 10.1 当前限制

1. **无真实 MCP transport**：仅进程内模拟，无 stdio/SSE/HTTP 实现
2. **无工具热更新**：server 工具列表变更后需重新连接
3. **无 server 生命周期管理**：连接后无法断开
4. **无并发调用**：同一 server 的工具串行执行
5. **无 streaming 支持**：`call_tool` 同步返回完整结果

### 10.2 s15 集成方向

- 将 MCP 工具池与 Subagent、Task System、Background Tasks 合并
- 支持 MCP 工具在子 agent 中的调用
- 支持长时间 MCP 工具调用的后台化

---

## 11. 使用示例

### 11.1 连接 docs server 并搜索

**用户输入**：
```
连接 docs server，搜索 agent hooks，并告诉我当前文档 API 版本。
```

**工具调用轨迹**：
```
1. connect_mcp(name="docs")
   → "Connected to MCP server 'docs'. Discovered 2 tools: search, get_version"

2. mcp__docs__search(query="agent hooks")
   → "[docs] Found 3 results for 'agent hooks'"

3. mcp__docs__get_version()
   → "[docs] API v2.1.0"
```

### 11.2 权限控制示例

**用户输入**：
```
连接 deploy server，查看 web 服务状态，不要触发部署。
```

**行为**：
- `mcp__deploy__status(service="web")` → 按策略 `"allow"` 自动执行
- `mcp__deploy__trigger(service="web")` → 按策略 `"confirm"` 提示用户确认

---

## 12. 文件结构

```
s14_mcp_plugin/
├── code.py                    # 完整实现 (530 行)
├── README.zh.md               # 中文文档
└── images/
    ├── mcp-architecture.svg   # 架构图 (中文)
    ├── mcp-architecture.en.svg
    └── mcp-architecture.ja.svg
```

---

## 13. 关键代码行号

| 组件 | 行号范围 |
|------|---------|
| `MCPClient` 类 | 160-187 |
| `connect_mcp` 工具 | 279-307 |
| `assemble_tool_pool()` | 313-356 |
| `normalize_mcp_name()` | 203-208 |
| `MCP_HOST_POLICY` | 195-200 |
| `permission_hook` (MCP 部分) | 402-407 |
| 模拟 server 定义 | 211-276 |

---

**文档版本**: v1  
**生成时间**: 2026-08-21  
**基于**: s14_mcp_plugin/code.py + README.zh.md (v9)
