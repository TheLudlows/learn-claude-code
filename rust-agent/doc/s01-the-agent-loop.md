# s01: Agent 循环

> 一个工具 + 一个循环 = 一个 Agent。

语言模型能推理代码, 却碰不到真实世界——读不了文件、跑不了测试、看不见报错。如果没有循环, 每次工具调用都得由人手动把结果粘回对话, 你自己就成了那个循环。Agent 的本质, 就是把这个"人肉循环"交给程序去做。

## 循环的结构

整个 Agent 是一个闭合回路: 用户的 prompt 进入模型, 模型决定调用工具, 工具在真实世界里执行, 结果再被送回模型, 模型据此决定下一步。回路持续运转, 直到模型不再需要工具为止。

```
User prompt → LLM → 工具执行 → tool_result 喂回 LLM → 循环, 直到 stop_reason != "tool_use"
```

整个流程只靠一个退出条件控制: 当 `stop_reason` 不再是 `"tool_use"`, 循环结束。换句话说, 模型什么时候不再调工具, 循环什么时候停——控制权完全在模型手里, 不在程序里。

## 消息如何累积

循环的每一步都在向同一个消息列表追加内容, 从不覆盖历史。用户输入是第一条 `user` 消息; 模型的回复作为 `assistant` 消息追加(其中可能含若干 `tool_use` 块); 而工具执行的结果, 则作为新的 `user` 消息追加, 每个结果用 `tool_use_id` 与当初的调用一一配对。

下一轮请求时, 模型能看到完整的来龙去脉: 原始问题、自己之前的判断、工具返回的事实。这种累积式上下文, 正是 Agent 能做多步推理的基础——它不是无记忆地一次次问答, 而是在一条不断变长的对话里推进。

## 核心代码

剥到最简, 整个循环不到 30 行:

```python
def agent_loop(query):
    messages = [{"role": "user", "content": query}]
    while True:
        response = client.messages.create(
            model=MODEL, system=SYSTEM, messages=messages,
            tools=TOOLS, max_tokens=8000,
        )
        messages.append({"role": "assistant", "content": response.content})
        if response.stop_reason != "tool_use":
            return
        results = []
        for block in response.content:
            if block.type == "tool_use":
                output = run_bash(block.input["command"])
                results.append({"type": "tool_result",
                                "tool_use_id": block.id, "content": output})
        messages.append({"role": "user", "content": results})
```

## 为什么这是地基

这一节只用了一个 `bash` 工具。后面所有章节——多工具分发、上下文压缩、子 Agent、安全校验——都是在这个循环之上叠加机制, 而循环本身的形状从不改变: 依旧是"调用模型 → 追加响应 → 执行工具 → 喂回结果"。理解了这 30 行, 就理解了 Agent 的骨架; 剩下的, 都是在往骨架上加肌肉。
