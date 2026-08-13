# rust-agent

Rust 版本的 Agent Loop 实现，对应 Python 版本的 `s01_agent_loop/code.py`。

## 核心原理

```
while stop_reason == "tool_use":
    response = LLM(messages, tools)
    execute tools
    append results
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

1. `Create a file called hello.py that prints "Hello, World!"`
2. `List all Python files in this directory`
3. `What is the current git branch?`