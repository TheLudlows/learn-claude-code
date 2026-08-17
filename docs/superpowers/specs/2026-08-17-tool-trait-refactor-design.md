# Tool Trait Refactor Design Spec

**Date:** 2026-08-17
**Status:** Draft
**Scope:** rust-agent/src/tools.rs 及相关文件

---

## 1. Problem Statement

### 当前架构：双重事实来源

工具系统存在两个必须手动同步的硬编码位置：

| 位置 | 文件:行 | 内容 |
|------|---------|------|
| `dispatch_tool()` match | tools.rs:383-429 | 工具名 → 执行逻辑 |
| `get_base_tool_definitions()` | tools.rs:443-550 | 工具名 → JSON Schema 定义 |
| `get_task_tool_definition()` | tools.rs:553-565 | task 工具单独定义 |

**问题：**
- 新增工具需改 4-6 处：dispatch match 臂、ToolDefinition vec、get_tool_definitions/get_subagent_tool_definitions 接线、permission.rs::check_rules、main.rs::execute_tool（如需 async）
- 手动同步容易漂移：C9 问题（task 工具定义在 get_base_tool_definitions 但 dispatch_tool 不处理它）是漂移的铁证
- 代码分散：一个工具的 schema 和执行逻辑相隔数百行

### 额外耦合点

- `permission.rs::check_rules()` (53-70) 按工具名做 match 规则检查——第三种隐式耦合
- `main.rs::execute_tool()` (72-80) 对 task 工具有特殊的 async 路径——第四种隐式耦合

---

## 2. Design Goals

1. **单一事实来源** — 工具的 name、description、schema、execute、permission 全部定义在同一处（一个 struct + impl）
2. **新增工具 = 一个文件** — 加一个 .rs 文件 + mod.rs 一行注册
3. **消除 dispatch match** — 用注册表替代 switch-case
4. **消除 definitions match** — 从注册表动态生成
5. **统一 async** — 所有工具都用 `async fn execute()`，同步工具内部不 await
6. **权限规则随工具走** — `check_permission()` 方法在工具 trait 上，不再独立 match

---

## 3. Architecture

### 3.1 Tool Trait

```rust
use async_trait::async_trait;

/// 工具执行上下文 —— 所有工具共享的依赖注入
pub struct ToolContext<'a> {
    pub client: &'a Client,
    pub hooks: &'a Hooks,
}

/// 工具权限检查结果
pub enum PermissionCheck {
    /// 无需额外权限检查（默认）
    Pass,
    /// 需要用户确认，附带原因描述
    NeedsApproval(&'static str),
}

/// 工具 trait —— 单一事实来源
#[async_trait]
pub trait Tool: Send + Sync {
    /// 工具名称（必须与 API schema 中的 name 一致）
    fn name(&self) -> &str;

    /// 工具描述（发送给模型的说明）
    fn description(&self) -> &str;

    /// JSON Schema 输入定义
    fn input_schema(&self) -> serde_json::Value;

    /// 权限规则 —— 默认无需额外检查
    fn check_permission(&self, _input: &serde_json::Value) -> PermissionCheck {
        PermissionCheck::Pass
    }

    /// 执行工具，返回结果字符串
    async fn execute(&self, ctx: &ToolContext<'_>, input: &serde_json::Value) -> String;

    /// 是否为 subagent 可用工具（默认 true）
    fn available_for_subagent(&self) -> bool {
        true
    }
}
```

**设计决策：**
- `async_trait` crate 处理 async trait 方法（Rust 原生 async trait 尚不支持 dyn dispatch）
- `Send + Sync` bound 确保工具可跨线程使用（dyn Tool: Send + Sync）
- `check_permission()` 默认 `Pass`，仅需要权限检查的工具覆写
- `available_for_subagent()` 默认 `true`，仅 `TaskTool` 返回 `false`
- `ToolContext` 携带所有工具可能需要的依赖，同步工具可忽略 `_ctx`

### 3.2 ToolRegistry

```rust
pub struct ToolRegistry {
    all_tools: Vec<Box<dyn Tool>>,
    index: HashMap<String, usize>,
}

impl ToolRegistry {
    pub fn new(tools: Vec<Box<dyn Tool>>) -> Self {
        let index: HashMap<String, usize> = tools
            .iter()
            .enumerate()
            .map(|(i, t)| (t.name().to_string(), i))
            .collect();
        Self { all_tools: tools, index }
    }

    /// 生成 ToolDefinition 列表
    ///
    /// - `subagent_only = true` → 仅返回 available_for_subagent() == true 的工具
    /// - `subagent_only = false` → 返回所有工具
    pub fn definitions(&self, subagent_only: bool) -> Vec<ToolDefinition> {
        self.all_tools.iter()
            .filter(|t| !subagent_only || t.available_for_subagent())
            .map(|t| ToolDefinition {
                name: t.name().to_string(),
                description: t.description().to_string(),
                input_schema: t.input_schema(),
            })
            .collect()
    }

    /// 分发工具调用 —— 替代 dispatch_tool() match
    pub async fn dispatch(
        &self,
        ctx: &ToolContext<'_>,
        name: &str,
        input: &serde_json::Value,
    ) -> String {
        let result = match self.index.get(name) {
            Some(&idx) => self.all_tools[idx].execute(ctx, input).await,
            None => return format!("[ERROR:unknown] Unknown tool: {}", name),
        };

        if result.starts_with("Error:") {
            with_error_prefix(name, &result)
        } else {
            result
        }
    }

    /// 权限检查 —— 替代 permission.rs::check_rules() match
    pub fn check_permission(&self, name: &str, input: &serde_json::Value) -> PermissionCheck {
        match self.index.get(name) {
            Some(&idx) => self.all_tools[idx].check_permission(input),
            None => PermissionCheck::Pass,
        }
    }
}
```

**替代关系：**

| 原来 | 现在 |
|------|------|
| `dispatch_tool(name, input)` | `registry.dispatch(ctx, name, input).await` |
| `get_base_tool_definitions()` | `registry.definitions(true)` |
| `get_tool_definitions()` | `registry.definitions(false)` |
| `get_subagent_tool_definitions()` | `registry.definitions(true)` |
| `permission::check_rules(name, input)` | `registry.check_permission(name, input)` |

### 3.3 build_registry()

```rust
// tools/mod.rs

pub fn build_registry() -> ToolRegistry {
    ToolRegistry::new(vec![
        Box::new(command::CommandTool),
        Box::new(read_file::ReadFileTool),
        Box::new(write_file::WriteFileTool),
        Box::new(edit_file::EditFileTool),
        Box::new(glob::GlobTool),
        Box::new(load_skill::LoadSkillTool),
        Box::new(todo_write::TodoWriteTool),
        Box::new(task::TaskTool),
    ])
}
```

---

## 4. Tool Implementations

### 4.1 CommandTool (tools/command.rs)

```rust
pub struct CommandTool;

#[async_trait]
impl Tool for CommandTool {
    fn name(&self) -> &str { "command" }
    fn description(&self) -> &str { "Run a shell command." }
    fn input_schema(&self) -> serde_json::Value { /* json!({...}) */ }

    fn check_permission(&self, input: &serde_json::Value) -> PermissionCheck {
        let cmd = input.get("command").and_then(|v| v.as_str()).unwrap_or("");
        if ["rm ", "> /etc/", "chmod 777"].iter().any(|kw| cmd.contains(kw)) {
            return PermissionCheck::NeedsApproval("Potentially destructive command");
        }
        PermissionCheck::Pass
    }

    async fn execute(&self, _ctx: &ToolContext<'_>, input: &serde_json::Value) -> String {
        match input.get("command").and_then(|c| c.as_str()) {
            Some(cmd) if !cmd.is_empty() => super::run_bash(cmd),
            _ => "Error: missing command".to_string(),
        }
    }
}
```

### 4.2 ReadFileTool (tools/read_file.rs)

```rust
pub struct ReadFileTool;

#[async_trait]
impl Tool for ReadFileTool {
    fn name(&self) -> &str { "read_file" }
    fn description(&self) -> &str { "Read file contents." }
    fn input_schema(&self) -> serde_json::Value { /* json!({...}) */ }

    fn check_permission(&self, input: &serde_json::Value) -> PermissionCheck {
        let path = input.get("path").and_then(|v| v.as_str()).unwrap_or("");
        if super::escapes_workspace_lexical(path) {
            return PermissionCheck::NeedsApproval("Access outside workspace");
        }
        PermissionCheck::Pass
    }

    async fn execute(&self, _ctx: &ToolContext<'_>, input: &serde_json::Value) -> String {
        let path = input.get("path").and_then(|p| p.as_str()).unwrap_or("");
        let limit = input.get("limit").and_then(|l| l.as_u64()).map(|l| l as u32);
        super::run_read_file(path, limit)
    }
}
```

### 4.3 WriteFileTool (tools/write_file.rs)

```rust
pub struct WriteFileTool;

#[async_trait]
impl Tool for WriteFileTool {
    fn name(&self) -> &str { "write_file" }
    fn description(&self) -> &str { "Write content to a file." }
    fn input_schema(&self) -> serde_json::Value { /* json!({...}) */ }

    fn check_permission(&self, input: &serde_json::Value) -> PermissionCheck {
        let path = input.get("path").and_then(|v| v.as_str()).unwrap_or("");
        if super::escapes_workspace_lexical(path) {
            return PermissionCheck::NeedsApproval("Access outside workspace");
        }
        PermissionCheck::Pass
    }

    async fn execute(&self, _ctx: &ToolContext<'_>, input: &serde_json::Value) -> String {
        let path = input.get("path").and_then(|p| p.as_str()).unwrap_or("");
        let content = input.get("content").and_then(|c| c.as_str()).unwrap_or("");
        super::run_write_file(path, content)
    }
}
```

### 4.4 EditFileTool (tools/edit_file.rs)

```rust
pub struct EditFileTool;

#[async_trait]
impl Tool for EditFileTool {
    fn name(&self) -> &str { "edit_file" }
    fn description(&self) -> &str { "Replace exact text in a file once." }
    fn input_schema(&self) -> serde_json::Value { /* json!({...}) */ }

    fn check_permission(&self, input: &serde_json::Value) -> PermissionCheck {
        let path = input.get("path").and_then(|v| v.as_str()).unwrap_or("");
        if super::escapes_workspace_lexical(path) {
            return PermissionCheck::NeedsApproval("Access outside workspace");
        }
        PermissionCheck::Pass
    }

    async fn execute(&self, _ctx: &ToolContext<'_>, input: &serde_json::Value) -> String {
        let path = input.get("path").and_then(|p| p.as_str()).unwrap_or("");
        let old_text = input.get("old_text").and_then(|o| o.as_str()).unwrap_or("");
        let new_text = input.get("new_text").and_then(|n| n.as_str()).unwrap_or("");
        super::run_edit_file(path, old_text, new_text)
    }
}
```

### 4.5 GlobTool (tools/glob.rs)

```rust
pub struct GlobTool;

#[async_trait]
impl Tool for GlobTool {
    fn name(&self) -> &str { "glob" }
    fn description(&self) -> &str { "Find files matching a glob pattern." }
    fn input_schema(&self) -> serde_json::Value { /* json!({...}) */ }

    // check_permission 默认 Pass，无需覆写

    async fn execute(&self, _ctx: &ToolContext<'_>, input: &serde_json::Value) -> String {
        let pattern = input.get("pattern").and_then(|p| p.as_str()).unwrap_or("");
        super::run_glob(pattern)
    }
}
```

### 4.6 LoadSkillTool (tools/load_skill.rs)

```rust
pub struct LoadSkillTool;

#[async_trait]
impl Tool for LoadSkillTool {
    fn name(&self) -> &str { "load_skill" }
    fn description(&self) -> &str { "Load the full SKILL.md content by skill name." }
    fn input_schema(&self) -> serde_json::Value { /* json!({...}) */ }

    async fn execute(&self, _ctx: &ToolContext<'_>, input: &serde_json::Value) -> String {
        crate::skills::run_load_skill(input)
    }
}
```

### 4.7 TodoWriteTool (tools/todo_write.rs)

```rust
pub struct TodoWriteTool;

#[async_trait]
impl Tool for TodoWriteTool {
    fn name(&self) -> &str { "todo_write" }
    fn description(&self) -> &str {
        "Create or replace the todo list for multi-step tasks. \
         Each call replaces the entire list; at most one item may be \
         in_progress. Use this to plan before starting work and \
         update statuses as you progress."
    }
    fn input_schema(&self) -> serde_json::Value { /* json!({...}) */ }

    async fn execute(&self, _ctx: &ToolContext<'_>, input: &serde_json::Value) -> String {
        match input.get("todos") {
            Some(todos) => crate::todo::run_todo_write(todos),
            None => "Error: missing todos".to_string(),
        }
    }
}
```

### 4.8 TaskTool (tools/task.rs)

```rust
pub struct TaskTool;

#[async_trait]
impl Tool for TaskTool {
    fn name(&self) -> &str { "task" }
    fn description(&self) -> &str {
        "Run a subagent with fresh conversation context and return its final text."
    }
    fn input_schema(&self) -> serde_json::Value { /* json!({...}) */ }

    fn available_for_subagent(&self) -> bool { false }  // 防止递归

    async fn execute(&self, ctx: &ToolContext<'_>, input: &serde_json::Value) -> String {
        match input.get("prompt").and_then(|p| p.as_str()) {
            Some(prompt) if !prompt.is_empty() => {
                crate::subagent::run_subagent_loop(ctx.client, prompt, ctx.hooks)
                    .await
                    .unwrap_or_else(|e| format!("Subagent error: {}", e))
            }
            _ => "Error: missing prompt".to_string(),
        }
    }
}
```

---

## 5. Integration Points

### 5.1 main.rs Changes

```rust
use tools::{build_registry, ToolContext};

async fn execute_tool(
    registry: &ToolRegistry,
    ctx: &ToolContext<'_>,
    name: &str,
    input: &serde_json::Value,
    hooks: &Hooks,
) -> String {
    if let Some(reason) = hooks.trigger_pre_tool(registry, name, input) {
        return reason;
    }
    registry.dispatch(ctx, name, input).await
}

async fn agent_loop(
    client: &Client,
    system: &str,
    messages: &mut Vec<Message>,
    hooks: &Hooks,
    registry: &ToolRegistry,
) -> Result<(), Box<dyn std::error::Error>> {
    loop {
        let ctx = ToolContext { client, hooks };
        let response = client
            .stream_messages(system, messages, &registry.definitions(false), 8000)
            .await?;
        // ... 处理响应 ...
        // execute_tool(registry, &ctx, name, input, hooks).await
    }
}

#[tokio::main]
async fn main() {
    // ...
    let registry = tools::build_registry();
    // registry 传给 agent_loop
}
```

### 5.2 subagent.rs Changes

```rust
use crate::tools::ToolRegistry;

pub async fn run_subagent_loop(
    client: &Client,
    prompt: &str,
    hooks: &Hooks,
    registry: &ToolRegistry,
) -> Result<String, Box<dyn std::error::Error>> {
    let ctx = ToolContext { client, hooks };
    // ...
    let response = client
        .stream_messages(SUB_SYSTEM, &messages, &registry.definitions(true), 8000)
        .await?;
    // ...
    let output = registry.dispatch(&ctx, name, input).await;
}
```

### 5.3 hooks.rs Changes

PreToolUse 回调签名变更：

```rust
use crate::tools::ToolRegistry;

type PreToolUseFn = fn(&ToolRegistry, &str, &serde_json::Value) -> Option<String>;

impl Hooks {
    pub fn trigger_pre_tool(
        &self,
        registry: &ToolRegistry,
        name: &str,
        input: &serde_json::Value,
    ) -> Option<String> {
        for cb in &self.pre_tool_callbacks {
            if let Some(reason) = cb(registry, name, input) {
                return Some(reason);
            }
        }
        None
    }
}
```

其他回调类型（UserPromptSubmit、PostToolUse、Stop）签名不变。

### 5.4 permission.rs Changes

`check_rules` match 被消除，权限规则移至各工具文件的 `check_permission()` 方法。

```rust
use crate::tools::{ToolRegistry, PermissionCheck};

pub fn permission_hook(
    registry: &ToolRegistry,
    name: &str,
    input: &serde_json::Value,
) -> Option<String> {
    // 闸门 1: 硬拒绝列表（不变）
    if name == "command" {
        let cmd = input.get("command").and_then(|v| v.as_str()).unwrap_or("");
        if let Some(p) = check_deny_list(cmd) {
            println!("\n\x1b[31m[blocked] '{}' is on the deny list\x1b[0m", p);
            return Some(format!("Permission denied: '{}' on deny list", p));
        }
    }

    // 闸门 2+3: 通过 registry 查规则，命中则问用户
    match registry.check_permission(name, input) {
        PermissionCheck::NeedsApproval(reason) => {
            if !ask_user(name, input, reason) {
                return Some("Permission denied by user".to_string());
            }
        }
        PermissionCheck::Pass => {}
    }

    None
}
```

`check_rules()` 函数删除，`escapes_workspace()` 和 `normalize()` 移到 `tools/mod.rs`（供文件类工具的 `check_permission()` 使用）。

---

## 6. File Structure

```
rust-agent/src/
├── main.rs              # build_registry()，传引用给 agent_loop/execute_tool
├── client.rs            # 不变
├── hooks.rs             # PreToolUse 签名加 &ToolRegistry
├── output.rs            # 不变
├── permission.rs        # permission_hook 改用 registry.check_permission()
├── subagent.rs          # 改用 registry.dispatch() + definitions(true)
├── skills.rs            # 不变
├── todo.rs              # 不变
└── tools/
    ├── mod.rs           # Tool trait + ToolContext + ToolRegistry + build_registry()
    │                    # + 共享工具函数（run_bash, run_read_file, etc.）
    │                    # + 路径安全函数（safe_path, safe_path_in, etc.）
    ├── command.rs       # CommandTool
    ├── read_file.rs     # ReadFileTool
    ├── write_file.rs    # WriteFileTool
    ├── edit_file.rs     # EditFileTool
    ├── glob.rs          # GlobTool
    ├── load_skill.rs    # LoadSkillTool
    ├── todo_write.rs    # TodoWriteTool
    └── task.rs          # TaskTool
```

### tools/mod.rs 内容

```rust
// Re-exports
mod command;
mod read_file;
mod write_file;
mod edit_file;
mod glob;
mod load_skill;
mod todo_write;
mod task;

// 公开 API
pub use self::registry::ToolRegistry;
pub use self::trait_def::{Tool, ToolContext, PermissionCheck};

// 内部模块
mod trait_def;    // Tool trait, ToolContext, PermissionCheck
mod registry;     // ToolRegistry

// 构建函数
pub fn build_registry() -> ToolRegistry { /* ... */ }

// 共享工具函数（供各 Tool impl 调用）
pub(crate) fn run_bash(command: &str) -> String { /* 原 tools.rs 内容 */ }
pub(crate) fn run_read_file(path: &str, limit: Option<u32>) -> String { /* ... */ }
pub(crate) fn run_write_file(path: &str, content: &str) -> String { /* ... */ }
pub(crate) fn run_edit_file(path: &str, old_text: &str, new_text: &str) -> String { /* ... */ }
pub(crate) fn run_glob(pattern: &str) -> String { /* ... */ }
pub(crate) fn with_error_prefix(prefix: &str, message: &str) -> String { /* ... */ }

// 路径安全函数
pub(crate) fn safe_path(path_str: &str) -> Result<PathBuf, String> { /* ... */ }
pub(crate) fn safe_path_in(workdir: &Path, path_str: &str) -> Result<PathBuf, String> { /* ... */ }
pub(crate) fn workdir() -> PathBuf { /* ... */ }
pub(crate) fn escapes_workspace_lexical(path: &str) -> bool { /* 从 permission.rs 移来 */ }
```

---

## 7. Change Summary

| 文件 | 变更类型 | 说明 |
|------|----------|------|
| `tools/mod.rs` | 重大重构 | 新增 trait + registry + build_registry；保留共享工具函数；删除 dispatch_tool、get_*_tool_definitions |
| `tools/command.rs` | 新建 | CommandTool impl |
| `tools/read_file.rs` | 新建 | ReadFileTool impl |
| `tools/write_file.rs` | 新建 | WriteFileTool impl |
| `tools/edit_file.rs` | 新建 | EditFileTool impl |
| `tools/glob.rs` | 新建 | GlobTool impl |
| `tools/load_skill.rs` | 新建 | LoadSkillTool impl |
| `tools/todo_write.rs` | 新建 | TodoWriteTool impl |
| `tools/task.rs` | 新建 | TaskTool impl |
| `main.rs` | 修改 | build_registry()，execute_tool/agent_loop 改用 registry |
| `hooks.rs` | 修改 | PreToolUse 回调签名加 &ToolRegistry |
| `permission.rs` | 修改 | 删除 check_rules match，改用 registry.check_permission()；escapes_workspace 移至 tools |
| `subagent.rs` | 修改 | 改用 registry.dispatch() + definitions(true) |
| `client.rs` | 不变 | — |
| `Cargo.toml` | 新增依赖 | `async-trait` |

---

## 8. Adding a New Tool (Post-Refactor)

**三步完成：**

1. 创建 `tools/new_tool.rs`：
   ```rust
   use async_trait::async_trait;
   use crate::tools::{Tool, ToolContext, PermissionCheck};

   pub struct NewTool;

   #[async_trait]
   impl Tool for NewTool {
       fn name(&self) -> &str { "new_tool" }
       fn description(&self) -> &str { "Does something new." }
       fn input_schema(&self) -> serde_json::Value {
           serde_json::json!({
               "type": "object",
               "properties": { "param": { "type": "string" } },
               "required": ["param"]
           })
       }
       async fn execute(&self, _ctx: &ToolContext<'_>, input: &serde_json::Value) -> String {
           // implementation
           "done".to_string()
       }
   }
   ```

2. 在 `tools/mod.rs` 加 `mod new_tool;`

3. 在 `build_registry()` 加一行：
   ```rust
   Box::new(new_tool::NewTool),
   ```

**无需再修改的地方：**
- ~~dispatch_tool match~~ → registry.dispatch 自动覆盖
- ~~ToolDefinition vec~~ → registry.definitions 自动覆盖
- ~~permission::check_rules~~ → 默认 PermissionCheck::Pass，需要时覆写 check_permission
- ~~main.rs::execute_tool~~ → 不再有 task 特殊路径

---

## 9. Testing Strategy

### 9.1 保留现有测试

所有现有单元测试应继续通过（可能需微调 import 路径）：
- `tools.rs` 中的 `test_tool`、`glob_match_tests`、`dispatch_tool_tests`、`run_bash_tests`、`safe_path_tests`
- `permission.rs` 中的 `tests`

### 9.2 新增测试

- **ToolRegistry 测试：**
  - `test_registry_dispatch_known_tool` — 已知工具名正确分发
  - `test_registry_dispatch_unknown_tool` — 未知工具名返回错误
  - `test_registry_definitions_includes_all` — `definitions(false)` 返回所有工具
  - `test_registry_definitions_subagent_excludes_task` — `definitions(true)` 不含 task
  - `test_registry_check_permission_default_pass` — 默认权限检查放行
  - `test_registry_check_permission_override` — 覆写权限检查生效

- **单工具测试：**
  - 每个工具文件内的 `#[cfg(test)] mod tests` 验证 schema 正确性和 execute 行为

### 9.3 迁移测试

`dispatch_tool_tests` 改为使用 `ToolRegistry`，重命名为 `registry_dispatch_tests`，移入 `tools/registry.rs` 或 `tools/mod.rs` 的测试模块：

```rust
#[cfg(test)]
mod registry_dispatch_tests {
    use super::*;
    use serde_json::json;

    fn test_registry() -> ToolRegistry { build_registry() }

    #[tokio::test]
    async fn test_error_prefix_on_read_file_error() {
        let registry = test_registry();
        let ctx = ToolContext { client: /* mock */, hooks: /* mock */ };
        let result = registry.dispatch(&ctx, "read_file", &json!({"path": "nonexistent.txt"})).await;
        assert!(result.starts_with("[ERROR:read_file]"));
    }

    #[tokio::test]
    async fn test_error_prefix_on_unknown_tool() {
        let registry = test_registry();
        let ctx = ToolContext { client: /* mock */, hooks: /* mock */ };
        let result = registry.dispatch(&ctx, "foo_bar", &json!({})).await;
        assert_eq!(result, "[ERROR:unknown] Unknown tool: foo_bar");
    }
}
```

**测试迁移总表：**

| 原测试模块 | 目标位置 | 变更 |
|-----------|---------|------|
| `dispatch_tool_tests` | `tools/mod.rs` 或 `tools/registry.rs` | 改用 `registry.dispatch()`，改 `#[tokio::test]` |
| `glob_match_tests` | `tools/mod.rs`（共享函数测试） | import 路径调整 |
| `run_bash_tests` | `tools/mod.rs`（共享函数测试） | import 路径调整 |
| `safe_path_tests` | `tools/mod.rs`（共享函数测试） | import 路径调整 |
| `test_tool` (test_glob) | `tools/glob.rs` | 改为测试 GlobTool::execute |

---

## 10. Migration Plan

### Phase 1: 基础设施
1. 添加 `async-trait` 依赖到 Cargo.toml
2. 创建 `tools/mod.rs`、`tools/trait_def.rs`、`tools/registry.rs`
3. 将共享工具函数和路径安全函数从 `tools.rs` 移入 `tools/mod.rs`
4. 将 `escapes_workspace` / `normalize` 从 `permission.rs` 移入 `tools/mod.rs`，**重命名为 `escapes_workspace_lexical`**（区分于 `safe_path` 的 canonical 检查；此函数仅做词法归一化，用于权限闸门 2 的启发式检查）

### Phase 2: 逐工具迁移
对每个工具：
1. 创建 `tools/<name>.rs`，实现 `impl Tool for <Name>Tool`
2. 在 `tools/mod.rs` 注册
3. 从 `dispatch_tool` match 和 `get_base_tool_definitions` 中删除对应条目
4. 运行测试确认行为不变

迁移顺序：command → read_file → write_file → edit_file → glob → load_skill → todo_write → task

### Phase 3: 集成点切换
1. 修改 `main.rs`：`build_registry()` + 传引用
2. 修改 `subagent.rs`：`registry.dispatch()` + `registry.definitions(true)`
3. 修改 `hooks.rs`：PreToolUse 签名加 `&ToolRegistry`
4. 修改 `permission.rs`：删除 `check_rules` match，改用 `registry.check_permission()`

### Phase 4: 清理
1. 删除 `dispatch_tool()` 函数
2. 删除 `get_base_tool_definitions()` / `get_tool_definitions()` / `get_subagent_tool_definitions()` / `get_task_tool_definition()`
3. 删除 `permission.rs::check_rules()`
4. 删除原 `tools.rs` 文件（已被 `tools/` 目录替代）
5. 更新所有 import 路径
6. 全量测试

---

## 11. Risks and Mitigations

| 风险 | 影响 | 缓解 |
|------|------|------|
| `async-trait` 引入 dyn dispatch 开销 | 工具调用本身不是热路径（LLM 调用才是），影响可忽略 | — |
| ToolContext 生命周期管理 | `'a` 生命周期可能导致借用检查复杂 | ToolContext 仅借引用，不拥有；registry 独立于 context |
| Hooks 签名变更影响其他回调 | 仅 PreToolUse 签名变更，其他不变 | 只改一个 type alias |
| 测试迁移可能遗漏 | 现有测试可能 import 已删除的函数 | Phase 4 清理前确保所有测试通过 |
| `escapes_workspace` 移模块后可见性 | permission.rs 和 tools/ 都需要用 | 放在 `tools/mod.rs` 里 `pub(crate)` |

---

## 12. Success Criteria

- [ ] `dispatch_tool()` match 不再存在
- [ ] `get_*_tool_definitions()` 函数不再存在
- [ ] `permission::check_rules()` match 不再存在
- [ ] `main.rs::execute_tool()` 不再有 task 特殊路径
- [ ] 新增工具只需 3 步（新文件 + mod + 注册一行）
- [ ] 所有现有测试通过
- [ ] `cargo build` 零 warning
