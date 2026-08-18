/*
compact.rs - Compact Tool Implementation (s08)

This module implements the CompactTool for requesting context compaction.
- The tool itself is a marker: execute() returns a placeholder string.
- The actual compaction is special-cased in agent_loop (like task tool).
- After all tools in a batch finish, agent_loop runs compact_history().
- Not available to subagents (prevents nested compaction).
*/

use crate::tools::trait_def::{PermissionCheck, Tool, ToolContext};
use async_trait::async_trait;
use serde_json::Value;

/// Compact Tool for requesting context compaction
///
/// This tool allows the AI agent to request that the conversation history
/// be summarized to free up context space. The actual compaction happens
/// after the entire tool batch completes (handled in agent_loop).
pub struct CompactTool;

#[async_trait]
impl Tool for CompactTool {
    /// Returns the tool's name
    fn name(&self) -> &str {
        "compact"
    }

    /// Returns a human-readable description
    fn description(&self) -> &str {
        "Summarize earlier conversation to free context space. Use after completing a stage when the next stage needs only a summary."
    }

    /// Returns the JSON schema for compact input (no parameters)
    fn input_schema(&self) -> Value {
        serde_json::json!({
            "type": "object",
            "properties": {}
        })
    }

    /// Default permission: Pass - compaction request is safe
    fn check_permission(&self, _input: &Value) -> PermissionCheck {
        PermissionCheck::Pass
    }

    /// Returns a placeholder. The actual compaction is handled in agent_loop
    /// after all tools in the batch have finished executing.
    async fn execute(&self, _ctx: &ToolContext<'_>, _input: &Value) -> String {
        "Compaction requested after this tool batch.".to_string()
    }

    /// Compact tool should not be available to subagents
    fn available_for_subagent(&self) -> bool {
        false
    }
}
