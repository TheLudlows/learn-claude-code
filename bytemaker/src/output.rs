//! 启动 logo 与 TODO 标题行渲染。
//!
//! 控制台 I/O 分离后，绝大多数 UX 输出（banner / status / error / blocked /
//! prompt / permission / render_tool_result）已收口为 `render::Coordinator`
//! 方法，经 `Backend` 写入滚动区；流式 token 走 `Coordinator::emit_partial`。
//! 本模块只保留两个尚无 Coordinator 对应物、且只写 stdout 的简单输出：
//! 启动 logo 与 TODO `heading` 标题行。着色走 `colored`，自动遵守 `NO_COLOR`。
//! 诊断行（[memory]/[snip_compact] 等）走 `tracing`，不在此处。

use colored::Colorize;

/// `NO_COLOR` 置位时关掉颜色（https://no-color.org，零依赖，方便 CI）。
pub(crate) fn colors_enabled() -> bool {
    std::env::var_os("NO_COLOR").is_none()
}

/// ByteMaker 启动 logo：5 行像素字标，cyan；`NO_COLOR` 置位时降级为原样。
///
/// 每个 glyph 固定 5 列宽，逐行用 2 空格拼接 9 个字母（B-y-t-e-M-a-k-e-r），
/// 保证各字母在每一行的列位对齐。整行拼好后 `trim_end` 只裁掉末字母右侧的
/// 占位空格（右缘参差，不影响对齐）。
pub fn logo() {
    let glyphs: [[&str; 5]; 9] = [
        // B
        ["#### ", "#   #", "#### ", "#   #", "#### "],
        // y
        ["#   #", " # # ", "  #  ", "  #  ", "   # "],
        // t
        ["  #  ", "#####", "  #  ", "  #  ", "  ## "],
        // e
        [" ####", "#   #", "#####", "#    ", " ### "],
        // M
        ["#   #", "## ##", "# # #", "#   #", "#   #"],
        // a
        [" ### ", "#   #", "#####", "#   #", "#   #"],
        // k
        ["#   #", "#  # ", "###  ", "#  # ", "#   #"],
        // e
        [" ####", "#   #", "#####", "#    ", " ### "],
        // r
        ["#### ", "#  # ", "#    ", "#    ", "#    "],
    ];
    let mut rows: [String; 5] = Default::default();
    for glyph in &glyphs {
        for (i, row) in glyph.iter().enumerate() {
            if !rows[i].is_empty() {
                rows[i].push_str("  ");
            }
            rows[i].push_str(row);
        }
    }
    // 保留每个 glyph 的完整 5 列宽以维持列对齐；只在整行拼好后裁掉末尾占位空格。
    for row in &mut rows {
        let len = row.trim_end().len();
        row.truncate(len);
    }
    let art = rows.join("\n");
    if colors_enabled() {
        println!("{}", art.cyan());
    } else {
        println!("{art}");
    }
}

/// 黄色标题 + 正文：`\n## {title}\n{body}`。
pub fn heading(title: &str, body: &str) {
    if colors_enabled() {
        println!("\n{}\n{}", format!("## {title}").yellow(), body);
    } else {
        println!("\n## {title}\n{body}");
    }
}
