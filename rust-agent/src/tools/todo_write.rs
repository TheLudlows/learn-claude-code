/*
todo_write.rs - Todo Write Tool Implementation

This module implements the TodoWriteTool for updating todo tasks.
- Implements Tool trait for todo management operations
- Uses run_todo_write() from todo.rs
- Default permission Pass
- Default available_for_subagent
*/

use crate::tools::trait_def::{PermissionCheck, Tool, ToolContext};
use async_trait::async_trait;
use serde_json::Value;

/// Todo Write Tool for updating todo tasks
///
/// This tool allows the AI agent to manage a todo list.
/// It accepts an array of todo items with content and status.
pub struct TodoWriteTool;

#[async_trait]
impl Tool for TodoWriteTool {
    /// Returns the tool's name
    fn name(&self) -> &str {
        "todo_write"
    }

    /// Returns a human-readable description
    fn description(&self) -> &str {
        "Update the todo list with new tasks. Accepts an array of todo items with content and status."
    }

    /// Returns the JSON schema for todo_write input
    fn input_schema(&self) -> Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "todos": {
                    "type": "array",
                    "description": "Array of todo items to update",
                    "items": {
                        "type": "object",
                        "properties": {
                            "content": {
                                "type": "string",
                                "description": "Description of the todo task"
                            },
                            "status": {
                                "type": "string",
                                "description": "Status of the task: 'pending', 'in_progress', or 'completed'",
                                "enum": ["pending", "in_progress", "completed"]
                            }
                        },
                        "required": ["content"]
                    }
                }
            },
            "required": ["todos"]
        })
    }

    /// Checks permission - always allow todo write operations
    fn check_permission(&self, _input: &Value) -> PermissionCheck {
        // Default: allow todo write operations
        PermissionCheck::Pass
    }

    /// Executes the todo write using run_todo_write() from todo.rs
    async fn execute(&self, _ctx: &ToolContext<'_>, input: &Value) -> String {
        match input.get("todos").and_then(|v| v.as_array()) {
            Some(_) => {}, // We just need to validate it's an array
            None => return "Error: todos must be an array".to_string(),
        };

        // Execute the todo write using the shared run_todo_write function
        crate::todo::run_todo_write(input)
    }

    /// Todo write tool should be available to subagents
    fn available_for_subagent(&self) -> bool {
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn test_todo_write_tool_name() {
        let tool = TodoWriteTool;
        assert_eq!(tool.name(), "todo_write");
    }

    #[test]
    fn test_todo_write_tool_description() {
        let tool = TodoWriteTool;
        assert!(tool.description().contains("Update the todo list"));
    }

    #[test]
    fn test_todo_write_tool_schema() {
        let tool = TodoWriteTool;
        let schema = tool.input_schema();

        assert_eq!(schema["type"], "object");
        assert!(schema["properties"].is_object());
        assert_eq!(schema["properties"]["todos"]["type"], "array");
        assert!(schema["properties"]["todos"]["items"].is_object());

        let required = schema["required"].as_array().unwrap();
        assert_eq!(required.len(), 1);
        assert_eq!(required[0], "todos");
    }

    #[test]
    fn test_permission_check() {
        let tool = TodoWriteTool;

        // Any input should pass (todo operations don't need special permissions)
        let test_inputs = vec![
            json!({"todos": []}),
            json!({"todos": [{"content": "Test task"}]}),
            json!({"todos": [{"content": "Test task", "status": "pending"}]}),
            json!({}),
        ];

        for input in test_inputs {
            match tool.check_permission(&input) {
                PermissionCheck::Pass => {} // Expected
                PermissionCheck::NeedsApproval(reason) => {
                    panic!("Todo write should not need approval: {:?} - {}", input, reason);
                }
            }
        }
    }

    #[test]
    fn test_available_for_subagent() {
        let tool = TodoWriteTool;
        assert!(tool.available_for_subagent());
    }
}