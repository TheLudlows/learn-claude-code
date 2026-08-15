# rust-agent

Rust 版本的 Agent 实现，整合 s01-s05 核心机制。

---

## 概览

```
s01 (Agent Loop)   -- 核心循环: LLM <-> Tools
s02 (Tool Use)     -- 工具分发: match 分支 + 路径沙箱
s03 (Permission)   -- 权限闸门: 危险命令拦截
s04 (Hooks)        -- 扩展点: Pre/Post/Stop/Prompt
s05 (TodoWrite)    -- 规划: 任务列表 + Nag 提醒
```

---

## s01: Agent Loop (Agent 循环)

**核心洞察**: "One loop & Bash is all you need" —— 一个循环 + 工具 = Agent

### 问题

语言模型能推理代码，但碰不到真实世界——不能读文件、跑测试、看报错。没有循环，每次工具调用你都得手动把结果粘回去。

### 解决方案

```
User prompt -> LLM -> Tools -> tool_result -> LLM -> ...
                              (loop until stop_reason != "tool_use")
```

### 关键代码

```rust
async fn agent_loop(client: &Client, system: &str, messages: &mut Vec<Message>) {
    loop {
        let response = client.stream_messages(system, messages, &tools, 8000).await?;
        messages.push(Message { role: "assistant", content: response.content.clone() });

        if response.stop_reason != "tool_use" {
            break;
        }

        let tool_results = execute_tools(&response.content);
        messages.push(Message { role: "user", content: tool_results });
    }
}
```

**关键点**: 循环本身保持不变，各章节只在其上叠加机制。

---

## s02: Tool Use (工具使用)

**核心洞察**: 加一个工具，只加一个 handler —— 循环不用动，新工具注册进 dispatch map 就行。

### 问题

只有 `bash` 时，所有操作都走 shell。`cat` 截断不可预测，`sed` 遇到特殊字符就崩，每次 bash 调用都是不受约束的安全面。

### 解决方案

dispatch map 将工具名映射到处理函数。加工具 = 加 handler + 加 schema。

### 关键代码

```rust
pub fn dispatch_tool(tool_name: &str, input: &Value) -> String {
    match tool_name {
        "command" => run_bash(cmd),
        "read_file" => run_read_file(path, limit),
        "write_file" => run_write_file(path, content),
        "edit_file" => run_edit_file(path, old_text, new_text),
        "glob" => run_glob(pattern),
        "todo_write" => crate::todo::run_todo_write(todos),
        _ => format!("[ERROR:unknown] Unknown tool: {}", tool_name),
    }
}
```

### 路径沙箱

```rust
fn safe_path(path_str: &str) -> Result<PathBuf, String> {
    let abs_path = workdir().join(path_str).canonicalize()?;
    if !abs_path.starts_with(&workdir().canonicalize()?) {
        return Err("Error: path escapes workspace".into());
    }
    Ok(abs_path)
}
```

**关键点**: 专用工具 (`read_file`, `write_file`) 可以在工具层面做路径沙箱。循环永远不变。

---

## s03: Permission (权限闸门)

**核心洞察**: 工具执行前设置闸门，拦截危险命令。

### 三道闸门

1. **rm 命令检查**: 防止 `rm -rf` 误删
2. **命令长度限制**: 防止超长命令注入
3. **可疑模式**: 检测 `sudo`、`wget` 等关键词

### 关键代码

```rust
pub fn permission_hook(name: &str, input: &Value) -> Option<String> {
    if name == "command" {
        let cmd = input["command"].as_str().unwrap_or("");
        if cmd.contains("rm -rf") {
            return Some("[ERROR:command] Dangerous command blocked".into());
        }
    }
    None
}
```

**注意**: s04 之后，权限检查通过 `hooks.on_pre_tool(permission_hook)` 注册。

---

## s04: Hooks (钩子系统)

**核心洞察**: 循环不把扩展逻辑写进体内，而是在固定节点触发回调。

### 四个扩展点

```
User prompt -> UserPromptSubmit (trigger_prompt)
     v
LLM response -> stop_reason check -> Stop (trigger_stop)
     v
tool_result -> PreToolUse (trigger_pre_tool) -> dispatch_tool -> PostToolUse (trigger_post_tool)
```

### 返回值语义

- **PreToolUse**: 返回 `Some(reason)` -> 阻止工具，reason 当 tool_result
- **Stop**: 返回 `Some(msg)` -> 注入 msg 并继续循环，不退出
- **UserPromptSubmit / PostToolUse**: 返回值不参与控制流

### 关键代码

```rust
pub type PreToolHook = fn(&str, &Value) -> Option<String>;

pub fn trigger_pre_tool(&self, name: &str, input: &Value) -> Option<String> {
    for f in &self.pre_tool {
        if let Some(reason) = f(name, input) {
            return Some(reason);  // 第一个 Some 短路
        }
    }
    None
}
```

**关键点**: 用裸 `fn` 指针实现零开销回调，免去 `Box<dyn Fn>` 的堆分配。

---

## s05: TodoWrite (待办写入)

**核心洞察**: 没有计划的 agent 走哪算哪 —— 先列步骤再动手，完成率翻倍。

### 问题

多步任务中，模型会丢失进度——重复做过的事、跳步、跑偏。对话越长越严重：工具结果不断填满上下文，系统提示的影响力逐渐被稀释。

### 解决方案

1. **TodoManager** 存储带状态的项目。同一时间只允许一个 `in_progress`。
2. **Nag reminder**: 模型连续 3 轮以上不调用 `todo_write` 时注入提醒。

### 关键代码

```rust
pub fn update(&mut self, todos: &Value) -> Result<String, String> {
    let mut in_progress_count = 0;
    for item in todos.as_array()? {
        if item["status"] == "in_progress" {
            in_progress_count += 1;
        }
    }
    if in_progress_count > 1 {
        return Err("Only one todo can be in_progress at a time".into());
    }
    self.items = validated;
    Ok(self.render())
}
```

### Nag 提醒

```rust
static ROUNDS_SINCE_TODO: AtomicUsize = AtomicUsize::new(0);

pub fn todo_reminder_hook(name: &str, ...) -> Option<String> {
    if name == "todo_write" {
        ROUNDS_SINCE_TODO.store(0, Ordering::SeqCst);
        None
    } else if count >= 3 {
        Some("<reminder>Update your todos.</reminder>".into())
    } else {
        None
    }
}
```

**关键点**: "同时只能有一个 in_progress" 强制顺序聚焦。Nag reminder 制造问责压力——你不更新计划，系统就追着你问。

---

## 安装

```bash
cargo build --release
```

## 配置

```bash
cp .env.example .env
# 编辑 .env 填入 ANTHROPIC_API_KEY 和 MODEL_ID
```

## 运行

```bash
cargo run --release
# 或
./target/release/rust-agent
```

## 示例提示词

1. `Read the file README.md and tell me what this project is about`
2. `Create a file called test.py that prints "hello", then read it back`
3. `Find all Python files in this directory`
4. `Refactor the file hello.py: add type hints, docstrings, and a main guard` (测试 TodoWrite)
5. `List all Python files and create a summary of what each does` (测试 Hooks)