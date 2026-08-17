/*
tools/glob.rs - Glob tool implementation

This module provides a tool for file system glob pattern matching.
*/

use async_trait::async_trait;
use serde_json::Value;
use crate::tools::trait_def::{PermissionCheck, Tool, ToolContext};
use crate::tools::run_glob;

/// Glob tool for file system pattern matching
pub struct GlobTool;

#[async_trait]
impl Tool for GlobTool {
    /// Returns the tool's name
    fn name(&self) -> &str {
        "glob"
    }

    /// Returns a human-readable description
    fn description(&self) -> &str {
        "Fast file pattern matching tool that works with any codebase size. Supports glob patterns like \"**/*.js\" or \"src/**/*.ts\". Returns matching file paths sorted by modification time."
    }

    /// Returns the JSON schema for glob tool input
    fn input_schema(&self) -> Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "pattern": {
                    "type": "string",
                    "description": "The glob pattern to match files against"
                },
                "path": {
                    "type": "string",
                    "description": "The directory to search in. If not specified, the current working directory will be used. IMPORTANT: Omit this field to use the default directory. DO NOT enter \"undefined\" or \"null\" - simply omit it for the default behavior. Must be a valid directory path if provided."
                }
            },
            "required": ["pattern"]
        })
    }

    /// Checks if the tool requires approval before execution
    ///
    /// Glob tool has default permission: Pass (allow access)
    fn check_permission(&self, _input: &Value) -> PermissionCheck {
        PermissionCheck::Pass
    }

    /// Executes the glob tool with the given input and context
    async fn execute(&self, _ctx: &ToolContext<'_>, input: &Value) -> String {
        // Extract pattern
        let pattern = input["pattern"]
            .as_str()
            .unwrap_or("");

        // Extract optional path
        let search_path = input["path"].as_str().map(|s| s.to_string());

        // Execute the glob search
        if let Some(path) = search_path {
            // Use specified search path
            let path_str = path.as_str();
            let result = crate::tools::glob_in(pattern, std::path::Path::new(path_str));
            result.join("\n")
        } else {
            // Use default current working directory
            run_glob(pattern)
        }
    }

    /// Indicates whether this tool should be available to subagents
    ///
    /// Glob tool is available for subagents by default
    fn available_for_subagent(&self) -> bool {
        true
    }
}