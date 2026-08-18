# Hook 模块重构 Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** 把 `hooks.rs` 的回调抽象从裸 `fn` 指针改为 `Hook` trait (`Box<dyn Trait>`),并做机制/策略分离 —— 注册表机制留在 `hooks.rs`,5 个内置钩子集中到新 `builtins.rs`,`permission.rs` 并入后删除。

**Architecture:** 分三个 green-commit 阶段。Task 1 原地完成抽象切换(traits + `Box<dyn>` 注册表 + 5 个钩子改结构体,文件布局不变)。Task 2 把 4 个非权限内置钩子迁到 `builtins.rs`。Task 3 把 `PermissionHook` 及其辅助迁入 `builtins.rs` 并删除 `permission.rs`。每阶段结束 `cargo build` + `cargo test` 全绿才提交。

**Tech Stack:** Rust 2021 edition,`async-trait`(本重构不涉及),`serde_json`,无新依赖。

**Spec:** `docs/superpowers/specs/2026-08-18-hook-refactor-design.md`

---

## 重要约定(执行前必读)

1. **"不用 fn" 的精确范围**:仅指 *钩子回调*(注册进 `Hooks` 的 4 类回调)改用 trait + 结构体实现。模块内部辅助函数(`check_deny_list`、`ask_user`)仍是普通 `fn` —— 它们不是钩子,不在禁用范围。
2. **不加 blanket impl**:不要写 `impl<F: Fn(...)> PromptHook for F`。否则从后门把 fn 放回,违背本次重构意图。
3. **commit 只 stage 本计划列出的文件**:用户工作区有未提交的 `compact.rs` / `read_file.rs`(本重构不碰)。每个 commit 命令都用显式 `git add <列出的文件>`,不要用 `git add -A` 或 `git add .`。
4. **当前分支 `main`**:沿用仓库既有工作流(近期 commit 都在 main)。若你倾向开分支,在动手前提出。
5. **TDD 适配**:这是行为保持型重构,现有测试即安全网。每个 task 的纪律是"改完 `cargo test` 仍全绿"。不为内置钩子新增单测(spec 明确排除)。
6. **已知既有测试缺口**:`todo_reminder_hook` 的计数器逻辑原本就没有单测覆盖(只有 `assemble_post_tool_messages` 的测试用字面量 reminder 串)。重构后仍是既有状态,不补测试 —— 但执行者需知道这里安全网较薄。

---

## File Structure(最终态)

| 文件 | 职责 |
|---|---|
| `src/hooks.rs` | 机制层:4 个 hook trait + `Hooks` 注册表 + 4 个 `trigger_*` + `assemble_post_tool_messages` + 注册表测试 |
| `src/builtins.rs` | 策略层:5 个内置钩子结构体(`ContextInjectHook`/`LargeOutputHook`/`SummaryHook`/`TodoReminderHook`/`PermissionHook`)+ `DENY_LIST`/`check_deny_list`/`ask_user` + 内置钩子测试 |
| `src/permission.rs` | **删除** |
| `src/lib.rs` | 模块声明:加 `builtins`,删 `permission` |
| `src/main.rs` | import + 注册代码改结构体 |
| `src/subagent.rs` | 不变(只用 `Hooks` + `assemble_post_tool_messages`) |
| `src/tools/registry.rs` / `trait_def.rs` | 不变(只用 `Hooks::new()` / `&Hooks`) |

---

## Task 1: 抽象切换(原地,文件布局不变)

把 `hooks.rs` 与 `permission.rs` 的回调从裸 `fn` 切到 trait + 结构体,`main.rs` 注册改结构体。改完编译通过、测试全绿。内置钩子此阶段仍留在各自原文件。

**Files:**
- Modify: `rust-agent/src/hooks.rs` (整体重写:traits + Hooks + 内置钩子结构体 + 测试)
- Modify: `rust-agent/src/permission.rs` (`permission_hook` fn → `PermissionHook` 结构体;测试调用改 `.on_pre_tool`)
- Modify: `rust-agent/src/main.rs:40-41, 236-241` (import + 注册)

### - [ ] Step 1: 重写 `rust-agent/src/hooks.rs`

把整个文件替换为以下内容(注意:4 个内置钩子此阶段仍留在此文件,Task 2 再迁出):

```rust
/*
hooks.rs - 钩子系统 (s04)

循环不把扩展逻辑写进体内, 而是在四个固定节点上触发回调:
  UserPromptSubmit  用户输入提交后、进入 LLM 前
  PreToolUse        工具执行前 (s03 的权限检查移到这里)
  PostToolUse       工具执行后
  Stop              循环即将退出时

返回值语义:
  PreToolUse  返回 Some(reason) -> 阻止本次工具, reason 直接当 tool_result
  PostToolUse 返回 Some(msg)    -> 由循环作为独立 user 消息注入（不覆盖 tool_result）
  Stop        返回 Some(msg)    -> 注入 msg 并继续循环, 不退出
  UserPromptSubmit 的返回值不参与控制流。

回调用 trait 对象 (Box<dyn HookTrait + Send + Sync>): 每个事件一个 trait,
钩子以结构体实现, 注册时装箱。相比裸 fn 指针多一次堆分配, 但换取了
钩子可携带 owned 状态 (如 TodoReminderHook 的计数器, 不再依赖 static 全局),
且 Send + Sync 超trait 保证 Box<dyn> 可跨 async 边界。循环只调 trigger_*,
具体逻辑全在回调里 —— 这正是 s04 的要点。
*/

use std::sync::atomic::{AtomicUsize, Ordering};

use crate::client::{ContentBlock, Message};
use crate::tools::registry::ToolRegistry;
use crate::tools::workdir;

// ---- 回调 trait ----
pub trait PromptHook: Send + Sync {
    fn on_prompt(&self, query: &str);
}
pub trait PreToolHook: Send + Sync {
    fn on_pre_tool(&self, registry: &ToolRegistry, name: &str, input: &serde_json::Value) -> Option<String>;
}
pub trait PostToolHook: Send + Sync {
    fn on_post_tool(&self, name: &str, input: &serde_json::Value, output: &str) -> Option<String>;
}
pub trait StopHook: Send + Sync {
    fn on_stop(&self, messages: &[Message]) -> Option<String>;
}

/// 钩子注册表: 事件 -> 回调列表。
#[derive(Default)]
pub struct Hooks {
    user_prompt: Vec<Box<dyn PromptHook>>,
    pre_tool: Vec<Box<dyn PreToolHook>>,
    post_tool: Vec<Box<dyn PostToolHook>>,
    stop: Vec<Box<dyn StopHook>>,
}

impl Hooks {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn on_prompt<H: PromptHook + 'static>(&mut self, h: H) {
        self.user_prompt.push(Box::new(h));
    }
    pub fn on_pre_tool<H: PreToolHook + 'static>(&mut self, h: H) {
        self.pre_tool.push(Box::new(h));
    }
    pub fn on_post_tool<H: PostToolHook + 'static>(&mut self, h: H) {
        self.post_tool.push(Box::new(h));
    }
    pub fn on_stop<H: StopHook + 'static>(&mut self, h: H) {
        self.stop.push(Box::new(h));
    }

    /// 用户输入后、进入 LLM 前触发。返回值不参与控制流。
    pub fn trigger_prompt(&self, query: &str) {
        for f in &self.user_prompt {
            f.on_prompt(query);
        }
    }

    /// 工具执行前触发。第一个返回 Some(reason) 的回调短路 -> 该工具被拦截。
    pub fn trigger_pre_tool(&self, registry: &ToolRegistry, name: &str, input: &serde_json::Value) -> Option<String> {
        for f in &self.pre_tool {
            if let Some(reason) = f.on_pre_tool(registry, name, input) {
                return Some(reason);
            }
        }
        None
    }

    /// 工具执行后触发。返回 Some(msg) -> 由调用方作为独立 user 消息注入（不覆盖 tool_result）。
    pub fn trigger_post_tool(&self, name: &str, input: &serde_json::Value, output: &str) -> Option<String> {
        for f in &self.post_tool {
            if let Some(msg) = f.on_post_tool(name, input, output) {
                return Some(msg);
            }
        }
        None
    }

    /// 循环即将退出时触发。返回 Some(msg) -> 注入 msg 并继续, 不退出。
    pub fn trigger_stop(&self, messages: &[Message]) -> Option<String> {
        for f in &self.stop {
            if let Some(msg) = f.on_stop(messages) {
                return Some(msg);
            }
        }
        None
    }
}

/// 把本轮工具结果与 PostToolUse 提醒组装成要追加的 user 消息。
///
/// tool_result 始终是真实工具输出（不被提醒覆盖）；若 PostToolUse 返回了提醒，
/// 则作为独立 user 消息追加在后 —— 与 Stop 钩子（agent_loop / run_subagent_loop）
/// 的注入方式一致。
pub fn assemble_post_tool_messages(
    tool_results: Vec<ContentBlock>,
    reminders: Vec<String>,
) -> Vec<Message> {
    let mut out: Vec<Message> = Vec::new();

    if !tool_results.is_empty() {
        out.push(Message {
            role: "user".to_string(),
            content: tool_results,
        });
    }

    if !reminders.is_empty() {
        out.push(Message {
            role: "user".to_string(),
            content: reminders
                .into_iter()
                .map(|r| ContentBlock::Text { text: r })
                .collect(),
        });
    }

    // 兜底：两者皆空时（stop_reason 被报为 tool_use 但 content 里没有 ToolUse 块，
    // 且无 PostToolUse 提醒），仍要回喂一条非空 user 消息——否则 Anthropic API 会以
    // "content cannot be empty" 返回 400。
    if out.is_empty() {
        out.push(Message {
            role: "user".to_string(),
            content: vec![ContentBlock::Text {
                text: "(no tool calls to execute)".to_string(),
            }],
        });
    }

    out
}

// ---- 内置钩子 (Task 2 将迁至 builtins.rs; 权限检查见 permission::PermissionHook) ----

/// PostToolUse: 在 3 轮未使用 todo_write 时注入提醒。
/// 计数器为 owned 字段, 不再依赖 static 全局 —— 不同 Hooks 实例互不干扰。
pub struct TodoReminderHook {
    rounds_since_todo: AtomicUsize,
}

impl TodoReminderHook {
    pub fn new() -> Self {
        Self {
            rounds_since_todo: AtomicUsize::new(0),
        }
    }
}

impl Default for TodoReminderHook {
    fn default() -> Self {
        Self::new()
    }
}

impl PostToolHook for TodoReminderHook {
    fn on_post_tool(&self, name: &str, _input: &serde_json::Value, _output: &str) -> Option<String> {
        if name == "todo_write" {
            self.rounds_since_todo.store(0, Ordering::SeqCst);
            None
        } else {
            let count = self.rounds_since_todo.fetch_add(1, Ordering::SeqCst) + 1;
            if count >= 3 {
                self.rounds_since_todo.store(0, Ordering::SeqCst);
                Some("<reminder>Update your todos.</reminder>".to_string())
            } else {
                None
            }
        }
    }
}

/// UserPromptSubmit: 记录当前工作目录。
pub struct ContextInjectHook;

impl PromptHook for ContextInjectHook {
    fn on_prompt(&self, _query: &str) {
        println!(
            "\x1b[90m[HOOK] UserPromptSubmit: working in {}\x1b[0m",
            workdir().display()
        );
    }
}

/// PostToolUse: 输出过大时提醒。
pub struct LargeOutputHook;

impl PostToolHook for LargeOutputHook {
    fn on_post_tool(&self, name: &str, _input: &serde_json::Value, output: &str) -> Option<String> {
        if output.len() > 100_000 {
            println!(
                "\x1b[33m[HOOK] Large output from {}: {} chars\x1b[0m",
                name,
                output.len()
            );
        }
        None
    }
}

/// Stop: 收尾统计本轮用过的工具次数。
pub struct SummaryHook;

impl StopHook for SummaryHook {
    fn on_stop(&self, messages: &[Message]) -> Option<String> {
        let tool_count = messages
            .iter()
            .flat_map(|m| m.content.iter())
            .filter(|b| matches!(b, ContentBlock::ToolResult { .. }))
            .count();
        println!(
            "\x1b[90m[HOOK] Stop: session used {} tool calls\x1b[0m",
            tool_count
        );
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::client::Message;

    struct AlwaysBlock;
    impl PreToolHook for AlwaysBlock {
        fn on_pre_tool(&self, _r: &ToolRegistry, _n: &str, _i: &serde_json::Value) -> Option<String> {
            Some("nope".to_string())
        }
    }
    struct NeverBlock;
    impl PreToolHook for NeverBlock {
        fn on_pre_tool(&self, _r: &ToolRegistry, _n: &str, _i: &serde_json::Value) -> Option<String> {
            None
        }
    }
    struct PanicIfCalled;
    impl PreToolHook for PanicIfCalled {
        fn on_pre_tool(&self, _r: &ToolRegistry, _n: &str, _i: &serde_json::Value) -> Option<String> {
            panic!("second hook must not run after a block")
        }
    }

    #[test]
    fn empty_registry_allows() {
        let h = Hooks::new();
        let registry = ToolRegistry::new();
        assert!(h.trigger_pre_tool(&registry, "command", &serde_json::json!({})).is_none());
    }

    #[test]
    fn pre_tool_first_some_short_circuits() {
        let mut h = Hooks::new();
        h.on_pre_tool(AlwaysBlock);
        h.on_pre_tool(PanicIfCalled); // 没短路就会 panic
        let registry = ToolRegistry::new();
        assert_eq!(
            h.trigger_pre_tool(&registry, "command", &serde_json::json!({})),
            Some("nope".to_string())
        );
    }

    #[test]
    fn none_passes_through() {
        let mut h = Hooks::new();
        h.on_pre_tool(NeverBlock);
        h.on_pre_tool(NeverBlock);
        let registry = ToolRegistry::new();
        assert!(h.trigger_pre_tool(&registry, "command", &serde_json::json!({})).is_none());
    }

    #[test]
    fn post_tool_reminder_is_separate_user_message_not_tool_result() {
        let tool_results = vec![ContentBlock::ToolResult {
            tool_use_id: "t1".to_string(),
            content: "real command output".to_string(),
        }];
        let msgs = assemble_post_tool_messages(
            tool_results,
            vec!["<reminder>Update your todos.</reminder>".to_string()],
        );

        // 提醒必须是独立 user 消息，不能塞进 tool_result
        assert_eq!(
            msgs.len(),
            2,
            "reminder must be a separate user message, not folded into tool_result"
        );

        // tool_result 消息原样保留：仍是真实输出
        match &msgs[0].content[0] {
            ContentBlock::ToolResult { content, .. } => {
                assert_eq!(content, "real command output");
            }
            _ => panic!("first message must still hold the real tool_result"),
        }

        // 提醒是新增的 user 消息、Text 块（不是 tool_result）
        assert_eq!(msgs[1].role, "user");
        match &msgs[1].content[0] {
            ContentBlock::Text { text } => {
                assert_eq!(text, "<reminder>Update your todos.</reminder>");
            }
            _ => panic!("reminder must be a Text block, not a tool_result"),
        }
    }

    #[test]
    fn no_reminder_yields_single_tool_results_message() {
        let tool_results = vec![ContentBlock::ToolResult {
            tool_use_id: "t1".to_string(),
            content: "out".to_string(),
        }];
        let msgs = assemble_post_tool_messages(tool_results, vec![]);
        assert_eq!(msgs.len(), 1);
    }

    #[test]
    fn empty_results_and_no_reminder_yields_placeholder_message() {
        // C8 回归：stop_reason 被报为 tool_use 但无 ToolUse 块时，不能产生空 content
        // 消息（否则 Anthropic API 400 "content cannot be empty"）。
        let msgs = assemble_post_tool_messages(vec![], vec![]);
        assert_eq!(msgs.len(), 1);
        assert_eq!(msgs[0].role, "user");
        assert!(!msgs[0].content.is_empty(), "must not emit empty content");
        match &msgs[0].content[0] {
            ContentBlock::Text { text } => assert!(!text.is_empty()),
            _ => panic!("placeholder must be a Text block"),
        }
    }

    #[test]
    fn empty_results_with_reminder_yields_only_reminder_message() {
        // 无 tool_result 但有提醒：不应再额外塞一条空 tool_result 消息。
        let msgs = assemble_post_tool_messages(
            vec![],
            vec!["<reminder>Update your todos.</reminder>".to_string()],
        );
        assert_eq!(msgs.len(), 1);
        assert_eq!(msgs[0].role, "user");
        match &msgs[0].content[0] {
            ContentBlock::Text { text } => {
                assert_eq!(text, "<reminder>Update your todos.</reminder>");
            }
            _ => panic!("must be the reminder Text block"),
        }
    }

    #[test]
    fn stop_some_forces_continue() {
        struct Force;
        impl StopHook for Force {
            fn on_stop(&self, _m: &[Message]) -> Option<String> {
                Some("keep going".to_string())
            }
        }
        let mut h = Hooks::new();
        h.on_stop(Force);
        assert_eq!(h.trigger_stop(&[]), Some("keep going".to_string()));
    }
}
```

### - [ ] Step 2: 改写 `rust-agent/src/permission.rs`

把 `permission_hook` fn 改为 `PermissionHook` 结构体实现 `PreToolHook`。`DENY_LIST` / `check_deny_list` / `ask_user` 保持为模块私有 `fn`(它们是辅助,不是钩子)。文件头注释更新。整体替换为:

```rust
/*
permission.rs - 三道闸门权限管线 (s03 逻辑, s04 暴露为 PreToolUse 钩子)

工具执行前依次过三道闸门:
  1. 拒绝列表(rm -rf /、sudo…)         命中 -> 直接拒绝
  2. 工具权限检查(通过 registry.check_permission)  命中 -> 交给闸门 3
  3. 用户审批(暂停等 y/N)              用户决定
三道都没命中 -> 放行。

s04: 本模块的 PermissionHook 实现 PreToolUse trait, on_pre_tool 返回
Option<String>(Some=拦截理由, None=放行), 注册为 PreToolUse 钩子,
由 hooks.trigger_pre_tool() 触发。

注: 字符串匹配仅用于演示闸门位置, 非完整安全边界(见 s03 README)。
文件类工具另有 tools::safe_path 做工作区沙箱(defense in depth)。
*/

use crate::hooks::PreToolHook;
use crate::tools::registry::ToolRegistry;
use crate::tools::trait_def::PermissionCheck;
use std::io::{self, Write};

/// 闸门 1: 硬拒绝列表 —— 永远禁止
const DENY_LIST: &[&str] = &[
    "rm -rf /", "sudo", "shutdown", "reboot", "mkfs", "dd if=", "> /dev/sda",
];

fn check_deny_list(command: &str) -> Option<&'static str> {
    // 与 command.rs::check_permission 对齐：命令先转小写再匹配。
    // DENY_LIST 条目均为小写，不转小写会让 "Sudo" / "RM -rf /" 绕过闸门 1。
    let command_lower = command.to_lowercase();
    DENY_LIST.iter().copied().find(|p| command_lower.contains(p))
}

/// 闸门 3: 暂停等用户确认
fn ask_user(name: &str, input: &serde_json::Value, reason: &str) -> bool {
    println!("\n\x1b[33m[permission] {}\x1b[0m", reason);
    println!("   Tool: {}({})", name, input);
    print!("   Allow? [y/N] ");
    io::stdout().flush().ok();
    let mut line = String::new();
    io::stdin().read_line(&mut line).ok();
    matches!(line.trim().to_lowercase().as_str(), "y" | "yes")
}

/// PreToolUse 钩子: 三道闸门串联, 返回 Some(reason) 表示拦截, None 表示放行。
///
/// 循环经 `hooks.trigger_pre_tool()` 调用; 末尾返回 None(而非 false),
/// 才符合 "三道都没命中 -> 放行" 的语义 —— 这也修掉了 s03 check_permission
/// 末尾 `return false` 把所有工具都拒掉的 bug。
pub struct PermissionHook;

impl PreToolHook for PermissionHook {
    fn on_pre_tool(&self, registry: &ToolRegistry, name: &str, input: &serde_json::Value) -> Option<String> {
        // 闸门 1: 硬拒绝
        if name == "command" {
            let cmd = input.get("command").and_then(|v| v.as_str()).unwrap_or("");
            if let Some(p) = check_deny_list(cmd) {
                println!("\n\x1b[31m[blocked] '{}' is on the deny list\x1b[0m", p);
                return Some(format!("Permission denied: '{}' on deny list", p));
            }
        }

        // 闸门 2: 使用 registry 检查工具权限
        if let Some(permission_check) = registry.check_permission(name, input) {
            match permission_check {
                PermissionCheck::Pass => {
                    // 通过权限检查，继续执行
                }
                PermissionCheck::NeedsApproval(reason) => {
                    // 闸门 3: 向用户请求确认
                    if !ask_user(name, input, reason) {
                        return Some("Permission denied by user".to_string());
                    }
                }
            }
        }
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tools::build_registry;

    #[test]
    fn deny_list_matches() {
        assert_eq!(check_deny_list("sudo apt update"), Some("sudo"));
        assert!(check_deny_list("rm -rf /").is_some());
        assert!(check_deny_list("ls -la").is_none());
    }

    #[test]
    fn deny_list_case_insensitive() {
        // N4 回归：闸门 1 必须与 command.rs 对齐走 to_lowercase，
        // 否则 "Sudo" / "RM -rf /" 能绕过硬拒绝。
        assert_eq!(check_deny_list("Sudo apt update"), Some("sudo"));
        assert_eq!(check_deny_list("SUDO reboot"), Some("sudo"));
        assert!(check_deny_list("RM -rf /").is_some());
        assert!(check_deny_list("Reboot now").is_some());
    }

    #[test]
    fn permission_hook_allows_safe() {
        // 安全命令: 不进 deny list, registry 检查通过 -> 放行
        let registry = ToolRegistry::new();
        assert_eq!(
            PermissionHook.on_pre_tool(&registry, "command", &serde_json::json!({"command": "ls"})),
            None
        );
    }

    #[test]
    fn permission_hook_blocks_deny_list() {
        // 命中闸门 1, 直接拦截(且不读 stdin)
        let registry = ToolRegistry::new();
        assert_eq!(
            PermissionHook.on_pre_tool(&registry, "command", &serde_json::json!({"command": "sudo apt update"})),
            Some("Permission denied: 'sudo' on deny list".to_string())
        );
    }

    #[test]
    fn permission_hook_uses_registry() {
        // 测试 permission_hook 确实调用了 registry.check_permission
        let registry = build_registry();

        // 对于 command 工具，默认权限检查应该通过
        let result = PermissionHook.on_pre_tool(&registry, "command", &serde_json::json!({"command": "ls"}));
        assert_eq!(result, None);
    }

    #[test]
    fn permission_hook_requires_approval() {
        // 使用 build_registry 创建一个包含所有工具的 registry
        let registry = build_registry();

        // 测试工具权限检查系统是否正常工作
        // 由于大多数工具默认不需要审批，我们可以测试这个系统
        let result = PermissionHook.on_pre_tool(
            &registry,
            "command",
            &serde_json::json!({"command": "ls"})
        );
        // command 工具不应该需要审批
        assert_eq!(result, None);
    }

    #[test]
    fn permission_hook_unknown_tool() {
        // 测试未知工具的处理
        let registry = ToolRegistry::new();
        let result = PermissionHook.on_pre_tool(
            &registry,
            "unknown_tool",
            &serde_json::json!({})
        );
        assert_eq!(result, None); // 未知工具直接放行
    }
}
```

### - [ ] Step 3: 更新 `rust-agent/src/main.rs` 的 import 与注册

**3a. 替换第 40–41 行的 import:**

旧:
```rust
use rust_agent::hooks::{assemble_post_tool_messages, context_inject_hook, large_output_hook, summary_hook, todo_reminder_hook, Hooks};
use rust_agent::permission::permission_hook;
```

新:
```rust
use rust_agent::hooks::{assemble_post_tool_messages, ContextInjectHook, Hooks, LargeOutputHook, SummaryHook, TodoReminderHook};
use rust_agent::permission::PermissionHook;
```

**3b. 替换第 236–241 行的注册块:**

旧:
```rust
    let mut hooks = Hooks::new();
    hooks.on_prompt(context_inject_hook);
    hooks.on_pre_tool(permission_hook); // s03 三道闸门, 搬成 PreToolUse 回调
    hooks.on_post_tool(large_output_hook);
    hooks.on_stop(summary_hook);
    hooks.on_post_tool(todo_reminder_hook);
```

新:
```rust
    let mut hooks = Hooks::new();
    hooks.on_prompt(ContextInjectHook);
    hooks.on_pre_tool(PermissionHook); // s03 三道闸门, 搬成 PreToolUse 回调
    hooks.on_post_tool(LargeOutputHook);
    hooks.on_stop(SummaryHook);
    hooks.on_post_tool(TodoReminderHook::new());
```

### - [ ] Step 4: 编译验证

Run: `cd rust-agent && cargo build`
Expected: 编译通过, 0 errors。若出现 `'static` 或 `Send/Sync` 相关错误, 检查钩子结构体是否含非 `Send+Sync` 字段(本计划的结构体均满足: unit struct 或仅 `AtomicUsize`)。

### - [ ] Step 5: 测试验证

Run: `cd rust-agent && cargo test`
Expected: 全部测试通过。关键确认:
- `hooks::tests::pre_tool_first_some_short_circuits` PASS(short-circuit 语义保持)
- `hooks::tests::stop_some_forces_continue` PASS
- `permission::tests::permission_hook_blocks_deny_list` PASS(闸门 1 仍拦截)
- 4 个 `assemble_post_tool_messages` 测试 PASS
- 总计 0 failures

### - [ ] Step 6: 提交

```bash
cd "D:\code\learn-claude-code"
git add rust-agent/src/hooks.rs rust-agent/src/permission.rs rust-agent/src/main.rs
git commit -m "refactor(hooks): 回调抽象从裸 fn 切到 Hook trait (Box<dyn Trait>)

- hooks.rs: 4 个 hook trait (Send+Sync) + Hooks 字段改 Vec<Box<dyn Trait>>
  + 泛型 on_*<H: Trait + 'static> 注册; 4 个内置钩子改结构体(TodoReminderHook
  计数器变 owned 字段, 消除 static 全局)
- permission.rs: permission_hook fn -> PermissionHook 结构体实现 PreToolHook
- main.rs: 注册改结构体
- 语义不变: short-circuit、Stop 注入续跑、assemble placeholder 兜底

Co-Authored-By: Claude <noreply@anthropic.com>"
```

---

## Task 2: 抽出 4 个非权限内置钩子到 `builtins.rs`

把 `ContextInjectHook` / `LargeOutputHook` / `SummaryHook` / `TodoReminderHook` 从 `hooks.rs` 迁到新 `builtins.rs`。`PermissionHook` 此阶段仍在 `permission.rs`。

**Files:**
- Create: `rust-agent/src/builtins.rs`
- Modify: `rust-agent/src/hooks.rs` (删除 4 个内置钩子结构体 + 不再需要的 `workdir`/`atomic` import)
- Modify: `rust-agent/src/lib.rs` (加 `pub mod builtins;`)
- Modify: `rust-agent/src/main.rs` (4 个内置钩子改从 `builtins` import)

### - [ ] Step 1: 创建 `rust-agent/src/builtins.rs`

```rust
/*
builtins.rs - 内置钩子集合

main.rs 默认注册的钩子集中于此(权限检查 PermissionHook 在 Task 3 迁入):
  ContextInjectHook  UserPromptSubmit  记录工作目录
  LargeOutputHook    PostToolUse       输出过大提醒
  SummaryHook        Stop              收尾工具次数统计
  TodoReminderHook   PostToolUse       3 轮未 todo_write 时提醒

均实现 hooks.rs 中对应 trait, 通过 hooks.on_* 注册。
*/

use std::sync::atomic::{AtomicUsize, Ordering};

use crate::client::{ContentBlock, Message};
use crate::hooks::{PostToolHook, PromptHook, StopHook};
use crate::tools::workdir;

/// PostToolUse: 在 3 轮未使用 todo_write 时注入提醒。
/// 计数器为 owned 字段, 不再依赖 static 全局 —— 不同 Hooks 实例互不干扰。
pub struct TodoReminderHook {
    rounds_since_todo: AtomicUsize,
}

impl TodoReminderHook {
    pub fn new() -> Self {
        Self {
            rounds_since_todo: AtomicUsize::new(0),
        }
    }
}

impl Default for TodoReminderHook {
    fn default() -> Self {
        Self::new()
    }
}

impl PostToolHook for TodoReminderHook {
    fn on_post_tool(&self, name: &str, _input: &serde_json::Value, _output: &str) -> Option<String> {
        if name == "todo_write" {
            self.rounds_since_todo.store(0, Ordering::SeqCst);
            None
        } else {
            let count = self.rounds_since_todo.fetch_add(1, Ordering::SeqCst) + 1;
            if count >= 3 {
                self.rounds_since_todo.store(0, Ordering::SeqCst);
                Some("<reminder>Update your todos.</reminder>".to_string())
            } else {
                None
            }
        }
    }
}

/// UserPromptSubmit: 记录当前工作目录。
pub struct ContextInjectHook;

impl PromptHook for ContextInjectHook {
    fn on_prompt(&self, _query: &str) {
        println!(
            "\x1b[90m[HOOK] UserPromptSubmit: working in {}\x1b[0m",
            workdir().display()
        );
    }
}

/// PostToolUse: 输出过大时提醒。
pub struct LargeOutputHook;

impl PostToolHook for LargeOutputHook {
    fn on_post_tool(&self, name: &str, _input: &serde_json::Value, output: &str) -> Option<String> {
        if output.len() > 100_000 {
            println!(
                "\x1b[33m[HOOK] Large output from {}: {} chars\x1b[0m",
                name,
                output.len()
            );
        }
        None
    }
}

/// Stop: 收尾统计本轮用过的工具次数。
pub struct SummaryHook;

impl StopHook for SummaryHook {
    fn on_stop(&self, messages: &[Message]) -> Option<String> {
        let tool_count = messages
            .iter()
            .flat_map(|m| m.content.iter())
            .filter(|b| matches!(b, ContentBlock::ToolResult { .. }))
            .count();
        println!(
            "\x1b[90m[HOOK] Stop: session used {} tool calls\x1b[0m",
            tool_count
        );
        None
    }
}
```

### - [ ] Step 2: 从 `rust-agent/src/hooks.rs` 删除已迁出的 4 个内置钩子

删除以下区块:
- `use std::sync::atomic::{AtomicUsize, Ordering};` (顶部 import)
- `use crate::tools::workdir;` (顶部 import)
- `// ---- 内置钩子 (Task 2 将迁至 builtins.rs; ...)` 注释及之后的 `TodoReminderHook` / `ContextInjectHook` / `LargeOutputHook` / `SummaryHook` 四个结构体及其 impl 块(到 `#[cfg(test)]` 之前为止)

删除后, `hooks.rs` 顶部 import 仅剩:
```rust
use crate::client::{ContentBlock, Message};
use crate::tools::registry::ToolRegistry;
```
文件尾(在 `assemble_post_tool_messages` 之后)直接接 `#[cfg(test)] mod tests`。

注意: tests 模块里的 `AlwaysBlock`/`NeverBlock`/`PanicIfCalled`/`Force` 是注册表测试用的 mock, **保留在 hooks.rs tests**, 不迁出。

### - [ ] Step 3: `rust-agent/src/lib.rs` 加模块声明

把:
```rust
pub mod client;
pub mod compact;
pub mod error;
pub mod hooks;
pub mod output;
pub mod permission;
pub mod skills;
pub mod subagent;
pub mod todo;
pub mod tools;
```
改为(在 `hooks` 后插入 `builtins`):
```rust
pub mod builtins;
pub mod client;
pub mod compact;
pub mod error;
pub mod hooks;
pub mod output;
pub mod permission;
pub mod skills;
pub mod subagent;
pub mod todo;
pub mod tools;
```

### - [ ] Step 4: `rust-agent/src/main.rs` import 改源

把 Task 1 Step 3a 写入的:
```rust
use rust_agent::hooks::{assemble_post_tool_messages, ContextInjectHook, Hooks, LargeOutputHook, SummaryHook, TodoReminderHook};
use rust_agent::permission::PermissionHook;
```
改为(4 个内置钩子从 `builtins` 来, `Hooks`/`assemble` 仍从 `hooks`):
```rust
use rust_agent::builtins::{ContextInjectHook, LargeOutputHook, SummaryHook, TodoReminderHook};
use rust_agent::hooks::{assemble_post_tool_messages, Hooks};
use rust_agent::permission::PermissionHook;
```

注册块(Task 1 写入的结构体调用)**不变**。

### - [ ] Step 5: 编译验证

Run: `cd rust-agent && cargo build`
Expected: 0 errors。若报 `cannot find type ContextInjectHook in hooks` 之类, 说明 Step 2 没删干净或 Step 4 import 没改对。

### - [ ] Step 6: 测试验证

Run: `cd rust-agent && cargo test`
Expected: 全绿, 0 failures。

### - [ ] Step 7: 提交

```bash
cd "D:\code\learn-claude-code"
git add rust-agent/src/builtins.rs rust-agent/src/hooks.rs rust-agent/src/lib.rs rust-agent/src/main.rs
git commit -m "refactor(hooks): 抽出 4 个内置钩子到 builtins.rs (机制/策略分离)

hooks.rs 只剩注册表机制 + assemble_post_tool_messages;
ContextInjectHook/LargeOutputHook/SummaryHook/TodoReminderHook 迁入 builtins.rs。

Co-Authored-By: Claude <noreply@anthropic.com>"
```

---

## Task 3: 迁入 `PermissionHook` 并删除 `permission.rs`

把 `PermissionHook` + `DENY_LIST` / `check_deny_list` / `ask_user` + 7 个测试从 `permission.rs` 迁入 `builtins.rs`,删除 `permission.rs`,`lib.rs` 去掉模块声明,`main.rs` 把 `PermissionHook` 的 import 源从 `permission` 改到 `builtins`。

**Files:**
- Modify: `rust-agent/src/builtins.rs` (追加 `PermissionHook` + 辅助 + tests)
- Delete: `rust-agent/src/permission.rs`
- Modify: `rust-agent/src/lib.rs` (删 `pub mod permission;`)
- Modify: `rust-agent/src/main.rs` (import 源切换)

### - [ ] Step 1: 把 `PermissionHook` 及辅助追加到 `rust-agent/src/builtins.rs`

在 `builtins.rs` 顶部 import 区追加 `PreToolHook` 与 `ToolRegistry` / `PermissionCheck` / `io`。把现有:
```rust
use crate::hooks::{PostToolHook, PromptHook, StopHook};
use crate::tools::workdir;
```
改为:
```rust
use std::io::{self, Write};

use crate::hooks::{PostToolHook, PreToolHook, PromptHook, StopHook};
use crate::tools::registry::ToolRegistry;
use crate::tools::trait_def::PermissionCheck;
use crate::tools::workdir;
```
(注意 `std::sync::atomic` 与 `crate::client` 已在文件顶部, 保留。)

在文件末尾(`SummaryHook` impl 之后)追加 `#[cfg(test)] mod tests` 之前的内置钩子本体:
```rust

// ---- 权限钩子 (原 permission.rs) ----

/// 闸门 1: 硬拒绝列表 —— 永远禁止
const DENY_LIST: &[&str] = &[
    "rm -rf /", "sudo", "shutdown", "reboot", "mkfs", "dd if=", "> /dev/sda",
];

fn check_deny_list(command: &str) -> Option<&'static str> {
    // 与 command.rs::check_permission 对齐：命令先转小写再匹配。
    // DENY_LIST 条目均为小写，不转小写会让 "Sudo" / "RM -rf /" 绕过闸门 1。
    let command_lower = command.to_lowercase();
    DENY_LIST.iter().copied().find(|p| command_lower.contains(p))
}

/// 闸门 3: 暂停等用户确认
fn ask_user(name: &str, input: &serde_json::Value, reason: &str) -> bool {
    println!("\n\x1b[33m[permission] {}\x1b[0m", reason);
    println!("   Tool: {}({})", name, input);
    print!("   Allow? [y/N] ");
    io::stdout().flush().ok();
    let mut line = String::new();
    io::stdin().read_line(&mut line).ok();
    matches!(line.trim().to_lowercase().as_str(), "y" | "yes")
}

/// PreToolUse 钩子: 三道闸门串联, 返回 Some(reason) 表示拦截, None 表示放行。
///
/// 循环经 `hooks.trigger_pre_tool()` 调用; 末尾返回 None(而非 false),
/// 才符合 "三道都没命中 -> 放行" 的语义 —— 这也修掉了 s03 check_permission
/// 末尾 `return false` 把所有工具都拒掉的 bug。
pub struct PermissionHook;

impl PreToolHook for PermissionHook {
    fn on_pre_tool(&self, registry: &ToolRegistry, name: &str, input: &serde_json::Value) -> Option<String> {
        // 闸门 1: 硬拒绝
        if name == "command" {
            let cmd = input.get("command").and_then(|v| v.as_str()).unwrap_or("");
            if let Some(p) = check_deny_list(cmd) {
                println!("\n\x1b[31m[blocked] '{}' is on the deny list\x1b[0m", p);
                return Some(format!("Permission denied: '{}' on deny list", p));
            }
        }

        // 闸门 2: 使用 registry 检查工具权限
        if let Some(permission_check) = registry.check_permission(name, input) {
            match permission_check {
                PermissionCheck::Pass => {
                    // 通过权限检查，继续执行
                }
                PermissionCheck::NeedsApproval(reason) => {
                    // 闸门 3: 向用户请求确认
                    if !ask_user(name, input, reason) {
                        return Some("Permission denied by user".to_string());
                    }
                }
            }
        }
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tools::build_registry;

    #[test]
    fn deny_list_matches() {
        assert_eq!(check_deny_list("sudo apt update"), Some("sudo"));
        assert!(check_deny_list("rm -rf /").is_some());
        assert!(check_deny_list("ls -la").is_none());
    }

    #[test]
    fn deny_list_case_insensitive() {
        // N4 回归：闸门 1 必须与 command.rs 对齐走 to_lowercase，
        // 否则 "Sudo" / "RM -rf /" 能绕过硬拒绝。
        assert_eq!(check_deny_list("Sudo apt update"), Some("sudo"));
        assert_eq!(check_deny_list("SUDO reboot"), Some("sudo"));
        assert!(check_deny_list("RM -rf /").is_some());
        assert!(check_deny_list("Reboot now").is_some());
    }

    #[test]
    fn permission_hook_allows_safe() {
        // 安全命令: 不进 deny list, registry 检查通过 -> 放行
        let registry = ToolRegistry::new();
        assert_eq!(
            PermissionHook.on_pre_tool(&registry, "command", &serde_json::json!({"command": "ls"})),
            None
        );
    }

    #[test]
    fn permission_hook_blocks_deny_list() {
        // 命中闸门 1, 直接拦截(且不读 stdin)
        let registry = ToolRegistry::new();
        assert_eq!(
            PermissionHook.on_pre_tool(&registry, "command", &serde_json::json!({"command": "sudo apt update"})),
            Some("Permission denied: 'sudo' on deny list".to_string())
        );
    }

    #[test]
    fn permission_hook_uses_registry() {
        // 测试 permission_hook 确实调用了 registry.check_permission
        let registry = build_registry();

        // 对于 command 工具，默认权限检查应该通过
        let result = PermissionHook.on_pre_tool(&registry, "command", &serde_json::json!({"command": "ls"}));
        assert_eq!(result, None);
    }

    #[test]
    fn permission_hook_requires_approval() {
        // 使用 build_registry 创建一个包含所有工具的 registry
        let registry = build_registry();

        // 测试工具权限检查系统是否正常工作
        // 由于大多数工具默认不需要审批，我们可以测试这个系统
        let result = PermissionHook.on_pre_tool(
            &registry,
            "command",
            &serde_json::json!({"command": "ls"})
        );
        // command 工具不应该需要审批
        assert_eq!(result, None);
    }

    #[test]
    fn permission_hook_unknown_tool() {
        // 测试未知工具的处理
        let registry = ToolRegistry::new();
        let result = PermissionHook.on_pre_tool(
            &registry,
            "unknown_tool",
            &serde_json::json!({})
        );
        assert_eq!(result, None); // 未知工具直接放行
    }
}
```

### - [ ] Step 2: 删除 `rust-agent/src/permission.rs`

```bash
cd "D:\code\learn-claude-code"
git rm rust-agent/src/permission.rs
```
(用 `git rm` 而非手动删, 让删除进入暂存区, 与本次 commit 一起记录。)

### - [ ] Step 3: `rust-agent/src/lib.rs` 去掉 `permission` 模块

把 Task 2 Step 3 写入的列表中的 `pub mod permission;` 这一行删除。最终 `lib.rs`:
```rust
pub mod builtins;
pub mod client;
pub mod compact;
pub mod error;
pub mod hooks;
pub mod output;
pub mod skills;
pub mod subagent;
pub mod todo;
pub mod tools;
```

### - [ ] Step 4: `rust-agent/src/main.rs` 把 `PermissionHook` 改从 `builtins` import

把 Task 2 Step 4 写入的:
```rust
use rust_agent::builtins::{ContextInjectHook, LargeOutputHook, SummaryHook, TodoReminderHook};
use rust_agent::hooks::{assemble_post_tool_messages, Hooks};
use rust_agent::permission::PermissionHook;
```
改为(5 个内置钩子全部来自 `builtins`, 删掉 `permission` 行):
```rust
use rust_agent::builtins::{ContextInjectHook, LargeOutputHook, PermissionHook, SummaryHook, TodoReminderHook};
use rust_agent::hooks::{assemble_post_tool_messages, Hooks};
```

注册块**不变**(仍是 `hooks.on_pre_tool(PermissionHook)`)。

### - [ ] Step 5: 编译验证

Run: `cd rust-agent && cargo build`
Expected: 0 errors。若报 `unresolved import rust_agent::permission`, 说明 main.rs 或别处仍有 `permission::` 引用 —— 检查并清除。

### - [ ] Step 6: 测试验证

Run: `cd rust-agent && cargo test`
Expected: 全绿, 0 failures。确认 `builtins::tests::permission_hook_blocks_deny_list` 等 7 个权限测试都 PASS(随文件迁入 builtins)。

### - [ ] Step 7: 提交

```bash
cd "D:\code\learn-claude-code"
git add rust-agent/src/builtins.rs rust-agent/src/lib.rs rust-agent/src/main.rs
# permission.rs 已由 git rm 暂存
git commit -m "refactor(hooks): 迁入 PermissionHook 到 builtins.rs, 删除 permission.rs

所有 5 个内置钩子现集中于 builtins.rs; permission.rs 内容(PermissionHook +
DENY_LIST/check_deny_list/ask_user + 7 测试)全部迁入; lib.rs 去掉 permission 模块;
main.rs 从 builtins 引入 PermissionHook。

Co-Authored-By: Claude <noreply@anthropic.com>"
```

---

## Task 4: 最终验证

全量确认重构后 crate 健康无回归。

**Files:** 无改动。

### - [ ] Step 1: 全量构建(含告警检查)

Run: `cd rust-agent && cargo build 2>&1`
Expected: `Finished` 无 error。留意 `unused import` 之类 warning —— 若 `hooks.rs` 残留未用的 `workdir`/`atomic` import(Task 2 应已删),此处会报;有则回去补删。

### - [ ] Step 2: 全量测试

Run: `cd rust-agent && cargo test 2>&1`
Expected: 全部 PASS, 0 failures。关键模块:
- `hooks::tests` — 8 个(3 PreTool + 4 assemble + 1 stop)
- `builtins::tests` — 7 个(权限相关, 自 permission.rs 迁入)
- `tools::registry::tests` — 3 个(确认 `Hooks::new()` 空 registry 派发不受影响)
- `tools::trait_def::tests` — 不受影响

### - [ ] Step 3: 确认无 `permission` 残留引用

Run: `cd rust-agent && grep -rn "permission::\|pub mod permission" src/`
Expected: 无输出(或仅 `permission` 作为 `PermissionCheck`/`permission_hook` 测试名等子串出现, 但不应有 `rust_agent::permission::` 路径或 `pub mod permission;`)。若有路径残留, 清除之。

### - [ ] Step 4: 确认无裸 fn 钩子注册残留

Run: `cd rust-agent && grep -n "on_prompt\|on_pre_tool\|on_post_tool\|on_stop" src/main.rs`
Expected: 5 行, 全部传入结构体值(`ContextInjectHook` / `PermissionHook` / `LargeOutputHook` / `SummaryHook` / `TodoReminderHook::new()`), 无 fn 标识符。

### - [ ] Step 5: 最终状态汇报

向用户报告:
- 三个 commit 的 hash 与标题
- `cargo test` 通过的测试数
- `permission.rs` 已删除
- 既有测试缺口提示:`TodoReminderHook` 计数器逻辑仍无单测覆盖(spec 排除新增),如需补可后续单独提。

---

## Self-Review(计划作者自检记录)

- **Spec 覆盖**: §3.1 traits/Hooks/trigger/assemble → Task 1; §3.2 builtins 5 structs → Task 2(4 个)+ Task 3(PermissionHook); §3.3 删 permission.rs → Task 3; §4 lib/main/hooks tests/permission tests → Tasks 1–3; §7 cargo build+test → 每 task Step + Task 4。无遗漏。
- **占位符扫描**: 无 TBD/TODO, 每个代码步骤含完整代码。
- **类型一致**: trait 方法名 `on_prompt`/`on_pre_tool`/`on_post_tool`/`on_stop` 全程一致; 结构体名 `ContextInjectHook`/`LargeOutputHook`/`SummaryHook`/`TodoReminderHook`/`PermissionHook` 跨 task 一致; `TodoReminderHook::new()` 一致。
- **非目标遵守**: 无 blanket impl、无新测试、无 `hooks/` 子目录。
