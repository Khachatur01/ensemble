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
//! Each harness-visible message reduces to one canonical line. Every turn ends
//! with an assistant message the harness will hold — an answer, or the harness
//! tool calls it was asked to run — and the key is the whole transcript at that
//! moment. On the next request, the longest stored transcript the request
//! still begins with is the conversation it continues; the manager's richer
//! history is restored and the messages the harness added since are appended.
//!
//! Keying on every completed turn, not only on a tool-call turn, is what makes
//! an internal delegation survive into the next *prompt*. A turn that delegated
//! and then answered hands back no tool call at all, but it did hand back an
//! answer, and the hidden delegation belongs behind it. Keying only on tool
//! calls made that delegation vanish the moment the harness sent its next
//! user message.
//!
//! **Full equality of the lines decides.** Two conversations that diverge never
//! match past the divergence. Two byte-identical conversations are the same
//! prompt sequence, and reusing the entry is a cache hit rather than a leak.
//!
//! A miss is safe. The manager simply forgets it delegated, and the transcript
//! is still well-formed, because the assistant message the harness holds
//! contains only the calls it was asked to run and it answered all of them.

use crate::port::{Message, Role};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use super::wire::message_signature;

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
    /// The canonical visible transcript this turn ended with.
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

    /// The manager's view of this conversation, if it was stored.
    ///
    /// Returns the stored history and the messages the harness has added since
    /// the stored turn — its tool results, and any later user prompts — for the
    /// caller to append.
    ///
    /// The longest matching prefix wins. A request may contain several
    /// completed turns, and the newest stored one is the one whose hidden
    /// delegation is most recent.
    pub fn restore(&self, visible: &[Message]) -> Option<(Vec<Message>, Vec<Message>)> {
        let lines = signatures(visible);

        let mut inner = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        let now = Instant::now();
        let hit = inner
            .entries
            .iter_mut()
            .filter(|e| e.prefix.len() <= lines.len() && lines[..e.prefix.len()] == e.prefix[..])
            .max_by_key(|e| e.prefix.len())?;
        hit.last_used = now;
        let split = hit.prefix.len();
        Some((hit.history.clone(), visible[split..].to_vec()))
    }

    /// Remember the manager's view against the transcript the harness will
    /// hold next time.
    ///
    /// `projected` is what the harness ends up with: the messages it sent, plus
    /// the assistant turn it was given — a final answer, or the calls it must
    /// run.
    pub fn remember(&self, projected: &[Message], history: Vec<Message>) {
        // An empty answer with no tool call is not a message a harness will
        // keep. Storing a prefix that ends there would only hide the previous,
        // still-matchable turn behind an entry that can never match.
        if let Some(last) = projected.last()
            && last.role == Role::Assistant
            && last.tool_calls.is_empty()
            && last.content.trim().is_empty()
        {
            return;
        }
        let prefix = signatures(projected);
        let bytes = weigh(&prefix, &history);
        let now = Instant::now();

        let mut inner = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        inner.entries.retain(|e| {
            let expired = now.duration_since(e.created) > self.max_age;
            // The same conversation, one turn further on. The old transcript
            // is a prefix of the new one (or identical), so it can never be
            // the longest match while the new entry is present; keeping it
            // would only grow the store.
            let superseded =
                e.prefix.len() <= prefix.len() && prefix[..e.prefix.len()] == e.prefix[..];
            !(expired || superseded)
        });
        inner.bytes = inner.entries.iter().map(|e| e.bytes).sum();

        inner.entries.push(Entry {
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

    /// A turn that produced only an answer, not a tool call: the shape that
    /// used to vanish from the store and take an internal delegation with it.
    fn plain_turn(seed: &str) -> (Vec<Message>, Vec<Message>, Vec<Message>) {
        let sent = vec![Message::user(seed)];
        let mut projected = sent.clone();
        projected.push(Message::assistant(format!("answer to {seed}")));
        let mut next = projected.clone();
        next.push(Message::user("follow-up"));
        (sent, projected, next)
    }

    #[test]
    fn a_finished_turn_with_no_tool_calls_is_remembered_for_the_next_prompt() {
        let store = ConversationStore::new();
        let (_, projected, next) = plain_turn("find the readme");
        let history = vec![
            Message::system("You are the manager."),
            Message::user("find the readme"),
            Message::assistant_calling("", vec![call("hidden")]),
            Message::tool_result("hidden", "the scout said README.md"),
            Message::assistant("answer to find the readme"),
        ];
        store.remember(&projected, history);

        let (restored, trailing) = store.restore(&next).expect("the finished turn is there");
        assert!(
            restored
                .iter()
                .any(|m| m.content.contains("the scout said")),
            "the delegation behind the answer is back"
        );
        assert_eq!(trailing.len(), 1);
        assert_eq!(trailing[0].content, "follow-up");
        assert_eq!(trailing[0].role, Role::User);
    }

    #[test]
    fn an_empty_answer_does_not_hide_the_previous_turn_from_the_next_request() {
        // A harness is not expected to keep an empty assistant message. If one
        // were stored as the newest prefix, the turn before it could no longer
        // be found; skipping it leaves the real transcript matchable.
        let store = ConversationStore::new();
        let (_, p1, _) = plain_turn("go");
        store.remember(&p1, hidden_history("go"));

        let mut empty = p1.clone();
        empty.push(Message::assistant(""));
        store.remember(&empty, hidden_history("go"));
        assert_eq!(store.len(), 1, "the empty turn was not inserted");

        let mut next = p1.clone();
        next.push(Message::user("still there?"));
        let (history, trailing) = store.restore(&next).expect("the real turn still matches");
        assert!(
            history
                .iter()
                .any(|m| m.content.contains("the planner said"))
        );
        assert_eq!(trailing[0].content, "still there?");
    }

    #[test]
    fn the_longest_matching_prefix_is_the_turn_restored() {
        // Two conversations can share a beginning. Whichever stored turn the
        // request has actually reached is the one whose hidden work belongs in
        // front of the new messages.
        let store = ConversationStore::new();
        let (_, short, _) = plain_turn("go");
        let mut long = short.clone();
        long.push(Message::user("more"));
        long.push(Message::assistant("answer to more"));

        {
            let mut inner = store.inner.lock().unwrap();
            inner.entries.push(Entry {
                prefix: signatures(&short),
                history: vec![Message::assistant("short history")],
                bytes: 1,
                last_used: Instant::now(),
                created: Instant::now(),
            });
            inner.entries.push(Entry {
                prefix: signatures(&long),
                history: vec![Message::assistant("long history")],
                bytes: 1,
                last_used: Instant::now(),
                created: Instant::now(),
            });
        }

        let mut next = long.clone();
        next.push(Message::user("next"));
        let (history, trailing) = store.restore(&next).expect("the longest prefix matches");
        assert_eq!(history[0].content, "long history");
        assert_eq!(trailing.len(), 1);
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
    fn a_request_with_no_completed_turn_is_still_not_looked_up() {
        // Restore is asked before the manager answers, so the last message is
        // normally the user's. No stored turn can end there, and the lookup
        // must be a miss rather than matching some earlier user prompt.
        let store = ConversationStore::new();
        let (_, projected, _) = plain_turn("hello");
        store.remember(
            &projected,
            vec![Message::user("hello"), Message::assistant("hi")],
        );

        let unanswered = vec![Message::user("hello")];
        assert!(store.restore(&unanswered).is_none());
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
