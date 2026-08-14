# s04: Hooks —— 挂在循环上，不写进循环里

> hook 在工具前后注入扩展逻辑；循环只调 `trigger_*`，具体行为全在回调里。

s03 的权限检查硬编码在循环体内。每加一个检查（记录每次 bash、操作后自动 git add），都得改 `agent_loop`。循环该是稳定核心，扩展应挂在外面。s04 把 `check_permission()` 从循环体内挪到 hook 上，循环不再直接调用任何检查函数，改由注册表决定跑什么。

## 四个事件

| 事件 | 触发时机 | 典型用途 |
|------|---------|---------|
| UserPromptSubmit | 用户输入后、进 LLM 前 | 注入上下文、输入校验 |
| PreToolUse | 工具执行前 | 权限检查、日志记录 |
| PostToolUse | 工具执行后 | 副作用（自动 git add）、输出检查 |
| Stop | 循环即将退出时 | 收尾统计、决定是否继续 |

`PreToolUse` 返回 `Some(reason)` → 本次工具被拦，reason 直接当 `tool_result`；`Stop` 返回 `Some(msg)` → 注入 msg 并继续循环，不退出。另外两个事件的返回值不参与控制流。

## 注册表 + 触发

```rust
pub struct Hooks { user_prompt, pre_tool, post_tool, stop: Vec<fn…> }

impl Hooks {
    pub fn trigger_pre_tool(&self, name, input) -> Option<String> {
        for f in &self.pre_tool {
            if let Some(reason) = f(name, input) { return Some(reason); } // 第一个 Some 短路
        }
        None
    }
    // trigger_post_tool / trigger_prompt / trigger_stop 同理
}
```

回调用裸 `fn` 指针（`Copy`、零开销），对应 Python「按名注册函数」的风格，免去 `Box<dyn Fn>` 的堆分配与 `Send/Sync` 约束。s03 的三道闸门原封不动搬成 `permission_hook`，注册进 `PreToolUse`。

## 循环里只换了调用点

```rust
// s03: if permission::check_permission(name, input) { dispatch } else { "denied" }
// s04: hook 替代硬编码
if let Some(reason) = hooks.trigger_pre_tool(name, input) {
    tool_results.push(ToolResult { tool_use_id: id, content: reason });
    continue;
}
let output = dispatch_tool(name, input);
hooks.trigger_post_tool(name, input, &output);
```

退出前多一道 `Stop`：

```rust
if response.stop_reason != "tool_use" {
    if let Some(force) = hooks.trigger_stop(messages) { // Some → 注入并继续
        messages.push(user_text(force));
        continue;
    }
    break;
}
```

输入侧加一道 `UserPromptSubmit`：用户输入后、进 LLM 前 `hooks.trigger_prompt(query)`。

## 相对 s03 的变更

| 组件 | 之前 (s03) | 之后 (s04) |
|------|-----------|-----------|
| 扩展方式 | `check_permission()` 硬编码在循环里 | `Hooks` 注册表 + `trigger_*` |
| 新模块 | — | `hooks.rs`（注册表 / 触发 / 4 个示例回调） |
| 权限 | `permission::check_permission -> bool` | `permission::permission_hook -> Option<String>`，注册为 PreToolUse |
| 退出控制 | 无 | `trigger_stop` 可阻止退出 |
| 输入拦截 | 无 | `trigger_prompt` 可注入上下文 |

> 注：顺带修掉 s03 `check_permission` 末尾的 `return false`——三道闸门都没命中时它仍拒绝一切，违背了文档「三道都没命中 → 放行」的语义。改成 `None` 放行后才正确。
