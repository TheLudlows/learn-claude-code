/*
task.rs - Task Tool Implementation

This module implements the TaskTool for running subagent tasks.
- Uses run_subagent_loop from subagent module
- Creates isolated subagent with specific prompt
- Default permission Pass
- Not available for subagents (avoids recursion)
*/

use crate::subagent::run_subagent_loop;
use crate::tools::trait_def::{PermissionCheck, Tool, ToolContext};
use async_trait::async_trait;
use serde_json::Value;

/// Task Tool for running subagent tasks
///
/// This tool allows the AI agent to delegate tasks to a subagent.
/// The subagent runs in isolation with a specific prompt and returns a summary.
pub struct TaskTool;

#[async_trait]
impl Tool for TaskTool {
    /// Returns the tool's name
    fn name(&self) -> &str {
        "task"
    }

    /// Returns a human-readable description
    fn description(&self) -> &str {
        "Run a subagent to complete a specific task. The subagent will execute independently and return a summary of the work completed."
    }

    /// Returns the JSON schema for task input
    fn input_schema(&self) -> Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "prompt": {
                    "type": "string",
                    "description": "The task prompt for the subagent to complete"
                },
                "max_turns": {
                    "type": "integer",
                    "description": "Maximum number of turns for the subagent (default: 30, max: 50)",
                    "minimum": 1,
                    "maximum": 50
                }
            },
            "required": ["prompt"]
        })
    }

    /// Default permission: Pass - task delegation is safe
    fn check_permission(&self, _input: &Value) -> PermissionCheck {
        PermissionCheck::Pass
    }

    /// Executes the task by running a subagent
    async fn execute(&self, ctx: &ToolContext<'_>, input: &Value) -> String {
        if let Some(prompt) = input.get("prompt").and_then(|v| v.as_str()) {
            let _max_turns = input.get("max_turns")
                .and_then(|v| v.as_u64())
                .unwrap_or(30) as usize;

            // Use the client from the tool context (it's guaranteed to be available)
            match run_subagent_loop(&ctx.client, &ctx.registry, prompt, &ctx.hooks).await {
                Ok(summary) => {
                    format!("Task completed:\n\n{}", summary)
                }
                Err(e) => {
                    format!("Error running subagent: {}", e)
                }
            }
        } else {
            "Error: No prompt provided".to_string()
        }
    }

    /// Task tool should not be available to subagents to avoid recursion
    fn available_for_subagent(&self) -> bool {
        false
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn test_task_tool_name() {
        let tool = TaskTool;
        assert_eq!(tool.name(), "task");
    }

    #[test]
    fn test_task_tool_description() {
        let tool = TaskTool;
        assert!(tool.description().contains("subagent"));
    }

    #[test]
    fn test_task_tool_schema() {
        let tool = TaskTool;
        let schema = tool.input_schema();

        assert_eq!(schema["type"], "object");
        assert!(schema["properties"].is_object());
        assert_eq!(schema["properties"]["prompt"]["type"], "string");
        assert_eq!(schema["properties"]["max_turns"]["type"], "integer");

        let required = schema["required"].as_array().unwrap();
        assert_eq!(required.len(), 1);
        assert_eq!(required[0], "prompt");
    }

    #[test]
    fn test_permission_check() {
        let tool = TaskTool;

        // Task tool should always pass
        let test_inputs = vec![
            json!({"prompt": "test task"}),
            json!({"prompt": "another test", "max_turns": 10}),
            json!({}),
            json!({"prompt": ""}),
        ];

        for input in test_inputs {
            match tool.check_permission(&input) {
                PermissionCheck::Pass => {} // Expected
                PermissionCheck::NeedsApproval(reason) => {
                    panic!("Task tool should not require approval: {:?}", reason);
                }
            }
        }
    }

    #[test]
    fn test_available_for_subagent() {
        let tool = TaskTool;
        assert!(!tool.available_for_subagent(), "Task tool should not be available to subagents");
    }
}