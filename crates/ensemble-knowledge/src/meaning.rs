//! Finding notes by what they mean, with `knowledge.recall.embed`. Each note's
//! title and body is a vector from the embedding model, kept in a cache in the
//! vault's `.git` folder, out of its history: it is made again from the notes
//! whenever they or the model change. A query's vector finds the notes that
//! point its way, at least `min_similarity` alike, and those are merged with
//! the notes its words find ([`search`](crate::search)), so a note found both
//! ways comes first. Without a model, or when it cannot be asked, notes are
//! found by their words alone.

use crate::Memory;
use crate::search;
use crate::vault::Note;
use ensemble_backends::embed::{cosine, embed};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, HashMap};
use std::sync::Mutex;

/// The cache's file, in the vault's `.git` folder.
const CACHE_FILE: &str = "ensemble-meaning.json";
/// The most notes embedded in one request.
const BATCH: usize = 32;
/// The most of a note's text embedded, in characters: embedding models read
/// little.
const MAX_TEXT: usize = 4000;
/// Reciprocal rank fusion's usual constant: how little a lower rank counts.
const FUSION_K: f32 = 60.0;

/// The vectors of the notes, by slug, for one model.
#[derive(Debug, Default, Serialize, Deserialize)]
struct Cache {
    model: String,
    notes: BTreeMap<String, Kept>,
}

/// One note's vector, and a hash of the text it is of.
#[derive(Debug, Serialize, Deserialize)]
struct Kept {
    hash: u64,
    vector: Vec<f32>,
}

/// What a [`Memory`] keeps to find notes by their meaning.
#[derive(Debug, Default)]
pub(crate) struct Meaning {
    /// Read from the file when first needed.
    cache: Mutex<Option<Cache>>,
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
        let notes = self.vault().notes();
        let mut cache = self
            .meaning()
            .cache
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        self.fill(&mut cache, &notes)
            .inspect_err(|e| self.say_once(e))
    }

    /// The notes at least `min_similarity` alike to `query`, most alike first.
    fn by_meaning<'n>(&self, notes: &'n [Note], query: &str) -> Result<Vec<&'n Note>, String> {
        let embed_with = self
            .knowledge()
            .recall
            .embed
            .as_ref()
            .ok_or("no embedding model")?;
        let mut cache = self
            .meaning()
            .cache
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        self.fill(&mut cache, notes)?;
        let cache = cache.as_ref().ok_or("no vectors")?;
        let asked = self
            .vectors(&[query.trim().to_string()])?
            .pop()
            .ok_or("the model gave no vector for the query")?;
        let mut alike: Vec<(f32, &Note)> = notes
            .iter()
            .filter_map(|n| {
                let kept = cache.notes.get(&n.slug)?;
                let similarity = cosine(&asked, &kept.vector);
                (similarity >= embed_with.min_similarity()).then_some((similarity, n))
            })
            .collect();
        alike.sort_by(|a, b| b.0.total_cmp(&a.0).then(a.1.slug.cmp(&b.1.slug)));
        Ok(alike.into_iter().map(|(_, n)| n).collect())
    }

    /// Give every note in `notes` a vector of its text, and forget the notes
    /// gone: read from the file first, written back when changed. How many it
    /// embedded.
    fn fill(&self, cache: &mut Option<Cache>, notes: &[Note]) -> Result<usize, String> {
        let model = match &self.knowledge().recall.embed {
            Some(e) => e.model.clone(),
            None => return Ok(0),
        };
        let path = self.vault().lock_dir().join(CACHE_FILE);
        let cache = cache.get_or_insert_with(|| {
            std::fs::read_to_string(&path)
                .ok()
                .and_then(|t| serde_json::from_str(&t).ok())
                .unwrap_or_default()
        });
        let mut changed = false;
        if cache.model != model {
            *cache = Cache {
                model,
                notes: BTreeMap::new(),
            };
            changed = true;
        }
        let before = cache.notes.len();
        cache
            .notes
            .retain(|slug, _| notes.iter().any(|n| &n.slug == slug));
        changed |= cache.notes.len() != before;
        let missing: Vec<(&Note, u64)> = notes
            .iter()
            .map(|n| (n, hash(&text_of(n))))
            .filter(|(n, h)| cache.notes.get(&n.slug).is_none_or(|k| k.hash != *h))
            .collect();
        let mut made = 0;
        let mut failed = None;
        for batch in missing.chunks(BATCH) {
            let texts: Vec<String> = batch.iter().map(|(n, _)| text_of(n)).collect();
            match self.vectors(&texts) {
                Ok(vectors) => {
                    for ((note, hash), vector) in batch.iter().zip(vectors) {
                        cache.notes.insert(
                            note.slug.clone(),
                            Kept {
                                hash: *hash,
                                vector,
                            },
                        );
                        made += 1;
                    }
                }
                Err(e) => {
                    failed = Some(e);
                    break;
                }
            }
        }
        if changed || made > 0 {
            let text = serde_json::to_string(cache).map_err(|e| e.to_string())?;
            let tmp = path.with_extension("tmp");
            std::fs::write(&tmp, text)
                .and_then(|()| std::fs::rename(&tmp, &path))
                .map_err(|e| format!("cannot write {}: {e}", path.display()))?;
        }
        match failed {
            Some(e) => Err(e),
            None => Ok(made),
        }
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

    /// The knowledge of a hub whose embedding model is at `url`. A new vault
    /// gets three notes: two about releases, in other words, and one about
    /// lunch.
    fn memory_with(tmp: &TempDir, url: &str) -> Memory {
        let yaml = format!(
            "version: 1\nowner: Ana\nllm_backends:\n  local: {{ type: openai-compatible-api, base_url: \"{url}\", timeout_seconds: 5 }}\nknowledge:\n  vault: \"{}\"\n  distill:\n    try:\n      - {{ backend: local, model: small }}\n    when_none: wait\n    max_tokens: 100\n  recall:\n    max_notes: 3\n    max_chars: 1000\n    embed: {{ backend: local, model: e, min_similarity: 0.5 }}\nserver: {{ listen: \"127.0.0.1:0\", registration: {{ type: closed }}, sessions: {{ hours: 1 }}, lockout: {{ failures: 3, minutes: 1 }} }}\nroles: {{ member: [share, recall] }}\n",
            tmp.0.join("vault").display()
        );
        let hub =
            ensemble_config::parse_hub(&yaml, std::path::Path::new("hub.yaml"), &BTreeMap::new())
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
        assert!(m.vault().lock_dir().join(CACHE_FILE).exists());
        // A changed note is embedded again; the others are not.
        let mut lunch = m.vault().note("lunch").unwrap();
        lunch.body = "Pizza on Fridays, and the ship's cook decides.".into();
        m.vault().write_note(&lunch).unwrap();
        assert_eq!(m.embed_notes().unwrap(), 1);
        assert_eq!(m.embed_notes().unwrap(), 0);
        // Another process reads the vectors from the cache.
        let again = memory_with(&tmp, &server.url);
        assert_eq!(again.embed_notes().unwrap(), 0);
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
