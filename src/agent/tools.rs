//! The agent's tools: what an agent can do to the project, and nothing else.
//!
//! Each tool is a name, a JSON schema the model sees, and an executor. An
//! executor returns `Err(text)` for anything the model got wrong (bad
//! arguments, a path outside the project, an edit that matches nothing); the
//! loop hands that text back as the tool result so the model can correct
//! itself. Every result is size-capped, with a marker where it was cut.

use super::sandbox::Sandbox;
use super::shell;
use crate::config::AgentTool;
use crate::port::ToolDef;
use serde_json::{Value, json};
use std::fs;
use std::path::Path;

const DEFAULT_READ_LINES: usize = 2000;
const MAX_GLOB_RESULTS: usize = 200;
const MAX_GREP_MATCHES: usize = 100;
const MAX_GREP_LINE_CHARS: usize = 300;
const MAX_GREP_FILE_BYTES: u64 = 1_000_000;
/// Build output and VCS internals are never what a model is looking for.
const SKIP_DIRS: &[&str] = &[".git", "target", "node_modules"];

pub fn wire_name(tool: AgentTool) -> &'static str {
    match tool {
        AgentTool::Read => "read_file",
        AgentTool::Glob => "glob",
        AgentTool::Grep => "grep",
        AgentTool::Edit => "edit_file",
        AgentTool::Write => "write_file",
        AgentTool::Bash => "bash",
        AgentTool::Fetch => "web_fetch",
    }
}

pub fn definitions(tools: &[AgentTool]) -> Vec<ToolDef> {
    tools.iter().map(|t| definition(*t)).collect()
}

fn definition(tool: AgentTool) -> ToolDef {
    let (description, parameters) = match tool {
        AgentTool::Read => (
            "Read a text file in the project. Paths are relative to the project root.",
            json!({
                "type": "object",
                "properties": {
                    "path": {"type": "string"},
                    "offset": {"type": "integer", "description": "First line to return, 1-based."},
                    "limit": {"type": "integer", "description": "Maximum lines to return."}
                },
                "required": ["path"]
            }),
        ),
        AgentTool::Glob => (
            "List project files whose path matches a glob. '*' matches within one \
             path segment, '**' any number of segments, '?' one character. A pattern \
             with no '/' matches file names at any depth.",
            json!({
                "type": "object",
                "properties": {"pattern": {"type": "string"}},
                "required": ["pattern"]
            }),
        ),
        AgentTool::Grep => (
            "Search file contents for a literal string. Returns 'path:line: text'. \
             Not a regular expression.",
            json!({
                "type": "object",
                "properties": {
                    "pattern": {"type": "string"},
                    "path": {"type": "string", "description": "File or directory to search. Default: the whole project."}
                },
                "required": ["pattern"]
            }),
        ),
        AgentTool::Edit => (
            "Replace one exact occurrence of old_string with new_string in a file. \
             Fails if old_string is not found, or appears more than once; include \
             enough surrounding text to make it unique.",
            json!({
                "type": "object",
                "properties": {
                    "path": {"type": "string"},
                    "old_string": {"type": "string"},
                    "new_string": {"type": "string"}
                },
                "required": ["path", "old_string", "new_string"]
            }),
        ),
        AgentTool::Write => (
            "Create a file, or replace it entirely. Parent directories are created.",
            json!({
                "type": "object",
                "properties": {
                    "path": {"type": "string"},
                    "content": {"type": "string"}
                },
                "required": ["path", "content"]
            }),
        ),
        AgentTool::Bash => (
            "Run a shell command (sh -c) in the project root. Returns stdout, then \
             stderr, then the exit code. It is sandboxed: it can write only inside \
             the project and /tmp, it has no secrets in its environment, and it \
             is killed at the timeout. Use it to build, test and run things; use \
             the file tools to read and edit.",
            json!({
                "type": "object",
                "properties": {
                    "command": {"type": "string"},
                    "timeout_seconds": {"type": "integer", "description": "Default 120, at most 600."}
                },
                "required": ["command"]
            }),
        ),
        AgentTool::Fetch => (
            "Fetch a public web page or text file over http(s) and return it as text; \
             HTML is turned into plain text. Local and private network addresses are \
             refused.",
            json!({
                "type": "object",
                "properties": {"url": {"type": "string"}},
                "required": ["url"]
            }),
        ),
    };
    ToolDef {
        name: wire_name(tool).into(),
        description: description.into(),
        parameters,
    }
}

/// Run one tool call. `allowed` is the agent's permission set: a call to a tool
/// outside it is refused here, whatever the model was offered. A result longer
/// than `max_bytes` is cut, so one call cannot fill the model's context.
/// `stop` turning true ends a long-running call early.
pub fn execute(
    sandbox: &Sandbox,
    allowed: &[AgentTool],
    shell_policy: &shell::Policy,
    name: &str,
    arguments: &str,
    max_bytes: usize,
    stop: &dyn Fn() -> bool,
) -> Result<String, String> {
    let tool = allowed
        .iter()
        .copied()
        .find(|t| wire_name(*t) == name)
        .ok_or_else(|| {
            let offered: Vec<&str> = allowed.iter().map(|t| wire_name(*t)).collect();
            format!("no tool '{name}'. Available tools: {}.", offered.join(", "))
        })?;
    let args: Value = serde_json::from_str(arguments)
        .map_err(|e| format!("arguments are not valid JSON ({e}): {arguments}"))?;
    let out = match tool {
        AgentTool::Read => read(sandbox, &args)?,
        AgentTool::Glob => glob(sandbox, &args)?,
        AgentTool::Grep => grep(sandbox, &args)?,
        AgentTool::Edit => edit(sandbox, &args)?,
        AgentTool::Write => write(sandbox, &args)?,
        AgentTool::Bash => bash(sandbox, &args, shell_policy, stop)?,
        AgentTool::Fetch => super::fetch::get(str_arg(&args, "url")?)?,
    };
    Ok(cap(out, max_bytes))
}

fn str_arg<'a>(args: &'a Value, key: &str) -> Result<&'a str, String> {
    args.get(key)
        .and_then(Value::as_str)
        .ok_or_else(|| format!("missing string argument '{key}'"))
}

fn uint_arg(args: &Value, key: &str) -> Result<Option<usize>, String> {
    match args.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(v) => v
            .as_u64()
            .map(|n| Some(n as usize))
            .ok_or_else(|| format!("argument '{key}' must be a non-negative integer")),
    }
}

fn cap(mut s: String, max_bytes: usize) -> String {
    if s.len() > max_bytes {
        let mut end = max_bytes;
        while !s.is_char_boundary(end) {
            end -= 1;
        }
        s.truncate(end);
        s.push_str(
            "\n[output truncated to save context; ask for less: read_file takes offset and limit, grep and glob take narrower patterns]",
        );
    }
    s
}

fn read(sb: &Sandbox, args: &Value) -> Result<String, String> {
    let path = sb.existing(str_arg(args, "path")?)?;
    if path.is_dir() {
        return Err("that is a directory; use glob to list files".into());
    }
    let text = fs::read_to_string(&path).map_err(|_| "not a readable text file".to_string())?;
    let offset = uint_arg(args, "offset")?.unwrap_or(1).max(1);
    let limit = uint_arg(args, "limit")?.unwrap_or(DEFAULT_READ_LINES);
    let total = text.lines().count();
    let body: Vec<&str> = text.lines().skip(offset - 1).take(limit).collect();
    let mut out = body.join("\n");
    let end = offset - 1 + body.len();
    if end < total {
        out.push_str(&format!(
            "\n[showing lines {offset}-{end} of {total}; pass offset to read more]"
        ));
    }
    Ok(out)
}

fn glob(sb: &Sandbox, args: &Value) -> Result<String, String> {
    let pattern = str_arg(args, "pattern")?;
    let mut found = Vec::new();
    let mut total = 0;
    walk(sb.root(), &mut |p| {
        let rel = sb.show(p);
        if glob_matches(pattern, &rel) {
            total += 1;
            if found.len() < MAX_GLOB_RESULTS {
                found.push(rel);
            }
        }
    });
    found.sort();
    if found.is_empty() {
        return Ok("no files match".into());
    }
    let mut out = found.join("\n");
    if total > found.len() {
        out.push_str(&format!("\n[{total} matches, showing {}]", found.len()));
    }
    Ok(out)
}

fn grep(sb: &Sandbox, args: &Value) -> Result<String, String> {
    let pattern = str_arg(args, "pattern")?;
    if pattern.is_empty() {
        return Err("pattern is empty".into());
    }
    let start = match args.get("path").and_then(Value::as_str) {
        Some(p) => sb.existing(p)?,
        None => sb.root().to_path_buf(),
    };
    let mut files = Vec::new();
    if start.is_file() {
        files.push(start);
    } else {
        walk(&start, &mut |p| files.push(p.to_path_buf()));
        files.sort();
    }
    let mut out = Vec::new();
    let mut truncated = false;
    'files: for file in files {
        if fs::metadata(&file).map_or(true, |m| m.len() > MAX_GREP_FILE_BYTES) {
            continue;
        }
        let Ok(text) = fs::read_to_string(&file) else {
            continue; // binary or not UTF-8
        };
        for (i, line) in text.lines().enumerate() {
            if line.contains(pattern) {
                if out.len() == MAX_GREP_MATCHES {
                    truncated = true;
                    break 'files;
                }
                let line: String = line.chars().take(MAX_GREP_LINE_CHARS).collect();
                out.push(format!("{}:{}: {}", sb.show(&file), i + 1, line.trim()));
            }
        }
    }
    if out.is_empty() {
        return Ok("no matches".into());
    }
    let mut s = out.join("\n");
    if truncated {
        s.push_str(&format!("\n[stopped after {MAX_GREP_MATCHES} matches]"));
    }
    Ok(s)
}

fn edit(sb: &Sandbox, args: &Value) -> Result<String, String> {
    let rel = str_arg(args, "path")?;
    let old = str_arg(args, "old_string")?;
    let new = str_arg(args, "new_string")?;
    if old.is_empty() {
        return Err("old_string is empty; use write_file to create a file".into());
    }
    let path = sb.existing(rel)?;
    let text = fs::read_to_string(&path).map_err(|_| "not a readable text file".to_string())?;
    match text.matches(old).count() {
        0 => Err("old_string was not found; read the file again and copy it exactly".into()),
        1 => {
            fs::write(&path, text.replacen(old, new, 1)).map_err(|e| e.to_string())?;
            Ok(format!("edited {}", sb.show(&path)))
        }
        n => Err(format!(
            "old_string appears {n} times; include more surrounding text so it is unique"
        )),
    }
}

fn bash(
    sb: &Sandbox,
    args: &Value,
    policy: &shell::Policy,
    stop: &dyn Fn() -> bool,
) -> Result<String, String> {
    let command = str_arg(args, "command")?;
    let timeout = match uint_arg(args, "timeout_seconds")? {
        Some(s) => std::time::Duration::from_secs(s as u64).min(shell::MAX_TIMEOUT),
        None => shell::DEFAULT_TIMEOUT,
    };
    shell::run(sb.root(), command, timeout, policy, stop)
}

fn write(sb: &Sandbox, args: &Value) -> Result<String, String> {
    let rel = str_arg(args, "path")?;
    let content = str_arg(args, "content")?;
    let path = sb.new_or_existing(rel)?;
    if path.is_dir() {
        return Err("that is a directory".into());
    }
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).map_err(|e| e.to_string())?;
    }
    fs::write(&path, content).map_err(|e| e.to_string())?;
    Ok(format!(
        "wrote {} ({} bytes)",
        sb.show(&path),
        content.len()
    ))
}

/// What happened to one file during a run.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Change {
    Added,
    Modified,
    Deleted,
}

impl Change {
    pub fn as_str(self) -> &'static str {
        match self {
            Change::Added => "added",
            Change::Modified => "modified",
            Change::Deleted => "deleted",
        }
    }
}

/// Every file a run changed, by project-relative path.
pub type Changes = std::collections::BTreeMap<String, Change>;

/// Fold one change into what is already known about that file: a file added
/// and then edited is still added, and one added then deleted never existed.
pub fn record(changes: &mut Changes, path: String, change: Change) {
    match (changes.get(&path), change) {
        (Some(Change::Added), Change::Modified) => {}
        (Some(Change::Added), Change::Deleted) => {
            changes.remove(&path);
        }
        (Some(Change::Deleted), Change::Added) => {
            changes.insert(path, Change::Modified);
        }
        _ => {
            changes.insert(path, change);
        }
    }
}

/// The file an `edit_file` or `write_file` call is about to change, if the
/// call names one inside the project. Other tools change nothing by name.
pub fn target(sb: &Sandbox, name: &str, arguments: &str) -> Option<std::path::PathBuf> {
    if name != wire_name(AgentTool::Edit) && name != wire_name(AgentTool::Write) {
        return None;
    }
    let args: Value = serde_json::from_str(arguments).ok()?;
    sb.new_or_existing(args.get("path")?.as_str()?).ok()
}

/// Above this many files a snapshot is not taken; `bash` changes then go
/// unreported rather than every command paying for a huge walk.
const MAX_SNAPSHOT_FILES: usize = 50_000;

/// Size and modification time of every project file, skipping [`SKIP_DIRS`].
/// Comparing two of them shows what a shell command changed.
pub type Snapshot = std::collections::BTreeMap<String, (Option<std::time::SystemTime>, u64)>;

pub fn snapshot(sb: &Sandbox) -> Option<Snapshot> {
    let mut out = Snapshot::new();
    let mut too_many = false;
    walk(sb.root(), &mut |p| {
        if out.len() >= MAX_SNAPSHOT_FILES {
            too_many = true;
            return;
        }
        if let Ok(meta) = fs::metadata(p) {
            out.insert(sb.show(p), (meta.modified().ok(), meta.len()));
        }
    });
    (!too_many).then_some(out)
}

pub fn diff(before: &Snapshot, after: &Snapshot, changes: &mut Changes) {
    for (path, stamp) in after {
        match before.get(path) {
            None => record(changes, path.clone(), Change::Added),
            Some(old) if old != stamp => record(changes, path.clone(), Change::Modified),
            Some(_) => {}
        }
    }
    for path in before.keys() {
        if !after.contains_key(path) {
            record(changes, path.clone(), Change::Deleted);
        }
    }
}

/// Call `visit` for every file under `dir`, skipping [`SKIP_DIRS`]. Symlinks
/// are not followed: a link is neither visited nor descended into.
fn walk(dir: &Path, visit: &mut dyn FnMut(&Path)) {
    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let Ok(kind) = entry.file_type() else {
            continue;
        };
        let path = entry.path();
        if kind.is_dir() {
            let name = entry.file_name();
            if !SKIP_DIRS.iter().any(|s| name == *s) {
                walk(&path, visit);
            }
        } else if kind.is_file() {
            visit(&path);
        }
    }
}

pub fn glob_matches(pattern: &str, path: &str) -> bool {
    let pat: Vec<&str> = pattern.split('/').collect();
    let segs: Vec<&str> = path.split('/').collect();
    if pat.len() == 1 && pat[0] != "**" {
        return segs
            .last()
            .is_some_and(|name| segment_matches(pat[0], name));
    }
    segments_match(&pat, &segs)
}

fn segments_match(pat: &[&str], segs: &[&str]) -> bool {
    match pat.split_first() {
        None => segs.is_empty(),
        Some((&"**", rest)) => (0..=segs.len()).any(|skip| segments_match(rest, &segs[skip..])),
        Some((p, rest)) => segs
            .split_first()
            .is_some_and(|(s, tail)| segment_matches(p, s) && segments_match(rest, tail)),
    }
}

fn segment_matches(pat: &str, s: &str) -> bool {
    fn go(p: &[char], s: &[char]) -> bool {
        match p.split_first() {
            None => s.is_empty(),
            Some(('*', rest)) => (0..=s.len()).any(|i| go(rest, &s[i..])),
            Some(('?', rest)) => !s.is_empty() && go(rest, &s[1..]),
            Some((c, rest)) => s.first() == Some(c) && go(rest, &s[1..]),
        }
    }
    let p: Vec<char> = pat.chars().collect();
    let s: Vec<char> = s.chars().collect();
    go(&p, &s)
}

#[cfg(test)]
mod tests {
    use super::super::sandbox::testing::TempDir;
    use super::*;

    const ALL: [AgentTool; 5] = [
        AgentTool::Read,
        AgentTool::Glob,
        AgentTool::Grep,
        AgentTool::Edit,
        AgentTool::Write,
    ];
    const TEST_CAP: usize = 8_000;

    const NO_NET: &shell::Policy = &shell::Policy {
        allow_network: false,
        readable: Vec::new(),
    };

    fn project() -> (TempDir, Sandbox) {
        let tmp = TempDir::new();
        let root = tmp.0.join("p");
        fs::create_dir_all(root.join("src/deep")).unwrap();
        fs::create_dir_all(root.join("target")).unwrap();
        fs::write(root.join("src/main.rs"), "fn main() {}\nlet x = 1;\n").unwrap();
        fs::write(root.join("src/deep/lib.rs"), "let x = 2;\n").unwrap();
        fs::write(root.join("target/junk.rs"), "let x = 3;\n").unwrap();
        fs::write(root.join("README.md"), "hello\n").unwrap();
        let sb = Sandbox::new(&root).unwrap();
        (tmp, sb)
    }

    fn run(sb: &Sandbox, name: &str, args: Value) -> Result<String, String> {
        execute(sb, &ALL, NO_NET, name, &args.to_string(), TEST_CAP, &|| {
            false
        })
    }

    #[test]
    fn glob_patterns_follow_the_documented_rules() {
        assert!(glob_matches("*.rs", "src/deep/lib.rs"));
        assert!(glob_matches("src/*.rs", "src/main.rs"));
        assert!(!glob_matches("src/*.rs", "src/deep/lib.rs"));
        assert!(glob_matches("src/**/*.rs", "src/deep/lib.rs"));
        assert!(glob_matches("src/**/*.rs", "src/main.rs"));
        assert!(glob_matches("**/lib.rs", "src/deep/lib.rs"));
        assert!(glob_matches("READ?E.md", "README.md"));
        assert!(!glob_matches("*.md", "src/main.rs"));
    }

    #[test]
    fn glob_lists_matches_and_skips_build_output() {
        let (_t, sb) = project();
        let out = run(&sb, "glob", json!({"pattern": "*.rs"})).unwrap();
        assert_eq!(out, "src/deep/lib.rs\nsrc/main.rs");
        assert_eq!(
            run(&sb, "glob", json!({"pattern": "*.zig"})).unwrap(),
            "no files match"
        );
    }

    #[test]
    fn grep_finds_literal_lines_with_locations() {
        let (_t, sb) = project();
        let out = run(&sb, "grep", json!({"pattern": "let x"})).unwrap();
        assert_eq!(
            out,
            "src/deep/lib.rs:1: let x = 2;\nsrc/main.rs:2: let x = 1;"
        );
        let one = run(&sb, "grep", json!({"pattern": "x", "path": "README.md"})).unwrap();
        assert_eq!(one, "no matches");
    }

    #[test]
    fn read_returns_the_file_and_pages_with_offset() {
        let (_t, sb) = project();
        assert_eq!(
            run(&sb, "read_file", json!({"path": "src/main.rs"})).unwrap(),
            "fn main() {}\nlet x = 1;"
        );
        let paged = run(&sb, "read_file", json!({"path": "src/main.rs", "limit": 1})).unwrap();
        assert!(
            paged.starts_with("fn main() {}\n[showing lines 1-1 of 2"),
            "{paged}"
        );
        let second = run(
            &sb,
            "read_file",
            json!({"path": "src/main.rs", "offset": 2}),
        )
        .unwrap();
        assert_eq!(second, "let x = 1;");
    }

    #[test]
    fn edit_needs_exactly_one_match() {
        let (_t, sb) = project();
        let args = |old: &str| json!({"path": "src/main.rs", "old_string": old, "new_string": "Z"});
        assert!(
            run(&sb, "edit_file", args("nope"))
                .unwrap_err()
                .contains("not found")
        );
        run(
            &sb,
            "write_file",
            json!({"path": "dup.txt", "content": "a a"}),
        )
        .unwrap();
        let dup = json!({"path": "dup.txt", "old_string": "a", "new_string": "b"});
        assert!(run(&sb, "edit_file", dup).unwrap_err().contains("2 times"));
        assert!(run(&sb, "edit_file", args("let x = 1;")).is_ok());
        assert_eq!(
            run(&sb, "read_file", json!({"path": "src/main.rs"})).unwrap(),
            "fn main() {}\nZ"
        );
    }

    #[test]
    fn write_creates_parents_and_refuses_to_leave_the_project() {
        let (tmp, sb) = project();
        run(
            &sb,
            "write_file",
            json!({"path": "new/dir/f.txt", "content": "hi"}),
        )
        .unwrap();
        assert_eq!(
            fs::read_to_string(tmp.0.join("p/new/dir/f.txt")).unwrap(),
            "hi"
        );
        let err = run(
            &sb,
            "write_file",
            json!({"path": "../evil", "content": "x"}),
        )
        .unwrap_err();
        assert!(err.contains("'..'"), "{err}");
        assert!(!tmp.0.join("evil").exists());
    }

    #[test]
    fn a_tool_outside_the_roles_permissions_is_refused() {
        let (_t, sb) = project();
        let err = execute(
            &sb,
            &[AgentTool::Read],
            NO_NET,
            "write_file",
            "{}",
            TEST_CAP,
            &|| false,
        )
        .unwrap_err();
        assert!(err.contains("Available tools: read_file"), "{err}");
    }

    #[test]
    fn bad_arguments_come_back_as_text_for_the_model() {
        let (_t, sb) = project();
        assert!(
            execute(&sb, &ALL, NO_NET, "read_file", "{oops", TEST_CAP, &|| false)
                .unwrap_err()
                .contains("not valid JSON")
        );
        assert!(
            run(&sb, "read_file", json!({}))
                .unwrap_err()
                .contains("'path'")
        );
    }

    #[test]
    fn a_snapshot_diff_finds_added_modified_and_deleted_files() {
        let (tmp, sb) = project();
        let before = snapshot(&sb).unwrap();
        fs::write(tmp.0.join("p/new.txt"), "n").unwrap();
        fs::write(tmp.0.join("p/README.md"), "changed and longer\n").unwrap();
        fs::remove_file(tmp.0.join("p/src/deep/lib.rs")).unwrap();
        fs::write(tmp.0.join("p/target/junk.rs"), "ignored").unwrap();
        let mut changes = Changes::new();
        diff(&before, &snapshot(&sb).unwrap(), &mut changes);
        let got: Vec<(&str, Change)> = changes.iter().map(|(p, c)| (p.as_str(), *c)).collect();
        assert_eq!(
            got,
            vec![
                ("README.md", Change::Modified),
                ("new.txt", Change::Added),
                ("src/deep/lib.rs", Change::Deleted),
            ]
        );
    }

    #[test]
    fn changes_to_one_file_fold_into_one_entry() {
        let mut c = Changes::new();
        record(&mut c, "a".into(), Change::Added);
        record(&mut c, "a".into(), Change::Modified);
        assert_eq!(c["a"], Change::Added);
        record(&mut c, "a".into(), Change::Deleted);
        assert!(c.is_empty(), "added then deleted never existed");
        record(&mut c, "b".into(), Change::Deleted);
        record(&mut c, "b".into(), Change::Added);
        assert_eq!(c["b"], Change::Modified);
    }

    #[test]
    fn only_edit_and_write_name_a_target() {
        let (_t, sb) = project();
        let t = target(&sb, "edit_file", r#"{"path":"src/main.rs"}"#).unwrap();
        assert_eq!(sb.show(&t), "src/main.rs");
        assert!(target(&sb, "write_file", r#"{"path":"../x"}"#).is_none());
        assert!(target(&sb, "read_file", r#"{"path":"src/main.rs"}"#).is_none());
    }

    #[test]
    fn large_results_are_cut_with_a_marker() {
        let (_t, sb) = project();
        let big = format!("{}\n", "x".repeat(100)).repeat(1000);
        run(
            &sb,
            "write_file",
            json!({"path": "big.txt", "content": big}),
        )
        .unwrap();
        let out = run(&sb, "read_file", json!({"path": "big.txt"})).unwrap();
        assert!(out.contains("[output truncated"));
        assert!(out.len() < TEST_CAP + 200);
    }
}
