/*
agent.rs - Agent 对象抽象 (s13)

持有 conf/tools/hooks/compactor/memory 以及原本散落在进程级全局单例里的共享状态
(skills / todo / task_store / bg_manager / cron_manager) 作为成员，通过 ToolContext
下传给工具，不再依赖 OnceLock/LazyLock 全局。子 agent 是 `child_agent` 产出的嵌套
实例：Arc-clone 共享 infra、刷新 per-loop 状态，从而：

- 修 S2：子 agent 工具调用经统一 `execute_tool`（含 trigger_pre_tool），不再旁路权限。
- 修 S4：child 拿到刷新的 Hooks（TodoReminder 计数器归零），hook 状态不跨隔离边界泄漏。
- 修 S8：`Agent::new` 返回 Result，TaskStore/CronManager 构造失败显式传播而非 panic。
- 修 D2：`max_turns` 从 task 工具入参真正传入循环。
- 修 A2：父子共用同一个 `run_loop`，消除手抄循环。
*/

use std::path::PathBuf;
use std::sync::Arc;

use crate::background_tasks::manager::BackgroundManager;
use crate::background_tasks::BackgroundStopHook;
use crate::builtins;
use crate::client::{CallResult, Client, ContentBlock, Message};
use crate::compact::{ContextCompactor, MAX_REACTIVE_RETRIES};
use crate::cron_scheduler::{self, CronManager};
use crate::error::AgentError;
use crate::hooks::{assemble_post_tool_messages, Hooks};
use crate::memory::{build_system, MemoryStore};
use crate::output;
use crate::skills::SkillLoader;
use crate::task_system::store::TaskStore;
use crate::todo::{SharedTodoManager, TodoManager};
use crate::tools;
use crate::tools::registry::ToolRegistry;
use crate::tools::trait_def::{AgentKind, ToolContext, ToolResult};

/// 所有 stream_messages 调用共用的 max_tokens（原 main.rs/subagent.rs 各硬编码 8000）。
pub const MAX_TOKENS: u32 = 8000;

/// 子 agent 的 system prompt（原 subagent.rs:21）。
const SUB_SYSTEM: &str = "You are a focused coding agent. Complete your task efficiently. Use tools as needed. Return a concise summary of your work.";

/// 循环终止结果。
pub enum LoopOutcome {
    /// 模型结束（非 tool_use 且 Stop 钩子未强制继续）。
    Completed,
    /// 达到 max_turns 上限（仅子 agent）。
    MaxTurnsReached,
}

/// 构造 Agent 所需的配置。
pub struct AgentConfig {
    pub api_key: String,
    pub base_url: String,
    pub model: String,
    pub workdir: PathBuf,
    pub skills_dir: PathBuf,
}

pub struct Agent {
    // ---- 共享 infra：child_agent Arc-clone ----
    pub(crate) client: Arc<Client>,
    pub(crate) registry: Arc<ToolRegistry>,
    pub(crate) skills: Arc<SkillLoader>,
    pub(crate) task_store: Arc<TaskStore>,
    pub(crate) bg_manager: Arc<BackgroundManager>,
    pub(crate) todo_manager: Arc<SharedTodoManager>,
    pub(crate) workdir: PathBuf,

    // ---- per-loop 状态：child 刷新 ----
    pub(crate) cron_manager: Option<Arc<CronManager>>,
    pub(crate) compactor: ContextCompactor,
    pub(crate) memory: MemoryStore,
    pub(crate) hooks: Hooks,
    pub(crate) base_system: String,
    pub(crate) max_turns: usize,
    pub(crate) kind: AgentKind,
    /// s13: this agent's owner name ("agent" for Lead/subagent; teammate name for teammates).
    pub(crate) owner: String,
    /// s13: shared team context (Lead + teammates have Some; s06 subagents have None).
    pub(crate) team: Option<Arc<crate::team::TeamCtx>>,
    pub(crate) max_tokens: u32,
}

impl Agent {
    /// 构造主 agent。TaskStore / CronManager 构造失败在此传播（修 S8）。
    pub async fn new(cfg: AgentConfig) -> Result<Agent, AgentError> {
        let client = Arc::new(Client::new(cfg.api_key, cfg.base_url, cfg.model));
        let skills = Arc::new(SkillLoader::scan(cfg.skills_dir.clone()));
        let task_store = Arc::new(
            TaskStore::new(cfg.workdir.clone())
                .map_err(|e| AgentError::Other(format!("task store init: {e}")))?,
        );
        let bg_manager = Arc::new(BackgroundManager::new(
            cfg.workdir.join(".task_outputs").join("background"),
        ));
        let todo_manager = Arc::new(SharedTodoManager::new(TodoManager::new()));

        let cron_manager = Some({
            let cm = Arc::new(
                CronManager::new(cfg.workdir.clone())
                    .await
                    .map_err(|e| AgentError::Other(format!("cron init: {e}")))?,
            );
            let _ = cm.load_durable().await;
            cm
        });

        let compactor = ContextCompactor::new(
            cfg.workdir.join(".transcripts"),
            cfg.workdir.join(".task_outputs").join("tool-results"),
        );
        let memory = MemoryStore::new(cfg.workdir.join(".memory"));

        let registry = Arc::new(tools::build_registry());
        let base_system = build_base_system(&skills, &cfg.workdir);
        let hooks = Self::build_hooks(&bg_manager, &todo_manager);
        let team = Arc::new(
            crate::team::TeamCtx::new(cfg.workdir.clone(), Arc::clone(&task_store))
                .map_err(|e| AgentError::Other(format!("team init: {e}")))?,
        );

        Ok(Agent {
            client,
            registry,
            skills,
            task_store,
            bg_manager,
            todo_manager,
            workdir: cfg.workdir,
            cron_manager,
            compactor,
            memory,
            hooks,
            base_system,
            max_turns: usize::MAX,
            kind: AgentKind::Lead,
            owner: "agent".to_string(),
            team: Some(team),
            max_tokens: MAX_TOKENS,
        })
    }

    /// 产出嵌套子 agent：Arc-clone 共享 infra，刷新 per-loop 状态。
    /// cron_manager 置 None（子 agent 不投递定时任务）；
    /// compactor 使用隔离子目录（避免与 Lead 文件竞争）；
    /// memory 使用 read_only 模式（可召回但不写盘）；
    /// max_turns 设为有限值以约束循环。
    pub fn child_agent(&self, max_turns: usize, sub_system: &str) -> Agent {
        let subagent_id = format!("subagent_{}", fastrand::u64(..));
        let subagent_dir = self.workdir.join(".subagents").join(&subagent_id);

        let compactor = ContextCompactor::new(
            subagent_dir.join(".transcripts"),
            subagent_dir.join(".task_outputs").join("tool-results"),
        );

        let memory = MemoryStore::new_read_only(self.workdir.join(".memory"));

        Agent {
            client: Arc::clone(&self.client),
            registry: Arc::clone(&self.registry),
            skills: Arc::clone(&self.skills),
            task_store: Arc::clone(&self.task_store),
            bg_manager: Arc::clone(&self.bg_manager),
            todo_manager: Arc::clone(&self.todo_manager),
            workdir: self.workdir.clone(),
            cron_manager: None,
            compactor,
            memory,
            hooks: Self::build_hooks(&self.bg_manager, &self.todo_manager),
            base_system: sub_system.to_string(),
            max_turns,
            kind: AgentKind::Subagent,
            owner: "agent".to_string(),
            team: None,
            max_tokens: self.max_tokens,
        }
    }

    /// 启动 cron 调度器（原 init_manager + start_runtime）。
    pub async fn start_cron_runtime(&self) -> Result<(), AgentError> {
        if let Some(cron) = &self.cron_manager {
            cron.start_scheduler()
                .await
                .map_err(|e| AgentError::Other(format!("cron start: {e}")))?;
        }
        Ok(())
    }

    /// 装配默认 hook 集（原 main.rs:299-305）。
    /// BackgroundStopHook 经构造器 DI 拿到 bg_manager（原读全局 get_manager）。
    /// TodoReminderHook 经构造器 DI 拿到 todo_manager（每次注入当前 todo 列表）。
    fn build_hooks(bg: &Arc<BackgroundManager>, todo: &Arc<SharedTodoManager>) -> Hooks {
        let mut h = Hooks::new();
        h.on_prompt(builtins::ContextInjectHook);
        h.on_pre_tool(builtins::PermissionHook);
        h.on_post_tool(builtins::LargeOutputHook);
        h.on_stop(builtins::SummaryHook);
        h.on_post_tool(builtins::TodoReminderHook::new(Arc::clone(todo)));
        h.on_stop(BackgroundStopHook::new(Arc::clone(bg)));
        h
    }

    /// Produce a persistent teammate agent: shares infra, kind=Teammate, team=Some,
    /// fresh non-interactive hooks. Does NOT reference the Lead agent, so there is
    /// no TeamCtx → Agent → TeamCtx Arc cycle.
    /// cron_manager 置 None；compactor 使用隔离子目录；memory 使用 read_only；
    /// max_turns 设为 usize::MAX（Teammate 无轮次限制）。
    pub fn child_teammate(&self, name: &str, system: &str, team: Arc<crate::team::TeamCtx>) -> Agent {
        let teammate_dir = self.workdir.join(".teammates").join(name);

        let compactor = ContextCompactor::new(
            teammate_dir.join(".transcripts"),
            teammate_dir.join(".task_outputs").join("tool-results"),
        );

        let memory = MemoryStore::new_read_only(self.workdir.join(".memory"));

        Agent {
            client: Arc::clone(&self.client),
            registry: Arc::clone(&self.registry),
            skills: Arc::clone(&self.skills),
            task_store: Arc::clone(&self.task_store),
            bg_manager: Arc::clone(&self.bg_manager),
            todo_manager: Arc::clone(&self.todo_manager),
            workdir: self.workdir.clone(),
            cron_manager: None,
            compactor,
            memory,
            hooks: Self::build_teammate_hooks(),
            base_system: system.to_string(),
            max_turns: usize::MAX,
            kind: AgentKind::Teammate,
            owner: name.to_string(),
            team: Some(team),
            max_tokens: self.max_tokens,
        }
    }

    /// Teammate hook set: non-interactive permission (no stdin) + large-output
    /// reminder. No TodoReminder/Summary — teammates are non-interactive.
    fn build_teammate_hooks() -> Hooks {
        let mut h = Hooks::new();
        h.on_pre_tool(builtins::PermissionHook);
        h.on_post_tool(builtins::LargeOutputHook);
        h
    }

    pub fn owner(&self) -> &str {
        &self.owner
    }
    pub fn team(&self) -> Option<&Arc<crate::team::TeamCtx>> {
        self.team.as_ref()
    }
    pub fn lead_notify(&self) -> Option<&tokio::sync::Notify> {
        self.team.as_ref().map(|t| t.lead_notify())
    }

    /// 用户输入提交后触发 UserPromptSubmit 钩子。
    pub fn trigger_prompt(&self, query: &str) {
        self.hooks.trigger_prompt(query);
    }

    /// 当前工作目录（供 main 的 banner 等使用）。
    pub fn workdir(&self) -> &PathBuf {
        &self.workdir
    }

    pub fn base_system(&self) -> &str {
        &self.base_system
    }

    /// 已加载技能数（供 main 的启动 banner）。
    pub fn skills_len(&self) -> usize {
        self.skills.len()
    }

    // ---- 内部工具执行（父子共用，修 S2）----

    /// 单个工具调用 + PreToolUse 拦截。子 agent 也走这里 → trigger_pre_tool 不再旁路。
    async fn execute_tool(&self, name: &str, input: &serde_json::Value) -> ToolResult {
        if let Some(reason) = self.hooks.trigger_pre_tool(&self.registry, name, input) {
            return ToolResult::Denied {
                name: name.to_string(),
                reason,
            };
        }
        // s13 plan gate: teammates cannot run mutating tools until the plan is approved.
        if self.kind == AgentKind::Teammate
            && matches!(name, "command" | "write_file" | "edit_file")
        {
            if let Some(team) = &self.team {
                let gate = team.protocols.gate(&self.owner);
                if gate.blocks_mutating_tools() {
                    return ToolResult::Denied {
                        name: name.to_string(),
                        reason: format!(
                            "Blocked: plan status is {:?}. Submit or revise the plan and wait for approval.",
                            gate
                        ),
                    };
                }
            }
        }
        let ctx = ToolContext { agent: self };
        self.registry.dispatch(name, &ctx, input, self.kind).await
    }

    /// 执行本轮所有 ToolUse 块，返回要追加的 user 消息（不原地改 messages，规避
    /// `&self` 与 `&mut messages` 借用冲突）。`as_content()` 绑一次（原 main 3×/subagent 2×）。
    async fn execute_tool_use_blocks(&self, content: &[ContentBlock]) -> Vec<Message> {
        let mut tool_results = Vec::new();
        let mut reminders: Vec<String> = Vec::new();
        for block in content {
            if let ContentBlock::ToolUse { id, name, input } = block {
                let result = self.execute_tool(name, input).await;
                let content_str = result.as_content();
                {
                    let mut out = std::io::stdout().lock();
                    output::render_tool_result(name, &content_str, &mut out);
                }
                if result.was_executed() {
                    if let Some(msg) = self.hooks.trigger_post_tool(name, input, &content_str) {
                        reminders.push(msg);
                    }
                }
                tool_results.push(ContentBlock::ToolResult {
                    tool_use_id: id.clone(),
                    content: content_str,
                });
            }
        }
        assemble_post_tool_messages(tool_results, reminders)
    }

    /// 统一循环（原 main.rs::agent_loop + subagent.rs::run_subagent_loop 合并）。
    /// cron_manager 仍为 Option（仅 Lead 持有）；compactor/memory/max_turns 始终有值。
    pub async fn run_loop(
        &self,
        messages: &mut Vec<Message>,
        active_request: &str,
    ) -> Result<LoopOutcome, AgentError> {
        // s09：召回相关记忆拼进 system（每请求一次，与压缩正交）。
        // read_only 实例也可召回；extract/consolidate 阶段 read_only 自动跳过写盘。
        let recalled = self.memory.load_memories(&self.client, messages).await;
        let index = self.memory.read_memory_index();
        let system = build_system(&self.base_system, &index, &recalled);

        let mut reactive_retries = 0u32;
        let max = self.max_turns;

        for _turn in 1..=max {
            // 循环顶部：被动兜底收集已完成后台任务（子 agent 不在顶部拉取，与现状一致）。
            if self.kind == AgentKind::Lead {
                let _ = self.bg_manager.collect_and_inject(messages);
            }

            // s12：循环顶部收集待交付的定时任务（子 agent cron_manager=None 跳过）。
            let mut waiting_for_ack: Vec<cron_scheduler::CronJob> = Vec::new();
            let scheduled_start = messages.len();
            if let Some(cron) = &self.cron_manager {
                let jobs = cron.consume_queue();
                for job in &jobs {
                    messages.push(Message::user_text(format!("[Scheduled] {}", job.prompt)));
                    let preview: String = job.prompt.chars().take(60).collect();
                    println!("  [cron] delivered {}: {}", job.id, preview);
                }
                waiting_for_ack = jobs;
            }

            // s13: teammates drain their own inbox each turn (Lead's inbox is
            // drained by main.rs outside run_loop). An accepted shutdown ends the loop.
            if self.kind == AgentKind::Teammate {
                if let Some(team) = &self.team {
                    if crate::team::drain_inbox(team, &self.owner, messages) {
                        return Ok(LoopOutcome::Completed);
                    }
                }
            }

            // s08：每次调用模型前运行压缩管线。
            self.compactor.prepare(&self.client, messages, active_request).await?;

            let defs = self.registry.definitions_for(self.kind);
            let response = match self
                .client
                .stream_messages(&system, messages, &defs, self.max_tokens, None, tokio_util::sync::CancellationToken::new())
                .await
            {
                CallResult::Success(r) => {
                    reactive_retries = 0;
                    r
                }
                // prompt_too_long 且还有重试预算：压缩后重试。
                CallResult::PromptTooLong(_) if reactive_retries < MAX_REACTIVE_RETRIES => {
                    output::status("[reactive compact]");
                    self.compactor
                        .reactive_compact(&self.client, messages, active_request)
                        .await?;
                    reactive_retries += 1;
                    continue;
                }
                // prompt_too_long 耗尽重试 / 无 compactor / 其他错误：恢复定时任务后返回。
                CallResult::PromptTooLong(e) | CallResult::Failure(e) => {
                    if !waiting_for_ack.is_empty() {
                        messages.truncate(scheduled_start);
                        if let Some(cron) = &self.cron_manager {
                            cron.restore_jobs(&waiting_for_ack);
                        }
                    }
                    return Err(e);
                }
                CallResult::Cancelled => {
                    if !waiting_for_ack.is_empty() {
                        messages.truncate(scheduled_start);
                        if let Some(cron) = &self.cron_manager {
                            cron.restore_jobs(&waiting_for_ack);
                        }
                    }
                    return Err(AgentError::Other("Cancelled".to_string()));
                }
            };

            {
                let mut out = std::io::stdout().lock();
                output::render(&response, &mut out);
            }

            // 追加助手响应（含 text 与 tool_use 块，原样回传下一轮）。
            messages.push(Message::assistant_content(response.content.clone()));

            // 模型调用成功后确认定时任务。
            if !waiting_for_ack.is_empty() {
                if let Some(cron) = &self.cron_manager {
                    if let Err(e) = cron.acknowledge_jobs(&waiting_for_ack).await {
                        println!("  [cron] acknowledgement failed: {}", e);
                    }
                }
                waiting_for_ack.clear();
            }

            // 检查是否需要调用工具。
            if response.stop_reason != "tool_use" {
                if let Some(force) = self.hooks.trigger_stop(messages) {
                    messages.push(Message::user_text(force));
                    continue;
                }
                // read_only 实例的 extract/consolidate 内部直接返回 0，无需额外判断。
                if self.memory.extract_memories(&self.client, messages).await > 0 {
                    let _ = self.memory.consolidate_memories(&self.client).await;
                }
                return Ok(LoopOutcome::Completed);
            }

            // 执行本轮工具调用 + PostToolUse 提醒（父子共用 helper）。
            messages.extend(self.execute_tool_use_blocks(&response.content).await);
        }

        Ok(LoopOutcome::MaxTurnsReached)
    }

    /// 子 agent 入口（原 subagent.rs::run_subagent_loop）。
    /// 产出 child agent（共享 infra、刷新状态），跑 run_loop，提取最终文本。
    pub async fn run_subagent(&self, prompt: &str, max_turns: usize) -> Result<String, AgentError> {
        let max_turns = max_turns.clamp(1, 50);
        let child = self.child_agent(max_turns, SUB_SYSTEM);
        output::status("[Subagent started]");

        let mut messages: Vec<Message> = vec![Message::user_text(prompt)];

        let outcome = child.run_loop(&mut messages, prompt).await?;

        let result = match outcome {
            LoopOutcome::Completed => {
                let last_assistant = messages
                    .iter()
                    .rev()
                    .find(|m| m.role.as_str() == "assistant")
                    .map(|m| m.content.as_slice())
                    .unwrap_or(&[]);
                match extract_final_text(last_assistant) {
                    Some(text) => {
                        output::status("[Subagent done]");
                        text
                    }
                    None => {
                        output::status("[Subagent done - no text]");
                        "(no summary)".to_string()
                    }
                }
            }
            LoopOutcome::MaxTurnsReached => {
                output::status(&format!(
                    "[Subagent stopped after {} turns without final answer]",
                    max_turns
                ));
                format!(
                    "Subagent stopped after {} turns without a final answer.",
                    max_turns
                )
            }
        };
        Ok(result)
    }
}

/// 提取响应中的最终文本（不含 tool_use）。无 Text 块返回 None（原 subagent.rs:28-44）。
fn extract_final_text(content: &[ContentBlock]) -> Option<String> {
    let texts: Vec<String> = content
        .iter()
        .filter_map(|block| {
            if let ContentBlock::Text { text } = block {
                Some(text.clone())
            } else {
                None
            }
        })
        .collect();
    if texts.is_empty() {
        None
    } else {
        Some(texts.join("\n"))
    }
}

/// 组装 system prompt（原 main.rs:283-296）。
fn build_base_system(skills: &SkillLoader, workdir: &std::path::Path) -> String {
    let catalog = skills.catalog();
    let os = std::env::consts::OS;
    if catalog.is_empty() {
        format!(
            "You are a coding agent at {} on {}. Before starting any multi-step task, use todo_write to plan your steps. Update status as you go. You can use tools as needed.",
            workdir.display(),
            os
        )
    } else {
        format!(
            "You are a coding agent at {} on {}. Before starting any multi-step task, use todo_write to plan your steps. Update status as you go. You can use tools as needed.\n\n\
             Skills available:\n{}\n\n\
             Use load_skill to read the full instructions when a skill applies.",
            workdir.display(),
            os,
            catalog
        )
    }
}

/// 测试专用：在 tempdir 内构造一个隔离的 Agent（无 cron/compactor/memory），
/// 替代原 `TestToolContext`。全局单例已消除，可在同一进程并行构造多个互不污染。
#[cfg(test)]
pub struct TestAgent {
    // 保持 tempdir 活着，隔离 task/bg/skills 文件
    _tmp: tempfile::TempDir,
    agent: Agent,
}

#[cfg(test)]
impl TestAgent {
    pub fn new() -> Self {
        let tmp = tempfile::TempDir::new().expect("tempdir");
        let workdir = tmp.path().to_path_buf();
        let client = Arc::new(Client::new(
            "test-key".into(),
            "http://localhost".into(),
            "test-model".into(),
        ));
        let skills = Arc::new(SkillLoader::scan(workdir.join("skills"))); // 空目录 -> 空
        // TaskStore::new 会把传入目录与 current_dir 比较以拦越界，tempdir 不在工作区，
        // 故用 cfg-test 的 create_test_store 直接装配（绕过校验）。
        let task_store = Arc::new(crate::task_system::store::create_test_store(&workdir));
        let bg_manager = Arc::new(BackgroundManager::new(
            workdir.join(".task_outputs").join("background"),
        ));
        let todo_manager = Arc::new(SharedTodoManager::new(TodoManager::new()));
        let registry = Arc::new(tools::build_registry());
        let hooks = Agent::build_hooks(&bg_manager, &todo_manager);
        let team = Arc::new(
            crate::team::TeamCtx::new(workdir.clone(), Arc::clone(&task_store)).unwrap(),
        );
        let compactor = ContextCompactor::new(
            workdir.join(".transcripts"),
            workdir.join(".task_outputs").join("tool-results"),
        );
        let memory = MemoryStore::new_read_only(workdir.join(".memory"));

        let agent = Agent {
            client,
            registry,
            skills,
            task_store,
            bg_manager,
            todo_manager,
            workdir,
            cron_manager: None,
            compactor,
            memory,
            hooks,
            base_system: "test system".into(),
            max_turns: usize::MAX,
            kind: AgentKind::Lead,
            owner: "agent".to_string(),
            team: Some(team),
            max_tokens: MAX_TOKENS,
        };
        Self { _tmp: tmp, agent }
    }

    pub fn context(&self) -> ToolContext<'_> {
        ToolContext { agent: &self.agent }
    }

    pub fn agent(&self) -> &Agent {
        &self.agent
    }
}

#[cfg(test)]
impl Default for TestAgent {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tools::trait_def::ToolResult;
    use serde_json::json;
    use std::sync::Arc;

    #[test]
    fn test_agent_constructs_isolated() {
        // 全局单例已消除：可在同进程构造多个互不污染的 Agent（旧 OnceLock 不可并行）。
        let a = TestAgent::new();
        assert!(a.agent().kind == AgentKind::Lead);
        assert_eq!(a.agent().max_turns, usize::MAX);
        assert!(a.agent().cron_manager.is_none()); // TestAgent 跳过 cron
        let _b = TestAgent::new(); // 第二个，互不干扰
    }

    #[test]
    fn child_agent_shares_infra_and_scopes_per_loop_state() {
        let a = TestAgent::new();
        let child = a.agent().child_agent(30, "sub");
        // 共享 infra：Arc 指针相同
        assert!(Arc::ptr_eq(&a.agent().client, &child.client));
        assert!(Arc::ptr_eq(&a.agent().registry, &child.registry));
        assert!(Arc::ptr_eq(&a.agent().task_store, &child.task_store));
        assert!(Arc::ptr_eq(&a.agent().bg_manager, &child.bg_manager));
        // per-loop 状态刷新
        assert!(child.kind == AgentKind::Subagent);
        assert_eq!(child.max_turns, 30);
        assert!(child.cron_manager.is_none()); // 子 agent 不投递定时任务
        // compactor/memory 始终有值，但子 agent 使用隔离目录和 read_only 模式
        assert_eq!(child.base_system, "sub");
    }

    #[tokio::test]
    async fn subagent_execute_tool_runs_pre_tool_denies_destructive() {
        // S2 回归：child agent 的 execute_tool 必须经 trigger_pre_tool。
        // 旧 subagent.rs 直接 registry.dispatch(for_subagent=true) 旁路了 pre_tool。
        let a = TestAgent::new();
        let child = a.agent().child_agent(30, "sub");
        let result = child.execute_tool("command", &json!({"command": "rm -rf /"})).await;
        assert!(
            matches!(result, ToolResult::Denied { .. }),
            "destructive command must be denied via pre_tool, got {:?}",
            result
        );
    }

    #[tokio::test]
    async fn agent_new_propagates_task_store_failure() {
        // S8 回归：TaskStore 构造失败时 Agent::new 返回 Err（旧 LazyLock 在首次工具调用 panic）。
        let file = tempfile::NamedTempFile::new().expect("tempfile");
        let cfg = AgentConfig {
            api_key: "k".into(),
            base_url: "http://localhost".into(),
            model: "m".into(),
            workdir: file.path().to_path_buf(),
            skills_dir: file.path().join("skills"),
        };
        let result = Agent::new(cfg).await;
        assert!(
            result.is_err(),
            "Agent::new should fail when task store can't be created"
        );
    }

    #[tokio::test]
    async fn execute_tool_plan_gate_blocks_command_for_teammate() {
        // s13: a teammate with gate=Pending cannot run command; the gate sits
        // after pre_tool, so a non-destructive command is blocked by the gate.
        let tmp = tempfile::TempDir::new().unwrap();
        let store = Arc::new(crate::task_system::store::create_test_store(tmp.path()));
        let team = Arc::new(crate::team::TeamCtx::new(tmp.path().to_path_buf(), store).unwrap());
        team.protocols
            .set_gate("alice", crate::team::protocols::GateStatus::Pending);
        let a = TestAgent::new();
        let child = a.agent().child_teammate("alice", "sub", Arc::clone(&team));
        let r = child
            .execute_tool("command", &serde_json::json!({"command": "ls"}))
            .await;
        assert!(
            matches!(r, ToolResult::Denied { .. }),
            "teammate command must be gated, got {:?}",
            r
        );
    }

    #[tokio::test]
    async fn execute_tool_allows_command_when_approved() {
        let tmp = tempfile::TempDir::new().unwrap();
        let store = Arc::new(crate::task_system::store::create_test_store(tmp.path()));
        let team = Arc::new(crate::team::TeamCtx::new(tmp.path().to_path_buf(), store).unwrap());
        team.protocols
            .set_gate("alice", crate::team::protocols::GateStatus::Approved);
        let a = TestAgent::new();
        let child = a.agent().child_teammate("alice", "sub", Arc::clone(&team));
        // child has no assignment -> ctx.cwd() would error for a teammate, but
        // the gate passes; command runs (via workdir()) and is not Denied.
        let r = child
            .execute_tool("command", &serde_json::json!({"command": "echo hi"}))
            .await;
        assert!(
            !matches!(r, ToolResult::Denied { .. }) || r.as_content().contains("Claim a Task"),
            "should not be gated when approved, got {:?}",
            r
        );
    }
}
