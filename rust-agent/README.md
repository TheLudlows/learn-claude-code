# rust-agent

Rust 版本的 s02 Tool Use 实现，对应 Python 版本的 `s02_tool_use/code.py`。

## 核心原理

```
+----------+      +-------+      +--------------------------+
|   User   | ---> |  LLM  | ---> | Tool Dispatch            |
|  prompt  |      |       |      | bash       -> run_bash   |
+----------+      +---+---+      | read_file  -> run_read   |
                          ^          | write_file -> run_write  |
                          |          | edit_file  -> run_edit   |
                          +----------| glob       -> run_glob   |
                          tool_result+--------------------------+
```

**关键洞察**: 循环保持不变；只有工具注册和分发机制在增长。

### 工具分发机制

```rust
fn dispatch_tool(tool_name: &str, input: &serde_json::Value) -> String {
    match tool_name {
        "bash" => run_bash(cmd),
        "read_file" => run_read_file(path, limit),
        "write_file" => run_write_file(path, content),
        "edit_file" => run_edit_file(path, old_text, new_text),
        "glob" => run_glob(pattern),
        _ => format!("Unknown tool: {}", tool_name),
    }
}
```

## 安装

```bash
cargo build --release
```

## 配置

复制 `.env.example` 为 `.env` 并填入配置：

```bash
cp .env.example .env
```

## 运行

```bash
cargo run --release
```

或者运行编译后的二进制文件：

```bash
./target/release/rust-agent
```

## 尝试的示例

1. `Read the file README.md and tell me what this project is about`
2. `Create a file called test.py that prints "hello", then read it back`
3. `Find all Python files in this directory`
4. `Read both README.md and requirements.txt, then create a summary file`