# Hook 模块重构设计

- 日期: 2026-08-18
- 范围: `rust-agent/src/hooks.rs` 及其连带模块
- 目标: 机制与策略分离 —— 把钩子注册表机制与具体钩子策略拆开,并把回调抽象从裸 `fn` 指针改为 `Hook` trait (`Box<dyn Trait>`)

## 1. 背景与动机

`hooks.rs` 当前(330 行)混了五件事:注册表结构 `Hooks` + 4 个 `trigger_*`、消息组装函数 `assemble_post_tool_messages`、带全局可变状态的 `todo_reminder_hook`(`static AtomicUsize`)、三个示例钩子(`context_inject_hook` / `large_output_hook` / `summary_hook`)、测试。

`permission_hook` 已经是独立 peer 模块 `permission.rs` 里的 PreToolUse 钩子,但其余四个内置钩子仍堆在 `hooks.rs` 里,机制与策略没分开。

回调抽象方面,文件头明确记录了"裸 `fn` 指针,零开销,免 `Box<dyn Fn>` 的堆分配与 Send/Sync 约束"。本次重构按用户指示推翻该决策,改用 trait,顺带消除 `todo_reminder_hook` 的 `static` 全局计数器(改为 owned 实例状态)。

## 2. 目标与非目标

### 目标
- `hooks.rs` 只保留机制:4 个 hook trait、`Hooks` 注册表、4 个 `trigger_*`、`assemble_post_tool_messages`。
- 5 个内置钩子(含 `permission_hook`)集中到新 `builtins.rs`,全部改为实现对应 trait 的结构体。
- `todo_reminder_hook` 的 `static ROUNDS_SINCE_TODO` 消失,计数器变为 `TodoReminderHook` 的 owned `AtomicUsize` 字段。
- 保留所有现有语义:4 事件的触发时机、short-circuit(`PreToolUse`/`PostToolUse`/`Stop` 第一个 `Some` 短路)、`assemble_post_tool_messages` 的 placeholder 兜底。

### 非目标
- 不引入统一 `Hook` trait + `Event` 枚举(过度设计)。
- 不加 blanket `impl Trait for Fn`(否则从后门把 fn 放回,违背"不用 fn")。
- 不拆 `hooks/` 子目录(`hooks.rs` 体量在瘦身后已合适)。
- 不为内置钩子新增单测(纯搬迁 + 抽象切换,`todo_reminder` 的可测试性改善是 trait 改动的副产品,但本次不补测试)。

## 3. 架构

### 3.1 `hooks.rs`(机制层,瘦身)

定义 4 个 trait,各带 `Send + Sync` 超trait(使 `Box<dyn Trait>` 自动 `Send + Sync`,保持 `Hooks` 可跨 async 传递 —— 与裸 `fn` 指针时代的行为一致),各一个方法,签名与现有 fn 指针逐一对应:

```rust
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
```

`Hooks` 结构字段类型从 `Vec<fn...>` 改为 `Vec<Box<dyn TraitX>>`,4 个 `trigger_*` 迭代改调方法,short-circuit 语义不变:

```rust
pub struct Hooks {
    user_prompt: Vec<Box<dyn PromptHook>>,
    pre_tool:    Vec<Box<dyn PreToolHook>>,
    post_tool:   Vec<Box<dyn PostToolHook>>,
    stop:        Vec<Box<dyn StopHook>>,
}
```

注册用泛型 helper(内部 boxing,调用方传具体类型,不碰 fn):

```rust
pub fn on_pre_tool<H: PreToolHook + 'static>(&mut self, h: H) {
    self.pre_tool.push(Box::new(h));
}
// on_prompt / on_post_tool / on_stop 同构
```

`assemble_post_tool_messages` 原样保留于此文件(桥梁函数,与 PostToolUse 语义耦合,且不依赖 `Hooks` 本身)。

文件头注释更新:去掉对"裸 fn 指针"的论证,改为说明 trait 抽象与机制/策略分离;返回值语义表(short-circuit、Stop 注入续跑)保留。

### 3.2 `builtins.rs`(策略层,新建)

5 个内置钩子改为结构体,集中于此:

| 结构体 | 实现 trait | 状态 | 来源 |
|---|---|---|---|
| `ContextInjectHook` | `PromptHook` | unit struct | hooks.rs `context_inject_hook` |
| `LargeOutputHook` | `PostToolHook` | unit struct | hooks.rs `large_output_hook` |
| `SummaryHook` | `StopHook` | unit struct | hooks.rs `summary_hook` |
| `TodoReminderHook` | `PostToolHook` | `AtomicUsize` 计数器字段,`TodoReminderHook::new()` 初始化为 0 | hooks.rs `todo_reminder_hook` + `static ROUNDS_SINCE_TODO` |
| `PermissionHook` | `PreToolHook` | unit struct | permission.rs `permission_hook` |

`PermissionHook` 的三道闸门逻辑原样搬进 `on_pre_tool`;`permission.rs` 的 `DENY_LIST` / `check_deny_list` / `ask_user` 一并迁入 `builtins.rs`(设为模块私有,仅 `PermissionHook` 用)。

`TodoReminderHook::on_post_tool` 逻辑与现有 `todo_reminder_hook` 完全一致,只是计数器从 `static` 换成 `self.rounds_since_todo`:

```rust
pub struct TodoReminderHook {
    rounds_since_todo: AtomicUsize,
}
impl TodoReminderHook {
    pub fn new() -> Self { Self { rounds_since_todo: AtomicUsize::new(0) } }
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
```

### 3.3 `permission.rs`(删除)

全部内容迁入 `builtins.rs` 后,该文件变空 → 删除;`lib.rs` 去掉 `pub mod permission;`。

## 4. 连带改动

### `lib.rs`
- 加 `pub mod builtins;`
- 删 `pub mod permission;`

### `main.rs`
- import 从 `rust_agent::hooks::{...5 个 hook fns..., Hooks}` + `rust_agent::permission::permission_hook` 改为:
  ```rust
  use rust_agent::hooks::{assemble_post_tool_messages, Hooks};
  use rust_agent::builtins::{ContextInjectHook, LargeOutputHook, PermissionHook, SummaryHook, TodoReminderHook};
  ```
- 注册代码(原 237–241 行)改为:
  ```rust
  hooks.on_prompt(ContextInjectHook);
  hooks.on_pre_tool(PermissionHook);
  hooks.on_post_tool(LargeOutputHook);
  hooks.on_stop(SummaryHook);
  hooks.on_post_tool(TodoReminderHook::new());
  ```
- 其余(trigger 调用、`agent_loop`、`execute_tool`)不变。

### `hooks.rs` 测试
- 3 个 PreTool test fn(`always_block`/`never_block`/`panic_if_called`)改写为实现 `PreToolHook` 的小结构体(`AlwaysBlock`/`NeverBlock`/`PanicIfCalled`)。
- `stop_some_forces_continue` 里的内联 `force` fn 改为 `Force` 结构体实现 `StopHook`。
- 8 个测试的断言与逻辑不变(`empty_registry_allows` / `pre_tool_first_some_short_circuits` / `none_passes_through` / 4 个 assemble 测试 / `stop_some_forces_continue`)。

### `permission.rs` 测试(随文件迁入 `builtins.rs`)
- `deny_list_matches` / `deny_list_case_insensitive` / `permission_hook_allows_safe` / `permission_hook_blocks_deny_list` / `permission_hook_uses_registry` / `permission_hook_requires_approval` / `permission_hook_unknown_tool` 共 7 个测试迁入 `builtins.rs` 的 `#[cfg(test)] mod tests`。
- 直接调用 `permission_hook(&registry, name, input)` 的改为 `PermissionHook.on_pre_tool(&registry, name, input)`。
- `check_deny_list` 的两个测试原样(函数一并迁入,签名不变)。

### 不变
- `subagent.rs`:只用 `Hooks` + `assemble_post_tool_messages`,不变。
- `tools/registry.rs` / `tools/trait_def.rs`:只用 `Hooks::new()` 或 `&Hooks` 类型,不变(三处 registry.rs 测试只构造空 `Hooks`)。

## 5. 数据流(不变)

```
user query
  → trigger_prompt()  (PromptHook, 返回值不参与控制流)
  → LLM
  → stop_reason == tool_use ?
      否 → trigger_stop()  (StopHook, Some -> 注入 user 消息续跑, None -> 退出)
      是 → 对每个 ToolUse:
             trigger_pre_tool()  (PreToolHook, Some -> 拦截, reason 当 tool_result)
             dispatch_tool
             trigger_post_tool() (PostToolHook, Some -> 提醒进独立 user 消息)
           assemble_post_tool_messages(tool_results, reminders) 喂回 LLM
```

## 6. 错误处理(不变)

- PreToolUse 拦截:返回的 `reason` 直接作为 `tool_result` 喂回,工具不执行。
- 空内容兜底:`assemble_post_tool_messages` 在 tool_results 与 reminders 皆空时塞一条 `(no tool calls to execute)` Text 块,避免 Anthropic API 400 "content cannot be empty"。保留。
- trait 方法的 `Send + Sync` 超trait保证 `Box<dyn Trait>` 可跨 async 边界,与裸 fn 时代等价。

## 7. 测试与验证

- `hooks.rs` 8 个测试保留(3 个改写为结构体)。
- `builtins.rs` 接收 `permission.rs` 的 7 个测试(1 处调用改 `PermissionHook.on_pre_tool`)。
- 验证步骤:
  1. `cargo build`(确认 trait 改动全链路编译通过)
  2. `cargo test`(确认 hooks + builtins + registry 全绿)
- `cargo test` 全绿即为完成标准。

## 8. 文件清单

| 文件 | 动作 |
|---|---|
| `src/hooks.rs` | 瘦身:trait 定义 + `Hooks` + `trigger_*` + `assemble_post_tool_messages` + 测试(改结构体);删 4 个内置钩子 + `static` |
| `src/builtins.rs` | 新建:5 个钩子结构体 + `DENY_LIST`/`check_deny_list`/`ask_user` + 测试 |
| `src/permission.rs` | 删除 |
| `src/lib.rs` | 加 `pub mod builtins;`,删 `pub mod permission;` |
| `src/main.rs` | import 与注册代码改结构体 |
