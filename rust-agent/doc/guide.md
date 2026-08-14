# Rust Agent 开发指南

> 一个工具 + 一个循环 = 一个 Agent。

语言模型能推理代码，却碰不到真实世界——读不了文件、跑不了测试、看不见报错。如果没有循环，每次工具调用都得由人手动把结果粘回对话，你自己就成了那个循环。Agent 的本质，就是把"人肉循环"交给程序去做。

---

## 1. 核心循环结构

### 基本原理

整个 Agent 是一个闭合回路：

```
User prompt → LLM → 工具执行 → tool_result 喂回 LLM → 循环，直到 stop_reason != "tool_use"
```

用户输入进入模型，模型决定调用工具，工具在真实世界执行，结果被送回模型，模型据此决定下一步。回路持续运转，直到模型不再需要工具为止。

### 消息累积

循环的每一步都在向同一个消息列表追加内容，从不覆盖历史：

1. 用户输入是第一条 `user` 消息
2. 模型的回复作为 `assistant` 消息追加（可能含多个 `tool_use` 块）
3. 工具执行的结果作为新的 `user` 消息追加，每个结果用 `tool_use_id` 与当初的调用一一配对

下一轮请求时，模型能看到完整的来龙去脉：原始问题、自己之前的判断、工具返回的事实。这种累积式上下文，正是 Agent 能做多步推理的基础。

### 最简实现

```python
def agent_loop(query):
    messages = [{"role": "user", "content": query}]
    while True:
        response = client.messages.create(
            model=MODEL, system=SYSTEM, messages=messages,
            tools=TOOLS, max_tokens=8000,
        )
        messages.append({"role": "assistant", "content": response.content})
        if response.stop_reason != "tool_use":
            return
        results = []
        for block in response.content:
            if block.type == "tool_use":
                output = run_bash(block.input["command"])
                results.append({"type": "tool_result",
                                "tool_use_id": block.id, "content": output})
        messages.append({"role": "user", "content": results})
```

---

## 2. 多工具分发

### Dispatch Map

加一个工具，只加一个 handler——循环不用动，新工具注册进 dispatch map 就行。

核心机制是一张"工具名 → 处理函数"的映射表：

```python
TOOL_HANDLERS = {
    "bash":       lambda **kw: run_bash(kw["command"]),
    "read_file":  lambda **kw: run_read(kw["path"], kw.get("limit")),
    "write_file": lambda **kw: run_write(kw["path"], kw["content"]),
    "edit_file":  lambda **kw: run_edit(kw["path"], kw["old_text"], kw["new_text"]),
}
```

循环中用查表代替 if/elif：

```python
for block in response.content:
    if block.type == "tool_use":
        handler = TOOL_HANDLERS.get(block.name)
        output = handler(**block.input) if handler \
                 else f"Unknown tool: {block.name}"
        results.append({"type": "tool_result",
                         "tool_use_id": block.id, "content": output})
```

查不到名字也不崩——返回 `Unknown tool` 当作工具结果喂回去，模型下一轮自己会换别的办法。

### 路径沙箱

文件类工具先过一道 `safe_path`，把相对路径解析成绝对路径，确认其仍落在工作目录之内：

```python
def safe_path(p: str) -> Path:
    path = (WORKDIR / p).resolve()
    if not path.is_relative_to(WORKDIR):
        raise ValueError(f"Path escapes workspace: {p}")
    return path
```

这样无论模型怎么构造路径，文件操作都被锁死在 workspace 里。

---

## 3. 权限控制

### 三道闸门

工具执行前先过三道闸门：

| 闸门 | 作用 | 命中后 |
|------|------|--------|
| 1. 拒绝列表 | 永远禁止(`rm -rf /`、`sudo`) | 直接拒绝，不执行 |
| 2. 规则匹配 | 取决于上下文（写工作区外、`rm` 文件） | 交给闸门 3 |
| 3. 用户审批 | 闸门 2 命中后暂停等确认 | 用户决定 |

三道都没命中 → 放行。大部分日常操作走这条路。

```rust
pub fn check_permission(name: &str, input: &serde_json::Value) -> bool {
    // 闸门 1: 硬拒绝
    if name == "bash" {
        if let Some(p) = check_deny_list(/* command */) {
            return false;
        }
    }
    // 闸门 2 + 3: 规则命中 → 问用户
    if let Some(reason) = check_rules(name, input) {
        if !ask_user(name, input, reason) {
            return false;
        }
    }
    true
}
```

循环中只需一行判断：

```rust
let output = if permission::check_permission(name, input) {
    dispatch_tool(name, input)
} else {
    "Permission denied.".to_string()
};
```

### 安全架构

把拒绝逻辑从 `run_bash` 里拎出来，放到执行前的闸门——工具只管执行，安全面集中在一处。文件工具仍由 `tools::safe_path` 做工作区沙箱（defense in depth）。

---

## 4. Hooks 系统

### 扩展问题

权限检查硬编码在循环体里，每加一个检查都得改循环。循环该是稳定核心，扩展应挂在外面。

### 四个事件

| 事件 | 触发时机 | 典型用途 | 返回值影响 |
|------|---------|---------|-----------|
| UserPromptSubmit | 用户输入后、进 LLM 前 | 注入上下文、输入校验 | 不参与控制流 |
| PreToolUse | 工具执行前 | 权限检查、日志记录 | `Some(reason)` → 阻止执行 |
| PostToolUse | 工具执行后 | 副作用（自动 git add）、输出检查 | 不参与控制流 |
| Stop | 循环即将退出时 | 收尾统计、决定是否继续 | `Some(msg)` → 注入并继续循环 |

### 注册表

```rust
pub struct Hooks {
    user_prompt: Vec<fn(&str)>,
    pre_tool:    Vec<fn(&str, &serde_json::Value) -> Option<String>>,
    post_tool:   Vec<fn(&str, &serde_json::Value, &str)>,
    stop:        Vec<fn(&[Message]) -> Option<String>>,
}

impl Hooks {
    pub fn trigger_pre_tool(&self, name: &str, input: &serde_json::Value) -> Option<String> {
        for f in &self.pre_tool {
            if let Some(reason) = f(name, input) { return Some(reason); }
        }
        None
    }
}
```

回调用裸 `fn` 指针（`Copy`、零开销），免去 `Box<dyn Fn>` 的堆分配与 `Send/Sync` 约束。

### 循环集成

```rust
// PreToolUse
if let Some(reason) = hooks.trigger_pre_tool(name, input) {
    tool_results.push(ToolResult { tool_use_id: id, content: reason });
    continue;
}
let output = dispatch_tool(name, input);
hooks.trigger_post_tool(name, input, &output);

// Stop
if response.stop_reason != "tool_use" {
    if let Some(force) = hooks.trigger_stop(messages) {
        messages.push(user_text(force));
        continue;
    }
    break;
}

// UserPromptSubmit
hooks.trigger_prompt(query);
```

---

## 演进脉络

| 版本 | 核心 | 变更 |
|------|------|------|
| s01 | 单工具循环 | 基础消息累积机制 |
| s02 | 多工具分发 | dispatch map + 路径沙箱 |
| s03 | 权限控制 | 三道闸门，执行前判断 |
| s04 | Hooks 系统 | 扩展逻辑外挂，四个事件 |

循环本身的形状从不改变：依旧是"调用模型 → 追加响应 → 执行工具 → 喂回结果"。理解了这 30 行，就理解了 Agent 的骨架；剩下的，都是在往骨架上加肌肉。

> **设计原则**：
> - 循环不变：只增 handler 和 schema
> - 安全前置：检查发生在执行之前
> - 扩展外挂：通过 Hooks 挂载，不侵入核心