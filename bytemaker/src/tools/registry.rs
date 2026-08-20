/*
registry.rs - Tool Registry

This module implements the ToolRegistry that manages tool registration, dispatch,
and definition generation. It provides:
- Tool registration and storage
- Dynamic dispatch for tool execution
- ToolDefinition generation for API integration
- Permission checking before execution
*/

use std::collections::BTreeMap;

use crate::tools::trait_def::{PermissionCheck, Tool, ToolDefinition, ToolContext, ToolResult};
use serde_json::Value;

/// Registry for managing and dispatching tools
///
/// This struct holds all registered tools and provides methods for
/// tool discovery, execution, and permission checking.
pub struct ToolRegistry {
    /// Collection of registered tools stored as a BTreeMap keyed by tool name.
    /// BTreeMap 保证按名称排序、确定性的迭代顺序，查找 O(log n)。
    tools: BTreeMap<String, Box<dyn Tool>>,
}

impl ToolRegistry {
    /// Create a new empty tool registry
    pub fn new() -> Self {
        Self { tools: BTreeMap::new() }
    }

    /// Register a new tool in the registry
    ///
    /// The tool's name is extracted at registration time and used as the key.
    /// If a tool with the same name is already registered, it will be replaced.
    ///
    /// # Arguments
    /// * `tool` - A boxed tool implementing the Tool trait
    pub fn register(&mut self, tool: Box<dyn Tool>) {
        let name = tool.name().to_string();
        self.tools.insert(name, tool);
    }

    /// Dispatch a tool call by name
    ///
    /// `for_subagent` 为 `true` 时（子 agent 上下文），对 `available_for_subagent() == false`
    /// 的工具返回 `ToolResult::Rejected` 而非执行——`definitions_for_subagent` 已在**声明层**
    /// 过滤掉这类工具，此处在**派发层**再挡一道，防止模型幻觉出 `task` 调用导致子 agent 递归委托。
    ///
    /// # Arguments
    /// * `name` - The name of the tool to dispatch
    /// * `ctx` - The execution context
    /// * `input` - The parsed input JSON for the tool call
    /// * `for_subagent` - 是否在子 agent 上下文中派发
    ///
    /// # Returns
    /// * `ToolResult::Output(result)` - 工具执行成功
    /// * `ToolResult::Rejected` - 子 agent 上下文调用受限工具
    /// * `ToolResult::NotFound` - 工具未注册
    pub async fn dispatch(
        &self,
        name: &str,
        ctx: &ToolContext<'_>,
        input: &Value,
        for_subagent: bool,
    ) -> ToolResult {
        match self.tools.get(name) {
            Some(tool) => {
                if for_subagent && !tool.available_for_subagent() {
                    ToolResult::Rejected {
                        name: name.to_string(),
                        reason: "Tool is not available in subagent context".to_string(),
                    }
                } else {
                    ToolResult::Output(tool.execute(ctx, input).await)
                }
            }
            None => {
                let available: Vec<String> = self.tools.keys().cloned().collect();
                ToolResult::NotFound {
                    name: name.to_string(),
                    available,
                }
            }
        }
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
        self.tools.get(name).map(|tool| tool.check_permission(input))
    }

    /// Generate ToolDefinition list for all registered tools
    ///
    /// Definitions are returned in alphabetical order by tool name (BTreeMap 的确定性排序)。
    ///
    /// # Returns
    /// A vector of ToolDefinition structs for API integration
    pub fn definitions(&self) -> Vec<ToolDefinition> {
        self.tools
            .values()
            .map(|tool| ToolDefinition {
                name: tool.name().to_string(),
                description: tool.description().to_string(),
                input_schema: tool.input_schema(),
            })
            .collect()
    }

    /// Generate ToolDefinition list for tools available to subagents
    ///
    /// Definitions are returned in alphabetical order by tool name.
    ///
    /// # Returns
    /// A vector of ToolDefinition structs for subagent API integration
    pub fn definitions_for_subagent(&self) -> Vec<ToolDefinition> {
        self.tools
            .values()
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
        self.tools.contains_key(name)
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

    #[tokio::test]
    async fn test_registry_dispatch_known_tool() {
        use crate::agent::TestAgent;
        use crate::tools::trait_def::ToolResult;

        let mut registry = ToolRegistry::new();
        registry.register(Box::new(TestTool {
            name: "known_tool".to_string(),
            description: "A known tool".to_string(),
        }));

        let input = json!({"input": "test_value"});

        // ctx.agent 来自隔离的 TestAgent（tempdir），TestTool::execute 不读 ctx。
        let tagent = TestAgent::new();
        let ctx = tagent.context();

        let result = registry.dispatch("known_tool", &ctx, &input, false).await;

        assert!(matches!(result, ToolResult::Output(_)));
        assert_eq!(result.as_content(), "TestTool executed with: Object {\"input\": String(\"test_value\")}");
        assert!(result.was_executed());
    }

    #[tokio::test]
    async fn test_registry_dispatch_unknown_tool() {
        use crate::agent::TestAgent;

        let registry = ToolRegistry::new();
        let input = json!({"test": "value"});

        let tagent = TestAgent::new();
        let ctx = tagent.context();

        let result = registry.dispatch("unknown_tool", &ctx, &input, false).await;

        assert!(matches!(&result, ToolResult::NotFound { name: _, available: _ }));
        assert!(!result.was_executed());
    }
    
    #[test]
    fn test_registry_new_empty() {
        let registry = ToolRegistry::new();
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

    #[test]
    fn test_registry_definitions_includes_all() {
        let mut registry = ToolRegistry::new();

        // Register multiple tools
        registry.register(Box::new(TestTool {
            name: "command".to_string(),
            description: "Execute shell commands".to_string(),
        }));

        registry.register(Box::new(TestTool {
            name: "read_file".to_string(),
            description: "Read file contents".to_string(),
        }));

        registry.register(Box::new(TestTool {
            name: "write_file".to_string(),
            description: "Write file contents".to_string(),
        }));

        let definitions = registry.definitions();

        // Check that all registered tools are included
        assert_eq!(definitions.len(), 3);
        assert!(definitions.iter().any(|def| def.name == "command"));
        assert!(definitions.iter().any(|def| def.name == "read_file"));
        assert!(definitions.iter().any(|def| def.name == "write_file"));

        // Check that the definitions contain correct information
        for def in definitions {
            assert!(!def.name.is_empty());
            assert!(!def.description.is_empty());
            assert!(def.input_schema.is_object());
        }
    }

    #[test]
    fn test_registry_definitions_subagent_excludes_task() {
        use crate::tools::task::TaskTool;

        let mut registry = ToolRegistry::new();

        // Register various tools including TaskTool
        registry.register(Box::new(TestTool {
            name: "command".to_string(),
            description: "Execute shell commands".to_string(),
        }));

        registry.register(Box::new(TestTool {
            name: "read_file".to_string(),
            description: "Read file contents".to_string(),
        }));

        // TaskTool should not be available to subagents
        registry.register(Box::new(TaskTool));

        let all_definitions = registry.definitions();
        let subagent_definitions = registry.definitions_for_subagent();

        // All tools should be in the full definitions
        assert_eq!(all_definitions.len(), 3);

        // TaskTool should be excluded from subagent definitions
        assert_eq!(subagent_definitions.len(), 2);
        assert!(subagent_definitions.iter().any(|def| def.name == "command"));
        assert!(subagent_definitions.iter().any(|def| def.name == "read_file"));
        assert!(!subagent_definitions.iter().any(|def| def.name == "task"));
    }

    #[tokio::test]
    async fn test_dispatch_for_subagent_rejects_task() {
        // A1 残留回归：dispatch 的 for_subagent=true 必须在「派发层」挡掉 task——
        // 否则模型幻觉出 task 调用时子 agent 仍会递归委托。
        use crate::agent::TestAgent;
        use crate::tools::task::TaskTool;
        use crate::tools::trait_def::ToolResult;

        let mut registry = ToolRegistry::new();
        registry.register(Box::new(TaskTool));

        let tagent = TestAgent::new();
        let ctx = tagent.context();

        // 子 agent 上下文派发 task：返回 Rejected，不执行（不触发递归、不触网络）
        let result = registry
            .dispatch("task", &ctx, &json!({"prompt": "recurse"}), true)
            .await;
        assert!(matches!(&result, ToolResult::Rejected { name: _, reason: _ }), "dispatch should return Rejected, got {:?}", result);
        assert!(
            result.as_content().contains("not available in subagent context"),
            "subagent dispatch of task must be rejected, got: {}",
            result.as_content()
        );
        assert!(!result.was_executed(), "rejected tool must not be marked as executed");
    }

    #[test]
    fn test_registry_check_permission_override() {
        let mut registry = ToolRegistry::new();
        registry.register(Box::new(RestrictedTestTool {
            name: "restricted_tool".to_string(),
        }));

        // Test with input that requires approval
        let restricted_input = json!({"action": "restricted"});
        let result = registry.check_permission("restricted_tool", &restricted_input);

        assert!(result.is_some());
        match result.unwrap() {
            PermissionCheck::NeedsApproval(reason) => {
                assert!(reason.contains("approval"));
            }
            PermissionCheck::Pass => panic!("Expected NeedsApproval for restricted action"),
        }

        // Test with input that doesn't require approval
        let safe_input = json!({"action": "safe"});
        let safe_result = registry.check_permission("restricted_tool", &safe_input);

        assert!(safe_result.is_some());
        assert_eq!(safe_result.unwrap(), PermissionCheck::Pass);
    }
}