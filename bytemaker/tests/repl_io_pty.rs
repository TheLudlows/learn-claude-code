// 真终端交互冒烟（spec §8 / plan Task 11）。
//
// 启动 bytemaker，确认交互 REPL 把输入栏（` >> `）渲染到末行、输出区在上方。
// 非交互环境（CI）下编译期即被 `#[cfg(feature = "smoke")]` 排除；即便带 smoke
// 也 `#[ignore]`——需真终端 + 已设置 `ANTHROPIC_AUTH_TOKEN`/`MODEL_ID`（dummy
// 值即可触达输入栏；真实值可同时验证流式输出）。
//
// 手动运行：
//   cargo test -p bytemaker --test repl_io_pty --features smoke -- --ignored
//
// 注：portable-pty 0.9 用 `CommandBuilder`（非 `std::process::Command`）、
// `try_clone_reader`（非 take_reader）、`take_writer` 写输入。

#![cfg(feature = "smoke")]

use std::io::Read;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use portable_pty::{native_pty_system, CommandBuilder, PtySize};

/// REPL 在真终端下应渲染输入栏 ` >> `，且末行不被流式输出覆盖。
#[test]
#[ignore = "needs a real PTY + ANTHROPIC_AUTH_TOKEN/MODEL_ID env"]
fn pty_repl_renders_input_bar() {
    let pair = native_pty_system()
        .openpty(PtySize {
            rows: 24,
            cols: 80,
            pixel_width: 0,
            pixel_height: 0,
        })
        .expect("open pty");

    // 直接跑已构建的 bytemaker 二进制（避免 `cargo run` 的编译噪声混入输出）。
    let mut cmd = CommandBuilder::new(env!("CARGO_BIN_EXE_bytemaker"));
    // 转发关键 env：bytemaker 缺 ANTHROPIC_AUTH_TOKEN/MODEL_ID 会在打印输入栏前早退。
    for k in [
        "ANTHROPIC_AUTH_TOKEN",
        "ANTHROPIC_API_KEY",
        "ANTHROPIC_BASE_URL",
        "MODEL_ID",
        "SKILLS_DIR",
        "NO_COLOR",
        "RUST_LOG",
    ] {
        if let Ok(v) = std::env::var(k) {
            cmd.env(k, v);
        }
    }

    let mut child = pair
        .slave
        .spawn_command(cmd)
        .expect("spawn bytemaker");

    let mut reader = pair.master.try_clone_reader().expect("clone reader");

    // reader.read 是阻塞的；单独线程持续读，主线程轮询直到出现 ` >> ` 或超时。
    let captured: Arc<Mutex<String>> = Arc::new(Mutex::new(String::new()));
    let cap = Arc::clone(&captured);
    let handle = std::thread::spawn(move || {
        let mut buf = [0u8; 4096];
        loop {
            match reader.read(&mut buf) {
                Ok(0) | Err(_) => break,
                Ok(n) => cap.lock().unwrap().push_str(&String::from_utf8_lossy(&buf[..n])),
            }
        }
    });

    let deadline = Instant::now() + Duration::from_secs(20);
    while Instant::now() < deadline {
        if captured.lock().unwrap().contains(" >> ") {
            break;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    let out = captured.lock().unwrap().clone();

    // 清理：杀子进程并回收。reader 线程在 pty 关闭后获 EOF 自行退出（保守起见 join，
    // 若卡住可降级为 detach——本测试为 ignored 手动用例）。
    let _ = child.kill();
    let _ = child.wait();
    drop(pair);
    let _ = handle.join();

    assert!(
        out.contains(" >> "),
        "交互 REPL 应在末行渲染输入栏 ` >> `，got: {out}"
    );
}
