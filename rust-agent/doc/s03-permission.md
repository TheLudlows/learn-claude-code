# s03: 权限 —— 执行前做权限判断

> 工具执行前先过三道闸门: 硬拒绝 → 规则匹配 → 用户审批。

s02 的循环一字不改, 唯一变动是在 dispatch 前插一道 `check_permission()`。安全边界由代码负责, 判断发生在工具执行之前。

## 三道闸门

| 闸门 | 作用 | 命中后 |
|------|------|--------|
| 1. 拒绝列表 | 永远禁止(`rm -rf /`、`sudo`) | 直接拒绝, 不执行 |
| 2. 规则匹配 | 取决于上下文(写工作区外、`rm` 文件) | 交给闸门 3 |
| 3. 用户审批 | 闸门 2 命中后暂停等确认 | 用户决定 |

三道都没命中 → 放行。大部分日常操作走这条路。

```rust
pub fn check_permission(name: &str, input: &serde_json::Value) -> bool {
    // 闸门 1: 硬拒绝
    if name == "bash" {
        if let Some(p) = check_deny_list(/* command */) {
            return false;                       // 直接拒, 不问
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

循环里只多一行判断:

```rust
let output = if permission::check_permission(name, input) {
    dispatch_tool(name, input)
} else {
    "Permission denied.".to_string()
};
```

## 相对 s02 的变更

| 组件 | 之前 (s02) | 之后 (s03) |
|------|-----------|-----------|
| bash 安全 | `run_bash` 内硬编码危险词表 | 拒绝列表上移到闸门 1, 工具不再自检 |
| 新模块 | — | `permission.rs`(deny list / rules / ask / pipeline) |
| 循环 | 直接 dispatch | dispatch 前插 `check_permission()` |

把拒绝逻辑从 `run_bash` 里拎出来, 放到执行前的闸门——工具只管执行, 安全面集中在一处。文件工具仍由 `tools::safe_path` 做工作区沙箱(defense in depth)。

> 注: 字符串匹配仅用于演示闸门位置, 非完整安全边界。
