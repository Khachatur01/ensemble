//! The vault: a git repository of text, laid out so a person can read it.
//!
//! ```text
//! shares/<yyyy>/<mm>/<id>.md   each share as it arrived; never changed
//! notes/<slug>.md              written only by the pipeline
//! review/<share-id>.md         what the pipeline would not write, and why
//! log.jsonl                    one line per share the pipeline has processed
//! .ensemble/embeddings.db      the notes' vectors, a cache out of the history
//! ```
//!
//! The notes are a product of the shares: a new note format rebuilds them from
//! the shares instead of migrating them. Any session may add a share, writing
//! one file at once; only the worker that holds the vault's lock runs `git`,
//! so two processes never fight over git's index.

use serde::{Deserialize, Serialize};
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

/// The author of every commit in a vault: only Ensemble writes one.
const GIT_NAME: &str = "ensemble";
const GIT_EMAIL: &str = "ensemble@localhost";
/// The folder of what is made again from the notes, out of the history.
const CACHE_DIR: &str = ".ensemble";

/// One vault, by its directory.
#[derive(Debug, Clone)]
pub struct Vault {
    root: PathBuf,
}

/// What someone shared, as it arrived.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Share {
    /// `20261110-153012-123456-ab`: when it arrived, to the microsecond, so ids
    /// sort in the order shares arrived; two random digits keep two processes
    /// apart.
    pub id: String,
    /// `2026-11-10T15:30:12Z`.
    pub at: String,
    /// The person it came from: the account that shared it.
    pub shared_by: String,
    /// The agent CLI that sent it, as it named itself, when it did.
    pub client: Option<String>,
    /// The working folder's name, never its path.
    pub project: Option<String>,
    /// Who may recall what comes of it.
    pub visibility: Visibility,
    pub title: String,
    pub text: String,
}

/// Who may recall what comes of a share, as the person who shared it chose.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Visibility {
    /// Only the person who shared it.
    #[default]
    Private,
    /// Everyone on the hub who may recall.
    Hub,
}

impl Visibility {
    pub const ALL: [&str; 2] = ["private", "hub"];

    pub fn as_str(self) -> &'static str {
        match self {
            Visibility::Private => "private",
            Visibility::Hub => "hub",
        }
    }

    pub fn parse(text: &str) -> Option<Self> {
        match text.trim() {
            "private" => Some(Visibility::Private),
            "hub" => Some(Visibility::Hub),
            _ => None,
        }
    }
}

/// Who may recall a note: the audience of the shares it came from. A note is
/// made only from shares of one audience, so what was shared privately never
/// reaches anyone else through a note.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Audience {
    /// Only this person.
    Only(String),
    /// Everyone on the hub who may recall.
    Hub,
}

impl Audience {
    pub fn of(share: &Share) -> Self {
        match share.visibility {
            Visibility::Private => Audience::Only(share.shared_by.clone()),
            Visibility::Hub => Audience::Hub,
        }
    }

    /// Whether the account `who` may recall it.
    pub fn sees(&self, who: &str) -> bool {
        match self {
            Audience::Only(person) => person == who,
            Audience::Hub => true,
        }
    }
}

#[derive(Debug, Serialize, Deserialize)]
struct ShareHead {
    id: String,
    at: String,
    shared_by: String,
    client: Option<String>,
    project: Option<String>,
    /// Missing in a share kept before there was a choice: private.
    #[serde(default)]
    visibility: Visibility,
    title: String,
}

/// One note, as the pipeline writes it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Note {
    /// Its file name without `.md`, which `[[...]]` links name.
    pub slug: String,
    /// decision, fact, how-to, lesson, commitment, preference or ownership.
    pub kind: String,
    pub title: String,
    pub tags: Vec<String>,
    /// Who may recall it.
    pub audience: Audience,
    /// The shares it was distilled from, oldest first.
    pub sources: Vec<String>,
    /// The people those shares came from.
    pub shared_by: Vec<String>,
    /// The pipeline version that last wrote it.
    pub pipeline: u32,
    pub updated: String,
    pub body: String,
    /// The notes it relates to, by slug: written after the body as a
    /// `## Related` list of `[[links]]`, which Obsidian and the like follow.
    pub related: Vec<String>,
}

#[derive(Debug, Serialize, Deserialize)]
struct NoteHead {
    kind: String,
    title: String,
    tags: Vec<String>,
    /// Missing in a note written before there was a choice: private.
    #[serde(default)]
    visibility: Visibility,
    /// Whose a private note is.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    owner: Option<String>,
    sources: Vec<String>,
    shared_by: Vec<String>,
    pipeline: u32,
    updated: String,
}

/// What the pipeline did with one share.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Processed {
    pub share: String,
    pub pipeline: u32,
    /// `notes`, `skipped` (nothing worth keeping) or `review`.
    pub outcome: String,
    /// The notes it created or changed.
    pub notes: Vec<String>,
    pub at: String,
}

impl Vault {
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Vault { root: root.into() }
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Is it a git repository? `ensemble hub init` makes it one.
    pub fn exists(&self) -> bool {
        self.root.join(".git").exists()
    }

    /// Where the worker's lock lives: inside `.git`, so it is never committed.
    pub fn lock_dir(&self) -> PathBuf {
        self.root.join(".git")
    }

    fn shares_dir(&self) -> PathBuf {
        self.root.join("shares")
    }

    fn notes_dir(&self) -> PathBuf {
        self.root.join("notes")
    }

    fn review_dir(&self) -> PathBuf {
        self.root.join("review")
    }

    fn log_path(&self) -> PathBuf {
        self.root.join("log.jsonl")
    }

    /// Make the directory a git repository, unless it is one.
    /// `.ensemble/`: what is made again from the notes, such as their vectors,
    /// made when first asked for and kept out of the vault's history by its
    /// `.gitignore`.
    pub fn cache_dir(&self) -> Result<PathBuf, String> {
        let dir = self.root.join(CACHE_DIR);
        fs::create_dir_all(&dir).map_err(|e| format!("cannot create {}: {e}", dir.display()))?;
        let ignore = self.root.join(".gitignore");
        let ignored = format!("/{CACHE_DIR}/");
        let text = fs::read_to_string(&ignore).unwrap_or_default();
        if !text.lines().any(|l| l.trim() == ignored) {
            let sep = if text.is_empty() || text.ends_with('\n') {
                ""
            } else {
                "\n"
            };
            write_atomically(&ignore, &format!("{text}{sep}{ignored}\n"))
                .map_err(|e| format!("cannot write {}: {e}", ignore.display()))?;
        }
        Ok(dir)
    }

    pub fn init(&self) -> Result<(), String> {
        fs::create_dir_all(&self.root)
            .map_err(|e| format!("cannot create {}: {e}", self.root.display()))?;
        if self.exists() {
            return Ok(());
        }
        self.git(&["init", "-q"])
    }

    /// Keep a share, as it is, and give it an id. It is written in one step, so
    /// the worker never reads half of it.
    pub fn add_share(
        &self,
        shared_by: &str,
        client: Option<&str>,
        project: Option<&str>,
        visibility: Visibility,
        title: &str,
        text: &str,
    ) -> Result<Share, String> {
        let micros = next_micros();
        let now = std::time::UNIX_EPOCH + std::time::Duration::from_micros(micros);
        let at = ensemble_core::time::utc(now);
        // 2026-11-10T15:30:12Z -> 20261110-153012
        let stamp: String = at
            .chars()
            .filter(|c| c.is_ascii_digit() || *c == 'T')
            .collect::<String>()
            .replace('T', "-");
        let share = Share {
            id: format!("{stamp}-{:06}-{}", micros % 1_000_000, random_hex(2)),
            at,
            shared_by: shared_by.to_string(),
            client: client.map(str::to_string),
            project: project.map(str::to_string),
            visibility,
            title: title.trim().to_string(),
            text: text.trim().to_string(),
        };
        let path = self.share_path(&share.id);
        if let Some(dir) = path.parent() {
            fs::create_dir_all(dir).map_err(|e| format!("cannot create {}: {e}", dir.display()))?;
        }
        let head = ShareHead {
            id: share.id.clone(),
            at: share.at.clone(),
            shared_by: share.shared_by.clone(),
            client: share.client.clone(),
            project: share.project.clone(),
            visibility: share.visibility,
            title: share.title.clone(),
        };
        write_atomically(&path, &with_front_matter(&head, &share.text))
            .map_err(|e| format!("cannot write {}: {e}", path.display()))?;
        Ok(share)
    }

    /// `shares/2026/11/<id>.md`.
    fn share_path(&self, id: &str) -> PathBuf {
        let (year, month) = (id.get(0..4).unwrap_or("0000"), id.get(4..6).unwrap_or("00"));
        self.shares_dir()
            .join(year)
            .join(month)
            .join(format!("{id}.md"))
    }

    /// Every share, oldest first. A file that cannot be read as one is left out.
    pub fn shares(&self) -> Vec<Share> {
        let mut out = Vec::new();
        for year in entries(&self.shares_dir()) {
            for month in entries(&year) {
                for file in entries(&month) {
                    if file.extension().is_some_and(|e| e == "md")
                        && let Some(share) = read_share(&file)
                    {
                        out.push(share);
                    }
                }
            }
        }
        out.sort_by(|a, b| a.id.cmp(&b.id));
        out
    }

    /// Every note, by slug.
    pub fn notes(&self) -> Vec<Note> {
        let mut out: Vec<Note> = entries(&self.notes_dir())
            .into_iter()
            .filter(|p| p.extension().is_some_and(|e| e == "md"))
            .filter_map(|p| read_note(&p))
            .collect();
        out.sort_by(|a, b| a.slug.cmp(&b.slug));
        out
    }

    pub fn note(&self, slug: &str) -> Option<Note> {
        read_note(&self.notes_dir().join(format!("{slug}.md")))
    }

    pub fn write_note(&self, note: &Note) -> Result<(), String> {
        let dir = self.notes_dir();
        fs::create_dir_all(&dir).map_err(|e| format!("cannot create {}: {e}", dir.display()))?;
        let head = NoteHead {
            kind: note.kind.clone(),
            title: note.title.clone(),
            tags: note.tags.clone(),
            visibility: match note.audience {
                Audience::Only(_) => Visibility::Private,
                Audience::Hub => Visibility::Hub,
            },
            owner: match &note.audience {
                Audience::Only(person) => Some(person.clone()),
                Audience::Hub => None,
            },
            sources: note.sources.clone(),
            shared_by: note.shared_by.clone(),
            pipeline: note.pipeline,
            updated: note.updated.clone(),
        };
        let path = dir.join(format!("{}.md", note.slug));
        let text = format!("{}{}", note.body.trim_end(), related_section(&note.related));
        write_atomically(&path, &with_front_matter(&head, &text))
            .map_err(|e| format!("cannot write {}: {e}", path.display()))
    }

    /// Set a share, or one item of it, aside for a person to read, with why.
    pub fn write_review(&self, share: &Share, why: &str, detail: &str) -> Result<(), String> {
        let dir = self.review_dir();
        fs::create_dir_all(&dir).map_err(|e| format!("cannot create {}: {e}", dir.display()))?;
        let path = dir.join(format!("{}.md", share.id));
        let mut text = fs::read_to_string(&path).unwrap_or_else(|_| {
            format!(
                "# {}\n\nShare: shares/{}/{}/{}.md\n",
                share.title,
                &share.id[0..4.min(share.id.len())],
                share.id.get(4..6).unwrap_or("00"),
                share.id
            )
        });
        text.push_str(&format!("\n## {why}\n\n{}\n", detail.trim()));
        write_atomically(&path, &text).map_err(|e| format!("cannot write {}: {e}", path.display()))
    }

    /// How many items wait in `review/`.
    pub fn review_count(&self) -> usize {
        entries(&self.review_dir())
            .iter()
            .filter(|p| p.extension().is_some_and(|e| e == "md"))
            .count()
    }

    /// What the pipeline has done, in order.
    pub fn log(&self) -> Vec<Processed> {
        fs::read_to_string(self.log_path())
            .unwrap_or_default()
            .lines()
            .filter_map(|l| serde_json::from_str(l).ok())
            .collect()
    }

    pub fn append_log(&self, line: &Processed) -> Result<(), String> {
        use std::io::Write;
        let path = self.log_path();
        let mut file = fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)
            .map_err(|e| format!("cannot open {}: {e}", path.display()))?;
        let json = serde_json::to_string(line).map_err(|e| e.to_string())?;
        writeln!(file, "{json}").map_err(|e| format!("cannot write {}: {e}", path.display()))
    }

    /// Remove every note, review item and log line: what a rebuild starts from.
    pub fn clear_products(&self) -> Result<(), String> {
        for dir in [self.notes_dir(), self.review_dir()] {
            if dir.exists() {
                fs::remove_dir_all(&dir)
                    .map_err(|e| format!("cannot remove {}: {e}", dir.display()))?;
            }
        }
        match fs::remove_file(self.log_path()) {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(format!("cannot remove the log: {e}")),
        }
    }

    /// Commit everything in the vault, when anything changed.
    pub fn commit(&self, message: &str) -> Result<(), String> {
        self.git(&["add", "-A"])?;
        let staged = std::process::Command::new("git")
            .args(["diff", "--cached", "--quiet"])
            .current_dir(&self.root)
            .status()
            .map_err(|e| format!("cannot run git: {e}"))?;
        if staged.success() {
            return Ok(());
        }
        self.git(&[
            "-c",
            &format!("user.name={GIT_NAME}"),
            "-c",
            &format!("user.email={GIT_EMAIL}"),
            "commit",
            "-q",
            "--no-verify",
            "-m",
            message,
        ])
    }

    fn git(&self, args: &[&str]) -> Result<(), String> {
        let out = std::process::Command::new("git")
            .args(args)
            .current_dir(&self.root)
            .output()
            .map_err(|e| format!("cannot run git: {e}"))?;
        if out.status.success() {
            Ok(())
        } else {
            Err(format!(
                "git {} in {} failed: {}",
                args.first().unwrap_or(&""),
                self.root.display(),
                String::from_utf8_lossy(&out.stderr).trim()
            ))
        }
    }
}

/// `---`, the head as YAML, `---`, then the text.
fn with_front_matter<T: Serialize>(head: &T, text: &str) -> String {
    let yaml = serde_norway::to_string(head).expect("a head serializes");
    format!("---\n{}---\n\n{}\n", yaml, text.trim_end())
}

/// The head and the text of a file written by [`with_front_matter`].
fn split_front_matter(file: &str) -> Option<(&str, &str)> {
    let rest = file.strip_prefix("---\n")?;
    let end = rest.find("\n---\n")?;
    Some((&rest[..end + 1], rest[end + 5..].trim_start_matches('\n')))
}

fn read_share(path: &Path) -> Option<Share> {
    let file = fs::read_to_string(path).ok()?;
    let (head, text) = split_front_matter(&file)?;
    let head: ShareHead = serde_norway::from_str(head).ok()?;
    Some(Share {
        id: head.id,
        at: head.at,
        shared_by: head.shared_by,
        client: head.client,
        project: head.project,
        visibility: head.visibility,
        title: head.title,
        text: text.trim_end().to_string(),
    })
}

fn read_note(path: &Path) -> Option<Note> {
    let file = fs::read_to_string(path).ok()?;
    let (head, body) = split_front_matter(&file)?;
    let head: NoteHead = serde_norway::from_str(head).ok()?;
    let (body, related) = split_related(body.trim_end());
    Some(Note {
        slug: path.file_stem()?.to_string_lossy().into_owned(),
        kind: head.kind,
        title: head.title,
        tags: head.tags,
        audience: match head.visibility {
            Visibility::Hub => Audience::Hub,
            // Whose: as written, else whoever shared it first.
            Visibility::Private => Audience::Only(
                head.owner
                    .or_else(|| head.shared_by.first().cloned())
                    .unwrap_or_default(),
            ),
        },
        sources: head.sources,
        shared_by: head.shared_by,
        pipeline: head.pipeline,
        updated: head.updated,
        body: body.to_string(),
        related,
    })
}

const RELATED: &str = "## Related";

/// The `## Related` list after a note's body; nothing when it relates to none.
fn related_section(related: &[String]) -> String {
    if related.is_empty() {
        return String::new();
    }
    let links: Vec<String> = related.iter().map(|s| format!("- [[{s}]]")).collect();
    format!("\n\n{RELATED}\n\n{}", links.join("\n"))
}

/// A note's text as its body and the slugs of its `## Related` list: the last
/// heading of the text, when every line under it is a `- [[link]]`.
fn split_related(text: &str) -> (&str, Vec<String>) {
    let start = if text.starts_with(&format!("{RELATED}\n")) {
        0
    } else {
        match text.rfind(&format!("\n{RELATED}\n")) {
            Some(i) => i + 1,
            None => return (text, Vec::new()),
        }
    };
    let mut related: Vec<String> = Vec::new();
    for line in text[start + RELATED.len()..].lines().map(str::trim) {
        if line.is_empty() {
            continue;
        }
        let slug = line
            .strip_prefix("- [[")
            .and_then(|l| l.strip_suffix("]]"))
            // `[[slug|shown as]]` links to slug.
            .and_then(|l| l.split('|').next())
            .map(str::trim);
        match slug {
            Some(slug) if !slug.is_empty() => {
                if !related.iter().any(|r| r == slug) {
                    related.push(slug.to_string());
                }
            }
            _ => return (text, Vec::new()),
        }
    }
    (text[..start].trim_end(), related)
}

/// The entries of `dir`, sorted; none when it does not exist.
fn entries(dir: &Path) -> Vec<PathBuf> {
    let mut out: Vec<PathBuf> = fs::read_dir(dir)
        .map(|rd| rd.filter_map(Result::ok).map(|e| e.path()).collect())
        .unwrap_or_default();
    out.sort();
    out
}

/// Write through a temporary file in the same directory, so a reader never
/// sees half of it. The temporary name is not `.md`, so no listing picks it up.
fn write_atomically(path: &Path, text: &str) -> io::Result<()> {
    let tmp = path.with_extension(format!("tmp-{}", std::process::id()));
    fs::write(&tmp, text)?;
    fs::rename(&tmp, path)
}

/// Microseconds since 1970, never the same twice in this process, so the shares
/// one process adds sort in the order it added them.
fn next_micros() -> u64 {
    use std::sync::atomic::{AtomicU64, Ordering};
    static LAST: AtomicU64 = AtomicU64::new(0);
    let now = SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_micros() as u64);
    let mut last = LAST.load(Ordering::Relaxed);
    loop {
        let next = now.max(last + 1);
        match LAST.compare_exchange(last, next, Ordering::Relaxed, Ordering::Relaxed) {
            Ok(_) => return next,
            Err(seen) => last = seen,
        }
    }
}

/// `n` hex digits nobody can guess: from std's randomly keyed hasher, the
/// time and the process.
pub(crate) fn random_hex(n: usize) -> String {
    use std::hash::{BuildHasher, Hasher};
    let mut h = std::collections::hash_map::RandomState::new().build_hasher();
    h.write_u128(
        SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| d.as_nanos()),
    );
    h.write_u32(std::process::id());
    let hex = format!("{:016x}", h.finish());
    hex[..n.min(16)].to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use ensemble_core::workspace::testing::TempDir;

    #[test]
    fn a_share_is_kept_as_it_arrived_and_read_back_in_order() {
        let tmp = TempDir::new();
        let vault = Vault::new(tmp.0.join("vault"));
        vault.init().unwrap();
        assert!(vault.exists());
        let first = vault
            .add_share(
                "Ana",
                Some("claude-code"),
                Some("shop"),
                Visibility::Hub,
                " Ports ",
                "We use 8080.\n",
            )
            .unwrap();
        let second = vault
            .add_share(
                "Ana",
                None,
                None,
                Visibility::Private,
                "Second: with \"quotes\"",
                "---\nnot a head\n",
            )
            .unwrap();
        assert!(first.id < second.id, "ids sort in the order shares arrived");
        let year = &first.id[0..4];
        assert!(vault.root().join("shares").join(year).exists());
        let shares = vault.shares();
        assert_eq!(shares.len(), 2);
        let back = shares.iter().find(|s| s.id == first.id).unwrap();
        assert_eq!(back, &first);
        assert_eq!(back.title, "Ports");
        let back = shares.iter().find(|s| s.id == second.id).unwrap();
        assert_eq!(back.text, "---\nnot a head");
        assert_eq!(back.title, "Second: with \"quotes\"");
        assert_eq!(back.visibility, Visibility::Private);
        // A share kept before there was a choice is private.
        let path = vault.share_path(&second.id);
        let file = fs::read_to_string(&path)
            .unwrap()
            .replace("visibility: private\n", "");
        fs::write(&path, file).unwrap();
        let back = vault
            .shares()
            .into_iter()
            .find(|s| s.id == second.id)
            .unwrap();
        assert_eq!(
            (back.visibility, Audience::of(&back)),
            (Visibility::Private, Audience::Only("Ana".into()))
        );
    }

    #[test]
    fn a_related_list_is_the_last_heading_of_links_only() {
        let (body, related) = split_related("Text.\n\n## Related\n\n- [[a]]\n- [[b|B]]\n- [[a]]");
        assert_eq!((body, related), ("Text.", vec!["a".into(), "b".into()]));
        let (body, related) = split_related("## Related\n- [[a]]");
        assert_eq!((body, related), ("", vec!["a".to_string()]));
        for text in [
            "Text.\n\n## Related\n\nSee [[a]].",
            "## Related\n\n- [[a]]\n\n## Later\n\nMore.",
            "Text.\n## Related work\n- [[a]]",
            "Text.",
        ] {
            assert_eq!(split_related(text), (text, Vec::new()), "{text}");
        }
    }

    #[test]
    fn notes_reviews_and_the_log_are_written_and_committed() {
        let tmp = TempDir::new();
        let vault = Vault::new(tmp.0.join("vault"));
        vault.init().unwrap();
        let share = vault
            .add_share("Ana", None, None, Visibility::Private, "t", "x")
            .unwrap();
        let note = Note {
            slug: "ports".into(),
            kind: "fact".into(),
            title: "Ports".into(),
            tags: vec!["net".into()],
            audience: Audience::Only("Ana".into()),
            sources: vec![share.id.clone()],
            shared_by: vec!["Ana".into()],
            pipeline: 1,
            updated: "2026-11-10T00:00:00Z".into(),
            body: "The shop listens on 8080.".into(),
            related: vec!["deploys".into()],
        };
        vault.write_note(&note).unwrap();
        let file = fs::read_to_string(vault.notes_dir().join("ports.md")).unwrap();
        assert!(
            file.ends_with("on 8080.\n\n## Related\n\n- [[deploys]]\n"),
            "{file}"
        );
        assert!(file.contains("visibility: private\nowner: Ana\n"), "{file}");
        assert_eq!(vault.note("ports"), Some(note.clone()));
        assert_eq!(vault.notes(), vec![note.clone()]);
        // A note for the hub names no owner.
        let hub = Note {
            slug: "hub-ports".into(),
            audience: Audience::Hub,
            ..note.clone()
        };
        vault.write_note(&hub).unwrap();
        let file = fs::read_to_string(vault.notes_dir().join("hub-ports.md")).unwrap();
        assert!(
            file.contains("visibility: hub\n") && !file.contains("owner:"),
            "{file}"
        );
        assert_eq!(vault.note("hub-ports"), Some(hub));
        // One written before there was a choice is its first sharer's.
        let old = fs::read_to_string(vault.notes_dir().join("ports.md"))
            .unwrap()
            .replace("visibility: private\nowner: Ana\n", "scope: personal\n");
        fs::write(vault.notes_dir().join("ports.md"), old).unwrap();
        assert_eq!(
            vault.note("ports").unwrap().audience,
            Audience::Only("Ana".into())
        );
        fs::remove_file(vault.notes_dir().join("hub-ports.md")).unwrap();
        vault.write_review(&share, "contradicts", "a").unwrap();
        vault.write_review(&share, "unreadable", "b").unwrap();
        assert_eq!(vault.review_count(), 1, "one file per share");
        let line = Processed {
            share: share.id.clone(),
            pipeline: 1,
            outcome: "notes".into(),
            notes: vec!["ports".into()],
            at: share.at.clone(),
        };
        vault.append_log(&line).unwrap();
        assert_eq!(vault.log(), vec![line]);
        vault.commit("first").unwrap();
        vault.commit("nothing changed: no commit").unwrap();
        let log = std::process::Command::new("git")
            .args(["log", "--format=%an %s"])
            .current_dir(vault.root())
            .output()
            .unwrap();
        assert_eq!(String::from_utf8_lossy(&log.stdout), "ensemble first\n");

        vault.clear_products().unwrap();
        assert!(vault.notes().is_empty() && vault.log().is_empty());
        assert_eq!(vault.review_count(), 0);
        assert_eq!(vault.shares().len(), 1, "shares are never cleared");
    }
}
