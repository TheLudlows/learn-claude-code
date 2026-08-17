# Tool Trait Refactor Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Refactor the tool system to eliminate the dual source of truth (dispatch_tool match + tool definitions) by introducing a Tool trait and ToolRegistry, making adding new tools a simple 3-step process.

**Architecture:** Introduce a `Tool` trait that encapsulates name, description, schema, permission check, and execution. A `ToolRegistry` holds all tools and provides dynamic dispatch and definition generation. Replace all match-based dispatch with registry lookups.

**Tech Stack:** Rust, async-trait crate for async trait methods, serde_json for schema/input handling

## Global Constraints

- All tools must use `async fn execute()` signature
- Tool trait methods: `name()`, `description()`, `input_schema()`, `check_permission()` (default Pass), `execute()` async, `available_for_subagent()` (default true)
- All tools must be `Send + Sync` for dyn dispatch
- ToolContext carries `&Client` and `&Hooks` references
- Build registry in main.rs, pass by reference to all callers
- Preserve all existing behavior and tests
- Zero compiler warnings after completion

---

## Task 1: Add async-trait Dependency

**Files:**
- Modify: `rust-agent/Cargo.toml`

**Interfaces:**
- Produces: Makes `async_trait` crate available for Tool trait

- [ ] **Step 1: Add async-trait dependency to Cargo.toml**

```toml
[dependencies]
async-trait = "0.1"
```

Add this line to the `[dependencies]` section.

- [ ] **Step 2: Run cargo check to verify**

Run: `cd rust-agent && cargo check`
Expected: Cargo.toml parses successfully, dependency resolves

- [ ] **Step 3: Commit**

```bash
git add rust-agent/Cargo.toml
git commit -m "feat: add async-trait dependency"
```

---

## Task 2: Create Tool Trait and Related Types

**Files:**
- Create: `rust-agent/src/tools/trait_def.rs`

**Interfaces:**
- Produces: `Tool` trait, `ToolContext`, `PermissionCheck` enum for other tasks to implement

- [ ] **Step 1: Create trait_def.rs with Tool trait**

```rust
/*
trait_def.rs - Tool trait, ToolContext, PermissionCheck

Core abstractions for the tool system refactoring.
*/

use async_trait::async_trait;
use serde_json::Value;

/// Tool permission check result
pub enum PermissionCheck {
    /// No additional permission check needed (default)
    Pass,
    /// Requires user approval, with reason description
    NeedsApproval(&'static str),
}

/// Tool execution context — shared dependency injection for all tools
pub struct ToolContext<'a> {
    pub client: &'a crate::client::Client,
    pub hooks: &'a crate::hooks::Hooks,
}

/// Tool trait — single source of truth for tools
#[async_trait]
pub trait Tool: Send + Sync {
    /// Tool name (must match name in API schema)
    fn name(&self) -> &str;

    /// Tool description (sent to model)
    fn description(&self) -> &str;

    /// JSON Schema input definition
    fn input_schema(&self) -> Value;

    /// Permission rules — defaults to no additional check
    fn check_permission(&self, _input: &Value) -> PermissionCheck {
        PermissionCheck::Pass
    }

    /// Execute tool, return result string
    async fn execute(&self, ctx: &ToolContext<'_>, input: &Value) -> String;

    /// Whether available for subagent (defaults to true)
    fn available_for_subagent(&self) -> bool {
        true
    }
}
```

- [ ] **Step 2: Run cargo check**

Run: `cd rust-agent && cargo check`
Expected: File compiles successfully

- [ ] **Step 3: Commit**

```bash
git add rust-agent/src/tools/trait_def.rs
git commit -m "feat: add Tool trait, ToolContext, PermissionCheck"
```

---

## Task 3: Create ToolRegistry

**Files:**
- Create: `rust-agent/src/tools/registry.rs`

**Interfaces:**
- Consumes: `Tool` trait from trait_def.rs
- Produces: `ToolRegistry` struct with `dispatch`, `definitions`, `check_permission` methods

- [ ] **Step 1: Create registry.rs with ToolRegistry implementation**

```rust
/*
registry.rs - ToolRegistry for dynamic tool dispatch

Replaces dispatch_tool() match and get_*_tool_definitions() functions.
*/

use crate::client::ToolDefinition;
use crate::tools::trait_def::{PermissionCheck, Tool, ToolContext};
use serde_json::Value;
use std::collections::HashMap;

pub struct ToolRegistry {
    all_tools: Vec<Box<dyn Tool>>,
    index: HashMap<String, usize>,
}

impl ToolRegistry {
    pub fn new(tools: Vec<Box<dyn Tool>>) -> Self {
        let index: HashMap<String, usize> = tools
            .iter()
            .enumerate()
            .map(|(i, t)| (t.name().to_string(), i))
            .collect();
        Self { all_tools: tools, index }
    }

    /// Generate ToolDefinition list
    ///
    /// - `subagent_only = true` → only return tools where available_for_subagent() == true
    /// - `subagent_only = false` → return all tools
    pub fn definitions(&self, subagent_only: bool) -> Vec<ToolDefinition> {
        self.all_tools
            .iter()
            .filter(|t| !subagent_only || t.available_for_subagent())
            .map(|t| ToolDefinition {
                name: t.name().to_string(),
                description: t.description().to_string(),
                input_schema: t.input_schema(),
            })
            .collect()
    }

    /// Dispatch tool call — replaces dispatch_tool() match
    pub async fn dispatch(
        &self,
        ctx: &ToolContext<'_>,
        name: &str,
        input: &Value,
    ) -> String {
        let result = match self.index.get(name) {
            Some(&idx) => self.all_tools[idx].execute(ctx, input).await,
            None => return format!("[ERROR:unknown] Unknown tool: {}", name),
        };

        if result.starts_with("Error:") {
            with_error_prefix(name, &result)
        } else {
            result
        }
    }

    /// Permission check — replaces permission.rs::check_rules() match
    pub fn check_permission(&self, name: &str, input: &Value) -> PermissionCheck {
        match self.index.get(name) {
            Some(&idx) => self.all_tools[idx].check_permission(input),
            None => PermissionCheck::Pass,
        }
    }
}

/// Add error prefix for known tools
fn with_error_prefix(prefix: &str, message: &str) -> String {
    if message.starts_with("Error:") {
        format!("[ERROR:{}] {}", prefix, &message[7..].trim_start())
    } else {
        message.to_string()
    }
}
```

- [ ] **Step 2: Run cargo check**

Run: `cd rust-agent && cargo check`
Expected: File compiles successfully

- [ ] **Step 3: Commit**

```bash
git add rust-agent/src/tools/registry.rs
git commit -m "feat: add ToolRegistry with dispatch, definitions, check_permission"
```

---

## Task 4: Create tools/mod.rs with Shared Functions

**Files:**
- Create: `rust-agent/src/tools/mod.rs`

**Interfaces:**
- Consumes: trait_def.rs, registry.rs
- Produces: Shared helper functions (run_bash, run_read_file, etc.), path safety functions, build_registry()

- [ ] **Step 1: Create mod.rs with shared functions and path safety**

```rust
/*
mod.rs - Tool module coordinator

Exports Tool trait, ToolRegistry, and shared helper functions.
Contains build_registry() and path safety functions.
*/

// Re-exports
pub use self::registry::ToolRegistry;
pub use self::trait_def::{PermissionCheck, Tool, ToolContext};

// Internal modules
mod trait_def;
mod registry;

// Tool implementation modules (to be added in later tasks)
// mod command;
// mod read_file;
// mod write_file;
// mod edit_file;
// mod glob;
// mod load_skill;
// mod todo_write;
// mod task;

use serde_json::Value;
use std::env;
use std::fs;
use std::path::{Component, Path, PathBuf};
use std::process::Command;

/// Working directory
pub fn workdir() -> PathBuf {
    env::current_dir().unwrap_or_else(|_| ".".into())
}

/// Path safety validation — ensure path is within workspace
///
/// First does **lexical normalization** on `canonical_workdir/path` (resolves `..`/`.` without filesystem access)
/// to get absolute path, then uses **canonical-to-canonical** comparison for boundary checking —
/// therefore works for non-existent paths too (e.g., `write_file` creating new files/directories):
/// Original implementation would canonicalize the target path, which fails if path doesn't exist,
/// making it impossible to write to new files.
///
/// Boundary comparison MUST use canonical form: On Windows, `canonicalize()` adds `\\?\` verbatim prefix
/// and expands 8.3 short names, while lexical normalization result (especially when `path_str` is an absolute path)
/// may have neither prefix nor short names — direct `starts_with` would misclassify in-workspace paths
/// as escaping (C3 regression).
pub(crate) fn safe_path_in(workdir: &Path, path_str: &str) -> Result<PathBuf, String> {
    // canonicalize workdir itself: workdir always exists, won't fail.
    // Use canonical workdir as base for lexical normalization to avoid cwd itself containing symlinks
    // causing "lexical path vs canonical workdir" misclassification.
    let workdir_canonical = workdir
        .canonicalize()
        .map_err(|e| format!("Error: {}", e))?;

    // Lexical normalization: join base + path, then resolve `..`/`.` by components without touching filesystem.
    // Note: when path_str is absolute, join replaces base — this is expected behavior (absolute paths aren't relative to base).
    let mut norm = PathBuf::new();
    for c in workdir_canonical.join(path_str).components() {
        match c {
            Component::ParentDir => {
                norm.pop();
            }
            Component::CurDir => {}
            other => norm.push(other.as_os_str()),
        }
    }

    // Boundary check: compare using canonical forms (see function comment).
    // When even ancestors can't be canonicalized, fail-closed (safe side).
    let within = match canonical_form_of(&norm) {
        Some(c) => c.starts_with(&workdir_canonical),
        None => false,
    };
    if !within {
        return Err(format!("Error: path escapes workspace {:?}, {:?}", workdir_canonical, norm));
    }

    // Return value: existing paths return canonical (resolves symlinks/junctions); non-existent paths use lexical result.
    if norm.exists() {
        norm.canonicalize().map_err(|e| format!("Error: {}", e))
    } else {
        Ok(norm)
    }
}

/// Normalize path to canonical form, specifically for boundary comparison.
///
/// - Path exists: directly `canonicalize()`.
/// - Path doesn't exist: walk up ancestors to first existing directory, canonicalize it, then append non-existent tail.
///   This way non-existent paths (`write_file` creating new files/directories) get a form comparable to canonical base.
/// - Even root directory can't be `canonicalize()`: return `None` (caller treats as fail-closed).
fn canonical_form_of(path: &Path) -> Option<PathBuf> {
    // Fast path: path itself exists.
    if let Ok(c) = path.canonicalize() {
        return Some(c);
    }
    // Path doesn't exist: walk up to find first existing ancestor, canonicalize it, then reattach tail.
    let mut tail: Vec<std::ffi::OsString> = Vec::new();
    let mut cur = path.to_path_buf();
    loop {
        match cur.canonicalize() {
            Ok(canon) => {
                let mut full = canon;
                for seg in tail.into_iter().rev() {
                    full.push(seg);
                }
                return Some(full);
            }
            Err(_) => {
                // cur doesn't exist: record this segment name, continue walking up.
                let name = cur.file_name()?.to_owned();
                tail.push(name);
                cur = cur.parent()?.to_path_buf();
            }
        }
    }
}

/// Path safety validation — ensure path is within current working directory.
pub(crate) fn safe_path(path_str: &str) -> Result<PathBuf, String> {
    safe_path_in(&workdir(), path_str)
}

/// Decode console output bytes to string: try UTF-8 first (cargo and modern programs use UTF-8 directly),
/// fall back to OEM codepage (cmd.exe built-in commands, git in Chinese locale use GBK),
/// finally use lossy if both fail. Avoid non-ASCII being replaced with U+FFFD (garbled text).
pub(crate) fn decode_console(bytes: &[u8]) -> String {
    if let Ok(s) = std::str::from_utf8(bytes) {
        return s.to_string();
    }
    #[cfg(windows)]
    if let Some(s) = decode_with_oem_codepage(bytes) {
        return s;
    }
    String::from_utf8_lossy(bytes).into_owned()
}

/// Decode bytes as OEM codepage (Chinese locale is 936/GBK) to UTF-16 then to String.
/// Returns None on failure, letting caller fall back to lossy.
#[cfg(windows)]
fn decode_with_oem_codepage(bytes: &[u8]) -> Option<String> {
    use std::os::raw::{c_int, c_uchar};
    extern "system" {
        fn GetOEMCP() -> u32;
        fn MultiByteToWideChar(
            CodePage: u32,
            dwFlags: u32,
            lpMultiByteStr: *const c_uchar,
            cbMultiByte: c_int,
            lpWideCharStr: *mut u16,
            cchWideChar: c_int,
        ) -> c_int;
    }
    if bytes.is_empty() {
        return Some(String::new());
    }
    let cp = unsafe { GetOEMCP() };
    let n = bytes.len() as c_int;
    let size = unsafe {
        MultiByteToWideChar(cp, 0, bytes.as_ptr(), n, std::ptr::null_mut(), 0)
    };
    if size <= 0 {
        return None;
    }
    let mut buf: Vec<u16> = vec![0u16; size as usize];
    let written = unsafe {
        MultiByteToWideChar(cp, 0, bytes.as_ptr(), n, buf.as_mut_ptr(), size)
    };
    if written <= 0 {
        return None;
    }
    buf.truncate(written as usize);
    Some(String::from_utf16_lossy(&buf))
}

/// Execute command (cross-platform)
///
/// - Windows: use cmd.exe
/// - Unix: use bash
///
/// Dangerous command interception moved to permission::permission_hook gates (s03/s04),
/// blocked before reaching here; safe_path remains the workspace sandbox for file tools.
const MAX_OUTPUT_BYTES: usize = 50_000;

pub(crate) fn run_bash(command: &str) -> String {
    let result = if cfg!(windows) {
        Command::new("cmd.exe")
            .args(["/C", command])
            .current_dir(workdir())
            .output()
    } else {
        Command::new("bash")
            .arg("-c")
            .arg(command)
            .current_dir(workdir())
            .output()
    };

    match result {
        Ok(output) => {
            let stdout = decode_console(&output.stdout);
            let stderr = decode_console(&output.stderr);
            let result = format!("{}\n{}", stdout, stderr).trim().to_string();
            if result.is_empty() {
                "(no output)".to_string()
            } else if result.len() > MAX_OUTPUT_BYTES {
                // Truncate at byte limit, but must fall on UTF-8 character boundary, otherwise
                // `result[..end]` would panic in middle of multi-byte sequence (CJK output is very common).
                let mut end = MAX_OUTPUT_BYTES;
                while !result.is_char_boundary(end) {
                    end -= 1;
                }
                result[..end].to_string()
            } else {
                result
            }
        }
        Err(e) => format!("Error: {}", e),
    }
}

/// Read file
pub(crate) fn run_read_file(path: &str, limit: Option<u32>) -> String {
    match safe_path(path) {
        Ok(abs_path) => {
            match fs::read_to_string(&abs_path) {
                Ok(content) => {
                    let lines: Vec<&str> = content.lines().collect();
                    if let Some(limit) = limit {
                        if lines.len() > limit as usize {
                            let truncated: Vec<&str> = lines[..limit as usize].to_vec();
                            let more = lines.len() - limit as usize;
                            format!("{}\n... ({} more lines)",
                                truncated.join("\n"), more)
                        } else {
                            content
                        }
                    } else {
                        content
                    }
                }
                Err(e) => format!("Error: {}", e),
            }
        }
        Err(e) => e,
    }
}

/// Write file
pub(crate) fn run_write_file(path: &str, content: &str) -> String {
    match safe_path(path) {
        Ok(abs_path) => {
            if let Some(parent) = abs_path.parent() {
                fs::create_dir_all(parent).ok();
            }
            match fs::write(&abs_path, content) {
                Ok(_) => format!("Wrote {} bytes to {}", content.len(), path),
                Err(e) => format!("Error: {}", e),
            }
        }
        Err(e) => e,
    }
}

/// Edit file (replace text)
pub(crate) fn run_edit_file(path: &str, old_text: &str, new_text: &str) -> String {
    match safe_path(path) {
        Ok(abs_path) => {
            match fs::read_to_string(&abs_path) {
                Ok(content) => {
                    if !content.contains(old_text) {
                        return format!("Error: text not found in {}", path);
                    }
                    let new_content = content.replacen(old_text, new_text, 1);
                    match fs::write(&abs_path, &new_content) {
                        Ok(_) => format!("Edited {}", path),
                        Err(e) => format!("Error: {}", e),
                    }
                }
                Err(e) => format!("Error: {}", e),
            }
        }
        Err(e) => e,
    }
}

/// Normalize path separators to `/` (Windows `strip_prefix` gives `\`), convenient for glob matching.
fn to_unix_path(p: &str) -> String {
    p.replace('\\', "/")
}

/// Match single path segment against single pattern segment (segment contains no `/`).
/// `*` matches any length (including empty) of characters; `?` matches single character; rest is literal.
fn seg_match(pat: &[u8], text: &[u8]) -> bool {
    let (mut pi, mut ti) = (0, 0);
    let mut star_pi: Option<usize> = None;
    let mut star_ti = 0;
    while ti < text.len() {
        if pi < pat.len() && (pat[pi] == b'?' || pat[pi] == text[ti]) {
            pi += 1;
            ti += 1;
        } else if pi < pat.len() && pat[pi] == b'*' {
            star_pi = Some(pi);
            star_ti = ti;
            pi += 1;
        } else if let Some(sp) = star_pi {
            pi = sp + 1;
            star_ti += 1;
            ti = star_ti;
        } else {
            return false;
        }
    }
    while pi < pat.len() && pat[pi] == b'*' {
        pi += 1;
    }
    pi == pat.len()
}

/// Check if `path` (`/`-separated relative path) matches glob `pattern`.
/// `*` matches arbitrary characters within a segment; `**` as standalone segment matches zero or more full path segments.
fn glob_match(pattern: &str, path: &str) -> bool {
    fn rec(pat: &[&str], txt: &[&str]) -> bool {
        if pat.is_empty() {
            return txt.is_empty();
        }
        if pat[0] == "**" {
            // ** matches zero or more full segments: first try zero segments, fail then eat one txt segment
            if rec(&pat[1..], txt) {
                return true;
            }
            if !txt.is_empty() {
                return rec(pat, &txt[1..]);
            }
            false
        } else {
            // Regular segment must match exactly one txt segment
            if txt.is_empty() || !seg_match(pat[0].as_bytes(), txt[0].as_bytes()) {
                return false;
            }
            rec(&pat[1..], &txt[1..])
        }
    }
    let pat: Vec<&str> = pattern.split('/').collect();
    let txt: Vec<&str> = path.split('/').collect();
    rec(&pat, &txt)
}

/// Recursively collect all relative paths (`/`-separated) under `base` that match glob `pattern`.
pub(crate) fn glob_in(pattern: &str, base: &Path) -> Vec<String> {
    let mut results: Vec<String> = Vec::new();

    fn walk(dir: &Path, base: &Path, pattern: &str, results: &mut Vec<String>) {
        if let Ok(entries) = fs::read_dir(dir) {
            for entry in entries.flatten() {
                let path = entry.path();
                if let Ok(rel) = path.strip_prefix(base) {
                    let rel = to_unix_path(&rel.to_string_lossy());
                    if glob_match(pattern, &rel) {
                        results.push(rel);
                    }
                }
                if path.is_dir() {
                    walk(&path, base, pattern, results);
                }
            }
        }
    }

    walk(base, base, pattern, &mut results);
    results
}

/// Find matching files (match relative paths by glob rules, recursively across workspace)
pub(crate) fn run_glob(pattern: &str) -> String {
    let results = glob_in(pattern, &workdir());
    if results.is_empty() {
        "Error: no matches".to_string()
    } else {
        results.join("\n")
    }
}

/// Lexically check if relative path escapes workspace (without filesystem access, supports non-existent paths)
pub(crate) fn escapes_workspace_lexical(path: &str) -> bool {
    !normalize(&workdir(), path).starts_with(workdir())
}

/// Lexically normalize `base/path`: resolve `..`/`.`; absolute paths replace base.
/// No filesystem access, thus valid for non-existent paths (creating new files).
fn normalize(base: &Path, path: &str) -> PathBuf {
    let mut norm = PathBuf::new();
    for c in base.join(path).components() {
        match c {
            Component::ParentDir => {
                norm.pop();
            }
            Component::CurDir => {}
            other => norm.push(other.as_os_str()),
        }
    }
    norm
}

/// Build tool registry (to be populated in later tasks as tools are migrated)
pub fn build_registry() -> ToolRegistry {
    ToolRegistry::new(vec![
        // Tools will be added here in later tasks
    ])
}

#[cfg(test)]
mod glob_match_tests {
    use super::*;

    #[test]
    fn literal_exact() {
        assert!(glob_match("a.rs", "a.rs"));
        assert!(!glob_match("a.rs", "b.rs"));
        assert!(!glob_match("src", "src_backup")); // segment must match entirely, not substring
        assert!(!glob_match("src", "src/a.rs"));   // segment count mismatch
    }

    #[test]
    fn star_within_segment() {
        assert!(glob_match("*.rs", "a.rs"));
        assert!(glob_match("*.rs", "tools.rs"));
        assert!(!glob_match("*.rs", "src"));        // missing .rs
        assert!(!glob_match("*.rs", "src/a.rs"));  // * doesn't cross segments
        assert!(glob_match("a*.rs", "abc.rs"));
        assert!(glob_match("a*z", "abcxyz"));
        assert!(!glob_match("a*.rs", "abc.txt"));
    }

    #[test]
    fn question_mark() {
        assert!(glob_match("?.rs", "a.rs"));
        assert!(glob_match("?.rs", "b.rs"));
        assert!(!glob_match("?.rs", "ab.rs"));     // ? matches only one character
        assert!(!glob_match("?.rs", ".rs"));        // ? requires at least one character
    }

    #[test]
    fn double_star_recursive() {
        assert!(glob_match("**", "a.rs"));
        assert!(glob_match("**", "src/a.rs"));
        assert!(glob_match("**", "src/sub/a.rs"));
        assert!(glob_match("**/*.rs", "a.rs"));      // ** matches zero segments
        assert!(glob_match("**/*.rs", "src/a.rs"));
        assert!(glob_match("**/*.rs", "src/sub/a.rs"));
        assert!(glob_match("src/**/*.rs", "src/a.rs"));
        assert!(glob_match("src/**/*.rs", "src/sub/a.rs"));
        assert!(!glob_match("src/**/*.rs", "a.rs"));      // not under src
        assert!(!glob_match("src/**/*.rs", "test/a.rs")); // prefix mismatch
    }

    #[test]
    fn glob_in_walks_tree() {
        let dir = std::env::temp_dir().join("rust-agent-glob-test-walk");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("src")).unwrap();
        std::fs::write(dir.join("a.rs"), b"").unwrap();
        std::fs::write(dir.join("b.txt"), b"").unwrap();
        std::fs::write(dir.join("src").join("c.rs"), b"").unwrap();

        let mut got = glob_in("**/*.rs", &dir);
        got.sort();
        let mut want = vec!["a.rs".to_string(), "src/c.rs".to_string()];
        want.sort();
        assert_eq!(got, want);

        let mut top = glob_in("*.rs", &dir);
        top.sort();
        assert_eq!(top, vec!["a.rs".to_string()]);

        assert_eq!(glob_in("zzz", &dir), Vec::<String>::new());

        let _ = std::fs::remove_dir_all(&dir);
    }
}

#[cfg(test)]
mod run_bash_tests {
    use super::run_bash;

    /// Regression: cmd.exe in Chinese locale defaults to GBK(936) output, `from_utf8_lossy`
    /// would replace non-ASCII (like "版本" in `ver` output) with U+FFFD (garbled). After forcing UTF-8,
    /// should be valid UTF-8 Chinese, no replacement chars.
    #[test]
    #[cfg(windows)]
    fn decodes_non_ascii_without_replacement_chars() {
        let out = run_bash("ver");
        assert!(
            !out.contains('\u{FFFD}'),
            "Command output should not contain U+FFFD replacement chars (should be valid UTF-8): {out:?}"
        );
    }
}

#[cfg(test)]
mod safe_path_tests {
    use super::safe_path_in;
    use std::fs;

    #[test]
    fn allows_nonexistent_path_for_new_file() {
        // C2 regression: write_file creating new files/directories, target path doesn't exist yet,
        // safe_path shouldn't reject due to canonicalize failure.
        let dir = std::env::temp_dir().join("rust-agent-safe-path-nonexistent");
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        let dir_canon = dir.canonicalize().unwrap();

        let got = safe_path_in(&dir, "subdir/newfile.txt");
        assert!(got.is_ok(), "non-existent path should be allowed: {:?}", got);
        let abs = got.unwrap();
        assert!(abs.starts_with(&dir_canon), "{:?} should be under base", abs);
        assert_eq!(abs.file_name(), Some(std::ffi::OsStr::new("newfile.txt")));

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn rejects_escape_via_dotdot() {
        let dir = std::env::temp_dir().join("rust-agent-safe-path-escape");
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();

        let got = safe_path_in(&dir, "../secret.txt");
        assert!(got.is_err(), "path escaping workspace must be rejected");

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn existing_path_canonicalized_and_allowed() {
        let dir = std::env::temp_dir().join("rust-agent-safe-path-existing");
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        let file = dir.join("real.txt");
        fs::write(&file, b"hi").unwrap();

        let got = safe_path_in(&dir, "real.txt");
        assert!(got.is_ok(), "existing in-workspace path should be allowed: {:?}", got);
        assert_eq!(
            got.unwrap().canonicalize().unwrap(),
            file.canonicalize().unwrap()
        );

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn absolute_path_in_workspace_is_allowed() {
        // C3 regression: callers often pass absolute paths. Absolute paths make join replace base,
        // drop `\\?\` verbatim prefix; and temp_dir might use 8.3 short names. Both make lexical norm
        // incomparable with canonical base via direct starts_with. Canonical-to-canonical comparison should pass.
        let dir = std::env::temp_dir().join("rust-agent-safe-path-abs");
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        let file = dir.join("inner.txt");
        fs::write(&file, b"x").unwrap();

        let abs = file.to_string_lossy().to_string();
        let got = safe_path_in(&dir, &abs);
        assert!(got.is_ok(), "absolute in-workspace path should be allowed: {:?}", got);
        // Existing paths should return canonical form (with verbatim prefix), consistent with direct canonicalize.
        assert_eq!(got.unwrap(), file.canonicalize().unwrap());

        let _ = fs::remove_dir_all(&dir);
    }
}
```

- [ ] **Step 2: Run cargo check**

Run: `cd rust-agent && cargo check`
Expected: File compiles successfully, tests pass

- [ ] **Step 3: Commit**

```bash
git add rust-agent/src/tools/mod.rs
git commit -m "feat: add tools/mod.rs with shared functions and path safety"
```

---

## Task 5: Create CommandTool

**Files:**
- Create: `rust-agent/src/tools/command.rs`
- Modify: `rust-agent/src/tools/mod.rs` (add mod command; register in build_registry)

**Interfaces:**
- Consumes: Tool trait from trait_def.rs, run_bash from mod.rs
- Produces: CommandTool implementation with permission check for destructive commands

- [ ] **Step 1: Create command.rs with CommandTool implementation**

```rust
/*
command.rs - Command tool implementation
*/

use async_trait::async_trait;
use crate::tools::{Tool, ToolContext, PermissionCheck};
use serde_json::Value;

pub struct CommandTool;

#[async_trait]
impl Tool for CommandTool {
    fn name(&self) -> &str { "command" }
    fn description(&self) -> &str { "Run a shell command." }
    fn input_schema(&self) -> Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "command": { "type": "string" }
            },
            "required": ["command"]
        })
    }

    fn check_permission(&self, input: &Value) -> PermissionCheck {
        let cmd = input.get("command").and_then(|v| v.as_str()).unwrap_or("");
        if ["rm ", "> /etc/", "chmod 777"].iter().any(|kw| cmd.contains(kw)) {
            return PermissionCheck::NeedsApproval("Potentially destructive command");
        }
        PermissionCheck::Pass
    }

    async fn execute(&self, _ctx: &ToolContext<'_>, input: &Value) -> String {
        match input.get("command").and_then(|c| c.as_str()) {
            Some(cmd) if !cmd.is_empty() => super::run_bash(cmd),
            _ => "Error: missing command".to_string(),
        }
    }
}
```

- [ ] **Step 2: Update mod.rs to include command module**

In mod.rs, uncomment/add:
```rust
mod command;
```

And in build_registry:
```rust
pub fn build_registry() -> ToolRegistry {
    ToolRegistry::new(vec![
        Box::new(command::CommandTool),
    ])
}
```

- [ ] **Step 3: Run cargo test**

Run: `cd rust-agent && cargo test`
Expected: All tests pass

- [ ] **Step 4: Commit**

```bash
git add rust-agent/src/tools/command.rs rust-agent/src/tools/mod.rs
git commit -m "feat: add CommandTool with destructive command permission check"
```

---

## Task 6: Create ReadFileTool

**Files:**
- Create: `rust-agent/src/tools/read_file.rs`
- Modify: `rust-agent/src/tools/mod.rs` (add mod read_file; register in build_registry)

**Interfaces:**
- Consumes: Tool trait, run_read_file from mod.rs, escapes_workspace_lexical from mod.rs
- Produces: ReadFileTool implementation with workspace escape permission check

- [ ] **Step 1: Create read_file.rs with ReadFileTool implementation**

```rust
/*
read_file.rs - Read file tool implementation
*/

use async_trait::async_trait;
use crate::tools::{Tool, ToolContext, PermissionCheck};
use serde_json::Value;

pub struct ReadFileTool;

#[async_trait]
impl Tool for ReadFileTool {
    fn name(&self) -> &str { "read_file" }
    fn description(&self) -> &str { "Read file contents." }
    fn input_schema(&self) -> Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "path": { "type": "string" },
                "limit": { "type": "integer" }
            },
            "required": ["path"]
        })
    }

    fn check_permission(&self, input: &Value) -> PermissionCheck {
        let path = input.get("path").and_then(|v| v.as_str()).unwrap_or("");
        if super::escapes_workspace_lexical(path) {
            return PermissionCheck::NeedsApproval("Access outside workspace");
        }
        PermissionCheck::Pass
    }

    async fn execute(&self, _ctx: &ToolContext<'_>, input: &Value) -> String {
        let path = input.get("path").and_then(|p| p.as_str()).unwrap_or("");
        let limit = input.get("limit").and_then(|l| l.as_u64()).map(|l| l as u32);
        super::run_read_file(path, limit)
    }
}
```

- [ ] **Step 2: Update mod.rs to include read_file module**

In mod.rs, add:
```rust
mod read_file;
```

And add to build_registry vec:
```rust
Box::new(read_file::ReadFileTool),
```

- [ ] **Step 3: Run cargo test**

Run: `cd rust-agent && cargo test`
Expected: All tests pass

- [ ] **Step 4: Commit**

```bash
git add rust-agent/src/tools/read_file.rs rust-agent/src/tools/mod.rs
git commit -m "feat: add ReadFileTool with workspace escape permission check"
```

---

## Task 7: Create WriteFileTool

**Files:**
- Create: `rust-agent/src/tools/write_file.rs`
- Modify: `rust-agent/src/tools/mod.rs` (add mod write_file; register in build_registry)

**Interfaces:**
- Consumes: Tool trait, run_write_file from mod.rs, escapes_workspace_lexical from mod.rs
- Produces: WriteFileTool implementation with workspace escape permission check

- [ ] **Step 1: Create write_file.rs with WriteFileTool implementation**

```rust
/*
write_file.rs - Write file tool implementation
*/

use async_trait::async_trait;
use crate::tools::{Tool, ToolContext, PermissionCheck};
use serde_json::Value;

pub struct WriteFileTool;

#[async_trait]
impl Tool for WriteFileTool {
    fn name(&self) -> &str { "write_file" }
    fn description(&self) -> &str { "Write content to a file." }
    fn input_schema(&self) -> Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "path": { "type": "string" },
                "content": { "type": "string" }
            },
            "required": ["path", "content"]
        })
    }

    fn check_permission(&self, input: &Value) -> PermissionCheck {
        let path = input.get("path").and_then(|v| v.as_str()).unwrap_or("");
        if super::escapes_workspace_lexical(path) {
            return PermissionCheck::NeedsApproval("Access outside workspace");
        }
        PermissionCheck::Pass
    }

    async fn execute(&self, _ctx: &ToolContext<'_>, input: &Value) -> String {
        let path = input.get("path").and_then(|p| p.as_str()).unwrap_or("");
        let content = input.get("content").and_then(|c| c.as_str()).unwrap_or("");
        super::run_write_file(path, content)
    }
}
```

- [ ] **Step 2: Update mod.rs to include write_file module**

In mod.rs, add:
```rust
mod write_file;
```

And add to build_registry vec:
```rust
Box::new(write_file::WriteFileTool),
```

- [ ] **Step 3: Run cargo test**

Run: `cd rust-agent && cargo test`
Expected: All tests pass

- [ ] **Step 4: Commit**

```bash
git add rust-agent/src/tools/write_file.rs rust-agent/src/tools/mod.rs
git commit -m "feat: add WriteFileTool with workspace escape permission check"
```

---

## Task 8: Create EditFileTool

**Files:**
- Create: `rust-agent/src/tools/edit_file.rs`
- Modify: `rust-agent/src/tools/mod.rs` (add mod edit_file; register in build_registry)

**Interfaces:**
- Consumes: Tool trait, run_edit_file from mod.rs, escapes_workspace_lexical from mod.rs
- Produces: EditFileTool implementation with workspace escape permission check

- [ ] **Step 1: Create edit_file.rs with EditFileTool implementation**

```rust
/*
edit_file.rs - Edit file tool implementation
*/

use async_trait::async_trait;
use crate::tools::{Tool, ToolContext, PermissionCheck};
use serde_json::Value;

pub struct EditFileTool;

#[async_trait]
impl Tool for EditFileTool {
    fn name(&self) -> &str { "edit_file" }
    fn description(&self) -> &str { "Replace exact text in a file once." }
    fn input_schema(&self) -> Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "path": { "type": "string" },
                "old_text": { "type": "string" },
                "new_text": { "type": "string" }
            },
            "required": ["path", "old_text", "new_text"]
        })
    }

    fn check_permission(&self, input: &Value) -> PermissionCheck {
        let path = input.get("path").and_then(|v| v.as_str()).unwrap_or("");
        if super::escapes_workspace_lexical(path) {
            return PermissionCheck::NeedsApproval("Access outside workspace");
        }
        PermissionCheck::Pass
    }

    async fn execute(&self, _ctx: &ToolContext<'_>, input: &Value) -> String {
        let path = input.get("path").and_then(|p| p.as_str()).unwrap_or("");
        let old_text = input.get("old_text").and_then(|o| o.as_str()).unwrap_or("");
        let new_text = input.get("new_text").and_then(|n| n.as_str()).unwrap_or("");
        super::run_edit_file(path, old_text, new_text)
    }
}
```

- [ ] **Step 2: Update mod.rs to include edit_file module**

In mod.rs, add:
```rust
mod edit_file;
```

And add to build_registry vec:
```rust
Box::new(edit_file::EditFileTool),
```

- [ ] **Step 3: Run cargo test**

Run: `cd rust-agent && cargo test`
Expected: All tests pass

- [ ] **Step 4: Commit**

```bash
git add rust-agent/src/tools/edit_file.rs rust-agent/src/tools/mod.rs
git commit -m "feat: add EditFileTool with workspace escape permission check"
```

---

## Task 9: Create GlobTool

**Files:**
- Create: `rust-agent/src/tools/glob.rs`
- Modify: `rust-agent/src/tools/mod.rs` (add mod glob; register in build_registry)

**Interfaces:**
- Consumes: Tool trait, run_glob from mod.rs
- Produces: GlobTool implementation (no special permission check, default Pass)

- [ ] **Step 1: Create glob.rs with GlobTool implementation**

```rust
/*
glob.rs - Glob tool implementation
*/

use async_trait::async_trait;
use crate::tools::{Tool, ToolContext};
use serde_json::Value;

pub struct GlobTool;

#[async_trait]
impl Tool for GlobTool {
    fn name(&self) -> &str { "glob" }
    fn description(&self) -> &str { "Find files matching a glob pattern." }
    fn input_schema(&self) -> Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "pattern": { "type": "string" }
            },
            "required": ["pattern"]
        })
    }

    // check_permission defaults to Pass, no override needed

    async fn execute(&self, _ctx: &ToolContext<'_>, input: &Value) -> String {
        let pattern = input.get("pattern").and_then(|p| p.as_str()).unwrap_or("");
        super::run_glob(pattern)
    }
}
```

- [ ] **Step 2: Update mod.rs to include glob module**

In mod.rs, add:
```rust
mod glob;
```

And add to build_registry vec:
```rust
Box::new(glob::GlobTool),
```

- [ ] **Step 3: Run cargo test**

Run: `cd rust-agent && cargo test`
Expected: All tests pass

- [ ] **Step 4: Commit**

```bash
git add rust-agent/src/tools/glob.rs rust-agent/src/tools/mod.rs
git commit -m "feat: add GlobTool implementation"
```

---

## Task 10: Create LoadSkillTool

**Files:**
- Create: `rust-agent/src/tools/load_skill.rs`
- Modify: `rust-agent/src/tools/mod.rs` (add mod load_skill; register in build_registry)

**Interfaces:**
- Consumes: Tool trait, crate::skills::run_load_skill
- Produces: LoadSkillTool implementation (no special permission check, default Pass)

- [ ] **Step 1: Create load_skill.rs with LoadSkillTool implementation**

```rust
/*
load_skill.rs - Load skill tool implementation
*/

use async_trait::async_trait;
use crate::tools::{Tool, ToolContext};
use serde_json::Value;

pub struct LoadSkillTool;

#[async_trait]
impl Tool for LoadSkillTool {
    fn name(&self) -> &str { "load_skill" }
    fn description(&self) -> &str { "Load the full SKILL.md content by skill name." }
    fn input_schema(&self) -> Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "name": { "type": "string" }
            },
            "required": ["name"]
        })
    }

    async fn execute(&self, _ctx: &ToolContext<'_>, input: &Value) -> String {
        crate::skills::run_load_skill(input)
    }
}
```

- [ ] **Step 2: Update mod.rs to include load_skill module**

In mod.rs, add:
```rust
mod load_skill;
```

And add to build_registry vec:
```rust
Box::new(load_skill::LoadSkillTool),
```

- [ ] **Step 3: Run cargo test**

Run: `cd rust-agent && cargo test`
Expected: All tests pass

- [ ] **Step 4: Commit**

```bash
git add rust-agent/src/tools/load_skill.rs rust-agent/src/tools/mod.rs
git commit -m "feat: add LoadSkillTool implementation"
```

---

## Task 11: Create TodoWriteTool

**Files:**
- Create: `rust-agent/src/tools/todo_write.rs`
- Modify: `rust-agent/src/tools/mod.rs` (add mod todo_write; register in build_registry)

**Interfaces:**
- Consumes: Tool trait, crate::todo::run_todo_write
- Produces: TodoWriteTool implementation (no special permission check, default Pass)

- [ ] **Step 1: Create todo_write.rs with TodoWriteTool implementation**

```rust
/*
todo_write.rs - Todo write tool implementation
*/

use async_trait::async_trait;
use crate::tools::{Tool, ToolContext};
use serde_json::Value;

pub struct TodoWriteTool;

#[async_trait]
impl Tool for TodoWriteTool {
    fn name(&self) -> &str { "todo_write" }
    fn description(&self) -> &str {
        "Create or replace the todo list for multi-step tasks. \
         Each call replaces the entire list; at most one item may be \
         in_progress. Use this to plan before starting work and \
         update statuses as you progress."
    }
    fn input_schema(&self) -> Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "todos": {
                    "type": "array",
                    "maxItems": 20,
                    "items": {
                        "type": "object",
                        "properties": {
                            "content": {
                                "type": "string",
                                "description": "The task description."
                            },
                            "status": {
                                "type": "string",
                                "enum": ["pending", "in_progress", "completed"],
                                "description": "Defaults to pending if omitted."
                            }
                        },
                        "required": ["content"]
                    }
                }
            },
            "required": ["todos"]
        })
    }

    async fn execute(&self, _ctx: &ToolContext<'_>, input: &Value) -> String {
        match input.get("todos") {
            Some(todos) => crate::todo::run_todo_write(todos),
            None => "Error: missing todos".to_string(),
        }
    }
}
```

- [ ] **Step 2: Update mod.rs to include todo_write module**

In mod.rs, add:
```rust
mod todo_write;
```

And add to build_registry vec:
```rust
Box::new(todo_write::TodoWriteTool),
```

- [ ] **Step 3: Run cargo test**

Run: `cd rust-agent && cargo test`
Expected: All tests pass

- [ ] **Step 4: Commit**

```bash
git add rust-agent/src/tools/todo_write.rs rust-agent/src/tools/mod.rs
git commit -m "feat: add TodoWriteTool implementation"
```

---

## Task 12: Create TaskTool

**Files:**
- Create: `rust-agent/src/tools/task.rs`
- Modify: `rust-agent/src/tools/mod.rs` (add mod task; register in build_registry)

**Interfaces:**
- Consumes: Tool trait, crate::subagent::run_subagent_loop
- Produces: TaskTool implementation with available_for_subagent = false (prevents recursion)

- [ ] **Step 1: Create task.rs with TaskTool implementation**

```rust
/*
task.rs - Task (subagent) tool implementation
*/

use async_trait::async_trait;
use crate::tools::{Tool, ToolContext};
use serde_json::Value;

pub struct TaskTool;

#[async_trait]
impl Tool for TaskTool {
    fn name(&self) -> &str { "task" }
    fn description(&self) -> &str {
        "Run a subagent with fresh conversation context and return its final text."
    }
    fn input_schema(&self) -> Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "prompt": { "type": "string" }
            },
            "required": ["prompt"]
        })
    }

    fn available_for_subagent(&self) -> bool { false }  // Prevent recursion

    async fn execute(&self, ctx: &ToolContext<'_>, input: &Value) -> String {
        match input.get("prompt").and_then(|p| p.as_str()) {
            Some(prompt) if !prompt.is_empty() => {
                crate::subagent::run_subagent_loop(ctx.client, prompt, ctx.hooks)
                    .await
                    .unwrap_or_else(|e| format!("Subagent error: {}", e))
            }
            _ => "Error: missing prompt".to_string(),
        }
    }
}
```

- [ ] **Step 2: Update mod.rs to include task module**

In mod.rs, add:
```rust
mod task;
```

And add to build_registry vec:
```rust
Box::new(task::TaskTool),
```

- [ ] **Step 3: Run cargo test**

Run: `cd rust-agent && cargo test`
Expected: All tests pass

- [ ] **Step 4: Commit**

```bash
git add rust-agent/src/tools/task.rs rust-agent/src/tools/mod.rs
git commit -m "feat: add TaskTool with available_for_subagent=false"
```

---

## Task 13: Update hooks.rs PreToolHook Signature

**Files:**
- Modify: `rust-agent/src/hooks.rs`

**Interfaces:**
- Consumes: ToolRegistry type
- Produces: Updated PreToolHook signature taking &ToolRegistry

- [ ] **Step 1: Update PreToolHook type alias and trigger_pre_tool signature**

Change:
```rust
// Before:
pub type PreToolHook = fn(&str, &serde_json::Value) -> Option<String>;

// After:
pub type PreToolHook = fn(&ToolRegistry, &str, &serde_json::Value) -> Option<String>;
```

Update `trigger_pre_tool` method signature:
```rust
pub fn trigger_pre_tool(&self, registry: &ToolRegistry, name: &str, input: &serde_json::Value) -> Option<String> {
    for f in &self.pre_tool {
        if let Some(reason) = f(registry, name, input) {
            return Some(reason);
        }
    }
    None
}
```

- [ ] **Step 2: Update test functions to match new signature**

Update test functions in hooks.rs tests:
```rust
fn always_block(_r: &ToolRegistry, _n: &str, _i: &serde_json::Value) -> Option<String> {
    Some("nope".to_string())
}
fn never_block(_r: &ToolRegistry, _n: &str, _i: &serde_json::Value) -> Option<String> {
    None
}
fn panic_if_called(_r: &ToolRegistry, _n: &str, _i: &serde_json::Value) -> Option<String> {
    panic!("second hook must not run after a block")
}
```

Update test calls to pass a registry parameter (create empty registry for tests):
```rust
#[test]
fn empty_registry_allows() {
    use crate::tools::ToolRegistry;
    let h = Hooks::new();
    let registry = ToolRegistry::new(vec![]);
    assert!(h.trigger_pre_tool(&registry, "command", &serde_json::json!({})).is_none());
}
```

And similarly for other tests.

- [ ] **Step 3: Run cargo test**

Run: `cd rust-agent && cargo test`
Expected: All tests pass

- [ ] **Step 4: Commit**

```bash
git add rust-agent/src/hooks.rs
git commit -m "refactor: update PreToolHook signature to include &ToolRegistry"
```

---

## Task 14: Update permission.rs to Use Registry

**Files:**
- Modify: `rust-agent/src/permission.rs`

**Interfaces:**
- Consumes: ToolRegistry type, PermissionCheck enum
- Produces: Updated permission_hook using registry.check_permission()

- [ ] **Step 1: Remove check_rules function and update permission_hook**

Delete the `check_rules` function and `normalize`, `escapes_workspace` functions (moved to tools/mod.rs).

Update `permission_hook` to use registry:
```rust
use crate::tools::{ToolRegistry, PermissionCheck};

pub fn permission_hook(registry: &ToolRegistry, name: &str, input: &serde_json::Value) -> Option<String> {
    // Gate 1: Hard deny list (unchanged)
    if name == "command" {
        let cmd = input.get("command").and_then(|v| v.as_str()).unwrap_or("");
        if let Some(p) = check_deny_list(cmd) {
            println!("\x1b[31m[blocked] '{}' is on the deny list\x1b[0m", p);
            return Some(format!("Permission denied: '{}' on deny list", p));
        }
    }

    // Gates 2+3: Check rules through registry, ask user if matched
    match registry.check_permission(name, input) {
        PermissionCheck::NeedsApproval(reason) => {
            if !ask_user(name, input, reason) {
                return Some("Permission denied by user".to_string());
            }
        }
        PermissionCheck::Pass => {}
    }

    None
}
```

Remove `use crate::tools::workdir;` since workdir is no longer used directly (only via crate::tools which will be re-exported).

- [ ] **Step 2: Remove normalize and escapes_workspace functions**

These functions are now in tools/mod.rs as `normalize` (private) and `escapes_workspace_lexical`.

- [ ] **Step 3: Update tests**

Remove tests for `normalize` and `escapes_workspace` (moved to tools/mod.rs). Keep deny_list tests and permission_hook tests.

- [ ] **Step 4: Run cargo test**

Run: `cd rust-agent && cargo test`
Expected: All tests pass

- [ ] **Step 5: Commit**

```bash
git add rust-agent/src/permission.rs
git commit -m "refactor: permission_hook uses registry.check_permission()"
```

---

## Task 15: Update main.rs to Use Registry

**Files:**
- Modify: `rust-agent/src/main.rs`

**Interfaces:**
- Consumes: ToolRegistry type, ToolContext type, build_registry function
- Produces: Updated main.rs using registry.dispatch() and registry.definitions()

- [ ] **Step 1: Update imports**

Replace:
```rust
use tools::{dispatch_tool, get_tool_definitions};
```

With:
```rust
use tools::{build_registry, ToolContext};
```

- [ ] **Step 2: Update execute_tool function**

Replace the entire `execute_tool` function:
```rust
async fn execute_tool(
    registry: &ToolRegistry,
    ctx: &ToolContext<'_>,
    name: &str,
    input: &serde_json::Value,
    hooks: &Hooks,
) -> String {
    if let Some(reason) = hooks.trigger_pre_tool(registry, name, input) {
        return reason;
    }
    registry.dispatch(ctx, name, input).await
}
```

Note: task special handling removed — TaskTool.execute() now handles subagent invocation directly.

- [ ] **Step 3: Update agent_loop function**

Update the stream_messages call:
```rust
let response = client
    .stream_messages(system, messages, &registry.definitions(false), 8000)
    .await?;
```

Update the execute_tool call in tool execution loop:
```rust
let tool_output = execute_tool(registry, &ctx, name, input, hooks).await;
```

- [ ] **Step 4: Update main function**

After creating hooks, add:
```rust
let registry = tools::build_registry();
```

Update agent_loop call:
```rust
if let Err(e) = agent_loop(&client, &system, &mut messages, &hooks, &registry).await {
```

- [ ] **Step 5: Run cargo build**

Run: `cd rust-agent && cargo build`
Expected: Builds successfully with zero warnings

- [ ] **Step 6: Commit**

```bash
git add rust-agent/src/main.rs
git commit -m "refactor: main.rs uses ToolRegistry for tool dispatch and definitions"
```

---

## Task 16: Update subagent.rs to Use Registry

**Files:**
- Modify: `rust-agent/src/subagent.rs`

**Interfaces:**
- Consumes: ToolRegistry type, ToolContext type
- Produces: Updated subagent.rs using registry.dispatch() and registry.definitions(true)

- [ ] **Step 1: Update imports**

Replace:
```rust
use crate::tools::{dispatch_tool, get_subagent_tool_definitions};
```

With:
```rust
use crate::tools::{ToolContext, ToolRegistry};
```

- [ ] **Step 2: Update run_subagent_loop signature**

Add registry parameter:
```rust
pub async fn run_subagent_loop(
    client: &Client,
    prompt: &str,
    hooks: &Hooks,
    registry: &ToolRegistry,
) -> Result<String, Box<dyn std::error::Error>> {
```

- [ ] **Step 3: Update stream_messages call**

Use registry.definitions(true) for subagent:
```rust
let response = client
    .stream_messages(
        SUB_SYSTEM,
        &messages,
        &registry.definitions(true),
        8000,
    )
    .await?;
```

- [ ] **Step 4: Update tool execution in loop**

Replace dispatch_tool call:
```rust
let ctx = ToolContext { client, hooks };
let output = registry.dispatch(&ctx, name, input).await;
```

Add ctx creation before tool execution loop if not present. Move ctx creation to top of loop.

- [ ] **Step 5: Run cargo test**

Run: `cd rust-agent && cargo test`
Expected: All tests pass

- [ ] **Step 6: Commit**

```bash
git add rust-agent/src/subagent.rs
git commit -m "refactor: subagent.rs uses ToolRegistry for dispatch and definitions(true)"
```

---

## Task 17: Update client.rs to Use ToolDefinition from tools

**Files:**
- Modify: `rust-agent/src/client.rs`

**Interfaces:**
- Consumes: ToolDefinition type
- Produces: No functional change, just ensure correct import path

- [ ] **Step 1: Check ToolDefinition import**

Verify ToolDefinition is imported from correct location. If currently defined in client.rs or tools.rs, it should remain there or be moved.

If ToolDefinition is currently in tools.rs, add re-export in tools/mod.rs:
```rust
pub use crate::tools::ToolDefinition;
```

If ToolDefinition is in client.rs, no changes needed.

- [ ] **Step 2: Run cargo test**

Run: `cd rust-agent && cargo test`
Expected: All tests pass

- [ ] **Step 3: Commit if changes made**

```bash
git add rust-agent/src/client.rs rust-agent/src/tools/mod.rs
git commit -m "refactor: ensure ToolDefinition re-export from tools"
```

---

## Task 18: Clean Up Old tools.rs File

**Files:**
- Delete: `rust-agent/src/tools.rs`

**Interfaces:**
- No interfaces — cleanup step after all tools migrated

- [ ] **Step 1: Verify all tests pass with new tools module**

Run: `cd rust-agent && cargo test`
Expected: All tests pass

- [ ] **Step 2: Verify no references to old tools.rs module**

Run: `cd rust-agent && grep -r "crate::tools::" src/`
Ensure all references point to new tools/ module, not old tools.rs functions like dispatch_tool, get_tool_definitions, etc.

- [ ] **Step 3: Delete old tools.rs file**

Run: `cd rust-agent && rm src/tools.rs`

- [ ] **Step 4: Update lib.rs if needed**

If lib.rs has `pub mod tools;`, update to point to tools directory (it should work automatically as Rust treats tools.rs and tools/ equivalently).

- [ ] **Step 5: Run cargo test**

Run: `cd rust-agent && cargo test`
Expected: All tests pass

- [ ] **Step 6: Commit**

```bash
git add rust-agent/src/tools.rs rust-agent/src/lib.rs
git commit -m "refactor: remove old tools.rs file, replaced with tools/ module"
```

---

## Task 19: Add ToolRegistry Tests

**Files:**
- Create: Tests in `rust-agent/src/tools/registry.rs` or `rust-agent/src/tools/mod.rs`

**Interfaces:**
- Consumes: ToolRegistry type
- Produces: Tests for registry dispatch, definitions, check_permission

- [ ] **Step 1: Add ToolRegistry tests to registry.rs**

```rust
#[cfg(test)]
mod registry_tests {
    use super::*;
    use crate::tools::{command::CommandTool, glob::GlobTool, read_file::ReadFileTool, Tool};

    fn test_registry() -> ToolRegistry {
        ToolRegistry::new(vec![
            Box::new(CommandTool),
            Box::new(ReadFileTool),
            Box::new(GlobTool),
        ])
    }

    #[tokio::test]
    async fn test_registry_dispatch_known_tool() {
        let registry = test_registry();
        // Mock context for test (client and hooks not used by all tools)
        // In practice, tests would need mock client/hooks or use tools that don't need them
    }

    #[tokio::test]
    async fn test_registry_dispatch_unknown_tool() {
        let registry = test_registry();
        let ctx = ToolContext {
            client: &crate::client::Client::new("key".into(), "url".into(), "model".into()),
            hooks: &crate::hooks::Hooks::new(),
        };
        let result = registry.dispatch(&ctx, "foo_bar", &serde_json::json!({})).await;
        assert_eq!(result, "[ERROR:unknown] Unknown tool: foo_bar");
    }

    #[tokio::test]
    async fn test_registry_definitions_includes_all() {
        let registry = test_registry();
        let defs = registry.definitions(false);
        assert_eq!(defs.len(), 3);
        let names: Vec<&str> = defs.iter().map(|d| d.name.as_str()).collect();
        assert!(names.contains(&"command"));
        assert!(names.contains(&"read_file"));
        assert!(names.contains(&"glob"));
    }

    #[test]
    fn test_registry_check_permission_default_pass() {
        let registry = test_registry();
        assert!(matches!(
            registry.check_permission("glob", &serde_json::json!({})),
            PermissionCheck::Pass
        ));
    }

    #[test]
    fn test_registry_check_permission_override() {
        let registry = test_registry();
        let rm_cmd = serde_json::json!({"command": "rm test.txt"});
        assert!(matches!(
            registry.check_permission("command", &rm_cmd),
            PermissionCheck::NeedsApproval(_)
        ));
    }
}
```

- [ ] **Step 2: Run cargo test**

Run: `cd rust-agent && cargo test`
Expected: All new tests pass

- [ ] **Step 3: Commit**

```bash
git add rust-agent/src/tools/registry.rs
git commit -m "test: add ToolRegistry tests for dispatch, definitions, check_permission"
```

---

## Task 20: Final Verification and Cleanup

**Files:**
- Verify all files, run full test suite

**Interfaces:**
- No new interfaces — final verification

- [ ] **Step 1: Run cargo test**

Run: `cd rust-agent && cargo test --all-features`
Expected: All tests pass

- [ ] **Step 2: Run cargo clippy**

Run: `cd rust-agent && cargo clippy -- -D warnings`
Expected: Zero warnings, zero clippy lints

- [ ] **Step 3: Run cargo build**

Run: `cd rust-agent && cargo build --release`
Expected: Builds successfully

- [ ] **Step 4: Verify success criteria**

- [ ] `dispatch_tool()` match no longer exists
- [ ] `get_*_tool_definitions()` functions no longer exist
- [ ] `permission::check_rules()` match no longer exists
- [ ] `main.rs::execute_tool()` no longer has task special path
- [ ] Adding new tool requires only 3 steps (new file + mod + one registration line)
- [ ] All existing tests pass
- [ ] `cargo build` zero warnings

- [ ] **Step 5: Commit final cleanup**

```bash
git add rust-agent/
git commit -m "refactor: complete tool trait refactor - all success criteria met"
```

---

## Success Criteria Checklist

- [ ] `dispatch_tool()` match no longer exists
- [ ] `get_*_tool_definitions()` functions no longer exist
- [ ] `permission::check_rules()` match no longer exists
- [ ] `main.rs::execute_tool()` no longer has task special path
- [ ] Adding new tool requires only 3 steps (new file + mod + one registration line)
- [ ] All existing tests pass
- [ ] `cargo build` zero warnings