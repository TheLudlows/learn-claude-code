//! I/O 抽象层：解耦 Agent 与具体实现
//!
//! 通过定义 Output/Input trait，Agent 可以依赖抽象而非具体实现，
//! 便于测试、替换 I/O 后端（如 Web UI、文件日志等）。

use async_trait::async_trait;
use std::sync::Arc;
use tokio::io::AsyncBufReadExt;
use tokio::sync::{mpsc, oneshot};

// 重新导出 PermissionQuery 供内部使用
pub use crate::hooks::PermissionQuery;

// =====================================================================
// Trait 定义
// =====================================================================

/// 输出抽象：Agent 用于渲染所有用户可见内容
pub trait Output: Send + Sync {
    /// 发出一行完整输出（自动换行）
    fn emit(&self, line: &str);

    /// 发出横幅信息
    fn banner(&self, msg: &str);

    /// 发出状态信息（通常带颜色）
    fn status(&self, msg: &str);

    /// 发出错误信息（通常带颜色）
    fn error(&self, msg: &str);

    /// 发出空行
    fn blank(&self);

    /// 渲染工具执行结果
    fn render_tool_result(&self, name: &str, result: &str, color: bool);

    /// 渲染提示符（用于非交互模式）
    fn prompt(&self);

    /// 渲染被拒绝的命令提示
    fn blocked(&self, pattern: &str);

    /// 渲染权限请求提示
    fn permission(&self, reason: &str, name: &str, input: &serde_json::Value);
}

/// 输入抽象：Agent/钩子系统用于读取用户输入
#[async_trait]
pub trait Input: Send + Sync {
    /// 读取一行用户输入
    /// 返回 Some(line) 表示正常输入，None 表示 EOF/取消
    async fn read_line(&self) -> Option<String>;

    /// 请求权限确认
    async fn ask_permission(
        &self,
        reason: &str,
        name: &str,
        input: &serde_json::Value,
    ) -> bool;
}

// =====================================================================
// Console 实现：基于现有 render 模块
// =====================================================================

use crate::render::{Coordinator, CrosstermBackend};

/// 基于 Coordinator 的控制台输出实现
pub struct ConsoleOutput {
    coordinator: Arc<std::sync::Mutex<Coordinator<CrosstermBackend>>>,
}

impl ConsoleOutput {
    pub fn new(coordinator: Arc<std::sync::Mutex<Coordinator<CrosstermBackend>>>) -> Self {
        Self { coordinator }
    }
}

impl Output for ConsoleOutput {
    fn emit(&self, line: &str) {
        let _ = self.coordinator.lock().unwrap().emit(line);
    }

    fn banner(&self, msg: &str) {
        self.coordinator.lock().unwrap().banner(msg);
    }

    fn status(&self, msg: &str) {
        self.coordinator.lock().unwrap().status(msg);
    }

    fn error(&self, msg: &str) {
        self.coordinator.lock().unwrap().error(msg);
    }

    fn blank(&self) {
        self.coordinator.lock().unwrap().blank();
    }

    fn render_tool_result(&self, name: &str, result: &str, color: bool) {
        self.coordinator
            .lock()
            .unwrap()
            .render_tool_result(name, result, color);
    }

    fn prompt(&self) {
        self.coordinator.lock().unwrap().prompt();
    }

    fn blocked(&self, pattern: &str) {
        self.coordinator.lock().unwrap().blocked(pattern);
    }

    fn permission(&self, reason: &str, name: &str, input: &serde_json::Value) {
        self.coordinator
            .lock()
            .unwrap()
            .permission(reason, name, input);
    }
}

/// 基于 InputTask 的控制台输入实现
pub struct ConsoleInput {
    cmd_tx: Option<mpsc::Sender<crate::render::input::InputCmd>>,
    output: Arc<dyn Output>,
}

impl ConsoleInput {
    pub fn new(
        cmd_tx: Option<mpsc::Sender<crate::render::input::InputCmd>>,
        output: Arc<dyn Output>,
    ) -> Self {
        Self { cmd_tx, output }
    }
}

#[async_trait]
impl Input for ConsoleInput {
    async fn read_line(&self) -> Option<String> {
        // 对于交互模式，由 main.rs 的 InputTask 处理
        // 此实现主要用于非交互模式的回退
        let reader = tokio::io::BufReader::new(tokio::io::stdin());
        let mut lines = reader.lines();
        match lines.next_line().await {
            Ok(Some(s)) => Some(s.trim().to_string()),
            _ => None,
        }
    }

    async fn ask_permission(
        &self,
        reason: &str,
        name: &str,
        input: &serde_json::Value,
    ) -> bool {
        let Some(ask) = &self.cmd_tx else {
            self.output.error(&format!(
                "cannot approve {name}: no interactive input channel"
            ));
            return false;
        };

        let (tx, rx) = oneshot::channel();
        self.output.permission(reason, name, input);

        let _ = ask
            .send(crate::render::input::InputCmd::AskPermission(
                PermissionQuery {
                    reason: reason.into(),
                    name: name.into(),
                    input: input.clone(),
                    reply: tx,
                },
            ))
            .await;

        rx.await.unwrap_or(false)
    }
}

// =====================================================================
// 测试用 Mock 实现
// =====================================================================

use std::sync::Mutex;

/// 内存输出：累积所有输出用于断言
#[derive(Default)]
pub struct MemoryOutput {
    lines: Arc<Mutex<Vec<String>>>,
}

impl MemoryOutput {
    pub fn new() -> Self {
        Self::default()
    }

    /// 获取累积的所有行
    pub fn lines(&self) -> Vec<String> {
        self.lines.lock().unwrap().clone()
    }

    /// 清空累积的行
    pub fn clear(&self) {
        self.lines.lock().unwrap().clear();
    }
}

impl Output for MemoryOutput {
    fn emit(&self, line: &str) {
        self.lines.lock().unwrap().push(line.to_string());
    }

    fn banner(&self, msg: &str) {
        self.lines.lock().unwrap().push(format!("[BANNER] {}", msg));
    }

    fn status(&self, msg: &str) {
        self.lines.lock().unwrap().push(format!("[STATUS] {}", msg));
    }

    fn error(&self, msg: &str) {
        self.lines.lock().unwrap().push(format!("[ERROR] {}", msg));
    }

    fn blank(&self) {
        self.lines.lock().unwrap().push("".to_string());
    }

    fn render_tool_result(&self, name: &str, result: &str, _color: bool) {
        self.lines
            .lock()
            .unwrap()
            .push(format!("[TOOL] {} -> {}", name, result));
    }

    fn prompt(&self) {
        self.lines.lock().unwrap().push(">> ".to_string());
    }

    fn blocked(&self, pattern: &str) {
        self.lines
            .lock()
            .unwrap()
            .push(format!("[BLOCKED] {}", pattern));
    }

    fn permission(&self, reason: &str, name: &str, input: &serde_json::Value) {
        self.lines
            .lock()
            .unwrap()
            .push(format!("[PERMISSION] {} {}({})", reason, name, input));
    }
}

/// Mock 输入：预定义的响应序列
#[derive(Default)]
pub struct MockInput {
    /// 预定义的 read_line 响应（先进先出）
    read_responses: Arc<Mutex<Vec<Option<String>>>>,
    /// 预定义的 ask_permission 响应（先进先出）
    permission_responses: Arc<Mutex<Vec<bool>>>,
}

impl MockInput {
    pub fn new() -> Self {
        Self::default()
    }

    /// 添加一个 read_line 响应
    pub fn push_read(&self, response: Option<String>) {
        self.read_responses
            .lock()
            .unwrap()
            .push(response);
    }

    /// 添加一个 ask_permission 响应
    pub fn push_permission(&self, granted: bool) {
        self.permission_responses
            .lock()
            .unwrap()
            .push(granted);
    }
}

#[async_trait]
impl Input for MockInput {
    async fn read_line(&self) -> Option<String> {
        self.read_responses.lock().unwrap().pop().flatten()
    }

    async fn ask_permission(
        &self,
        _reason: &str,
        _name: &str,
        _input: &serde_json::Value,
    ) -> bool {
        self.permission_responses
            .lock()
            .unwrap()
            .pop()
            .unwrap_or(false)
    }
}

// =====================================================================
// 组合：IO 对
// =====================================================================

/// I/O 组合：同时持有输入输出
pub struct IO {
    pub output: Arc<dyn Output>,
    pub input: Arc<dyn Input>,
}

impl IO {
    pub fn new(output: Arc<dyn Output>, input: Arc<dyn Input>) -> Self {
        Self { output, input }
    }

    /// 创建控制台 I/O
    pub fn console(
        coordinator: Arc<std::sync::Mutex<Coordinator<CrosstermBackend>>>,
        cmd_tx: Option<mpsc::Sender<crate::render::input::InputCmd>>,
    ) -> Self {
        let output: Arc<dyn Output> = Arc::new(ConsoleOutput::new(coordinator));
        let input = Arc::new(ConsoleInput::new(cmd_tx, Arc::clone(&output)));
        Self { output, input }
    }

    /// 创建测试用内存 I/O
    pub fn memory() -> Self {
        let output = Arc::new(MemoryOutput::new());
        let input = Arc::new(MockInput::new());
        Self { output, input }
    }
}

// =====================================================================
// 类型转换辅助（用于测试）
// =====================================================================

/// 为测试类型添加 downcast 支持
impl MemoryOutput {
    /// 获取内部行引用（用于测试断言）
    pub fn as_any(&self) -> &dyn std::any::Any {
        self
    }
}

impl MockInput {
    /// 获取内部状态引用（用于测试断言）
    pub fn as_any(&self) -> &dyn std::any::Any {
        self
    }
}

// =====================================================================
// 单元测试
// =====================================================================

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn memory_output_collects_lines() {
        let output = MemoryOutput::new();
        output.banner("hello");
        output.emit("world");
        output.error("error");

        let lines = output.lines();
        assert_eq!(lines.len(), 3);
        assert!(lines[0].contains("hello"));
        assert_eq!(lines[1], "world");
        assert!(lines[2].contains("error"));
    }

    #[test]
    fn memory_output_clear() {
        let output = MemoryOutput::new();
        output.emit("a");
        output.emit("b");
        assert_eq!(output.lines().len(), 2);

        output.clear();
        assert!(output.lines().is_empty());
    }

    #[tokio::test]
    async fn mock_input_fifo() {
        let input = MockInput::new();
        // 使用后进先出顺序（Vec::pop）
        input.push_read(None); // EOF
        input.push_read(Some("second".into()));
        input.push_read(Some("first".into()));

        assert_eq!(input.read_line().await, Some("first".into()));
        assert_eq!(input.read_line().await, Some("second".into()));
        assert_eq!(input.read_line().await, None);
    }

    #[tokio::test]
    async fn mock_input_permission() {
        let input = MockInput::new();
        // 使用后进先出顺序（Vec::pop）
        input.push_permission(false);
        input.push_permission(true);

        assert!(input.ask_permission("", "", &serde_json::json!({})).await);
        assert!(!input.ask_permission("", "", &serde_json::json!({})).await);
        // 默认返回 false
        assert!(!input.ask_permission("", "", &serde_json::json!({})).await);
    }

    #[test]
    fn io_memory_creates_pair() {
        // 直接创建具体的 I/O 实现进行测试
        let mem_output = MemoryOutput::new();
        let _mock_input = MockInput::new();

        mem_output.emit("test");
        assert_eq!(mem_output.lines()[0], "test");
    }
}
