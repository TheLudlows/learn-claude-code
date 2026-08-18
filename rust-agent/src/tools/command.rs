/*
command.rs - Command Tool Implementation

This module implements:
- CommandTool: Tool trait implementation for shell command execution
- run_bash(): Async cross-platform command execution with timeout
- decode_console(): Console output decoding (UTF-8 → OEM codepage → lossy)
*/

use crate::tools::trait_def::{PermissionCheck, Tool, ToolContext};
use async_trait::async_trait;
use serde_json::Value;
use tokio::time::{timeout, Duration};

/// Command execution timeout in seconds.
const COMMAND_TIMEOUT_SECS: u64 = 30;

/// Maximum output size in bytes before truncation.
const MAX_OUTPUT_BYTES: usize = 50_000;

/// 执行命令（跨平台，带超时）
///
/// - Windows: 使用 cmd.exe
/// - Unix: 使用 bash
///
/// 危险命令的拦截已移至 builtins::PermissionHook 闸门(s03/s04),
/// 在到达这里之前就已被拒; safe_path 仍是文件工具的工作区沙箱。
pub(crate) async fn run_bash(command: &str) -> String {
    let result = timeout(Duration::from_secs(COMMAND_TIMEOUT_SECS), async {
        if cfg!(windows) {
            tokio::process::Command::new("cmd.exe")
                .args(["/C", command])
                .current_dir(crate::tools::workdir())
                .output()
                .await
        } else {
            tokio::process::Command::new("bash")
                .arg("-c")
                .arg(command)
                .current_dir(crate::tools::workdir())
                .output()
                .await
        }
    })
    .await;

    match result {
        Ok(Ok(output)) => {
            let stdout = decode_console(&output.stdout);
            let stderr = decode_console(&output.stderr);
            let result = format!("{}\n{}", stdout, stderr).trim().to_string();
            if result.is_empty() {
                "(no output)".to_string()
            } else if result.len() > MAX_OUTPUT_BYTES {
                // 按字节上限截断，但必须落在 UTF-8 字符边界上，否则
                // `result[..end]` 会在多字节序列（CJK 输出极常见）中间 panic。
                let mut end = MAX_OUTPUT_BYTES;
                while !result.is_char_boundary(end) {
                    end -= 1;
                }
                result[..end].to_string()
            } else {
                result
            }
        }
        Ok(Err(e)) => format!("Error: {}", e),
        Err(_) => format!(
            "Error: command timed out after {} seconds",
            COMMAND_TIMEOUT_SECS
        ),
    }
}

/// 把命令输出字节解码成字符串：先按 UTF-8（cargo 等现代程序直接用 UTF-8），
/// 失败再按 GBK 解码（cmd.exe 内建命令、git 等在中文 locale 下用 GBK/代码页 936），
/// 都不行才退化为 lossy。避免非 ASCII 被替成 U+FFFD（乱码）。
///
/// 使用 `encoding_rs` 替代手写 Windows FFI（`GetOEMCP` / `MultiByteToWideChar`），
/// 零 unsafe、零分配（返回 `Cow<str>`）、跨平台。
pub(crate) fn decode_console(bytes: &[u8]) -> String {
    if let Ok(s) = std::str::from_utf8(bytes) {
        return s.to_string();
    }
    // 中文 Windows OEM 代码页为 936 (GBK)；encoding_rs::GBK 覆盖所有 GBK 字符。
    // 非中文 locale 下 GBK 解码可能产生乱码，但比 lossy 替换符（U+FFFD）好。
    let (decoded, _encoding, _had_errors) = encoding_rs::GBK.decode(bytes);
    decoded.into_owned()
}

/// Command Tool for executing shell commands
///
/// This tool allows the AI agent to execute shell commands in a controlled
/// environment. It includes safety checks for potentially destructive commands
/// and follows the Tool trait interface.
pub struct CommandTool;

#[async_trait]
impl Tool for CommandTool {
    fn name(&self) -> &str {
        "command"
    }

    fn description(&self) -> &str {
        "Execute a shell command and return its output. Commands are run in a workspace-isolated environment with safety checks."
    }

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

        PermissionCheck::Pass
    }

    async fn execute(&self, _ctx: &ToolContext<'_>, input: &Value) -> String {
        if let Some(command) = input.get("command").and_then(|v| v.as_str()) {
            run_bash(command).await
        } else {
            "Error: No command provided".to_string()
        }
    }

    fn available_for_subagent(&self) -> bool {
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tools::trait_def::PermissionCheck;
    use serde_json::json;

    // ---- CommandTool tests ----

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

        let safe_commands = vec![
            json!({"command": "ls -la"}),
            json!({"command": "git status"}),
            json!({"command": "cargo build"}),
            json!({"command": "echo hello world"}),
            json!({"command": "cat file.txt"}),
            json!({"command": "mkdir test_dir"}),
            json!({"command": "rm file.txt"}),
            json!({"command": "chmod 644 file.txt"}),
        ];

        for cmd in safe_commands {
            match tool.check_permission(&cmd) {
                PermissionCheck::Pass => {}
                PermissionCheck::NeedsApproval(reason) => {
                    panic!("Safe command was rejected: {:?} - {}", cmd, reason);
                }
            }
        }
    }

    #[test]
    fn test_permission_check_destructive_commands() {
        let tool = CommandTool;

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

        let cmd1 = json!({"command": "RM -rf /etc"});
        let cmd2 = json!({"command": "chmod 777 /usr"});

        match tool.check_permission(&cmd1) {
            PermissionCheck::NeedsApproval(_) => {}
            PermissionCheck::Pass => panic!("Case sensitive check failed"),
        }

        match tool.check_permission(&cmd2) {
            PermissionCheck::NeedsApproval(_) => {}
            PermissionCheck::Pass => panic!("Case sensitive check failed"),
        }
    }

    #[test]
    fn test_permission_subdir_protection() {
        let tool = CommandTool;

        let protected_commands = vec![
            json!({"command": "rm /etc/passwd"}),
            json!({"command": "rm -rf /usr/local"}),
            json!({"command": "rm -rf /lib/systemd"}),
            json!({"command": "chmod 777 /bin/bash"}),
            json!({"command": "echo test > /usr/share/file"}),
        ];

        for cmd in protected_commands {
            match tool.check_permission(&cmd) {
                PermissionCheck::NeedsApproval(_) => {}
                PermissionCheck::Pass => {
                    panic!("Protected command was approved: {:?}", cmd);
                }
            }
        }
    }

    #[test]
    fn test_permission_dev_null_allowed() {
        let tool = CommandTool;

        let dev_null_commands = vec![
            json!({"command": "echo 'test' > /dev/null"}),
            json!({"command": "some_command > /dev/null"}),
        ];

        for cmd in dev_null_commands {
            match tool.check_permission(&cmd) {
                PermissionCheck::Pass => {}
                PermissionCheck::NeedsApproval(reason) => {
                    panic!("/dev/null redirect was rejected: {:?} - {}", cmd, reason);
                }
            }
        }
    }

    // ---- run_bash async tests ----

    /// 回归：cmd.exe 在中文 locale 默认按 GBK(936) 输出，`from_utf8_lossy` 会把
    /// 非 ASCII（如 `ver` 输出里的 "版本"）替换成 U+FFFD（乱码）。强制 UTF-8 后
    /// 应为合法 UTF-8 中文，不含替换符。
    #[tokio::test]
    #[cfg(windows)]
    async fn decodes_non_ascii_without_replacement_chars() {
        let out = run_bash("ver").await;
        assert!(
            !out.contains('\u{FFFD}'),
            "命令输出不应含 U+FFFD 替换符（应为合法 UTF-8）: {out:?}"
        );
    }

    #[tokio::test]
    async fn run_bash_executes_simple_command() {
        let out = run_bash("echo hello_from_rust_agent").await;
        assert!(
            out.contains("hello_from_rust_agent"),
            "expected 'hello_from_rust_agent' in output, got: {}",
            out
        );
    }

    #[tokio::test]
    async fn run_bash_timeout_kills_long_command() {
        // Use a command that sleeps longer than COMMAND_TIMEOUT_SECS.
        // On Windows: ping -n sends one ping per second; -w 1000 waits 1s per ping.
        // On Unix: sleep N sleeps for N seconds.
        let cmd = if cfg!(windows) {
            "ping -n 120 127.0.0.1"
        } else {
            "sleep 120"
        };
        let out = run_bash(cmd).await;
        assert!(
            out.contains("timed out"),
            "expected timeout message, got: {}",
            out
        );
    }

    #[tokio::test]
    async fn run_bash_truncates_large_output() {
        // Generate output larger than MAX_OUTPUT_BYTES (50,000 bytes).
        // On Windows: use a for loop in cmd.exe.
        // On Unix: use head -c or python/yes.
        let cmd = if cfg!(windows) {
            "for /L %i in (1,1,10000) do @echo LINE_%i_PADDING_DATA_TO_MAKE_IT_LONGER_AAAAAAAAAAAAAAAAAAAAAAA"
        } else {
            "python3 -c \"print('A' * 100 * 1000)\""
        };
        let out = run_bash(cmd).await;
        // Output should be truncated to at most MAX_OUTPUT_BYTES
        assert!(
            out.len() <= MAX_OUTPUT_BYTES + 100, // small margin for UTF-8 boundary adjustment
            "output should be truncated, got {} bytes",
            out.len()
        );
    }
}
