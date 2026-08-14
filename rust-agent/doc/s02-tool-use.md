# s02: 工具使用

> 加一个工具, 只加一个 handler——循环不用动, 新工具注册进 dispatch map 就行。

只有 `bash` 时, 一切操作都得走 shell: `cat` 截断的行数不可预测, `sed` 碰到特殊字符就崩, 而每一次 bash 调用都是一块不受约束的安全面。专用工具(`read_file`、`write_file`)的好处在于, 可以在工具这一层就做路径沙箱——把危险挡在执行之前, 而不是寄希望于 shell 自己乖巧。

## dispatch map: 一张表代替一串 if

核心机制是一张"工具名 → 处理函数"的映射表。循环拿到模型返回的 `tool_use` 块后, 不再用一长串 `if/elif` 去判断该调谁, 而是按名字查表, 一次查找就找到对应的 handler。

```python
TOOL_HANDLERS = {
    "bash":       lambda **kw: run_bash(kw["command"]),
    "read_file":  lambda **kw: run_read(kw["path"], kw.get("limit")),
    "write_file": lambda **kw: run_write(kw["path"], kw["content"]),
    "edit_file":  lambda **kw: run_edit(kw["path"], kw["old_text"], kw["new_text"]),
}
```

加一个工具, 就是给这张表添一行; 循环里那几行"取名字、查表、执行、塞回 tool_result"的代码, 一个字都不用改。这正是这一节的关键洞察: **循环不变, 只增 handler 和 schema**。

## 路径沙箱

文件类工具都先过一道 `safe_path`: 把传入的相对路径解析成绝对路径, 再确认它仍落在工作目录之内; 一旦试图 `../` 逃出去, 直接拒绝。这样无论模型怎么构造路径, 文件操作都被锁死在 workspace 里。

```python
def safe_path(p: str) -> Path:
    path = (WORKDIR / p).resolve()
    if not path.is_relative_to(WORKDIR):
        raise ValueError(f"Path escapes workspace: {p}")
    return path
```

## 循环里的分发

循环体与 s01 几乎逐字相同, 唯一的区别是"执行工具"那一步从硬编码的 `run_bash` 换成了查表分发:

```python
for block in response.content:
    if block.type == "tool_use":
        handler = TOOL_HANDLERS.get(block.name)
        output = handler(**block.input) if handler \
                 else f"Unknown tool: {block.name}"
        results.append({"type": "tool_result",
                         "tool_use_id": block.id, "content": output})
```

查不到名字也不崩——返回一句 `Unknown tool` 当作工具结果喂回去, 模型下一轮自己会换别的办法。整个系统的鲁棒性, 就藏在这些"失败也是合法输入"的小细节里。
