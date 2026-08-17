/*
skills.rs - Skill Loading (s07)

启动时扫描 `skills_dir` 下每个技能子目录里的 `SKILL.md`（即 `skills/<name>/SKILL.md`），解析 YAML frontmatter 的 name/description，
把「名称 + 描述」目录编入 system prompt（每次调用都付这点开销）；模型需要完整说明时
调用 `load_skill(name)`，返回的完整 `SKILL.md` 正文作为 `tool_result` 追加到消息列表。

    skills/                    启动时
    +------------------+       +------------------+
    | code-review/     | ----> | SkillLoader      |
    |   SKILL.md       |       | name + summary   |
    | pdf/             |       +--------+---------+
    |   SKILL.md       |                |
    +------------------+                v
                               system prompt catalog

    LLM -- load_skill(name) --> full SKILL.md
     ^                              |
     +--------- tool_result --------+

| 内容               | 进入模型的位置     | 何时加入            |
|-------------------|-------------------|---------------------|
| 技能名称和描述      | system prompt     | 启动时              |
| 完整 `SKILL.md`    | `tool_result`     | 调用 `load_skill` 时 |

全局访问机制沿用 `todo.rs` 的 `OnceLock` 模式。与 todo 不同：技能扫描后只读，
无需 `Mutex`，`OnceLock<SkillLoader>` 直接给出 `&'static`。

对应 Python 参考：s07_skill_loading/code.py 的 SkillLoader。
*/

use serde::Deserialize;
use std::collections::BTreeMap;
use std::fs;
use std::path::PathBuf;
use std::sync::OnceLock;

/// 单个技能：名称、描述、完整 `SKILL.md` 正文。
#[derive(Clone, Debug)]
pub struct Skill {
    pub name: String,
    pub description: String,
    /// `SKILL.md` 的完整原文（含 frontmatter），作为 `tool_result` 返回。
    pub content: String,
}

/// YAML frontmatter 中我们关心的字段（其余字段忽略）。两个字段都可选，缺省时回退。
#[derive(Default, Deserialize, Clone, Debug)]
struct SkillFrontmatter {
    name: Option<String>,
    description: Option<String>,
}

/// 技能加载器：启动时扫描一次，之后只读。
pub struct SkillLoader {
    /// 按 name 排序的注册表：目录顺序确定、查找 O(log n)；同名后者覆盖前者（与 Python dict 语义一致）。
    skills: BTreeMap<String, Skill>,
}

impl SkillLoader {
    /// 扫描 `skills_dir/*/SKILL.md`，构建注册表。
    ///
    /// `skills_dir` 不存在或不可读时返回空注册表（不 panic、不报错——agent 仍可运行，只是没有技能）。
    /// 仅扫一层直接子目录（对应 Python 的 `glob("*/SKILL.md")`），不递归——技能的
    /// `references/`、`scripts/` 子目录不会被当作技能。
    pub fn scan(skills_dir: PathBuf) -> SkillLoader {
        let mut skills: BTreeMap<String, Skill> = BTreeMap::new();

        if let Ok(entries) = fs::read_dir(&skills_dir) {
            for entry in entries.flatten() {
                let path = entry.path();
                if !path.is_dir() {
                    continue;
                }
                let manifest = path.join("SKILL.md");
                let content = match fs::read_to_string(&manifest) {
                    Ok(c) => c,
                    Err(_) => continue, // 子目录里没有 SKILL.md，跳过
                };

                let dir_name = path
                    .file_name()
                    .map(|n| n.to_string_lossy().to_string())
                    .unwrap_or_else(|| "<unknown>".to_string());

                let (fm, body) = parse_frontmatter(&content);

                let name = fm
                    .name
                    .map(|s| s.trim().to_string())
                    .filter(|s| !s.is_empty())
                    .unwrap_or(dir_name);

                // description：frontmatter 优先，否则回退到正文首行（去掉 # / 前导空白后归一化）。
                let description = fm
                    .description
                    .map(|d| normalize_description(&d))
                    .filter(|d| !d.is_empty())
                    .unwrap_or_else(|| {
                        body.lines()
                            .find(|l| !l.trim().is_empty())
                            .map(normalize_description)
                            .unwrap_or_default()
                    });

                skills.insert(
                    name.clone(),
                    Skill {
                        name,
                        description,
                        content,
                    },
                );
            }
        }

        SkillLoader { skills }
    }

    /// 技能数量。
    pub fn len(&self) -> usize {
        self.skills.len()
    }

    /// 技能目录（仅 name + description），编入 system prompt。
    /// 每行 `- {name}: {description}`；无技能时返回空串。
    pub fn catalog(&self) -> String {
        self.skills
            .values()
            .map(|s| format!("- {}: {}", s.name, s.description))
            .collect::<Vec<_>>()
            .join("\n")
    }

    /// 按 name 查注册表（**不是文件路径**），返回完整 `SKILL.md` 正文。
    /// 未命中时返回错误串，列出可用技能。`dispatch_tool` 的 `Error:` 前缀逻辑会把它包成 `[ERROR:load_skill] ...`。
    pub fn load(&self, name: &str) -> String {
        match self.skills.get(name) {
            Some(skill) => skill.content.clone(),
            None => {
                let available = self
                    .skills
                    .keys()
                    .cloned()
                    .collect::<Vec<_>>()
                    .join(", ");
                let available = if available.is_empty() {
                    "none".to_string()
                } else {
                    available
                };
                format!("Error: Unknown skill '{}'. Available: {}", name, available)
            }
        }
    }
}

/// 解析 YAML frontmatter。
///
/// 文件以 `---` 开头时，把首尾 `---` 之间的部分用 `serde_yaml` 解析成 `SkillFrontmatter`，
/// 其后为正文。frontmatter 缺失、段数不足、YAML 解析失败、或解析结果非 mapping 时，
/// 一律回退到 `{ name: None, description: None }` + 全文作正文——永不 panic。
/// 与 Python 参考 `parse_frontmatter` 的容错行为一致。
fn parse_frontmatter(text: &str) -> (SkillFrontmatter, String) {
    let text = text.strip_prefix('\u{feff}').unwrap_or(text); // 容忍 BOM

    if !text.starts_with("---") {
        return (SkillFrontmatter::default(), text.to_string());
    }

    // `splitn(3, "---")` 得到 ["", frontmatter, body...]。注意 body 里可能再含 `---`，
    // 所以用 splitn(3) 而非 split。
    let parts: Vec<&str> = text.splitn(3, "---").collect();
    if parts.len() < 3 {
        return (SkillFrontmatter::default(), text.to_string());
    }

    // parts[0] 是 "---" 之前的空串；parts[1] 是 frontmatter；parts[2] 是正文（含前导换行）。
    let fm_text = parts[1];
    let body = parts[2].trim_start_matches(['\r', '\n']).to_string();

    match serde_yaml::from_str::<SkillFrontmatter>(fm_text) {
        Ok(fm) => (fm, body),
        Err(_) => (SkillFrontmatter::default(), text.to_string()),
    }
}

/// 归一化 description：去掉前导 `#`/空白，按空白切分再用单空格拼接。
/// 把 `description: |` 多行块标量（如 agent-builder）压成一行；对应 Python 的
/// `" ".join(desc.lstrip("# ").split())`。
fn normalize_description(desc: &str) -> String {
    let trimmed = desc.trim_start_matches(['#', ' ', '\t']).trim();
    trimmed
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

// ---- 全局注册表：沿用 todo.rs 的 OnceLock 模式，但只读无需 Mutex ----

/// 全局 SkillLoader 实例（扫描后只读）。
static SKILL_LOADER: OnceLock<SkillLoader> = OnceLock::new();

/// 初始化全局 SkillLoader。main 中扫描后调用一次。
pub fn set_instance(loader: SkillLoader) {
    let _ = SKILL_LOADER.get_or_init(|| loader);
}

/// 获取全局 SkillLoader 的引用。
fn get_instance() -> &'static SkillLoader {
    SKILL_LOADER
        .get()
        .expect("SkillLoader not initialized. Call set_instance() first().")
}

/// 读取全局注册表的技能目录，编入 system prompt。无技能时返回空串。
/// main 组装 system prompt 时用（与 `todo::run_todo_write` 同属「读全局实例」的公开入口）。
pub fn catalog() -> String {
    get_instance().catalog()
}

/// `load_skill` 工具处理函数：从 input 取 name，查全局注册表。
/// 形状与 `todo::run_todo_write` 一致——dispatch_tool 里直接转发 input 即可。
pub fn run_load_skill(input: &serde_json::Value) -> String {
    match input.get("name").and_then(|n| n.as_str()) {
        Some(name) => get_instance().load(name),
        None => "Error: missing name".to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::path::Path;

    /// 临时 skills 根目录，测试结束清理。
    fn temp_skills_root(label: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("rust-agent-skills-{}", label));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn write_skill(root: &Path, dir: &str, content: &str) {
        let skill_dir = root.join(dir);
        fs::create_dir_all(&skill_dir).unwrap();
        fs::write(skill_dir.join("SKILL.md"), content).unwrap();
    }

    #[test]
    fn parse_frontmatter_name_and_desc() {
        let text = "---\nname: code-review\ndescription: Do code reviews.\n---\n# Code Review\nbody";
        let (fm, body) = parse_frontmatter(text);
        assert_eq!(fm.name.as_deref(), Some("code-review"));
        assert_eq!(fm.description.as_deref(), Some("Do code reviews."));
        assert_eq!(body, "# Code Review\nbody");
    }

    #[test]
    fn parse_frontmatter_block_scalar_description() {
        // agent-builder 的 description: | 多行块标量：serde_yaml 解析成含换行的串，
        // normalize_description 把它压成一行。
        let text = "---\nname: agent-builder\ndescription: |\n  Design agents.\n  Use when users ask.\n---\nbody";
        let (fm, body) = parse_frontmatter(text);
        assert_eq!(fm.name.as_deref(), Some("agent-builder"));
        let norm = normalize_description(fm.description.as_deref().unwrap_or(""));
        assert_eq!(norm, "Design agents. Use when users ask.");
        assert_eq!(body, "body");
    }

    #[test]
    fn parse_frontmatter_missing_falls_back_to_full_text() {
        let text = "# Just a heading\nno frontmatter here";
        let (fm, body) = parse_frontmatter(text);
        assert!(fm.name.is_none());
        assert!(fm.description.is_none());
        assert_eq!(body, text);
    }

    #[test]
    fn parse_frontmatter_malformed_yaml_falls_back() {
        // 非法 YAML（键值不配对、裸冒号后非法内容）
        let text = "---\nname: : :\n---\nbody";
        let (fm, body) = parse_frontmatter(text);
        assert!(fm.name.is_none());
        // 回退时 body = 全文
        assert!(body.starts_with("---"));
    }

    #[test]
    fn parse_frontmatter_extra_fields_ignored() {
        let text = "---\nname: pdf\ndescription: Process PDFs.\nversion: 1.0\nauthor: bob\n---\nbody";
        let (fm, _) = parse_frontmatter(text);
        assert_eq!(fm.name.as_deref(), Some("pdf"));
        assert_eq!(fm.description.as_deref(), Some("Process PDFs."));
    }

    #[test]
    fn normalize_strips_hash_and_collapses_whitespace() {
        assert_eq!(normalize_description("#  Code   Review "), "Code Review");
        assert_eq!(normalize_description("  hello\nworld  "), "hello world");
        assert_eq!(normalize_description(""), "");
    }

    #[test]
    fn scan_collects_skills() {
        let root = temp_skills_root("scan-collects");
        write_skill(
            &root,
            "code-review",
            "---\nname: code-review\ndescription: Do code reviews.\n---\n# Code Review",
        );
        write_skill(
            &root,
            "pdf",
            "---\nname: pdf\ndescription: Process PDFs.\n---\n# PDF",
        );
        // 一个没有 SKILL.md 的子目录：应被跳过
        fs::create_dir_all(root.join("empty")).unwrap();

        let loader = SkillLoader::scan(root.clone());
        assert_eq!(loader.len(), 2);
        let cat = loader.catalog();
        assert!(cat.contains("- code-review: Do code reviews."));
        assert!(cat.contains("- pdf: Process PDFs."));
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn scan_uses_dir_name_when_frontmatter_name_absent() {
        let root = temp_skills_root("dir-name-fallback");
        write_skill(
            &root,
            "mcp-builder",
            "---\ndescription: Build MCP servers.\n---\n# MCP Builder",
        );

        let loader = SkillLoader::scan(root.clone());
        let skill = loader.skills.get("mcp-builder").expect("keyed by dir name");
        assert_eq!(skill.name, "mcp-builder");
        assert_eq!(skill.description, "Build MCP servers.");
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn scan_uses_first_body_line_when_description_absent() {
        let root = temp_skills_root("first-line-fallback");
        write_skill(&root, "misc", "---\nname: misc\n---\n# This is the heading\nbody");

        let loader = SkillLoader::scan(root.clone());
        let skill = loader.skills.get("misc").unwrap();
        assert_eq!(skill.description, "This is the heading");
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn scan_missing_dir_yields_empty() {
        let loader = SkillLoader::scan(PathBuf::from("/no/such/dir/here-xyz"));
        assert_eq!(loader.len(), 0);
        assert_eq!(loader.catalog(), "");
    }

    #[test]
    fn scan_not_recursive_into_references() {
        // 技能子目录里的 references/ 不应被当作技能
        let root = temp_skills_root("not-recursive");
        let skill_dir = root.join("agent-builder");
        fs::create_dir_all(skill_dir.join("references")).unwrap();
        fs::write(
            skill_dir.join("SKILL.md"),
            "---\nname: agent-builder\ndescription: Build agents.\n---\nbody",
        )
        .unwrap();
        // references 里放一个伪 SKILL.md，递归扫描会误收，但本实现不应收
        fs::write(
            skill_dir.join("references").join("SKILL.md"),
            "---\nname: phantom\ndescription: should not load.\n---\nbody",
        )
        .unwrap();

        let loader = SkillLoader::scan(root.clone());
        assert_eq!(loader.len(), 1);
        assert!(loader.skills.contains_key("agent-builder"));
        assert!(!loader.skills.contains_key("phantom"));
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn load_returns_full_content_on_hit() {
        let root = temp_skills_root("load-hit");
        let content = "---\nname: code-review\ndescription: Do code reviews.\n---\n# Code Review\n## Checklist\n- security";
        write_skill(&root, "code-review", content);

        let loader = SkillLoader::scan(root.clone());
        assert_eq!(loader.load("code-review"), content);
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn load_returns_error_listing_available_on_miss() {
        let root = temp_skills_root("load-miss");
        write_skill(
            &root,
            "code-review",
            "---\nname: code-review\ndescription: Do code reviews.\n---\nbody",
        );
        write_skill(
            &root,
            "pdf",
            "---\nname: pdf\ndescription: Process PDFs.\n---\nbody",
        );

        let loader = SkillLoader::scan(root.clone());
        let got = loader.load("nonexistent");
        assert!(got.starts_with("Error: Unknown skill 'nonexistent'."));
        // BTreeMap 顺序：code-review, pdf
        assert!(got.contains("Available: code-review, pdf"));
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn load_miss_when_no_skills_says_none() {
        let loader = SkillLoader::scan(PathBuf::from("/no/such/dir/here-xyz-empty"));
        let got = loader.load("whatever");
        assert!(got.contains("Available: none"));
    }

    #[test]
    fn load_by_loader_returns_full_content() {
        // 直接测 loader.load（不依赖全局 OnceLock 状态——OnceLock 初始化后不可重置，
        // 跨测试复用全局实例会串扰，故走实例方法）。
        // load 返回完整 SKILL.md（含 frontmatter），与 Python 参考 skill["content"] 一致。
        let root = temp_skills_root("load-by-loader");
        let content = "---\nname: code-review\ndescription: Do code reviews.\n---\n# Code Review\nbody";
        write_skill(&root, "code-review", content);
        let loader = SkillLoader::scan(root.clone());
        assert_eq!(loader.load("code-review"), content);
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn run_load_skill_missing_name_param_returns_error() {
        // run_load_skill 在缺 name 时提前返回，不触碰全局实例，故无需初始化即可测。
        let input = serde_json::json!({});
        assert_eq!(run_load_skill(&input), "Error: missing name");
    }

    #[test]
    fn duplicate_name_later_overwrites() {
        // 两个技能同名 frontmatter：BTreeMap insert 覆盖。读 dir 顺序虽不确定，
        // 但“最终只剩一个、key 为该 name”是确定的。
        let root = temp_skills_root("dup-name");
        write_skill(
            &root,
            "a",
            "---\nname: same\ndescription: first.\n---\nfirst body",
        );
        write_skill(
            &root,
            "b",
            "---\nname: same\ndescription: second.\n---\nsecond body",
        );

        let loader = SkillLoader::scan(root.clone());
        assert_eq!(loader.len(), 1); // 同名合并成一个
        let cat = loader.catalog();
        // 二选一，取决于扫描顺序，但描述与正文一致地属于其中一个
        assert!(
            cat.contains("first.") || cat.contains("second."),
            "catalog should contain one of the two: {}",
            cat
        );
        let _ = fs::remove_dir_all(&root);
    }
}
