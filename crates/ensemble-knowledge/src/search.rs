//! Finding notes by their words. A personal vault is small, so there is no
//! index file: each search reads the notes and ranks them with BM25, a title's
//! words counting three times. Used by recall, and by the pipeline to find the
//! notes a new item may belong to.

use crate::vault::Note;
use std::collections::{HashMap, HashSet};

/// Words too common to tell notes apart.
const STOPWORDS: [&str; 44] = [
    "a", "an", "and", "are", "as", "at", "be", "but", "by", "can", "do", "for", "from", "has",
    "have", "how", "i", "if", "in", "is", "it", "its", "of", "on", "or", "our", "so", "that",
    "the", "their", "there", "this", "to", "use", "was", "we", "what", "when", "which", "who",
    "why", "will", "with", "you",
];

/// How much more a title's word weighs than a body's.
const TITLE_WEIGHT: usize = 3;
// BM25's usual constants.
const K1: f64 = 1.2;
const B: f64 = 0.75;

/// The words of `text`: lowercase letters and digits, stopwords left out.
pub fn words(text: &str) -> Vec<String> {
    text.split(|c: char| !c.is_alphanumeric())
        .filter(|w| !w.is_empty())
        .map(str::to_lowercase)
        .filter(|w| !STOPWORDS.contains(&w.as_str()))
        .collect()
}

/// The notes that match `query`, best first, at most `limit`. A note that
/// shares no word with the query is not returned.
pub fn rank<'n>(notes: &'n [Note], query: &str, limit: usize) -> Vec<&'n Note> {
    let query: HashSet<String> = words(query).into_iter().collect();
    if query.is_empty() || notes.is_empty() {
        return Vec::new();
    }
    let docs: Vec<Vec<String>> = notes
        .iter()
        .map(|n| {
            let mut doc = Vec::new();
            for _ in 0..TITLE_WEIGHT {
                doc.extend(words(&n.title));
            }
            doc.extend(words(&n.tags.join(" ")));
            doc.extend(words(&n.body));
            doc
        })
        .collect();
    let average = docs.iter().map(Vec::len).sum::<usize>() as f64 / docs.len() as f64;
    let total = docs.len() as f64;
    let mut holding: HashMap<&str, usize> = HashMap::new();
    for doc in &docs {
        let unique: HashSet<&str> = doc.iter().map(String::as_str).collect();
        for w in unique {
            if query.contains(w) {
                *holding.entry(w).or_default() += 1;
            }
        }
    }
    let mut scored: Vec<(f64, usize)> = docs
        .iter()
        .enumerate()
        .filter_map(|(i, doc)| {
            let mut score = 0.0;
            for w in &query {
                let tf = doc.iter().filter(|d| *d == w).count() as f64;
                if tf == 0.0 {
                    continue;
                }
                let n = holding.get(w.as_str()).copied().unwrap_or(0) as f64;
                let idf = ((total - n + 0.5) / (n + 0.5) + 1.0).ln();
                let length = doc.len() as f64 / average.max(1.0);
                score += idf * tf * (K1 + 1.0) / (tf + K1 * (1.0 - B + B * length));
            }
            (score > 0.0).then_some((score, i))
        })
        .collect();
    scored.sort_by(|a, b| b.0.total_cmp(&a.0).then(a.1.cmp(&b.1)));
    scored
        .into_iter()
        .take(limit)
        .map(|(_, i)| &notes[i])
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    pub(crate) fn note(slug: &str, title: &str, body: &str) -> Note {
        Note {
            slug: slug.into(),
            kind: "fact".into(),
            title: title.into(),
            tags: Vec::new(),
            scope: "personal".into(),
            sources: vec!["s".into()],
            shared_by: vec!["Ana".into()],
            pipeline: 1,
            updated: "2026-11-10T00:00:00Z".into(),
            body: body.into(),
        }
    }

    #[test]
    fn words_are_lowercase_and_without_stopwords() {
        assert_eq!(
            words("The Shop's port is 8080."),
            vec!["shop", "s", "port", "8080"]
        );
    }

    #[test]
    fn the_note_about_the_query_comes_first_and_a_title_weighs_more() {
        let notes = vec![
            note(
                "deploys",
                "Deploys",
                "Deploys go out on Tuesdays. The port is not involved.",
            ),
            note("ports", "Shop port", "The shop listens on 8080."),
            note("db", "Database", "Postgres 16 on the shop server."),
        ];
        let found: Vec<&str> = rank(&notes, "which port does the shop use", 3)
            .iter()
            .map(|n| n.slug.as_str())
            .collect();
        assert_eq!(found[0], "ports");
        assert!(found.len() == 3, "every note sharing a word: {found:?}");
        assert_eq!(
            rank(&notes, "kubernetes", 3).len(),
            0,
            "no shared word, no note"
        );
        assert_eq!(rank(&notes, "the and of", 3).len(), 0, "stopwords only");
        assert_eq!(rank(&notes, "shop", 1).len(), 1, "at most the limit");
    }
}
