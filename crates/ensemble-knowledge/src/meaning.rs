//! Finding notes by what they mean, with `knowledge.recall.embed`. Each note's
//! title and body is a vector from the embedding model, kept in a Turso file in
//! the vault, `.ensemble/embeddings.db`, out of its history: a cache, made
//! again whenever a note or the model changes, as the notes are of the shares.
//! The file compares a query's vector with them (`vector_distance_cos`),
//! and the notes at least `min_similarity` alike are merged with the notes the
//! query's words find ([`search`](crate::search)), so a note found both ways
//! comes first. Without a model, or when it cannot be asked, notes are found by
//! their words alone.

use crate::Memory;
use crate::search;
use crate::vault::Note;
use ensemble_backends::embed::embed;
use ensemble_store::{self as store, Value};
use std::collections::HashMap;
use std::sync::Mutex;

/// The most notes embedded in one request.
const BATCH: usize = 32;
/// The most of a note's text embedded, in characters: embedding models read
/// little.
const MAX_TEXT: usize = 4000;
/// The cache, in the vault's [`cache_dir`](crate::Vault::cache_dir).
const CACHE_FILE: &str = "embeddings.db";
const SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS embeddings (
    slug TEXT PRIMARY KEY,
    model TEXT NOT NULL,
    hash INTEGER NOT NULL,
    dims INTEGER NOT NULL,
    vector BLOB NOT NULL
);
";
/// Reciprocal rank fusion's usual constant: how little a lower rank counts.
const FUSION_K: f32 = 60.0;

/// What a [`Memory`] keeps to find notes by their meaning.
#[derive(Debug, Default)]
pub(crate) struct Meaning {
    /// The last failure said, so it is said once, not on every recall.
    said: Mutex<Option<String>>,
}

impl Memory {
    /// The notes about `query`, best first, at most `limit`: by their words,
    /// and by what they mean when the hub has an embedding model.
    pub(crate) fn find<'n>(&self, notes: &'n [Note], query: &str, limit: usize) -> Vec<&'n Note> {
        let by_words = search::rank(notes, query, notes.len());
        let Some(_) = &self.knowledge().recall.embed else {
            return by_words.into_iter().take(limit).collect();
        };
        match self.by_meaning(notes, query) {
            Ok(by_meaning) => fuse(&[&by_words, &by_meaning], limit),
            Err(e) => {
                self.say_once(&e);
                by_words.into_iter().take(limit).collect()
            }
        }
    }

    /// Embed every note that has no vector for its text yet, so that a recall
    /// need not: what the hub's worker does after it writes notes. How many
    /// it embedded.
    pub fn embed_notes(&self) -> Result<usize, String> {
        if self.knowledge().recall.embed.is_none() {
            return Ok(0);
        }
        self.fill(&self.vault().notes())
            .inspect_err(|e| self.say_once(e))
    }

    /// Those of `notes` at least `min_similarity` alike to `query`, most
    /// alike first.
    fn by_meaning<'n>(&self, notes: &'n [Note], query: &str) -> Result<Vec<&'n Note>, String> {
        let e = self
            .knowledge()
            .recall
            .embed
            .as_ref()
            .ok_or("no embedding model")?;
        self.fill(notes)?;
        let asked = self
            .vectors(&[query.trim().to_string()])?
            .pop()
            .ok_or("the model gave no vector for the query")?;
        // A vector of zeros points nowhere: alike to nothing.
        if asked.iter().all(|x| *x == 0.0) {
            return Ok(Vec::new());
        }
        let rows = self.cache(async |c| {
            store::rows(
                c,
                "SELECT slug, 1 - vector_distance_cos(vector, vector32(?1)) FROM embeddings \
                 WHERE model = ?2 AND dims = ?3",
                vec![
                    Value::Text(vector_text(&asked)),
                    Value::Text(e.model.clone()),
                    Value::Integer(asked.len() as i64),
                ],
            )
            .await
        })?;
        let by_slug: HashMap<&str, &Note> = notes.iter().map(|n| (n.slug.as_str(), n)).collect();
        let mut alike: Vec<(f64, &Note)> = rows
            .iter()
            .filter_map(|row| {
                let note = by_slug.get(store::text(&row[0])?.as_str())?;
                let similarity = match row[1] {
                    Value::Real(s) => s,
                    _ => return None,
                };
                (similarity >= f64::from(e.min_similarity())).then_some((similarity, *note))
            })
            .collect();
        alike.sort_by(|a, b| b.0.total_cmp(&a.0).then(a.1.slug.cmp(&b.1.slug)));
        Ok(alike.into_iter().map(|(_, n)| n).collect())
    }

    /// Give every note in `notes` a vector of its text, by the hub's model,
    /// and forget the vectors of notes gone and of other models. How many it
    /// embedded.
    fn fill(&self, notes: &[Note]) -> Result<usize, String> {
        let model = match &self.knowledge().recall.embed {
            Some(e) => e.model.clone(),
            None => return Ok(0),
        };
        let kept = self.cache(async |c| {
            store::rows(c, "SELECT slug, model, hash FROM embeddings", vec![]).await
        })?;
        let mut have: HashMap<String, i64> = HashMap::new();
        let mut stale: Vec<String> = Vec::new();
        for row in &kept {
            let (Some(slug), Some(of)) = (store::text(&row[0]), store::text(&row[1])) else {
                continue;
            };
            match row[2] {
                Value::Integer(hash) if of == model && notes.iter().any(|n| n.slug == slug) => {
                    have.insert(slug, hash);
                }
                _ => stale.push(slug),
            }
        }
        if !stale.is_empty() {
            self.cache(async |c| {
                let deletes = stale
                    .iter()
                    .map(|slug| {
                        (
                            "DELETE FROM embeddings WHERE slug = ?1",
                            vec![Value::Text(slug.clone())],
                        )
                    })
                    .collect();
                store::transaction(c, deletes).await
            })?;
        }
        let missing: Vec<(&Note, i64)> = notes
            .iter()
            .map(|n| (n, hash(&text_of(n)) as i64))
            .filter(|(n, h)| have.get(&n.slug) != Some(h))
            .collect();
        let mut made = 0;
        for batch in missing.chunks(BATCH) {
            let texts: Vec<String> = batch.iter().map(|(n, _)| text_of(n)).collect();
            let vectors = self.vectors(&texts)?;
            self.cache(async |c| {
                let inserts = batch
                    .iter()
                    .zip(&vectors)
                    .map(|((note, hash), vector)| {
                        (
                            "INSERT OR REPLACE INTO embeddings (slug, model, hash, dims, vector) \
                             VALUES (?1, ?2, ?3, ?4, vector32(?5))",
                            vec![
                                Value::Text(note.slug.clone()),
                                Value::Text(model.clone()),
                                Value::Integer(*hash),
                                Value::Integer(vector.len() as i64),
                                Value::Text(vector_text(vector)),
                            ],
                        )
                    })
                    .collect();
                store::transaction(c, inserts).await
            })?;
            made += batch.len();
        }
        Ok(made)
    }

    /// The vectors of `texts`, from the hub's embedding model.
    fn vectors(&self, texts: &[String]) -> Result<Vec<Vec<f32>>, String> {
        let e = self
            .knowledge()
            .recall
            .embed
            .as_ref()
            .ok_or("no embedding model")?;
        let backend = self
            .backends()
            .get(&e.backend)
            .ok_or_else(|| format!("no backend '{}'", e.backend))?;
        embed(backend, &e.model, texts).map_err(|e| e.to_string())
    }

    /// Run `work` on the vault's cache of vectors.
    fn cache<T>(
        &self,
        work: impl AsyncFnOnce(&store::Connection) -> Result<T, String>,
    ) -> Result<T, String> {
        let path = self.vault().cache_dir()?.join(CACHE_FILE);
        store::open(&path, SCHEMA, work)
    }

    /// Say why notes are found by their words alone, unless it was the last
    /// thing said.
    fn say_once(&self, why: &str) {
        let mut said = self
            .meaning()
            .said
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        if said.as_deref() != Some(why) {
            eprintln!("ensemble: knowledge: finding notes by their words alone: {why}");
            *said = Some(why.to_string());
        }
    }
}

/// A vector as `vector32` reads it: `[0.1,0.2]`.
fn vector_text(vector: &[f32]) -> String {
    let numbers: Vec<String> = vector.iter().map(f32::to_string).collect();
    format!("[{}]", numbers.join(","))
}

/// What of a note is embedded: its title and body, cut at [`MAX_TEXT`].
fn text_of(note: &Note) -> String {
    format!("{}\n\n{}", note.title, note.body)
        .chars()
        .take(MAX_TEXT)
        .collect()
}

/// FNV-1a: the same text always gives the same number, in any build.
fn hash(text: &str) -> u64 {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for byte in text.bytes() {
        h ^= u64::from(byte);
        h = h.wrapping_mul(0x0100_0000_01b3);
    }
    h
}

/// Rankings merged: a note scores 1 / (K + its rank) in each, summed, so one
/// high in both comes first. At most `limit`.
fn fuse<'n>(rankings: &[&[&'n Note]], limit: usize) -> Vec<&'n Note> {
    let mut score: HashMap<&str, (f32, &'n Note)> = HashMap::new();
    for ranking in rankings {
        for (rank, note) in ranking.iter().enumerate() {
            score.entry(note.slug.as_str()).or_insert((0.0, note)).0 +=
                1.0 / (FUSION_K + rank as f32 + 1.0);
        }
    }
    let mut merged: Vec<(f32, &'n Note)> = score.into_values().collect();
    merged.sort_by(|a, b| b.0.total_cmp(&a.0).then(a.1.slug.cmp(&b.1.slug)));
    merged.into_iter().take(limit).map(|(_, n)| n).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::search::tests::note;
    use ensemble_backends::testing::{embedding_server, nowhere};
    use ensemble_core::workspace::testing::TempDir;

    /// The knowledge of a hub in `tmp`/hub whose embedding model is at `url`.
    /// A new vault gets three notes: two about releases, in other words, and
    /// one about lunch.
    fn memory_with(tmp: &TempDir, url: &str) -> Memory {
        let yaml = format!(
            "version: 1\nowner: Ana\nllm_backends:\n  local: {{ type: openai-compatible-api, base_url: \"{url}\", timeout_seconds: 5 }}\nknowledge:\n  vault: \"{}\"\n  distill:\n    try:\n      - {{ backend: local, model: small }}\n    when_none: wait\n    max_tokens: 100\n  recall:\n    max_notes: 3\n    max_chars: 1000\n    embed: {{ backend: local, model: e, min_similarity: 0.5 }}\nserver: {{ listen: \"127.0.0.1:0\", registration: {{ type: closed }}, sessions: {{ hours: 1 }}, lockout: {{ failures: 3, minutes: 1 }} }}\nroles: {{ member: [share, recall] }}\n",
            tmp.0.join("vault").display()
        );
        let hub = ensemble_config::parse_hub(
            &yaml,
            &tmp.0.join("hub/hub.yaml"),
            &std::collections::BTreeMap::new(),
        )
        .unwrap();
        let m = Memory::new(&hub, None);
        let vault = m.vault();
        if vault.exists() {
            return m;
        }
        vault.init().unwrap();
        for (slug, title, body) in [
            ("deploy-day", "Deploy day", "Deploys go out on Tuesdays."),
            (
                "release-train",
                "Release train",
                "We ship every second week.",
            ),
            ("lunch", "Lunch", "Pizza on Fridays."),
        ] {
            vault.write_note(&note(slug, title, body)).unwrap();
        }
        m
    }

    fn slugs(notes: &[&Note]) -> Vec<String> {
        notes.iter().map(|n| n.slug.clone()).collect()
    }

    #[test]
    fn notes_are_found_by_what_they_mean_as_well_as_by_their_words() {
        let tmp = TempDir::new();
        let server = embedding_server(&[
            ("deploy", &[1.0, 0.0]),
            ("ship", &[1.0, 0.0]),
            ("pizza", &[0.0, 1.0]),
        ]);
        let m = memory_with(&tmp, &server.url);
        let notes = m.vault().notes();
        // Its words find one; its meaning, the one in other words too, and
        // never lunch.
        assert_eq!(
            slugs(&search::rank(&notes, "when do we deploy", 3)),
            vec!["deploy-day"]
        );
        assert_eq!(
            slugs(&m.find(&notes, "when do we deploy", 3)),
            vec!["deploy-day", "release-train"]
        );
        let recalled = m.recall("when do we deploy");
        assert!(
            recalled.contains("[[release-train]]") && !recalled.contains("Pizza"),
            "{recalled}"
        );
        // The notes were embedded once, in one request; each of the two
        // queries is one more.
        assert_eq!(server.requests.lock().unwrap().len(), 3);
        let kept = |m: &Memory| -> Vec<String> {
            m.cache(async |c| {
                store::rows(c, "SELECT slug FROM embeddings ORDER BY slug", vec![]).await
            })
            .unwrap()
            .iter()
            .filter_map(|r| store::text(&r[0]))
            .collect()
        };
        assert_eq!(kept(&m), vec!["deploy-day", "lunch", "release-train"]);
        // In the vault, and out of its history.
        let vault = m.vault();
        assert!(vault.root().join(".ensemble/embeddings.db").exists());
        vault.commit("notes").unwrap();
        let tracked = std::process::Command::new("git")
            .args(["ls-files"])
            .current_dir(vault.root())
            .output()
            .unwrap();
        let tracked = String::from_utf8_lossy(&tracked.stdout);
        assert!(
            tracked.contains(".gitignore") && !tracked.contains(".ensemble"),
            "{tracked}"
        );
        vault.cache_dir().unwrap();
        assert_eq!(
            std::fs::read_to_string(vault.root().join(".gitignore")).unwrap(),
            "/.ensemble/\n",
            "named once"
        );
        // A changed note is embedded again; the others are not.
        let mut lunch = m.vault().note("lunch").unwrap();
        lunch.body = "Pizza on Fridays, and the ship's cook decides.".into();
        m.vault().write_note(&lunch).unwrap();
        assert_eq!(m.embed_notes().unwrap(), 1);
        assert_eq!(m.embed_notes().unwrap(), 0);
        // Another process finds them in the database.
        let again = memory_with(&tmp, &server.url);
        assert_eq!(again.embed_notes().unwrap(), 0);
        // A note gone takes its vector with it.
        std::fs::remove_file(m.vault().root().join("notes/lunch.md")).unwrap();
        assert_eq!(m.embed_notes().unwrap(), 0);
        assert_eq!(kept(&m), vec!["deploy-day", "release-train"]);
    }

    #[test]
    fn without_the_model_notes_are_found_by_their_words() {
        let tmp = TempDir::new();
        let m = memory_with(&tmp, &nowhere());
        let notes = m.vault().notes();
        assert_eq!(
            slugs(&m.find(&notes, "when do we deploy", 3)),
            vec!["deploy-day"]
        );
        assert!(m.embed_notes().is_err());
    }

    #[test]
    fn a_note_high_in_both_rankings_comes_first() {
        let (a, b, c) = (note("a", "A", ""), note("b", "B", ""), note("c", "C", ""));
        let merged = fuse(&[&[&a, &b], &[&c, &b]], 3);
        let slugs: Vec<&str> = merged.iter().map(|n| n.slug.as_str()).collect();
        assert_eq!(slugs, vec!["b", "a", "c"]);
        assert_eq!(fuse(&[&[&a, &b], &[]], 1)[0].slug, "a");
    }

    #[test]
    fn the_same_text_hashes_the_same() {
        assert_eq!(hash("deploys"), hash("deploys"));
        assert_ne!(hash("deploys"), hash("deploy"));
        assert_eq!(hash(""), 0xcbf2_9ce4_8422_2325);
    }
}
