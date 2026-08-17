/*
command.rs - Command Tool Implementation

This module implements the CommandTool for executing shell commands.
- Implements Tool trait for shell command execution
- Uses run_bash() from tools/mod.rs
- Has custom check_permission() for destructive commands
- Returns "NeedsApproval" for destructive commands
*/

use crate::tools::trait_def::{PermissionCheck, Tool, ToolContext};
use async_trait::async_trait;
use serde_json::Value;

/// Command Tool for executing shell commands
///
/// This tool allows the AI agent to execute shell commands in a controlled
/// environment. It includes safety checks for potentially destructive commands
/// and follows the Tool trait interface.
pub struct CommandTool;

#[async_trait]
impl Tool for CommandTool {
    /// Returns the tool's name
    fn name(&self) -> &str {
        "command"
    }

    /// Returns a human-readable description
    fn description(&self) -> &str {
        "Execute a shell command and return its output. Commands are run in a workspace-isolated environment with safety checks."
    }

    /// Returns the JSON schema for command input
    fn input_schema(&self) -> Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "command": {
                    "type": "string",
                    "description": "The shell command to execute (e.g., 'ls -la', 'git status')"
                }
            },
            "required": ["command"]
        })
    }

    /// Checks if the command requires approval for potentially destructive actions
    fn check_permission(&self, input: &Value) -> PermissionCheck {
        if let Some(command) = input.get("command").and_then(|v| v.as_str()) {
            // Check for destructive patterns
            let command_lower = command.to_lowercase();

            // Recursive operations on root
            if command_lower.contains("rm -rf /") || command_lower.starts_with("rm -rf/") {
                return PermissionCheck::NeedsApproval(
                    "This command performs a recursive delete from root. This will erase your system. This action requires explicit approval."
                );
            }

            // File deletion patterns
            if command_lower.contains("rm -rf ") || command_lower.contains("rm -rf/") {
                return PermissionCheck::NeedsApproval(
                    "This command performs a recursive delete. This action requires explicit approval."
                );
            }

            // Single file deletion to critical directories
            if (command_lower.contains("rm ") || command_lower.contains("rm -")) &&
               (command_lower.contains("/etc/") ||
                command_lower.contains("/usr/") ||
                command_lower.contains("/lib/") ||
                command_lower.contains("/bin/") ||
                command_lower.contains("/sbin/") ||
                command_lower.contains("/var/") ||
                command_lower.contains("/opt/") ||
                command_lower.contains("/boot/") ||
                command_lower.contains("/home/") ||
                command_lower.contains("/root/")) {
                return PermissionCheck::NeedsApproval(
                    "This command attempts to delete critical system files. This action requires explicit approval."
                );
            }

            // Critical system modifications
            if command_lower.contains("chmod 777 ") || command_lower.starts_with("chmod 777 ") {
                return PermissionCheck::NeedsApproval(
                    "This command grants broad permissions to files/folders. This action requires explicit approval."
                );
            }

            // Direct file overwrites to critical locations
            if (command_lower.contains(" > /etc/") ||
                command_lower.contains(" >> /etc/") ||
                command_lower.contains(" > /usr/") ||
                command_lower.contains(" >> /usr/") ||
                command_lower.contains(" > /lib/") ||
                command_lower.contains(" >> /lib/") ||
                command_lower.contains(" > /bin/") ||
                command_lower.contains(" >> /bin/") ||
                command_lower.contains(" > /sbin/") ||
                command_lower.contains(" >> /sbin/") ||
                command_lower.contains(" > /var/") ||
                command_lower.contains(" >> /var/") ||
                command_lower.contains(" > /opt/") ||
                command_lower.contains(" >> /opt/") ||
                command_lower.contains(" > /boot/") ||
                command_lower.contains(" >> /boot/")) &&
               !command_lower.contains(">/dev/null") {
                return PermissionCheck::NeedsApproval(
                    "This command attempts to overwrite critical system files. This action requires explicit approval."
                );
            }

            // Dangerous system operations
            if (command_lower.contains("fdisk ") ||
                command_lower.contains("mkfs ") ||
                command_lower.contains("dd ")) &&
               (command_lower.contains("/dev/sd") || command_lower.contains("/dev/hd")) {
                return PermissionCheck::NeedsApproval(
                    "This command modifies disk partitions or filesystems. This action requires explicit approval."
                );
            }
        }

        // Default: allow safe commands
        PermissionCheck::Pass
    }

    /// Executes the command using run_bash() from tools/mod.rs
    async fn execute(&self, _ctx: &ToolContext<'_>, input: &Value) -> String {
        if let Some(command) = input.get("command").and_then(|v| v.as_str()) {
            // Log the command execution (if hooks are available)
            // Note: In production, this would be handled through the hooks system
            // For now, we'll just execute the command

            // Execute the command using the shared run_bash function
            crate::tools::run_bash(command)
        } else {
            "Error: No command provided".to_string()
        }
    }

    /// Commands should be available to subagents with appropriate permission checks
    fn available_for_subagent(&self) -> bool {
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn test_command_tool_name() {
        let tool = CommandTool;
        assert_eq!(tool.name(), "command");
    }

    #[test]
    fn test_command_tool_description() {
        let tool = CommandTool;
        assert!(tool.description().contains("shell command"));
    }

    #[test]
    fn test_command_tool_schema() {
        let tool = CommandTool;
        let schema = tool.input_schema();

        assert_eq!(schema["type"], "object");
        assert!(schema["properties"].is_object());
        assert_eq!(schema["properties"]["command"]["type"], "string");

        let required = schema["required"].as_array().unwrap();
        assert_eq!(required.len(), 1);
        assert_eq!(required[0], "command");
    }

    #[test]
    fn test_permission_check_safe_commands() {
        let tool = CommandTool;

        // Safe commands should pass
        let safe_commands = vec![
            json!({"command": "ls -la"}),
            json!({"command": "git status"}),
            json!({"command": "cargo build"}),
            json!({"command": "echo hello world"}),
            json!({"command": "cat file.txt"}),
            json!({"command": "mkdir test_dir"}),
            json!({"command": "rm file.txt"}), // Single file removal
            json!({"command": "chmod 644 file.txt"}), // Safe permission change
        ];

        for cmd in safe_commands {
            match tool.check_permission(&cmd) {
                PermissionCheck::Pass => {} // Expected
                PermissionCheck::NeedsApproval(reason) => {
                    panic!("Safe command was rejected: {:?} - {}", cmd, reason);
                }
            }
        }
    }

    #[test]
    fn test_permission_check_destructive_commands() {
        let tool = CommandTool;

        // Destructive commands should need approval
        let destructive_commands = vec![
            json!({"command": "rm -rf /"}),
            json!({"command": "rm -rf /usr"}),
            json!({"command": "chmod 777 /etc"}),
            json!({"command": "echo 'danger' > /etc/passwd"}),
            json!({"command": "cat input.txt > /etc/config"}),
            json!({"command": "fdisk /dev/sda"}),
            json!({"command": "mkfs /dev/sda1"}),
            json!({"command": "dd if=/dev/zero of=/dev/sda"}),
        ];

        for cmd in destructive_commands {
            match tool.check_permission(&cmd) {
                PermissionCheck::NeedsApproval(reason) => {
                    // Should contain approval-related text
                    assert!(reason.contains("approval") || reason.contains("explicit approval"),
                           "Destructive command should mention approval: {:?}", cmd);
                }
                PermissionCheck::Pass => {
                    panic!("Destructive command was approved: {:?}", cmd);
                }
            }
        }
    }

    #[test]
    fn test_permission_case_insensitive() {
        let tool = CommandTool;

        // Test case sensitivity
        let cmd1 = json!({"command": "RM -rf /etc"});
        let cmd2 = json!({"command": "chmod 777 /usr"});

        match tool.check_permission(&cmd1) {
            PermissionCheck::NeedsApproval(_) => {} // Expected
            PermissionCheck::Pass => panic!("Case sensitive check failed"),
        }

        match tool.check_permission(&cmd2) {
            PermissionCheck::NeedsApproval(_) => {} // Expected
            PermissionCheck::Pass => panic!("Case sensitive check failed"),
        }
    }

    #[test]
    fn test_permission_subdir_protection() {
        let tool = CommandTool;

        // Test that critical subdirectories are protected
        let protected_commands = vec![
            json!({"command": "rm /etc/passwd"}),
            json!({"command": "rm -rf /usr/local"}),
            json!({"command": "rm -rf /lib/systemd"}),
            json!({"command": "chmod 777 /bin/bash"}),
            json!({"command": "echo test > /usr/share/file"}),
        ];

        for cmd in protected_commands {
            match tool.check_permission(&cmd) {
                PermissionCheck::NeedsApproval(_) => {} // Expected
                PermissionCheck::Pass => {
                    panic!("Protected command was approved: {:?}", cmd);
                }
            }
        }
    }

    #[test]
    fn test_permission_dev_null_allowed() {
        let tool = CommandTool;

        // Commands that redirect to /dev/null should be safe
        let dev_null_commands = vec![
            json!({"command": "echo 'test' > /dev/null"}),
            json!({"command": "some_command > /dev/null"}),
        ];

        for cmd in dev_null_commands {
            match tool.check_permission(&cmd) {
                PermissionCheck::Pass => {} // Expected
                PermissionCheck::NeedsApproval(reason) => {
                    panic!("/dev/null redirect was rejected: {:?} - {}", cmd, reason);
                }
            }
        }
    }
}