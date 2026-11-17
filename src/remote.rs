//! Helper teams that live in a git repository somewhere else.
//!
//! A remote helper team is a git URL pinned to one commit: `<url>#<sha>`. It is
//! cloned into the user's cache, checked out at exactly that commit, and checked
//! again every time it is used. Its config decides what runs on this machine
//! (agent commands, gate commands, tools, permission modes), so it is never used
//! until the person has read what it would run and trusted that very commit.
//!
//! Ensemble stores no git credentials: `git` does the talking, with whatever
//! SSH agent or credential helper the user already has.

use crate::config::{Adapter, Config, Isolation};
use crate::state;
use std::path::{Path, PathBuf};
use std::process::Command;

/// Where a helper team comes from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Source {
    /// A directory on this machine.
    Local(PathBuf),
    /// A git repository, at `sha` when it is pinned, else at `git_ref` (or at
    /// its default branch when that is `None` too).
    Remote {
        url: String,
        sha: Option<String>,
        git_ref: Option<String>,
    },
}

/// Is `text` 40 hexadecimal digits, a full commit id?
pub fn is_sha(text: &str) -> bool {
    text.len() == 40 && text.chars().all(|c| c.is_ascii_hexdigit())
}

/// Read what `team pull` was given: a path, or a git URL with an optional `@ref`,
/// or a pinned one, `#sha`.
pub fn parse_source(text: &str) -> Result<Source, String> {
    let text = text.trim();
    if text.is_empty() {
        return Err("the helper team is empty".into());
    }
    let local = text.starts_with('/')
        || text.starts_with("./")
        || text.starts_with("../")
        || text.starts_with('~')
        || text == "."
        || text == "..";
    if local || (Path::new(text).is_dir() && !text.contains("://")) {
        return Ok(Source::Local(PathBuf::from(text)));
    }
    let (rest, sha) = match text.rsplit_once('#') {
        Some((rest, sha)) if is_sha(sha) => (rest, Some(sha.to_ascii_lowercase())),
        Some((_, bad)) => {
            return Err(format!(
                "'#{bad}' is not a commit: a pinned helper team is <url>#<40 hex digits>"
            ));
        }
        None => (text, None),
    };
    // `@ref` comes after the last `/` or `:`, so `git@host:org/repo` has none.
    let after = rest.rfind(['/', ':']).map_or(0, |i| i + 1);
    let (url, git_ref) = match rest[after..].rfind('@') {
        Some(at) => (
            &rest[..after + at],
            Some(rest[after + at + 1..].to_string()),
        ),
        None => (rest, None),
    };
    if git_ref.as_deref() == Some("") {
        return Err(format!("'{text}' ends in '@' with no ref after it"));
    }
    if sha.is_some() && git_ref.is_some() {
        return Err("give a pin (#sha) or a ref (@ref), not both".into());
    }
    let url = normalize_url(url)?;
    Ok(Source::Remote { url, sha, git_ref })
}

/// `github.com/acme/org` is `https://github.com/acme/org`. Refuses what could
/// make git run a program or read an option.
fn normalize_url(url: &str) -> Result<String, String> {
    if url.starts_with('-') {
        return Err(format!("'{url}' is not a git URL"));
    }
    if url.contains("::") && !url.contains("://") {
        return Err(format!("'{url}': git transport helpers are not allowed"));
    }
    let known = ["https://", "http://", "ssh://", "git://", "file://"];
    if known.iter().any(|p| url.starts_with(p)) {
        return Ok(url.to_string());
    }
    // scp-style: git@host:path
    if let Some((user_host, path)) = url.split_once(':')
        && user_host.contains('@')
        && !path.is_empty()
    {
        return Ok(url.to_string());
    }
    if url.contains("://") {
        return Err(format!(
            "'{url}': only https, http, ssh, git and file URLs are allowed"
        ));
    }
    if url.split('/').count() >= 3 && url.contains('.') {
        return Ok(format!("https://{url}"));
    }
    Err(format!(
        "'{url}' is neither a directory nor a git URL. Use a path, or a URL such as github.com/acme/org@v1.0"
    ))
}

/// The pinned form of a remote: `<url>#<sha>`.
pub fn pinned(url: &str, sha: &str) -> String {
    format!("{url}#{sha}")
}

fn git(dir: Option<&Path>, args: &[&str]) -> Result<String, String> {
    let mut cmd = Command::new("git");
    cmd.args(["-c", "protocol.ext.allow=never"]);
    if let Some(dir) = dir {
        cmd.arg("-C").arg(dir);
    }
    let out = cmd
        .args(args)
        .env("GIT_TERMINAL_PROMPT", "0")
        .output()
        .map_err(|e| {
            if e.kind() == std::io::ErrorKind::NotFound {
                "`git` is not on PATH; it is needed to fetch a remote helper team".to_string()
            } else {
                format!("cannot run git: {e}")
            }
        })?;
    if out.status.success() {
        Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
    } else {
        Err(format!(
            "git {} failed: {}",
            args.first().copied().unwrap_or(""),
            String::from_utf8_lossy(&out.stderr).trim()
        ))
    }
}

/// The commit `git_ref` (a tag or branch; `None` is the default branch) of the
/// repository at `url` points at now.
pub fn resolve(url: &str, git_ref: Option<&str>) -> Result<String, String> {
    if let Some(r) = git_ref
        && is_sha(r)
    {
        return Ok(r.to_ascii_lowercase());
    }
    let candidates: Vec<String> = match git_ref {
        None => vec!["HEAD".into()],
        Some(r) => vec![
            format!("refs/tags/{r}^{{}}"),
            format!("refs/tags/{r}"),
            format!("refs/heads/{r}"),
        ],
    };
    for candidate in &candidates {
        let listed = git(None, &["ls-remote", "--", url, candidate])?;
        if let Some(sha) = listed.split_whitespace().next()
            && is_sha(sha)
        {
            return Ok(sha.to_string());
        }
    }
    Err(format!(
        "{url} has no {}",
        git_ref.map_or("default branch".to_string(), |r| format!(
            "tag or branch '{r}'"
        ))
    ))
}

/// Where the checkout of `url` at `sha` lives in the cache.
pub fn cache_dir(url: &str, sha: &str) -> Result<PathBuf, String> {
    Ok(state::cache_root()?
        .join("orgs")
        .join(state::labelled_text(url))
        .join(sha))
}

/// The checkout of `url` at exactly `sha`, cloned into the cache when it is
/// not there. A checkout that is there but is not that commit, or has been
/// changed, is refused: the cache is trusted to hold what was approved, not to
/// hold something that merely looks like it.
pub fn fetch(url: &str, sha: &str) -> Result<PathBuf, String> {
    if !is_sha(sha) {
        return Err(format!("'{sha}' is not a full commit id"));
    }
    let dir = cache_dir(url, sha)?;
    if dir.join(".git").exists() {
        verify(&dir, sha)?;
        return Ok(dir);
    }
    let parent = dir.parent().expect("a cache directory has a parent");
    std::fs::create_dir_all(parent)
        .map_err(|e| format!("cannot create {}: {e}", parent.display()))?;
    let partial = parent.join(format!("{sha}.partial-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&partial);
    let partial_str = partial.to_string_lossy().to_string();
    git(
        None,
        &["clone", "--quiet", "--no-checkout", "--", url, &partial_str],
    )?;
    let checkout = git(Some(&partial), &["checkout", "--quiet", "--detach", sha]);
    if let Err(e) = checkout {
        let _ = std::fs::remove_dir_all(&partial);
        return Err(format!("{url} has no commit {sha}: {e}"));
    }
    if let Err(e) = verify(&partial, sha) {
        let _ = std::fs::remove_dir_all(&partial);
        return Err(e);
    }
    std::fs::rename(&partial, &dir)
        .map_err(|e| format!("cannot move the checkout into {}: {e}", dir.display()))?;
    Ok(dir)
}

/// The checkout in `dir` is `sha`, unchanged.
fn verify(dir: &Path, sha: &str) -> Result<(), String> {
    let head = git(Some(dir), &["rev-parse", "HEAD"])?;
    if head != sha {
        return Err(format!(
            "the cached copy in {} is at {head}, not {sha}. Delete it and try again.",
            dir.display()
        ));
    }
    let changes = git(
        Some(dir),
        &["status", "--porcelain", "--untracked-files=all"],
    )?;
    if !changes.is_empty() {
        return Err(format!(
            "the cached copy in {} has been changed by hand, so it is not what was approved. Delete it and try again.",
            dir.display()
        ));
    }
    Ok(())
}

/// What a person has trusted: pinned helper teams, one `<url>#<sha>` per line.
fn trust_file() -> Result<PathBuf, String> {
    Ok(state::root()?.join("trusted-orgs"))
}

/// Has the person trusted this exact commit of this repository?
pub fn is_trusted(url: &str, sha: &str) -> Result<bool, String> {
    let file = trust_file()?;
    let text = std::fs::read_to_string(&file).unwrap_or_default();
    Ok(text.lines().any(|l| l.trim() == pinned(url, sha)))
}

/// Record that the person trusts this commit.
pub fn trust(url: &str, sha: &str) -> Result<(), String> {
    if is_trusted(url, sha)? {
        return Ok(());
    }
    let file = trust_file()?;
    if let Some(dir) = file.parent() {
        std::fs::create_dir_all(dir)
            .map_err(|e| format!("cannot create {}: {e}", dir.display()))?;
    }
    let mut text = std::fs::read_to_string(&file).unwrap_or_default();
    if !text.is_empty() && !text.ends_with('\n') {
        text.push('\n');
    }
    text.push_str(&pinned(url, sha));
    text.push('\n');
    std::fs::write(&file, text).map_err(|e| format!("cannot write {}: {e}", file.display()))
}

/// A helper team ready to use.
#[derive(Debug, Clone)]
pub struct Resolved {
    /// The directory with `config.yaml`.
    pub dir: PathBuf,
    /// Where its usage counters live when it is remote, so that an update of
    /// the pin does not reset the budgets.
    pub usage: Option<PathBuf>,
    /// For a remote one, its URL and commit.
    pub remote: Option<(String, String)>,
}

/// The directory to run `source` from. A remote one must be pinned, fetched and
/// trusted; none of that is asked of the user here, because it cannot be: the
/// caller may be an MCP server whose stdio belongs to the protocol.
pub fn use_source(source: &Source) -> Result<Resolved, String> {
    match source {
        Source::Local(dir) => Ok(Resolved {
            dir: dir.clone(),
            usage: None,
            remote: None,
        }),
        Source::Remote { url, sha: None, .. } => Err(format!(
            "{url} is not pinned to a commit, and Ensemble never follows a branch. `ensemble team pull {url}` pins it to the commit its default branch is at."
        )),
        Source::Remote {
            url,
            sha: Some(sha),
            ..
        } => {
            if !is_trusted(url, sha)? {
                return Err(format!(
                    "{url} at {sha} is not trusted yet, and its config decides what runs on this machine. Read what it would run and trust it with: ensemble team pull {}",
                    pinned(url, sha)
                ));
            }
            let dir = fetch(url, sha)?;
            Ok(Resolved {
                dir,
                usage: Some(state::root()?.join("orgs").join(state::labelled_text(url))),
                remote: Some((url.clone(), sha.clone())),
            })
        }
    }
}

/// What changed in `config.yaml` between two commits of the checkout in `dir`.
pub fn diff(dir: &Path, old: &str, new: &str) -> Result<String, String> {
    git(
        Some(dir),
        &["diff", "--no-color", old, new, "--", "config.yaml"],
    )
}

/// What the config would run, in plain text, with a warning for each risk.
pub fn describe(config: &Config) -> String {
    let mut out = String::new();
    let mut risks = Vec::new();
    out.push_str("Agents (each is started on your machine, with your permissions):\n");
    for a in config.agents.values() {
        match &a.adapter {
            Adapter::ClaudeCode(c) => {
                out.push_str(&format!(
                    "  {}: runs `{}` (Claude Code), tools [{}], permission mode {}\n",
                    a.name,
                    c.command,
                    c.tools.join(", "),
                    c.permission_mode
                ));
                if matches!(
                    c.permission_mode.as_str(),
                    "bypassPermissions" | "acceptEdits" | "auto"
                ) {
                    risks.push(format!(
                        "{} uses permission mode '{}', which lets it act without asking",
                        a.name, c.permission_mode
                    ));
                }
                if c.tools.iter().any(|t| crate::config::claude_tool_writes(t)) {
                    risks.push(format!(
                        "{} can change files{}",
                        a.name,
                        if c.tools.iter().any(|t| t.starts_with("Bash")) {
                            " and run shell commands"
                        } else {
                            ""
                        }
                    ));
                }
            }
            Adapter::OpenCode(o) => {
                out.push_str(&format!(
                    "  {}: runs `{}` (OpenCode), tools [{}]\n",
                    a.name,
                    o.command,
                    o.tools.join(", ")
                ));
                if o.tools
                    .iter()
                    .any(|t| crate::config::opencode_tool_writes(t))
                {
                    risks.push(format!(
                        "{} can change files{}",
                        a.name,
                        if o.tools.iter().any(|t| t.starts_with("bash")) {
                            " and run shell commands"
                        } else {
                            ""
                        }
                    ));
                }
            }
            Adapter::Codex(c) => {
                out.push_str(&format!(
                    "  {}: runs `{}` (Codex), sandbox {}\n",
                    a.name, c.command, c.sandbox
                ));
                if c.sandbox == "danger-full-access" {
                    risks.push(format!(
                        "{} runs with sandbox 'danger-full-access': nothing limits what it does on your machine",
                        a.name
                    ));
                } else if crate::config::codex_sandbox_writes(&c.sandbox) {
                    risks.push(format!(
                        "{} can change files in your working folder",
                        a.name
                    ));
                }
            }
            Adapter::Direct(l) => out.push_str(&format!(
                "  {}: one chat request to {}, no tools\n",
                a.name,
                l.fallback
                    .candidates
                    .iter()
                    .map(|c| format!("backend '{}'", c.backend))
                    .collect::<Vec<_>>()
                    .join(", else ")
            )),
            Adapter::Builtin(b) => {
                out.push_str(&format!(
                    "  {}: Ensemble's own agent loop on {}, tools [{}]\n",
                    a.name,
                    b.fallback
                        .candidates
                        .iter()
                        .map(|c| format!("backend '{}'", c.backend))
                        .collect::<Vec<_>>()
                        .join(", else "),
                    b.tools.join(", ")
                ));
                if b.tools.iter().any(|t| t == "shell") {
                    risks.push(format!(
                        "{} can run any command in your working folder",
                        a.name
                    ));
                } else if b
                    .tools
                    .iter()
                    .any(|t| crate::config::builtin_tool_writes(t))
                {
                    risks.push(format!(
                        "{} can change files in your working folder",
                        a.name
                    ));
                }
            }
            Adapter::Node(n) => {
                out.push_str(&format!(
                    "  {}: sends its tasks, with the files git tracks in your working folder, to `{}` on {} through the hub {}, and writes back what it changed\n",
                    a.name,
                    n.agent,
                    n.nodes.join(" or "),
                    n.hub
                ));
                risks.push(format!(
                    "{} sends your working folder's files to {}, whose owner can read them",
                    a.name,
                    n.nodes.join(" and ")
                ));
            }
        }
        if a.can_change_files() && a.isolation == Isolation::Shared {
            risks.push(format!(
                "{} changes files directly in your working folder (isolation: shared)",
                a.name
            ));
        }
    }
    if config.agents.is_empty() {
        out.push_str("  (none)\n");
    }
    out.push_str("\nGates (shell commands, run with `sh -c` in your working folder):\n");
    for (name, g) in &config.gates {
        out.push_str(&format!("  {name}: {}\n", g.command));
    }
    if config.gates.is_empty() {
        out.push_str("  (none)\n");
    }
    out.push_str("\nModel servers it sends your prompts to:\n");
    for (name, b) in &config.llm_backends {
        out.push_str(&format!("  {name}: {}\n", b.settings.base_url()));
    }
    if config.llm_backends.is_empty() {
        out.push_str("  (none)\n");
    }
    if !risks.is_empty() {
        out.push_str("\nWarnings:\n");
        for r in risks {
            out.push_str(&format!("  ! {r}\n"));
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::workspace::testing::TempDir;

    #[test]
    fn a_source_is_a_path_or_a_url_with_a_ref_or_a_pin() {
        assert_eq!(
            parse_source("/home/me/org").unwrap(),
            Source::Local("/home/me/org".into())
        );
        assert_eq!(
            parse_source("~/org").unwrap(),
            Source::Local("~/org".into())
        );
        assert_eq!(
            parse_source("github.com/acme/org@v1.2").unwrap(),
            Source::Remote {
                url: "https://github.com/acme/org".into(),
                sha: None,
                git_ref: Some("v1.2".into())
            }
        );
        assert_eq!(
            parse_source("git@github.com:acme/org").unwrap(),
            Source::Remote {
                url: "git@github.com:acme/org".into(),
                sha: None,
                git_ref: None
            }
        );
        assert_eq!(
            parse_source("git@github.com:acme/org@main").unwrap(),
            Source::Remote {
                url: "git@github.com:acme/org".into(),
                sha: None,
                git_ref: Some("main".into())
            }
        );
        let sha = "a".repeat(40);
        assert_eq!(
            parse_source(&format!("ssh://git@host/acme/org.git#{sha}")).unwrap(),
            Source::Remote {
                url: "ssh://git@host/acme/org.git".into(),
                sha: Some(sha.clone()),
                git_ref: None
            }
        );
        assert!(parse_source(&format!("github.com/a/b@v1#{sha}")).is_err());
        assert!(parse_source("github.com/a/b#nothex").is_err());
        assert!(parse_source("github.com/a/b@").is_err());
        assert!(parse_source("-oops").is_err());
        assert!(parse_source("ext::sh -c evil").is_err());
        assert!(parse_source("ftp://host/x/y").is_err());
        assert!(parse_source("nonsense").is_err());
    }

    #[test]
    fn describing_a_team_lists_what_runs_and_warns_about_the_risky() {
        let doc = "version: 1\nllm_backends: {}\nlimits: {}\ngates:\n  test: { command: \"rm -rf build && make\", timeout_seconds: 5 }\nagents:\n  doer:\n    max_seconds: 60\n    max_concurrent: 1\n    isolation: shared\n    budget: { type: unlimited }\n    adapter: { type: claude-code, model: sonnet, tools: [Read, \"Bash(*)\"], permission_mode: bypassPermissions, max_turns: 3, command: claude }\n";
        let config =
            crate::config::parse(doc, Path::new("c.yaml"), &std::collections::BTreeMap::new())
                .unwrap();
        let text = describe(&config);
        assert!(text.contains("doer: runs `claude`"), "{text}");
        assert!(text.contains("test: rm -rf build && make"), "{text}");
        assert!(text.contains("bypassPermissions"), "{text}");
        assert!(
            text.contains("can change files and run shell commands"),
            "{text}"
        );
        assert!(text.contains("directly in your working folder"), "{text}");
    }

    /// A repository with `config.yaml` containing `text`, tagged `tag`.
    fn repo(dir: &Path, text: &str, tag: &str) -> String {
        let run = |args: &[&str]| {
            let out = Command::new("git")
                .args(["-c", "user.name=t", "-c", "user.email=t@t"])
                .args(args)
                .current_dir(dir)
                .output()
                .unwrap();
            assert!(
                out.status.success(),
                "{args:?}: {}",
                String::from_utf8_lossy(&out.stderr)
            );
            String::from_utf8_lossy(&out.stdout).trim().to_string()
        };
        if !dir.join(".git").exists() {
            run(&["init", "--quiet", "-b", "main"]);
        }
        std::fs::write(dir.join("config.yaml"), text).unwrap();
        run(&["add", "."]);
        run(&["commit", "--quiet", "-m", "c"]);
        run(&["tag", "-f", tag]);
        run(&["rev-parse", "HEAD"])
    }

    /// Test the git-backed parts with the state and cache in a scratch place.
    /// They set process-wide variables, so they run as one test.
    #[test]
    fn pinning_caching_tampering_and_trust() {
        let tmp = TempDir::new();
        let origin = tmp.0.join("origin");
        std::fs::create_dir_all(&origin).unwrap();
        // SAFETY: this is the only test that sets these variables.
        unsafe {
            std::env::set_var("XDG_CACHE_HOME", tmp.0.join("cache"));
            std::env::set_var("XDG_STATE_HOME", tmp.0.join("state"));
        }
        let url = format!("file://{}", origin.display());

        let first = repo(&origin, "first\n", "v1");
        assert_eq!(resolve(&url, Some("v1")).unwrap(), first);
        assert_eq!(resolve(&url, None).unwrap(), first);
        assert!(
            resolve(&url, Some("nope"))
                .unwrap_err()
                .contains("no tag or branch")
        );
        assert_eq!(resolve(&url, Some(&first)).unwrap(), first);

        // Fetched once, then used from the cache even when the origin is gone.
        let dir = fetch(&url, &first).unwrap();
        assert_eq!(
            std::fs::read_to_string(dir.join("config.yaml")).unwrap(),
            "first\n"
        );
        let second = repo(&origin, "second\n", "v1");
        assert_ne!(first, second);
        // The tag moved; the pin did not.
        assert_eq!(resolve(&url, Some("v1")).unwrap(), second);
        let again = fetch(&url, &first).unwrap();
        assert_eq!(
            std::fs::read_to_string(again.join("config.yaml")).unwrap(),
            "first\n"
        );
        // A commit the origin has, not yet fetched, comes out as it was.
        let newer = fetch(&url, &second).unwrap();
        assert_eq!(
            std::fs::read_to_string(newer.join("config.yaml")).unwrap(),
            "second\n"
        );
        assert!(
            fetch(&url, &"0".repeat(40))
                .unwrap_err()
                .contains("no commit")
        );
        assert!(fetch(&url, "short").is_err());

        // Offline: the origin disappears, the cache still serves.
        let moved = tmp.0.join("gone");
        std::fs::rename(&origin, &moved).unwrap();
        assert!(fetch(&url, &first).is_ok());

        // A copy changed by hand is refused.
        std::fs::write(dir.join("config.yaml"), "evil\n").unwrap();
        assert!(fetch(&url, &first).unwrap_err().contains("changed by hand"));
        std::fs::write(dir.join("config.yaml"), "first\n").unwrap();
        std::fs::write(dir.join("extra"), "x").unwrap();
        assert!(fetch(&url, &first).unwrap_err().contains("changed by hand"));
        std::fs::remove_file(dir.join("extra")).unwrap();
        assert!(fetch(&url, &first).is_ok());

        // Nothing is used before it is trusted, and trust is for that commit.
        let source = |sha: &str| Source::Remote {
            url: url.clone(),
            sha: Some(sha.into()),
            git_ref: None,
        };
        let err = use_source(&source(&first)).unwrap_err();
        assert!(err.contains("ensemble team pull"), "{err}");
        assert!(!is_trusted(&url, &first).unwrap());
        trust(&url, &first).unwrap();
        trust(&url, &first).unwrap();
        assert!(is_trusted(&url, &first).unwrap());
        assert!(!is_trusted(&url, &second).unwrap());
        let used = use_source(&source(&first)).unwrap();
        assert_eq!(used.dir, dir);
        assert!(use_source(&source(&second)).is_err());
        let unpinned = Source::Remote {
            url: url.clone(),
            sha: None,
            git_ref: Some("v1".into()),
        };
        assert!(
            use_source(&unpinned)
                .unwrap_err()
                .contains("never follows a branch")
        );
        let text = std::fs::read_to_string(tmp.0.join("state/ensemble/trusted-orgs")).unwrap();
        assert_eq!(text.lines().count(), 1, "{text}");
    }
}
