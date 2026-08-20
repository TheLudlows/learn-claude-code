# bytemaker

## 安装

```bash
cargo build --release
```

## 配置

```bash
cp .env.example .env
# 编辑 .env 填入 ANTHROPIC_API_KEY 和 MODEL_ID
```

## 运行

```bash
cargo run --release
# 或
./target/release/bytemaker
```

## 示例提示词

1. `Read the file README.md and tell me what this project is about`
2. `Create a file called test.py that prints "hello", then read it back`
3. `Find all Python files in this directory`
4. `Refactor the file hello.py: add type hints, docstrings, and a main guard` (测试 TodoWrite)
5. `List all Python files and create a summary of what each does` (测试 Hooks)