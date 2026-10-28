//! What the harness is not allowed to see, kept between round trips.
//!
//! THE PROBLEM
//! -----------
//! The manager delegates *inside* one HTTP request. Those turns — the
//! delegation calls and their results — must never reach the harness, which is
//! the whole point of the design. But the harness owns the transcript: on the
//! next request it sends back what it was given, and that is missing exactly
//! the turns it was never shown. Left alone, the manager would forget it had
//! delegated and do the work twice.
//!
//! WHAT WAS REJECTED
//! -----------------
//! *Replaying statelessly* — re-running the delegations each round trip — is
//! wrong three times over: it is quadratic in model calls, the answers differ
//! between runs so the manager sees a transcript it did not produce, and a
//! delegation has side effects. A `coder` role with `Edit` permission would
//! write the file twice.
//!
//! *Smuggling the turns through the response* — packing them into a tool-call
//! id or an extra field — surfaces internal delegation to the harness, which
//! the design forbids, and relies on a client echoing back fields it never
//! promised to keep.
//!
//! HOW THIS ONE WORKS
//! ------------------
//! Each harness-visible message reduces to one canonical line. The key is
//! every line up to and including the last assistant message that carried tool
//! calls — the exact transcript the harness held when it was handed those
//! calls. A 64-bit hash picks a bucket and **full equality of the lines
//! decides**, so a collision costs one comparison and can never hand back
//! another conversation's turns.
//!
//! Two distinct conversations differ in at least one visible message, so in at
//! least one line, so they never match. Two byte-identical conversations are
//! the same prompt sequence, and reusing the entry is a cache hit rather than
//! a leak — said here rather than pretended impossible.
//!
//! A miss is safe. The manager simply forgets it delegated, and the transcript
//! is still well-formed, because the assistant message the harness holds
//! contains only the calls it was asked to run and it answered all of them.

use crate::port::Message;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use super::wire::message_signature;

/// FNV-1a. A bucket hint, never the decision — see the module note.
fn fingerprint(lines: &[String]) -> u64 {
    let mut h: u64 = 0xcbf29ce484222325;
    for line in lines {
        for b in line.as_bytes() {
            h ^= *b as u64;
            h = h.wrapping_mul(0x100000001b3);
        }
        h ^= 0xff;
        h = h.wrapping_mul(0x100000001b3);
    }
    h
}

fn signatures(messages: &[Message]) -> Vec<String> {
    messages.iter().map(message_signature).collect()
}

fn weigh(lines: &[String], history: &[Message]) -> usize {
    lines.iter().map(String::len).sum::<usize>()
        + history
            .iter()
            .map(|m| {
                m.content.len()
                    + m.tool_calls
                        .iter()
                        .map(|c| c.id.len() + c.name.len() + c.arguments.len())
                        .sum::<usize>()
            })
            .sum::<usize>()
}

struct Entry {
    fingerprint: u64,
    /// The canonical visible transcript. The proof, not the hash.
    prefix: Vec<String>,
    /// The conversation as the *manager* should see it, system turns and
    /// internal delegation included.
    history: Vec<Message>,
    bytes: usize,
    last_used: Instant,
    created: Instant,
}

struct Inner {
    entries: Vec<Entry>,
    bytes: usize,
}

pub struct ConversationStore {
    inner: Mutex<Inner>,
    max_entries: usize,
    max_bytes: usize,
    max_age: Duration,
}

impl Default for ConversationStore {
    fn default() -> Self {
        Self::with_limits(64, 32 * 1024 * 1024, Duration::from_secs(30 * 60))
    }
}

impl ConversationStore {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn with_limits(max_entries: usize, max_bytes: usize, max_age: Duration) -> Self {
        ConversationStore {
            inner: Mutex::new(Inner {
                entries: Vec::new(),
                bytes: 0,
            }),
            max_entries,
            max_bytes,
            max_age,
        }
    }

    /// Where the stored part of a transcript ends and this turn's new messages
    /// begin: the index just past the last assistant turn bearing tool calls.
    fn split_at(visible: &[Message]) -> Option<usize> {
        visible
            .iter()
            .rposition(|m| !m.tool_calls.is_empty())
            .map(|i| i + 1)
    }

    /// The manager's view of this conversation, if it was stored.
    ///
    /// Returns the stored history and the messages the harness has added since
    /// — its tool results — for the caller to append.
    pub fn restore(&self, visible: &[Message]) -> Option<(Vec<Message>, Vec<Message>)> {
        let split = Self::split_at(visible)?;
        let prefix = signatures(&visible[..split]);
        let fp = fingerprint(&prefix);

        let mut inner = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        let now = Instant::now();
        let hit = inner
            .entries
            .iter_mut()
            .find(|e| e.fingerprint == fp && e.prefix == prefix)?;
        hit.last_used = now;
        Some((hit.history.clone(), visible[split..].to_vec()))
    }

    /// Remember the manager's view against the transcript the harness will
    /// hold next time.
    ///
    /// `projected` is what the harness ends up with: the messages it sent, plus
    /// the assistant turn carrying only the calls it was asked to run.
    pub fn remember(&self, projected: &[Message], history: Vec<Message>) {
        if Self::split_at(projected).is_none() {
            // Nothing was handed back for the harness to answer, so there is
            // no follow-up request to match. Storing it would only be litter.
            return;
        }
        let prefix = signatures(projected);
        let fp = fingerprint(&prefix);
        let bytes = weigh(&prefix, &history);
        let now = Instant::now();

        let mut inner = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        inner.entries.retain(|e| {
            let expired = now.duration_since(e.created) > self.max_age;
            // The same conversation, one round trip further on. Keeping the
            // older step would only grow the store; the harness will never ask
            // for it again.
            let superseded = e.prefix.len() < prefix.len() && prefix.starts_with(&e.prefix);
            let same = e.fingerprint == fp && e.prefix == prefix;
            !(expired || superseded || same)
        });
        inner.bytes = inner.entries.iter().map(|e| e.bytes).sum();

        inner.entries.push(Entry {
            fingerprint: fp,
            prefix,
            history,
            bytes,
            last_used: now,
            created: now,
        });
        inner.bytes += bytes;

        while inner.entries.len() > self.max_entries || inner.bytes > self.max_bytes {
            let Some(oldest) = inner
                .entries
                .iter()
                .enumerate()
                .min_by_key(|(_, e)| e.last_used)
                .map(|(i, _)| i)
            else {
                break;
            };
            // Never evict the entry just inserted into an empty-ish store: if
            // one entry alone is over the byte budget, keeping it is better
            // than a store that can hold nothing.
            if inner.entries.len() == 1 {
                break;
            }
            let gone = inner.entries.remove(oldest);
            inner.bytes -= gone.bytes;
        }
    }

    pub fn len(&self) -> usize {
        self.inner
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .entries
            .len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::port::ToolCall;

    fn call(id: &str) -> ToolCall {
        ToolCall {
            id: id.into(),
            name: "read".into(),
            arguments: "{}".into(),
        }
    }

    /// One round trip: what the harness sent, what it got back, what it sends
    /// next.
    fn conversation(seed: &str) -> (Vec<Message>, Vec<Message>, Vec<Message>) {
        let sent = vec![Message::user(seed)];
        let projected = vec![
            Message::user(seed),
            Message::assistant_calling("", vec![call("call_x")]),
        ];
        let next = vec![
            Message::user(seed),
            Message::assistant_calling("", vec![call("call_x")]),
            Message::tool_result("call_x", "file contents"),
        ];
        (sent, projected, next)
    }

    /// The manager's private view: system turns and a delegation the harness
    /// never saw.
    fn hidden_history(seed: &str) -> Vec<Message> {
        vec![
            Message::system("You are the manager."),
            Message::user(seed),
            Message::assistant_calling("", vec![call("call_internal")]),
            Message::tool_result("call_internal", "the planner said: do it in two steps"),
            Message::assistant_calling("", vec![call("call_x")]),
        ]
    }

    #[test]
    fn the_hidden_turns_are_restored_when_the_harness_posts_the_tool_result() {
        let store = ConversationStore::new();
        let (_, projected, next) = conversation("fix the parser");
        store.remember(&projected, hidden_history("fix the parser"));

        let (history, trailing) = store.restore(&next).expect("the entry is there");
        assert!(
            history
                .iter()
                .any(|m| m.content.contains("the planner said")),
            "the delegation the harness never saw is back"
        );
        assert_eq!(trailing.len(), 1);
        assert_eq!(trailing[0].tool_call_id.as_deref(), Some("call_x"));
    }

    #[test]
    fn two_conversations_with_different_prefixes_never_see_each_others_turns() {
        let store = ConversationStore::new();
        let (_, p1, n1) = conversation("task one");
        let (_, p2, n2) = conversation("task two");
        store.remember(&p1, hidden_history("task one"));
        store.remember(&p2, hidden_history("task two"));

        let (h1, _) = store.restore(&n1).unwrap();
        let (h2, _) = store.restore(&n2).unwrap();
        assert!(h1.iter().any(|m| m.content.contains("task one")));
        assert!(!h1.iter().any(|m| m.content.contains("task two")));
        assert!(h2.iter().any(|m| m.content.contains("task two")));
        assert!(!h2.iter().any(|m| m.content.contains("task one")));
    }

    #[test]
    fn a_fingerprint_collision_is_ruled_out_by_comparing_the_prefix_itself() {
        // Force the collision rather than hunting for one: two entries sharing
        // a bucket must still be told apart by the lines themselves.
        let store = ConversationStore::new();
        let (_, p1, _) = conversation("alpha");
        store.remember(&p1, hidden_history("alpha"));
        {
            let mut inner = store.inner.lock().unwrap();
            let fp = inner.entries[0].fingerprint;
            let mut forged = Entry {
                fingerprint: fp,
                prefix: signatures(&conversation("beta").1),
                history: hidden_history("beta"),
                bytes: 1,
                last_used: Instant::now(),
                created: Instant::now(),
            };
            forged.fingerprint = fp;
            inner.entries.insert(0, forged);
        }
        let (_, _, n1) = conversation("alpha");
        let (history, _) = store.restore(&n1).unwrap();
        assert!(history.iter().any(|m| m.content.contains("alpha")));
        assert!(!history.iter().any(|m| m.content.contains("beta")));
    }

    #[test]
    fn a_superseded_entry_is_dropped_when_the_conversation_moves_on() {
        let store = ConversationStore::new();
        let (_, p1, _) = conversation("go");
        store.remember(&p1, hidden_history("go"));
        assert_eq!(store.len(), 1);

        // The same conversation, one exchange later.
        let mut p2 = p1.clone();
        p2.push(Message::tool_result("call_x", "contents"));
        p2.push(Message::assistant_calling("", vec![call("call_y")]));
        store.remember(&p2, hidden_history("go"));

        assert_eq!(store.len(), 1, "one live entry per conversation");
    }

    #[test]
    fn an_entry_past_the_age_limit_is_evicted_on_the_next_insert() {
        let store = ConversationStore::with_limits(64, 1 << 20, Duration::from_nanos(1));
        let (_, p1, n1) = conversation("old");
        store.remember(&p1, hidden_history("old"));
        std::thread::sleep(Duration::from_millis(5));

        let (_, p2, _) = conversation("new");
        store.remember(&p2, hidden_history("new"));

        assert!(store.restore(&n1).is_none(), "the stale entry is gone");
        assert_eq!(store.len(), 1);
    }

    #[test]
    fn the_store_evicts_least_recently_used_once_the_entry_budget_is_reached() {
        let store = ConversationStore::with_limits(2, 1 << 20, Duration::from_secs(600));
        for seed in ["a", "b", "c"] {
            let (_, p, _) = conversation(seed);
            store.remember(&p, hidden_history(seed));
        }
        assert_eq!(store.len(), 2);
        let (_, _, first) = conversation("a");
        assert!(store.restore(&first).is_none(), "the oldest went first");
        let (_, _, last) = conversation("c");
        assert!(store.restore(&last).is_some());
    }

    #[test]
    fn the_byte_budget_is_enforced_as_well_as_the_entry_count() {
        let store = ConversationStore::with_limits(64, 200, Duration::from_secs(600));
        for seed in ["a", "b", "c", "d", "e", "f"] {
            let (_, p, _) = conversation(seed);
            store.remember(&p, hidden_history(seed));
        }
        assert!(store.len() < 6, "something was evicted: {}", store.len());
    }

    #[test]
    fn a_lookup_miss_degrades_to_a_valid_transcript_rather_than_a_dangling_call() {
        // Nothing stored: the caller falls back to the harness's own messages,
        // which are well-formed on their own.
        let store = ConversationStore::new();
        let (_, _, next) = conversation("never stored");
        assert!(store.restore(&next).is_none());
    }

    #[test]
    fn a_first_turn_with_no_tool_calls_is_not_stored_and_not_looked_up() {
        let store = ConversationStore::new();
        let plain = vec![Message::user("hello")];
        store.remember(&plain, vec![Message::user("hello")]);
        assert!(store.is_empty(), "nothing to match a follow-up against");
        assert!(store.restore(&plain).is_none());
    }

    #[test]
    fn the_store_survives_several_threads_reading_and_writing_at_once() {
        let store = std::sync::Arc::new(ConversationStore::new());
        let mut handles = Vec::new();
        for t in 0..8 {
            let store = std::sync::Arc::clone(&store);
            handles.push(std::thread::spawn(move || {
                for i in 0..25 {
                    let seed = format!("thread {t} item {i}");
                    let (_, p, n) = conversation(&seed);
                    store.remember(&p, hidden_history(&seed));
                    if let Some((history, _)) = store.restore(&n) {
                        assert!(history.iter().any(|m| m.content.contains(&seed)));
                    }
                }
            }));
        }
        for h in handles {
            h.join().unwrap();
        }
        assert!(store.len() <= 64);
    }
}
