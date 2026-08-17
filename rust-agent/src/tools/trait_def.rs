/*
trait_def.rs - Core tool abstractions

This module defines the foundational types and traits for the tool system:
- PermissionCheck: Permission check results
- ToolContext: Dependency injection context for tools
- Tool: Async trait that all tools must implement
*/

use async_trait::async_trait;
use serde_json::Value;

/// Permission check result from a tool's permission check
#[derive(Debug, Clone, PartialEq)]
pub enum PermissionCheck {
    /// Tool can be executed without approval
    Pass,
    /// Tool requires user approval with a reason
    NeedsApproval(&'static str),
}

/// Context provided to tools during execution
///
/// This struct provides dependency injection for tools, giving them access
/// to shared resources like the HTTP client, hooks system, and tool registry.
pub struct ToolContext<'a> {
    /// HTTP client for making API requests
    pub client: &'a crate::client::Client,
    /// Hooks system for registering and triggering callbacks
    pub hooks: &'a crate::hooks::Hooks,
    /// Tool registry for accessing registered tools
    pub registry: &'a crate::tools::registry::ToolRegistry,
}

/// Tool definition structure for API integration
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct ToolDefinition {
    pub name: String,
    pub description: String,
    pub input_schema: serde_json::Value,
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

    /// Indicates whether this tool should be available to subagents
    ///
    /// Default implementation returns `true`. Some tools (like the task tool)
    /// should only be available to the main agent to prevent infinite recursion.
    ///
    /// # Returns
    /// `true` if the tool should be available to subagents, `false` otherwise
    fn available_for_subagent(&self) -> bool {
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    struct MockTool {
        name: String,
        description: String,
        schema: Value,
    }

    #[async_trait]
    impl Tool for MockTool {
        fn name(&self) -> &str {
            &self.name
        }

        fn description(&self) -> &str {
            &self.description
        }

        fn input_schema(&self) -> Value {
            self.schema.clone()
        }

        async fn execute(&self, _ctx: &ToolContext<'_>, _input: &Value) -> String {
            format!("Executed {}", self.name)
        }
    }

    struct RestrictedTool {
        name: String,
    }

    #[async_trait]
    impl Tool for RestrictedTool {
        fn name(&self) -> &str {
            &self.name
        }

        fn description(&self) -> &str {
            "A tool that requires approval"
        }

        fn input_schema(&self) -> Value {
            json!({
                "type": "object",
                "properties": {
                    "action": { "type": "string" }
                },
                "required": ["action"]
            })
        }

        fn check_permission(&self, input: &Value) -> PermissionCheck {
            if input.get("action").and_then(|v| v.as_str()) == Some("dangerous") {
                PermissionCheck::NeedsApproval("This action requires explicit approval")
            } else {
                PermissionCheck::Pass
            }
        }

        async fn execute(&self, _ctx: &ToolContext<'_>, input: &Value) -> String {
            format!("Executed restricted tool with action: {:?}", input)
        }

        fn available_for_subagent(&self) -> bool {
            false
        }
    }

    #[test]
    fn test_permission_check_pass() {
        let check = PermissionCheck::Pass;
        assert_eq!(check, PermissionCheck::Pass);
    }

    #[test]
    fn test_permission_check_needs_approval() {
        let check = PermissionCheck::NeedsApproval("Reason for approval");
        match check {
            PermissionCheck::NeedsApproval(reason) => {
                assert_eq!(reason, "Reason for approval");
            }
            PermissionCheck::Pass => panic!("Expected NeedsApproval"),
        }
    }

    #[test]
    fn test_mock_tool_basic_traits() {
        let tool = MockTool {
            name: "test_tool".to_string(),
            description: "A test tool".to_string(),
            schema: json!({
                "type": "object",
                "properties": {
                    "param": { "type": "string" }
                },
                "required": ["param"]
            }),
        };

        assert_eq!(tool.name(), "test_tool");
        assert_eq!(tool.description(), "A test tool");
        assert_eq!(tool.available_for_subagent(), true);
    }

    #[test]
    fn test_mock_tool_default_permission_check() {
        let tool = MockTool {
            name: "test_tool".to_string(),
            description: "A test tool".to_string(),
            schema: json!({}),
        };

        let input = json!({"param": "value"});
        assert_eq!(tool.check_permission(&input), PermissionCheck::Pass);
    }

    #[test]
    fn test_restricted_tool_custom_permission_check() {
        let tool = RestrictedTool {
            name: "restricted".to_string(),
        };

        let safe_input = json!({"action": "safe"});
        assert_eq!(tool.check_permission(&safe_input), PermissionCheck::Pass);

        let dangerous_input = json!({"action": "dangerous"});
        match tool.check_permission(&dangerous_input) {
            PermissionCheck::NeedsApproval(reason) => {
                assert!(reason.contains("explicit approval"));
            }
            PermissionCheck::Pass => panic!("Expected NeedsApproval for dangerous action"),
        }
    }

    #[test]
    fn test_restricted_tool_not_available_for_subagent() {
        let tool = RestrictedTool {
            name: "restricted".to_string(),
        };

        assert_eq!(tool.available_for_subagent(), false);
    }

    #[test]
    fn test_input_schema_format() {
        let tool = MockTool {
            name: "schema_test".to_string(),
            description: "Test schema".to_string(),
            schema: json!({
                "type": "object",
                "properties": {
                    "required_field": { "type": "string" },
                    "optional_field": { "type": "integer" }
                },
                "required": ["required_field"]
            }),
        };

        let schema = tool.input_schema();
        assert_eq!(schema["type"], "object");
        assert_eq!(schema["required"].as_array().unwrap().len(), 1);
        assert_eq!(schema["properties"]["required_field"]["type"], "string");
    }
}