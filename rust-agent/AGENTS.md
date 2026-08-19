# rust-agent

> AI agent 在本目录工作的速查手册：构建、测试、结构、约定、阶段映射。
> 详细设计见 `DESIGN.md`（s01–s09 逐节详解）。

## 这是什么

用 Rust 实现的 Claude Code 式 agent loop，是课程 s01–s11 的参考实现。
s12–s17 暂为课程各章的 Python 示例（`code.py`），尚未进入 rust-agent。

## 构建 / 运行

```sh
cargo build --release
cargo run --release          # 或 ./target/release/rust-agent
```

配置：`cp .env.example .env`，填入

- `ANTHROPIC_API_KEY`（或 `ANTHROPIC_AUTH_TOKEN`）
- `MODEL_ID`
- 可选：`ANTHROPIC_BASE_URL`（兼容供应商）、`SKILLS_DIR`（默认 `./skills`）

## 测试

```sh
cargo test                    # 单元测试，无需 API key
cargo test -- --ignored       # #[ignore] 烟雾测试，需 API key
```

大部分源文件含 `#[cfg(test)]` 单元测试；涉及真实模型调用的用 `#[ignore]` 标记，平时不跑。

## 代码结构

| 路径 | 职责 | 阶段 |
|---|---|---|
| `src/main.rs` | agent loop 入口（REPL） | s01 |
| `src/client.rs` | API 请求构造 + 流式解析 | — |
| `src/output.rs` | 终端渲染与着色 | — |
| `src/error.rs` | `AgentError`（含 `is_prompt_too_long`） | — |
| `src/builtins.rs` | 内置 Hooks（权限/大输出/总结/提醒/上下文注入） | s03/s04 |
| `src/hooks.rs` | Hook 注册表 + `trigger_*` | s04 |
| `src/todo.rs` | `TodoManager` | s05 |
| `src/subagent.rs` | 子 agent loop（`task` 工具的后端） | s06 |
| `src/skills.rs` | `SkillLoader` | s07 |
| `src/compact.rs` | `ContextCompactor`（四步管线） | s08 |
| `src/memory.rs` | `MemoryStore` | s09 |
| `src/task_system/` | 持久化任务图 | s10 |
| `src/background_tasks/` | 后台线程执行慢命令 | s11 |
| `src/tools/` | 工具分发（registry + 各工具实现） | s02 |
| `skills/` | 4 个示例技能 | s07 |
| `DESIGN.md` | 设计文档（s01–s09） | — |

运行时生成（勿提交）：`.memory/` `.transcripts/` `.task_outputs/` `.tasks/`

## 关键约定

- **循环不变**：新机制挂在 hooks 或工具上，不改 `while true` 主体。
- **全局单例**：`OnceLock` 持有 skills/todo，只读无需 `Mutex`。
- **字符数为单位**（对齐 Python `len`），不引 tokenizer。
- **best-effort**：memory/compact 失败降级，不中断主循环。
- **子 agent**：无 `task`/`compact`/`memory` 工具，30 轮上限。
- **新工具 = 一条 JSON 定义 + 一个 handler 注册**。

## 阶段映射（s01–s17）

| 阶段 | 组件 | rust-agent | 一句话 |
|---|---|---|---|
| s01 | Agent Loop | ✓ | `while true` + `stop_reason` |
| s02 | Tool Use | ✓ | handler 字典分发 |
| s03 | Permission | ✓ | 三道闸门 |
| s04 | Hooks | ✓ | 四个扩展点 |
| s05 | TodoWrite | ✓ | 规划清单 |
| s06 | Subagent | ✓ | 消息隔离的 `task` 工具 |
| s07 | Skill Loading | ✓ | 目录进 system，正文走 `load_skill` |
| s08 | Context Compact | ✓ | 四步压缩 + 反应式补救 |
| s09 | Memory | ✓ | `.memory/` 存储+召回+提取+整理 |
| s10 | Task System | ✓ | `task_system/` 持久化任务图 |
| s11 | Background Tasks | ✓ | `background_tasks/` 后台线程 |
| s12 | Cron Scheduler | ✗ 课程 | 按时间启动任务 |
| s13 | Agent Teams | ✗ 课程 | 团队运行时与协作协议 |
| s14 | MCP Tools | ✗ 课程 | 发现并调用外部工具 |
| s15 | Integrated Harness | ✗ 课程 | 多机制集成到一个循环 |
| s16 | Workflow Runtime | ✗ 课程 | 脚本编排多 agent |
| s17 | Goal Loop | ✗ 课程 | 独立判断器决定是否继续 |

## 改动时注意

- 改 `src/` 后先 `cargo test`。
- 新增工具：在 `src/tools/mod.rs` 的 `build_registry()` 注册定义和 handler（`registry.rs` 只放 `ToolRegistry` 结构）。
- 别把 `SKILL.md` 全文塞进 system prompt（s07 按需加载）。
- 压缩切点必须保护 `tool_use` / `tool_result` 配对（s08）。
- 记忆只存 `persistent` scope（s09），临时信息不落盘。
