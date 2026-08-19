# s12 Cron Scheduler 设计文档

**日期**: 2026-08-19
**目的**: 在 Rust 实现定时任务调度器，按指定时间将 prompt 注入到 agent 循环

## 背景

s12 实现了一个 cron 调度器，允许用户使用标准的 5 字段 cron 表达式来安排 prompt 在特定时间执行。与 s11 的后台任务不同，s12 不执行命令，而是在到期时将 prompt 作为用户消息注入到 agent 循环中。

## 设计决策

### 1. 线程模型

**选择**: 单个 tokio interval 每 1 秒轮询

**原因**: 
- 代码库已大量使用 tokio
- 比 Python 版本的两个线程更简单
- 异步原生，利用 tokio 的调度

### 2. 持久化路径

**选择**: `.scheduled_tasks.json`（工作目录下）

**原因**: 与 Python 版本一致，便于跨语言参考

### 3. 模块结构

**选择**: 单个文件 `cron_scheduler.rs`

**原因**: 用户偏好，简化代码组织

### 4. 工具命名

**选择**: `schedule_cron`、`list_crons`、`cancel_cron`

**原因**: 与 Python 版本一致

### 5. 错误处理

**选择**: `Result<T, String>`

**原因**: 与 background_tasks 模块一致

## 架构

### 整体流程

```
                    每 1 秒
                    ┌─────┐
                    │轮询 │
                    └──┬──┘
                       │
          ┌────────────┴────────────┐
          │                         │
      到期任务?                   Agent 空闲?
          │                         │
          ↓                         ↓
    入队到 delivery_queue      从队列提取
                               注入 [Scheduled] prompt
```

### 核心组件

#### 1. CronJob 数据结构

```rust
pub struct CronJob {
    pub id: String,                    // 格式: cron_{8位hex}
    pub cron: String,                  // 5字段 cron 表达式
    pub prompt: String,                // 触发后注入的 prompt
    pub recurring: bool,               // 是否循环执行
    pub durable: bool,                 // 是否持久化到磁盘
    pub pending_delivery: bool,        // 是否已入队但未交付
    pub last_fired: Option<String>,    // 最后触发时间 "YYYY-MM-DD HH:MM"
}
```

#### 2. CronState 共享状态

```rust
struct CronState {
    jobs: HashMap<String, CronJob>,        // 所有定时任务
    delivery_queue: VecDeque<CronJob>,     // 待交付的任务
}

pub struct CronManager {
    state: Arc<Mutex<CronState>>,
    workdir: PathBuf,
}
```

#### 3. CronManager 方法

- `schedule(cron, prompt, recurring, durable)` - 创建定时任务
- `cancel(job_id)` - 取消定时任务
- `list()` - 列出所有任务
- `poll_due_jobs(moment)` - 检查到期任务并入队
- `consume_queue()` - 消费待交付队列
- `save_durable()` - 保存持久化任务到磁盘
- `load_durable()` - 从磁盘加载持久化任务

## Cron 表达式

### 格式

5 字段：分钟 小时 日 月 星期

示例：
- `* * * * *` - 每分钟
- `0 9 * * *` - 每天 09:00
- `*/5 * * * *` - 每 5 分钟
- `0 9 * * 1-5` - 工作日 09:00

### 支持的模式

- `*` - 匹配任意值
- `*/N` - 每 N 个单位
- `N` - 精确匹配
- `N-M` - 范围匹配
- `N,M,...` - 列表匹配

### 字段范围

| 字段 | 最小值 | 最大值 |
|------|--------|--------|
| 分钟 | 0 | 59 |
| 小时 | 0 | 23 |
| 日 | 1 | 31 |
| 月 | 1 | 12 |
| 星期 | 0 | 6 (0=周日) |

### 匹配逻辑

- day 和 weekday 的关系是 OR（只要任一匹配即可）
- 如果两者都是 `*`，则匹配
- 如果一个是 `*`，则匹配另一个
- 如果两者都指定，则任一匹配即触发

## 持久化

### 存储格式

`.scheduled_tasks.json` 存储所有 durable 任务：

```json
[
  {
    "id": "cron_a1b2c3d4",
    "cron": "0 9 * * *",
    "prompt": "run tests",
    "recurring": true,
    "durable": true,
    "pending_delivery": false,
    "last_fired": "2026-08-19 09:00"
  }
]
```

### 原子更新

使用临时文件 + `std::fs::rename()`（Unix）或 `std::fs::replace()`（跨平台）实现原子更新。

### 加载时机

- 启动时调用 `load_durable()`
- 加载失败时打印错误但不中断启动

## 运行时

### 启动

```rust
pub async fn start_runtime(manager: Arc<CronManager>) -> JoinHandle<()> {
    tokio::spawn(async move {
        let mut interval = tokio::time::interval(Duration::from_secs(1));
        loop {
            interval.tick().await;
            manager.poll_due_jobs(Local::now()).await;
        }
    })
}
```

### 交付机制

在 `agent_loop` 顶部调用 `collect_and_inject()`：

```rust
pub fn collect_and_inject(messages: &mut Vec<Message>) -> Option<usize> {
    let jobs = CRON_MANAGER.consume_queue();
    if jobs.is_empty() {
        return None;
    }
    let count = jobs.len();
    for job in jobs {
        messages.push(Message {
            role: "user".to_string(),
            content: vec![ContentBlock::Text {
                text: format!("[Scheduled] {}", job.prompt),
            }],
        });
    }
    Some(count)
}
```

### 交付确认

- 任务成功接收后：
  - recurring 任务：清除 `pending_delivery`
  - one-shot 任务：从 `jobs` 中移除
- 模型调用失败：任务放回 `delivery_queue`

## 工具

### ScheduleCronTool

创建定时任务。

输入参数：
- `cron` (string, required): 5字段 cron 表达式
- `prompt` (string, required): 触发后注入的 prompt
- `recurring` (boolean, optional): 是否循环，默认 true
- `durable` (boolean, optional): 是否持久化，默认 true

### ListCronsTool

列出所有定时任务。

输出格式：
```
cron_a1b2c3d4: 0 9 * * * -> run tests [recurring, durable]
```

### CancelCronTool

取消定时任务。

输入参数：
- `job_id` (string, required): 任务 ID

## 集成点

### 1. main.rs

```rust
// 在 main() 函数中启动运行时
let cron_manager = Arc::new(CronManager::new(workdir()));
let _cron_handle = rust_agent::cron_scheduler::start_runtime(cron_manager.clone()).await;

// 注册工具
registry.register(Box::new(rust_agent::cron_scheduler::ScheduleCronTool));
registry.register(Box::new(rust_agent::cron_scheduler::ListCronsTool));
registry.register(Box::new(rust_agent::cron_scheduler::CancelCronTool));
```

### 2. agent_loop

在循环顶部调用 `collect_and_inject()`：

```rust
// 在 agent_loop 顶部
let _ = rust_agent::cron_scheduler::collect_and_inject(messages);
```

### 3. tools/mod.rs

在 `build_registry()` 中注册工具。

## 错误处理

### 验证错误

- cron 表达式格式错误（字段数量、取值范围）
- prompt 为空
- 任务 ID 不存在

### 运行时错误

- 持久化失败：回滚状态变更
- 并发冲突：使用 `Arc<Mutex<>>` 保护共享状态

## 边界情况

### 1. 进程重启

- durable 任务从磁盘恢复
- 停机期间错过的执行时间不补跑
- `pending_delivery` 的任务重新入队

### 2. 并发执行

- `Arc<Mutex<>>` 保证线程安全
- 锁粒度小，持锁期短

### 3. 重复触发

- `last_fired` 记录每分钟触发标记
- 同一分钟内只触发一次

### 4. Agent 忙碌

- 到期任务先入队，等待 agent 空闲
- 下一次循环机会时交付

## 测试策略

### 单元测试

- Cron 表达式解析和匹配
- 字段验证
- ID 生成唯一性

### 集成测试

- 任务调度、取消、列出
- 持久化读写
- 到期触发和交付

## 依赖

新增依赖：
- 无（使用现有 tokio、serde 等）

## 参考资料

- Python s12 实现: `s12_cron_scheduler/code.py`
- Rust background_tasks 模块: `rust-agent/src/background_tasks/`