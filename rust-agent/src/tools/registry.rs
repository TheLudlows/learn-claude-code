/*
registry.rs - Tool Registry

This module implements the ToolRegistry that manages tool registration, dispatch,
and definition generation. It provides:
- Tool registration and storage
- Dynamic dispatch for tool execution
- ToolDefinition generation for API integration
- Permission checking before execution
*/

use crate::tools::trait_def::{PermissionCheck, Tool, ToolContext};
use crate::tools_legacy::ToolDefinition;
use serde_json::Value;

/// Registry for managing and dispatching tools
///
/// This struct holds all registered tools and provides methods for
/// tool discovery, execution, and permission checking.
pub struct ToolRegistry {
    /// Collection of registered tools stored as trait objects
    tools: Vec<Box<dyn Tool>>,
}

impl ToolRegistry {
    /// Create a new empty tool registry
    pub fn new() -> Self {
        Self { tools: Vec::new() }
    }

    /// Register a new tool in the registry
    ///
    /// # Arguments
    /// * `tool` - A boxed tool implementing the Tool trait
    pub fn register(&mut self, tool: Box<dyn Tool>) {
        self.tools.push(tool);
    }

    /// Dispatch a tool call by name
    ///
    /// # Arguments
    /// * `name` - The name of the tool to dispatch
    /// * `ctx` - The execution context
    /// * `input` - The parsed input JSON for the tool call
    ///
    /// # Returns
    /// * `Some(result)` - The tool's output if the tool was found
    /// * `None` - If no tool with the given name was registered
    pub async fn dispatch(
        &self,
        name: &str,
        ctx: &ToolContext<'_>,
        input: &Value,
    ) -> Option<String> {
        for tool in &self.tools {
            if tool.name() == name {
                return Some(tool.execute(ctx, input).await);
            }
        }
        None
    }

    /// Check permission for a tool call
    ///
    /// # Arguments
    /// * `name` - The name of the tool to check
    /// * `input` - The parsed input JSON for the tool call
    ///
    /// # Returns
    /// * `Some(permission_check)` - The permission result if the tool was found
    /// * `None` - If no tool with the given name was registered
    pub fn check_permission(&self, name: &str, input: &Value) -> Option<PermissionCheck> {
        for tool in &self.tools {
            if tool.name() == name {
                return Some(tool.check_permission(input));
            }
        }
        None
    }

    /// Generate ToolDefinition list for all registered tools
    ///
    /// # Returns
    /// A vector of ToolDefinition structs for API integration
    pub fn definitions(&self) -> Vec<ToolDefinition> {
        self.tools
            .iter()
            .map(|tool| ToolDefinition {
                name: tool.name().to_string(),
                description: tool.description().to_string(),
                input_schema: tool.input_schema(),
            })
            .collect()
    }

    /// Generate ToolDefinition list for tools available to subagents
    ///
    /// # Returns
    /// A vector of ToolDefinition structs for subagent API integration
    pub fn definitions_for_subagent(&self) -> Vec<ToolDefinition> {
        self.tools
            .iter()
            .filter(|tool| tool.available_for_subagent())
            .map(|tool| ToolDefinition {
                name: tool.name().to_string(),
                description: tool.description().to_string(),
                input_schema: tool.input_schema(),
            })
            .collect()
    }

    /// Check if a tool with the given name is registered
    ///
    /// # Arguments
    /// * `name` - The name to check
    ///
    /// # Returns
    /// `true` if a tool with the given name is registered, `false` otherwise
    pub fn has_tool(&self, name: &str) -> bool {
        self.tools.iter().any(|tool| tool.name() == name)
    }

    /// Get the number of registered tools
    ///
    /// # Returns
    /// The count of registered tools
    pub fn tool_count(&self) -> usize {
        self.tools.len()
    }
}

impl Default for ToolRegistry {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use async_trait::async_trait;
    use serde_json::json;

    // Mock tool for testing
    struct TestTool {
        name: String,
        description: String,
    }

    #[async_trait]
    impl Tool for TestTool {
        fn name(&self) -> &str {
            &self.name
        }

        fn description(&self) -> &str {
            &self.description
        }

        fn input_schema(&self) -> Value {
            json!({
                "type": "object",
                "properties": {
                    "input": { "type": "string" }
                },
                "required": ["input"]
            })
        }

        async fn execute(&self, _ctx: &ToolContext<'_>, input: &Value) -> String {
            format!("TestTool executed with: {:?}", input)
        }
    }

    // Mock restricted tool for testing
    struct RestrictedTestTool {
        name: String,
    }

    #[async_trait]
    impl Tool for RestrictedTestTool {
        fn name(&self) -> &str {
            &self.name
        }

        fn description(&self) -> &str {
            "A restricted test tool"
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
            if input.get("action").and_then(|v| v.as_str()) == Some("restricted") {
                PermissionCheck::NeedsApproval("This action requires approval")
            } else {
                PermissionCheck::Pass
            }
        }

        async fn execute(&self, _ctx: &ToolContext<'_>, input: &Value) -> String {
            format!("RestrictedTestTool executed with: {:?}", input)
        }

        fn available_for_subagent(&self) -> bool {
            false
        }
    }

    // Create a mock context for testing
    // Create a dummy context for tests
    // TODO: Implement proper mock context for async tests
    // fn create_mock_context() -> ToolContext<'static> {
    // }

    #[test]
    fn test_registry_new_empty() {
        let registry = ToolRegistry::new();
        assert_eq!(registry.tool_count(), 0);
    }

    #[test]
    fn test_registry_default() {
        let registry = ToolRegistry::default();
        assert_eq!(registry.tool_count(), 0);
    }

    #[test]
    fn test_register_single_tool() {
        let mut registry = ToolRegistry::new();
        let tool = Box::new(TestTool {
            name: "test_tool".to_string(),
            description: "A test tool".to_string(),
        });

        registry.register(tool);
        assert_eq!(registry.tool_count(), 1);
        assert!(registry.has_tool("test_tool"));
    }

    #[test]
    fn test_register_multiple_tools() {
        let mut registry = ToolRegistry::new();

        registry.register(Box::new(TestTool {
            name: "tool1".to_string(),
            description: "First tool".to_string(),
        }));

        registry.register(Box::new(TestTool {
            name: "tool2".to_string(),
            description: "Second tool".to_string(),
        }));

        registry.register(Box::new(TestTool {
            name: "tool3".to_string(),
            description: "Third tool".to_string(),
        }));

        assert_eq!(registry.tool_count(), 3);
        assert!(registry.has_tool("tool1"));
        assert!(registry.has_tool("tool2"));
        assert!(registry.has_tool("tool3"));
    }

    #[test]
    fn test_definitions_single_tool() {
        let mut registry = ToolRegistry::new();
        registry.register(Box::new(TestTool {
            name: "test_tool".to_string(),
            description: "A test tool".to_string(),
        }));

        let definitions = registry.definitions();
        assert_eq!(definitions.len(), 1);
        assert_eq!(definitions[0].name, "test_tool");
        assert_eq!(definitions[0].description, "A test tool");
        assert_eq!(definitions[0].input_schema["type"], "object");
    }

    #[test]
    fn test_definitions_multiple_tools() {
        let mut registry = ToolRegistry::new();

        registry.register(Box::new(TestTool {
            name: "tool1".to_string(),
            description: "First tool".to_string(),
        }));

        registry.register(Box::new(TestTool {
            name: "tool2".to_string(),
            description: "Second tool".to_string(),
        }));

        let definitions = registry.definitions();
        assert_eq!(definitions.len(), 2);
        assert_eq!(definitions[0].name, "tool1");
        assert_eq!(definitions[1].name, "tool2");
    }

    #[test]
    fn test_definitions_for_subagent_excludes_restricted() {
        let mut registry = ToolRegistry::new();

        registry.register(Box::new(TestTool {
            name: "normal_tool".to_string(),
            description: "Normal tool".to_string(),
        }));

        registry.register(Box::new(RestrictedTestTool {
            name: "restricted_tool".to_string(),
        }));

        let all_definitions = registry.definitions();
        let subagent_definitions = registry.definitions_for_subagent();

        assert_eq!(all_definitions.len(), 2);
        assert_eq!(subagent_definitions.len(), 1);
        assert_eq!(subagent_definitions[0].name, "normal_tool");
    }

    #[test]
    fn test_check_permission_pass() {
        let mut registry = ToolRegistry::new();
        registry.register(Box::new(TestTool {
            name: "test_tool".to_string(),
            description: "Test tool".to_string(),
        }));

        let input = json!({"input": "test"});
        let result = registry.check_permission("test_tool", &input);

        assert!(result.is_some());
        assert_eq!(result.unwrap(), PermissionCheck::Pass);
    }

    #[test]
    fn test_check_permission_needs_approval() {
        let mut registry = ToolRegistry::new();
        registry.register(Box::new(RestrictedTestTool {
            name: "restricted_tool".to_string(),
        }));

        let restricted_input = json!({"action": "restricted"});
        let result = registry.check_permission("restricted_tool", &restricted_input);

        assert!(result.is_some());
        match result.unwrap() {
            PermissionCheck::NeedsApproval(reason) => {
                assert!(reason.contains("approval"));
            }
            PermissionCheck::Pass => panic!("Expected NeedsApproval"),
        }
    }

    #[test]
    fn test_check_permission_unknown_tool() {
        let registry = ToolRegistry::new();
        let input = json!({"test": "value"});
        let result = registry.check_permission("unknown_tool", &input);

        assert!(result.is_none());
    }

    #[test]
    fn test_has_tool_existing() {
        let mut registry = ToolRegistry::new();
        registry.register(Box::new(TestTool {
            name: "existing_tool".to_string(),
            description: "Existing tool".to_string(),
        }));

        assert!(registry.has_tool("existing_tool"));
    }

    #[test]
    fn test_has_tool_non_existing() {
        let registry = ToolRegistry::new();
        assert!(!registry.has_tool("non_existing_tool"));
    }

    // TODO: Fix async test with proper mock context
    // #[tokio::test]
    // async fn test_dispatch_success() {
    //     let mut registry = ToolRegistry::new();
    //     registry.register(Box::new(TestTool {
    //         name: "dispatch_test".to_string(),
    //         description: "Dispatch test tool".to_string(),
    //     }));

    //     let input = json!({"input": "test_value"});
    //     let ctx = create_mock_context();

    //     let result = registry.dispatch("dispatch_test", &ctx, &input).await;

    //     assert!(result.is_some());
    //     assert!(result.unwrap().contains("dispatch_test"));
    // }

    // TODO: Fix async test with proper mock context
    // #[tokio::test]
    // async fn test_dispatch_unknown_tool() {
    //     let registry = ToolRegistry::new();
    //     let input = json!({"test": "value"});
    //     let ctx = create_mock_context();

    //     let result = registry.dispatch("unknown_tool", &ctx, &input).await;

    //     assert!(result.is_none());
    // }

    #[test]
    fn test_tool_count_accuracy() {
        let mut registry = ToolRegistry::new();

        assert_eq!(registry.tool_count(), 0);

        registry.register(Box::new(TestTool {
            name: "tool1".to_string(),
            description: "Tool 1".to_string(),
        }));
        assert_eq!(registry.tool_count(), 1);

        registry.register(Box::new(TestTool {
            name: "tool2".to_string(),
            description: "Tool 2".to_string(),
        }));
        assert_eq!(registry.tool_count(), 2);

        registry.register(Box::new(TestTool {
            name: "tool3".to_string(),
            description: "Tool 3".to_string(),
        }));
        assert_eq!(registry.tool_count(), 3);
    }
}