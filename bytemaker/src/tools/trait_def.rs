/*
trait_def.rs - Core tool abstractions

This module defines the foundational types and traits for the tool system:
- PermissionCheck: Permission check results
- ToolContext: Dependency injection context for tools
- Tool: Async trait that all tools must implement
*/

use async_trait::async_trait;
use serde_json::Value;
use crate::error::AgentError;

/// Which agent context a tool call is dispatched in (s13).
/// Drives tool visibility (Lead-only vs subagent/teammate) and the plan gate.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AgentKind {
    Lead,
    Subagent,
    Teammate,
}

/// Permission check result from a tool's permission check
#[derive(Debug, Clone, PartialEq)]
pub enum PermissionCheck {
    /// Tool can be executed without approval
    Pass,
    /// Tool requires user approval with a reason
    NeedsApproval(&'static str),
}

/// 工具执行的统一结果类型。
///
/// 把 dispatch 层的 "找不到工具" / "子 agent 调用受限工具"、
/// pre_tool 层的 "权限拦截"、execute 层的 "真实输出" / "执行失败"
/// 统一到一个枚举里，使用类型化错误而非 String 编码语义。
#[derive(Debug, Clone, PartialEq)]
pub enum ToolResult {
    /// 工具执行成功（包括工具内部返回的错误串，如 "Error: path required"）
    Output(String),
    /// dispatch 层：工具未注册
    NotFound { name: String, available: Vec<String> },
    /// dispatch 层：子 agent 上下文调用受限工具
    Rejected { name: String, reason: String },
    /// pre_tool hook 拦截（权限拒绝等）
    Denied { name: String, reason: String },
    /// 工具执行失败（类型化错误）
    Error(AgentError),
}

impl ToolResult {
    /// 不管哪种结果，最终都要作为 tool_result 喂给 LLM
    pub fn as_content(&self) -> String {
        match self {
            Self::Output(s) => s.clone(),
            Self::NotFound { name, available } => {
                format!(
                    "Error: tool '{}' not found. Available tools: {}",
                    name,
                    available.join(", ")
                )
            }
            Self::Rejected { name, reason } => {
                format!("Error: Tool '{}' rejected: {}", name, reason)
            }
            Self::Denied { name, reason } => {
                format!("Error: Tool '{}' denied: {}", name, reason)
            }
            Self::Error(e) => {
                format!("Error: {}", e)
            }
        }
    }

    /// 只有真正执行过工具（Output），才应该触发 PostToolUse hook
    pub fn was_executed(&self) -> bool {
        matches!(self, Self::Output(_))
    }

    /// 转换为 AgentError（仅对 Error 变体）
    pub fn into_error(self) -> Option<AgentError> {
        match self {
            Self::Error(e) => Some(e),
            _ => None,
        }
    }
}

/// Context provided to tools during execution
///
/// 单字段持有 `&Agent`：所有共享状态（client/hooks/registry/skills/todo/task_store/
/// bg_manager/cron_manager/workdir）都归 `Agent` 持有，工具经 `ctx.agent.*` 访问，
/// 不再读进程级全局单例（修 s13：消除 OnceLock/LazyLock 全局）。
pub struct ToolContext<'a> {
    pub agent: &'a crate::agent::Agent,
}

impl<'a> ToolContext<'a> {
    /// The owning agent's name (Lead/subagent use "agent"; teammates use their name).
    pub fn owner(&self) -> &str {
        &self.agent.owner
    }

    /// Resolve the caller's working directory.
    ///
    /// - No team context (s06 subagents): the repo workdir.
    /// - Lead (team=Some, no assignment): the repo workdir.
    /// - Teammate with an active assignment: the assignment's cwd (task worktree
    ///   in Phase 2, repo dir in Phase 1); fail-closed if the binding is broken.
    /// - Teammate with no assignment: Err("Claim a Task...") — teammates must
    ///   claim before touching the workspace.
    pub fn cwd(&self) -> Result<std::path::PathBuf, String> {
        match &self.agent.team {
            None => Ok(self.agent.workdir.clone()),
            Some(team) => {
                if team.assignments.get(&self.agent.owner).is_some() {
                    crate::team::assignment::assignment_cwd(
                        &team.workdir,
                        &team.task_store,
                        &team.assignments,
                        &self.agent.owner,
                    )
                } else if self.agent.kind == AgentKind::Teammate {
                    Err("Claim a Task before using workspace tools.".into())
                } else {
                    Ok(self.agent.workdir.clone())
                }
            }
        }
    }
}

/// Tool definition structure for API integration
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct ToolDefinition {
    pub name: String,
    pub description: String,
    pub input_schema: Value,
}

/// Async trait that all tools must implement
///
/// This trait defines the interface that all tools in the system must follow.
/// Tools are responsible for:
/// - Defining their metadata (name, description, input schema)
/// - Checking if they require approval before execution
/// - Executing their logic asynchronously
#[async_trait]
pub trait Tool: Send + Sync {
    /// Returns the tool's name (used for dispatch and identification)
    fn name(&self) -> &str;

    /// Returns a human-readable description of what the tool does
    fn description(&self) -> &str;

    /// Returns the JSON schema for the tool's input parameters
    ///
    /// This schema is used to validate tool calls and inform the AI model
    /// about the expected input format.
    fn input_schema(&self) -> Value;

    /// Checks if the tool requires approval before execution
    ///
    /// Default implementation returns `PermissionCheck::Pass`, meaning
    /// no approval is required. Tools can override this to implement
    /// custom permission logic.
    ///
    /// # Arguments
    /// * `input` - The parsed input JSON that will be passed to execute
    ///
    /// # Returns
    /// * `PermissionCheck::Pass` - Tool can be executed immediately
    /// * `PermissionCheck::NeedsApproval(reason)` - User must approve with the given reason
    fn check_permission(&self, _input: &Value) -> PermissionCheck {
        PermissionCheck::Pass
    }

    /// Executes the tool with the given input and context
    ///
    /// # Arguments
    /// * `ctx` - Execution context providing access to shared resources
    /// * `input` - The parsed input JSON for this tool call
    ///
    /// # Returns
    /// The tool's output as a string, which will be sent back to the AI model
    async fn execute(&self, ctx: &ToolContext<'_>, input: &Value) -> String;

    /// Indicates whether this tool should be available in the given agent context.
    ///
    /// Default returns `true` (available to Lead, Subagent, and Teammate).
    /// Restricted tools (e.g. `task` to prevent recursion, or Lead-only
    /// coordination tools) override this. Drives both the definition list sent
    /// to the model and the dispatch-layer `Rejected` short-circuit.
    fn available_for(&self, _kind: AgentKind) -> bool {
        true
    }
}

#[cfg(test)]
mod kind_tests {
    use super::*;
    use async_trait::async_trait;

    struct LeadOnly;
    #[async_trait]
    impl Tool for LeadOnly {
        fn name(&self) -> &str { "lead_only" }
        fn description(&self) -> &str { "lead only" }
        fn input_schema(&self) -> serde_json::Value { serde_json::json!({"type":"object","properties":{}}) }
        async fn execute(&self, _: &ToolContext<'_>, _: &serde_json::Value) -> String { "x".into() }
        fn available_for(&self, kind: AgentKind) -> bool { kind == AgentKind::Lead }
    }

    #[test]
    fn available_for_filters_by_kind() {
        let t = LeadOnly;
        assert!(t.available_for(AgentKind::Lead));
        assert!(!t.available_for(AgentKind::Teammate));
        assert!(!t.available_for(AgentKind::Subagent));
    }
}
