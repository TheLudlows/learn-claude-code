diff --git a/rust-agent/src/client.rs b/rust-agent/src/client.rs
index fee1869..3e2a7a9 100644
--- a/rust-agent/src/client.rs
+++ b/rust-agent/src/client.rs
@@ -1,4 +1,4 @@
-use crate::tools::ToolDefinition;
+use crate::tools_legacy::ToolDefinition;
 use futures_util::StreamExt;
 use serde::{Deserialize, Serialize};
 
diff --git a/rust-agent/src/hooks.rs b/rust-agent/src/hooks.rs
index 6af1aa8..58d680d 100644
--- a/rust-agent/src/hooks.rs
+++ b/rust-agent/src/hooks.rs
@@ -21,7 +21,7 @@ hooks.rs - 钩子系统 (s04)
 use std::sync::atomic::{AtomicUsize, Ordering};
 
 use crate::client::{ContentBlock, Message};
-use crate::tools::workdir;
+use crate::tools_legacy::workdir;
 
 // ---- 回调类型 ----
 pub type PromptHook = fn(&str);
diff --git a/rust-agent/src/lib.rs b/rust-agent/src/lib.rs
index 19200bb..6c676ec 100644
--- a/rust-agent/src/lib.rs
+++ b/rust-agent/src/lib.rs
@@ -1 +1,9 @@
-pub mod todo;
\ No newline at end of file
+pub mod client;
+pub mod hooks;
+pub mod output;
+pub mod permission;
+pub mod skills;
+pub mod subagent;
+pub mod todo;
+pub mod tools_legacy;
+pub mod tools;
\ No newline at end of file
diff --git a/rust-agent/src/main.rs b/rust-agent/src/main.rs
index 024ad7d..92d9bce 100644
--- a/rust-agent/src/main.rs
+++ b/rust-agent/src/main.rs
@@ -34,23 +34,14 @@ API 交互(请求构造 + 流式解析)在 client.rs;工具与分发在 tools.rs
 Key insight: the loop stays the same; only the four trigger points are wired in.
 */
 
-mod client;
-mod hooks;
-mod output;
-mod permission;
-mod skills;
-mod subagent;
-mod todo;
-mod tools;
-
-use client::{Client, ContentBlock, Message};
+use rust_agent::client::{Client, ContentBlock, Message};
+use rust_agent::hooks::{assemble_post_tool_messages, context_inject_hook, large_output_hook, summary_hook, todo_reminder_hook, Hooks};
+use rust_agent::permission::permission_hook;
 use dotenv::dotenv;
-use hooks::{assemble_post_tool_messages, context_inject_hook, large_output_hook, summary_hook, todo_reminder_hook, Hooks};
-use permission::permission_hook;
 use std::env;
 use std::io::{self, Write};
 use std::path::PathBuf;
-use tools::{dispatch_tool, get_tool_definitions};
+use rust_agent::tools_legacy::{dispatch_tool, get_tool_definitions};
 
 /// 执行单个工具调用（含 PreToolUse 拦截）。
 ///
@@ -71,7 +62,7 @@ async fn execute_tool(
     // 执行工具（PostToolUse 提醒由调用方注入，见 agent_loop）
     if name == "task" {
         if let Some(prompt) = input.get("prompt").and_then(|p| p.as_str()) {
-            subagent::run_subagent_loop(client, prompt, hooks).await.unwrap_or_else(|e| format!("Subagent error: {}", e))
+            rust_agent::subagent::run_subagent_loop(client, prompt, hooks).await.unwrap_or_else(|e| format!("Subagent error: {}", e))
         } else {
             "Error: missing prompt".to_string()
         }
@@ -99,7 +90,7 @@ async fn agent_loop(
         // 打印这一轮的 LLM 内容（text + tool_use）；client 自身不打印。
         {
             let mut out = io::stdout().lock();
-            output::render(&response, &mut out);
+            rust_agent::output::render(&response, &mut out);
         }
 
         // 添加助手响应(含 text 与 tool_use 块, 原样回传给下一轮)
@@ -130,7 +121,7 @@ async fn agent_loop(
                 // 打印工具执行结果（此前只喂回 LLM，用户看不到工具返回了什么）
                 {
                     let mut out = io::stdout().lock();
-                    output::render_tool_result(name, &tool_output, &mut out);
+                    rust_agent::output::render_tool_result(name, &tool_output, &mut out);
                 }
                 // PostToolUse: 提醒作为独立 user 消息注入，不进 tool_result
                 if let Some(msg) = hooks.trigger_post_tool(name, input, &tool_output) {
@@ -187,9 +178,9 @@ async fn main() -> Result<(), Box<dyn std::error::Error>> {
         .ok()
         .filter(|s| !s.trim().is_empty())
         .unwrap_or_else(|| format!("{}/skills", cwd));
-    let loader = skills::SkillLoader::scan(PathBuf::from(&skills_dir));
+    let loader = rust_agent::skills::SkillLoader::scan(PathBuf::from(&skills_dir));
     let skill_count = loader.len();
-    skills::set_instance(loader);
+    rust_agent::skills::set_instance(loader);
     println!(
         "Loaded {} skill(s) from {}",
         skill_count, skills_dir
@@ -197,7 +188,7 @@ async fn main() -> Result<(), Box<dyn std::error::Error>> {
 
     // 组装 system prompt：固定的 agent 指令 + 技能目录（非空才加）+ load_skill 提示。
     // 目录只在 system prompt 里（每次调用都付这点开销）；完整正文在 load_skill 的 tool_result 里按需加载。
-    let catalog = skills::catalog();
+    let catalog = rust_agent::skills::catalog();
     let system = if catalog.is_empty() {
         format!(
             "You are a coding agent at {} on {}. Before starting any multi-step task, use todo_write to plan your steps. Update status as you go. You can use tools as needed.",
@@ -223,8 +214,8 @@ async fn main() -> Result<(), Box<dyn std::error::Error>> {
     let mut messages: Vec<Message> = Vec::new();
 
     // 初始化 TodoManager 并设置全局实例
-    let todo_manager = todo::TodoManager::new();
-    todo::set_instance(todo_manager);
+    let todo_manager = rust_agent::todo::TodoManager::new();
+    rust_agent::todo::set_instance(todo_manager);
 
     loop {
         print!("\x1b[36m You >> \x1b[0m");
diff --git a/rust-agent/src/permission.rs b/rust-agent/src/permission.rs
index 933b1fc..1d80b57 100644
--- a/rust-agent/src/permission.rs
+++ b/rust-agent/src/permission.rs
@@ -15,7 +15,7 @@ Option<String>(Some=拦截理由, None=放行), 注册为 PreToolUse 钩子,
 文件类工具另有 tools::safe_path 做工作区沙箱(defense in depth)。
 */
 
-use crate::tools::workdir;
+use crate::tools_legacy::workdir;
 use std::io::{self, Write};
 use std::path::{Component, PathBuf};
 
diff --git a/rust-agent/src/subagent.rs b/rust-agent/src/subagent.rs
index 173c569..908df1c 100644
--- a/rust-agent/src/subagent.rs
+++ b/rust-agent/src/subagent.rs
@@ -10,7 +10,7 @@
 
 use crate::client::{Client, ContentBlock, Message};
 use crate::hooks::{assemble_post_tool_messages, Hooks};
-use crate::tools::{dispatch_tool, get_subagent_tool_definitions};
+use crate::tools_legacy::{dispatch_tool, get_subagent_tool_definitions};
 
 /// 子 agent 的最大轮数限制
 const MAX_SUBAGENT_TURNS: usize = 30;
diff --git a/rust-agent/src/tools/mod.rs b/rust-agent/src/tools/mod.rs
new file mode 100644
index 0000000..7e360f1
--- /dev/null
+++ b/rust-agent/src/tools/mod.rs
@@ -0,0 +1,11 @@
+/*
+tools/mod.rs - Tool system module
+
+This module contains the tool system infrastructure:
+- trait_def: Core abstractions (Tool trait, ToolContext, PermissionCheck)
+- (Future modules will contain individual tool implementations)
+*/
+
+pub mod trait_def;
+
+pub use trait_def::{PermissionCheck, Tool, ToolContext};
\ No newline at end of file
diff --git a/rust-agent/src/tools/trait_def.rs b/rust-agent/src/tools/trait_def.rs
new file mode 100644
index 0000000..ca74a25
--- /dev/null
+++ b/rust-agent/src/tools/trait_def.rs
@@ -0,0 +1,258 @@
+/*
+trait_def.rs - Core tool abstractions
+
+This module defines the foundational types and traits for the tool system:
+- PermissionCheck: Permission check results
+- ToolContext: Dependency injection context for tools
+- Tool: Async trait that all tools must implement
+*/
+
+use async_trait::async_trait;
+use serde_json::Value;
+
+/// Permission check result from a tool's permission check
+#[derive(Debug, Clone, PartialEq)]
+pub enum PermissionCheck {
+    /// Tool can be executed without approval
+    Pass,
+    /// Tool requires user approval with a reason
+    NeedsApproval(&'static str),
+}
+
+/// Context provided to tools during execution
+///
+/// This struct provides dependency injection for tools, giving them access
+/// to shared resources like the HTTP client and hooks system.
+pub struct ToolContext<'a> {
+    /// HTTP client for making API requests
+    pub client: &'a crate::client::Client,
+    /// Hooks system for registering and triggering callbacks
+    pub hooks: &'a crate::hooks::Hooks,
+}
+
+/// Async trait that all tools must implement
+///
+/// This trait defines the interface that all tools in the system must follow.
+/// Tools are responsible for:
+/// - Defining their metadata (name, description, input schema)
+/// - Checking if they require approval before execution
+/// - Executing their logic asynchronously
+#[async_trait]
+pub trait Tool: Send + Sync {
+    /// Returns the tool's name (used for dispatch and identification)
+    fn name(&self) -> &str;
+
+    /// Returns a human-readable description of what the tool does
+    fn description(&self) -> &str;
+
+    /// Returns the JSON schema for the tool's input parameters
+    ///
+    /// This schema is used to validate tool calls and inform the AI model
+    /// about the expected input format.
+    fn input_schema(&self) -> Value;
+
+    /// Checks if the tool requires approval before execution
+    ///
+    /// Default implementation returns `PermissionCheck::Pass`, meaning
+    /// no approval is required. Tools can override this to implement
+    /// custom permission logic.
+    ///
+    /// # Arguments
+    /// * `input` - The parsed input JSON that will be passed to execute
+    ///
+    /// # Returns
+    /// * `PermissionCheck::Pass` - Tool can be executed immediately
+    /// * `PermissionCheck::NeedsApproval(reason)` - User must approve with the given reason
+    fn check_permission(&self, _input: &Value) -> PermissionCheck {
+        PermissionCheck::Pass
+    }
+
+    /// Executes the tool with the given input and context
+    ///
+    /// # Arguments
+    /// * `ctx` - Execution context providing access to shared resources
+    /// * `input` - The parsed input JSON for this tool call
+    ///
+    /// # Returns
+    /// The tool's output as a string, which will be sent back to the AI model
+    async fn execute(&self, ctx: &ToolContext<'_>, input: &Value) -> String;
+
+    /// Indicates whether this tool should be available to subagents
+    ///
+    /// Default implementation returns `true`. Some tools (like the task tool)
+    /// should only be available to the main agent to prevent infinite recursion.
+    ///
+    /// # Returns
+    /// `true` if the tool should be available to subagents, `false` otherwise
+    fn available_for_subagent(&self) -> bool {
+        true
+    }
+}
+
+#[cfg(test)]
+mod tests {
+    use super::*;
+    use serde_json::json;
+
+    struct MockTool {
+        name: String,
+        description: String,
+        schema: Value,
+    }
+
+    #[async_trait]
+    impl Tool for MockTool {
+        fn name(&self) -> &str {
+            &self.name
+        }
+
+        fn description(&self) -> &str {
+            &self.description
+        }
+
+        fn input_schema(&self) -> Value {
+            self.schema.clone()
+        }
+
+        async fn execute(&self, _ctx: &ToolContext<'_>, _input: &Value) -> String {
+            format!("Executed {}", self.name)
+        }
+    }
+
+    struct RestrictedTool {
+        name: String,
+    }
+
+    #[async_trait]
+    impl Tool for RestrictedTool {
+        fn name(&self) -> &str {
+            &self.name
+        }
+
+        fn description(&self) -> &str {
+            "A tool that requires approval"
+        }
+
+        fn input_schema(&self) -> Value {
+            json!({
+                "type": "object",
+                "properties": {
+                    "action": { "type": "string" }
+                },
+                "required": ["action"]
+            })
+        }
+
+        fn check_permission(&self, input: &Value) -> PermissionCheck {
+            if input.get("action").and_then(|v| v.as_str()) == Some("dangerous") {
+                PermissionCheck::NeedsApproval("This action requires explicit approval")
+            } else {
+                PermissionCheck::Pass
+            }
+        }
+
+        async fn execute(&self, _ctx: &ToolContext<'_>, input: &Value) -> String {
+            format!("Executed restricted tool with action: {:?}", input)
+        }
+
+        fn available_for_subagent(&self) -> bool {
+            false
+        }
+    }
+
+    #[test]
+    fn test_permission_check_pass() {
+        let check = PermissionCheck::Pass;
+        assert_eq!(check, PermissionCheck::Pass);
+    }
+
+    #[test]
+    fn test_permission_check_needs_approval() {
+        let check = PermissionCheck::NeedsApproval("Reason for approval");
+        match check {
+            PermissionCheck::NeedsApproval(reason) => {
+                assert_eq!(reason, "Reason for approval");
+            }
+            PermissionCheck::Pass => panic!("Expected NeedsApproval"),
+        }
+    }
+
+    #[test]
+    fn test_mock_tool_basic_traits() {
+        let tool = MockTool {
+            name: "test_tool".to_string(),
+            description: "A test tool".to_string(),
+            schema: json!({
+                "type": "object",
+                "properties": {
+                    "param": { "type": "string" }
+                },
+                "required": ["param"]
+            }),
+        };
+
+        assert_eq!(tool.name(), "test_tool");
+        assert_eq!(tool.description(), "A test tool");
+        assert_eq!(tool.available_for_subagent(), true);
+    }
+
+    #[test]
+    fn test_mock_tool_default_permission_check() {
+        let tool = MockTool {
+            name: "test_tool".to_string(),
+            description: "A test tool".to_string(),
+            schema: json!({}),
+        };
+
+        let input = json!({"param": "value"});
+        assert_eq!(tool.check_permission(&input), PermissionCheck::Pass);
+    }
+
+    #[test]
+    fn test_restricted_tool_custom_permission_check() {
+        let tool = RestrictedTool {
+            name: "restricted".to_string(),
+        };
+
+        let safe_input = json!({"action": "safe"});
+        assert_eq!(tool.check_permission(&safe_input), PermissionCheck::Pass);
+
+        let dangerous_input = json!({"action": "dangerous"});
+        match tool.check_permission(&dangerous_input) {
+            PermissionCheck::NeedsApproval(reason) => {
+                assert!(reason.contains("explicit approval"));
+            }
+            PermissionCheck::Pass => panic!("Expected NeedsApproval for dangerous action"),
+        }
+    }
+
+    #[test]
+    fn test_restricted_tool_not_available_for_subagent() {
+        let tool = RestrictedTool {
+            name: "restricted".to_string(),
+        };
+
+        assert_eq!(tool.available_for_subagent(), false);
+    }
+
+    #[test]
+    fn test_input_schema_format() {
+        let tool = MockTool {
+            name: "schema_test".to_string(),
+            description: "Test schema".to_string(),
+            schema: json!({
+                "type": "object",
+                "properties": {
+                    "required_field": { "type": "string" },
+                    "optional_field": { "type": "integer" }
+                },
+                "required": ["required_field"]
+            }),
+        };
+
+        let schema = tool.input_schema();
+        assert_eq!(schema["type"], "object");
+        assert_eq!(schema["required"].as_array().unwrap().len(), 1);
+        assert_eq!(schema["properties"]["required_field"]["type"], "string");
+    }
+}
\ No newline at end of file
diff --git a/rust-agent/src/tools.rs b/rust-agent/src/tools_legacy.rs
similarity index 99%
rename from rust-agent/src/tools.rs
rename to rust-agent/src/tools_legacy.rs
index 819b697..2df3083 100644
--- a/rust-agent/src/tools.rs
+++ b/rust-agent/src/tools_legacy.rs
@@ -578,7 +578,7 @@ pub fn get_tool_definitions() -> Vec<ToolDefinition> {
 
 #[cfg(test)]
 mod test_tool {
-    use crate::tools::run_glob;
+    use crate::tools_legacy::run_glob;
 
     #[test]
     fn test_glob() {
