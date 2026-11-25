//! What works on a task: a harness and an LLM, picked from what the runner
//! has (`Means`). Its harnesses a person added that are found and not known
//! to be logged out, each with the LLMs it can drive, are the **pairs**:
//! Claude Code and Codex with the models of their own vendor's a person added
//! to them, OpenCode with the pool's LLMs of an OpenAI-compatible provider and
//! of a known context window, and `builtin` with the pool's LLMs of a known
//! context window and not known to make no tool calls.
//!
//! A pair is usable now while its harness has a free run and its model's
//! provider a free call. The [`Picker`] chooses among them; the one Ensemble
//! has is [`Rules`], and a decision model may take its place. What a run
//! takes it holds until it ends ([`Taken`]), on every repository's worker of
//! the runner at once.

use crate::spec::{
    Adapter, BUILTIN_TOOLS, BuiltinAdapter, CLAUDE_CODE_PERMISSION_MODE, CLAUDE_CODE_TOOLS,
    CODEX_SANDBOX, ClaudeCodeAdapter, CodexAdapter, OPENCODE_TOOLS, OpenCodeAdapter, RunSpec,
};
use ensemble_config::{
    AnnPool, Harness, HarnessKind, LlmBackend, ModelKind, OPENAI_COMPATIBLE_API, Tier,
};
use std::collections::BTreeMap;
use std::sync::{Arc, Mutex, MutexGuard};

/// A harness and the LLM it would drive.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Pair {
    pub harness: HarnessKind,
    /// The provider of the pool its model is on; none for a model of the
    /// harness's own.
    pub provider: Option<String>,
    /// The model, as the harness names it.
    pub model: String,
    pub tier: Tier,
    pub context_window: Option<u32>,
    /// Whether its model makes tool calls; None: nobody said.
    pub tools: Option<bool>,
    /// What a person wrote its model is good and bad at.
    pub abilities: Option<String>,
}

impl Pair {
    /// `opencode on local:qwen3.5:9b`, `claude-code on sonnet`.
    pub fn name(&self) -> String {
        match &self.provider {
            Some(p) => format!("{} on {p}:{}", self.harness.as_str(), self.model),
            None => format!("{} on {}", self.harness.as_str(), self.model),
        }
    }
}

/// A pair as the picker sees it now.
#[derive(Debug, Clone, Copy)]
pub struct Choice<'a> {
    pub pair: &'a Pair,
    /// Its harness has a free run and its provider a free call.
    pub free: bool,
    /// Runs on its harness now.
    pub busy: u32,
}

/// Chooses the pair a run takes, of `choices`, in their fixed order (by
/// harness, then model): one at least `at_least` strong, when a pair that
/// strong exists, else one of the strongest. None: nothing fit is free now,
/// and the run waits.
pub trait Picker: Send + Sync {
    fn pick(&self, choices: &[Choice<'_>], at_least: Option<Tier>) -> Option<usize>;
}

/// The rules: the strongest free pair, the least busy harness among equals,
/// then the fixed order.
pub struct Rules;

impl Picker for Rules {
    fn pick(&self, choices: &[Choice<'_>], at_least: Option<Tier>) -> Option<usize> {
        let strongest = choices.iter().map(|c| c.pair.tier).max()?;
        let floor = at_least.map(|t| t.min(strongest));
        (0..choices.len())
            .filter(|&i| choices[i].free && floor.is_none_or(|f| choices[i].pair.tier >= f))
            .min_by_key(|&i| (std::cmp::Reverse(choices[i].pair.tier), choices[i].busy, i))
    }
}

/// The tier above `tier`, or `tier` when it is the strongest.
pub fn above(tier: Tier) -> Tier {
    match tier {
        Tier::Small => Tier::Medium,
        Tier::Medium | Tier::Strong => Tier::Strong,
    }
}

/// Runs and calls going on now: what a runner tells the hub, whenever it
/// changes, of how busy it is.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Busy {
    /// Runs on each harness.
    pub harnesses: BTreeMap<HarnessKind, u32>,
    /// Runs holding a call of each provider of the pool.
    pub providers: BTreeMap<String, u32>,
}

/// What a runner has to run tasks on: its harnesses that may run, its ANN
/// pool, and what is busy now, for every repository it works in.
pub struct Means {
    /// Its harnesses found and not known to be logged out.
    harnesses: Vec<Harness>,
    pool: AnnPool,
    /// The pool's providers, by name, as a run reaches them.
    backends: BTreeMap<String, LlmBackend>,
    picker: Box<dyn Picker>,
    busy: Mutex<Busy>,
    /// What is busy, each time it changes.
    told: tokio::sync::watch::Sender<Busy>,
}

impl Means {
    /// `harnesses`, those that may run, on `pool`, whose providers a run
    /// reaches as `backends` says; picked by the [`Rules`].
    pub fn new(
        harnesses: Vec<Harness>,
        pool: AnnPool,
        backends: BTreeMap<String, LlmBackend>,
    ) -> Self {
        Means {
            harnesses,
            pool,
            backends,
            picker: Box::new(Rules),
            busy: Mutex::new(Busy::default()),
            told: tokio::sync::watch::Sender::new(Busy::default()),
        }
    }

    /// Picked by `picker` in place of the rules.
    pub fn with_picker(mut self, picker: impl Picker + 'static) -> Self {
        self.picker = Box::new(picker);
        self
    }

    fn lock(&self) -> MutexGuard<'_, Busy> {
        self.busy.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// What is busy now, and again each time it changes.
    pub fn busy(&self) -> tokio::sync::watch::Receiver<Busy> {
        self.told.subscribe()
    }

    /// Its harnesses that may run.
    pub fn harnesses(&self) -> &[Harness] {
        &self.harnesses
    }

    /// The most calls at once to each provider of its pool it calls itself.
    pub fn provider_limits(&self) -> &BTreeMap<String, u32> {
        &self.pool.max_concurrent
    }

    fn harness(&self, kind: HarnessKind) -> Option<&Harness> {
        self.harnesses.iter().find(|h| h.kind == kind)
    }

    /// The provider `name` of the pool, as a run reaches it.
    pub fn backend(&self, name: &str) -> Option<&LlmBackend> {
        self.backends.get(name)
    }

    /// Every pair, by harness, then model.
    pub fn pairs(&self) -> Vec<Pair> {
        let mut out = Vec::new();
        for h in &self.harnesses {
            let mut pairs: Vec<Pair> = match h.kind {
                HarnessKind::ClaudeCode | HarnessKind::Codex => h
                    .models
                    .iter()
                    .map(|m| Pair {
                        harness: h.kind,
                        provider: None,
                        model: m.id.clone(),
                        tier: m.tier,
                        context_window: None,
                        tools: Some(true),
                        abilities: m.abilities.clone(),
                    })
                    .collect(),
                HarnessKind::OpenCode | HarnessKind::Builtin => self
                    .pool
                    .models
                    .iter()
                    .filter(|m| self.backends.contains_key(&m.provider))
                    .filter_map(|m| match m.kind {
                        ModelKind::Llm {
                            tier,
                            context_window: Some(window),
                            tools,
                        } => {
                            let fits = match h.kind {
                                HarnessKind::OpenCode => {
                                    self.backends[&m.provider].type_id() == OPENAI_COMPATIBLE_API
                                }
                                _ => tools != Some(false),
                            };
                            fits.then(|| Pair {
                                harness: h.kind,
                                provider: Some(m.provider.clone()),
                                model: m.id.clone(),
                                tier,
                                context_window: Some(window),
                                tools,
                                abilities: m.abilities.clone(),
                            })
                        }
                        _ => None,
                    })
                    .collect(),
            };
            pairs.sort_by(|a, b| (&a.provider, &a.model).cmp(&(&b.provider, &b.model)));
            out.extend(pairs);
        }
        out
    }

    /// Whether it can run a task at all: it has a pair.
    pub fn can_run(&self) -> bool {
        !self.pairs().is_empty()
    }

    /// Each harness with the pairs it is in, as the runner tells the hub.
    pub fn offers(&self) -> Vec<(HarnessKind, Vec<Pair>)> {
        let mut out: Vec<(HarnessKind, Vec<Pair>)> = Vec::new();
        for pair in self.pairs() {
            match out.last_mut() {
                Some((kind, pairs)) if *kind == pair.harness => pairs.push(pair),
                _ => out.push((pair.harness, vec![pair])),
            }
        }
        out
    }

    /// Take the pair the picker chooses now, at least `at_least` strong when
    /// a pair that strong exists: its harness's run and its provider's call
    /// are held until the [`Taken`] is dropped. None: nothing fit is free.
    pub fn take(self: &Arc<Self>, at_least: Option<Tier>) -> Option<Taken> {
        let pairs = self.pairs();
        let mut busy = self.lock();
        let choices: Vec<Choice<'_>> = pairs
            .iter()
            .map(|pair| {
                let runs = busy.harnesses.get(&pair.harness).copied().unwrap_or(0);
                let harness_free = self
                    .harness(pair.harness)
                    .is_some_and(|h| runs < h.max_concurrent);
                let provider_free = pair.provider.as_ref().is_none_or(|p| {
                    self.pool
                        .max_concurrent
                        .get(p)
                        .is_none_or(|max| busy.providers.get(p).copied().unwrap_or(0) < *max)
                });
                Choice {
                    pair,
                    free: harness_free && provider_free,
                    busy: runs,
                }
            })
            .collect();
        let pair = pairs[self.picker.pick(&choices, at_least)?].clone();
        *busy.harnesses.entry(pair.harness).or_default() += 1;
        if let Some(p) = &pair.provider {
            *busy.providers.entry(p.clone()).or_default() += 1;
        }
        self.told.send_replace(busy.clone());
        drop(busy);
        let spec = self.spec(&pair);
        Some(Taken {
            means: self.clone(),
            pair,
            spec,
        })
    }

    /// How a run on `pair` is started.
    fn spec(&self, pair: &Pair) -> RunSpec {
        let h = self
            .harness(pair.harness)
            .expect("a pair's harness is one of the runner's");
        let command = h.command.clone().unwrap_or_default();
        let provider = pair.provider.clone().unwrap_or_default();
        let window = pair.context_window.unwrap_or_default();
        let adapter = match pair.harness {
            HarnessKind::ClaudeCode => Adapter::ClaudeCode(ClaudeCodeAdapter {
                command,
                model: pair.model.clone(),
                tools: CLAUDE_CODE_TOOLS.iter().map(|t| t.to_string()).collect(),
                permission_mode: CLAUDE_CODE_PERMISSION_MODE.into(),
                max_turns: h.max_steps,
            }),
            HarnessKind::Codex => Adapter::Codex(CodexAdapter {
                command,
                model: pair.model.clone(),
                sandbox: CODEX_SANDBOX.into(),
            }),
            HarnessKind::OpenCode => Adapter::OpenCode(OpenCodeAdapter {
                command,
                provider,
                model: pair.model.clone(),
                context_window: window,
                tools: OPENCODE_TOOLS.iter().map(|t| t.to_string()).collect(),
                max_steps: h.max_steps,
            }),
            HarnessKind::Builtin => Adapter::Builtin(BuiltinAdapter {
                provider,
                model: pair.model.clone(),
                context_window: window,
                tools: BUILTIN_TOOLS.iter().map(|t| t.to_string()).collect(),
                max_steps: h.max_steps,
                max_tokens: h.max_tokens.unwrap_or(u32::MAX),
            }),
        };
        RunSpec {
            name: pair.name(),
            harness: pair.harness,
            tier: pair.tier,
            max_seconds: u32::try_from(h.max_seconds).unwrap_or(u32::MAX),
            adapter,
        }
    }
}

/// A pair a run holds: its harness's run and its provider's call, given back
/// when it is dropped.
pub struct Taken {
    means: Arc<Means>,
    pub pair: Pair,
    pub spec: RunSpec,
}

impl Taken {
    /// The runner's means it was taken of.
    pub fn means(&self) -> &Means {
        &self.means
    }
}

impl Drop for Taken {
    fn drop(&mut self) {
        let mut busy = self.means.lock();
        if let Some(n) = busy.harnesses.get_mut(&self.pair.harness) {
            *n = n.saturating_sub(1);
        }
        if let Some(p) = &self.pair.provider
            && let Some(n) = busy.providers.get_mut(p)
        {
            *n = n.saturating_sub(1);
        }
        self.means.told.send_replace(busy.clone());
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ensemble_config::{HarnessModel, PoolModel};

    fn harness(kind: HarnessKind, max_concurrent: u32) -> Harness {
        Harness {
            kind,
            missing: false,
            command: kind.program().map(String::from),
            max_seconds: 60,
            max_steps: 9,
            max_concurrent,
            max_tokens: (kind == HarnessKind::Builtin).then_some(300),
            models: match kind {
                HarnessKind::ClaudeCode => vec![
                    HarnessModel {
                        id: "haiku".into(),
                        tier: Tier::Medium,
                        abilities: None,
                    },
                    HarnessModel {
                        id: "sonnet".into(),
                        tier: Tier::Strong,
                        abilities: None,
                    },
                ],
                _ => Vec::new(),
            },
        }
    }

    fn llm(
        provider: &str,
        id: &str,
        tier: Tier,
        window: Option<u32>,
        tools: Option<bool>,
    ) -> PoolModel {
        PoolModel {
            provider: provider.into(),
            id: id.into(),
            kind: ModelKind::Llm {
                tier,
                context_window: window,
                tools,
            },
            missing: false,
            shared: false,
            abilities: None,
        }
    }

    /// A runner with `harnesses`, a local OpenAI-compatible server taking
    /// one call at a time and Anthropic's API, and these models.
    fn means(harnesses: Vec<Harness>) -> Arc<Means> {
        let mut pool = AnnPool::default();
        let local = LlmBackend::openai("local", "http://127.0.0.1:1/v1", 5);
        let mut claude = LlmBackend::openai("claude", "https://api.anthropic.com", 5);
        claude.settings = ensemble_config::BackendSettings::Anthropic {
            base_url: "https://api.anthropic.com".into(),
            api_key: None,
        };
        pool.direct.insert("local".into(), local.clone());
        pool.direct.insert("claude".into(), claude.clone());
        pool.max_concurrent.insert("local".into(), 1);
        pool.max_concurrent.insert("claude".into(), 4);
        pool.models = vec![
            llm("local", "small", Tier::Small, Some(32768), Some(true)),
            llm("local", "big", Tier::Strong, Some(65536), None),
            llm("local", "no-window", Tier::Strong, None, Some(true)),
            llm("local", "no-tools", Tier::Medium, Some(8192), Some(false)),
            llm(
                "claude",
                "sonnet-api",
                Tier::Strong,
                Some(200_000),
                Some(true),
            ),
        ];
        let backends = pool.direct.clone();
        Arc::new(Means::new(harnesses, pool, backends))
    }

    fn names(pairs: &[Pair]) -> Vec<String> {
        pairs.iter().map(Pair::name).collect()
    }

    #[test]
    fn each_harness_drives_the_llms_it_can() {
        let m = means(vec![
            harness(HarnessKind::ClaudeCode, 1),
            harness(HarnessKind::OpenCode, 1),
            harness(HarnessKind::Builtin, 1),
        ]);
        assert_eq!(
            names(&m.pairs()),
            [
                "claude-code on haiku",
                "claude-code on sonnet",
                // OpenCode drives OpenAI-compatible providers alone, and a
                // model whose window is known.
                "opencode on local:big",
                "opencode on local:no-tools",
                "opencode on local:small",
                // builtin drives any provider, never a model that makes no
                // tool calls.
                "builtin on claude:sonnet-api",
                "builtin on local:big",
                "builtin on local:small",
            ]
        );
        assert!(m.can_run());
        assert!(!means(Vec::new()).can_run(), "no harness, nothing to run");
    }

    #[test]
    fn the_strongest_free_pair_is_taken_and_held_until_it_is_dropped() {
        let m = means(vec![
            harness(HarnessKind::OpenCode, 2),
            harness(HarnessKind::Builtin, 1),
        ]);
        let told = m.busy();
        let first = m.take(None).unwrap();
        assert_eq!(first.spec.name, "opencode on local:big");
        assert_eq!(
            told.borrow().harnesses[&HarnessKind::OpenCode],
            1,
            "what is busy is told"
        );
        assert_eq!(told.borrow().providers["local"], 1);
        // The local server takes one call at a time: its strong model is
        // busy, so the strongest free one is Anthropic's, on builtin.
        let second = m.take(None).unwrap();
        assert_eq!(second.spec.name, "builtin on claude:sonnet-api");
        // builtin runs one at a time, and the local server is busy.
        assert!(m.take(None).is_none());
        drop(first);
        assert_eq!(told.borrow().harnesses[&HarnessKind::OpenCode], 0);
        assert_eq!(told.borrow().providers["local"], 0);
        assert_eq!(m.take(None).unwrap().spec.name, "opencode on local:big");
    }

    #[test]
    fn a_retry_asks_for_a_stronger_tier_or_the_strongest_there_is() {
        let m = means(vec![harness(HarnessKind::ClaudeCode, 1)]);
        let rules = Rules;
        let pairs = m.pairs();
        let choices: Vec<Choice<'_>> = pairs
            .iter()
            .map(|pair| Choice {
                pair,
                free: pair.model != "sonnet",
                busy: 0,
            })
            .collect();
        // Sonnet is busy: a run that needs strong waits for it.
        assert_eq!(rules.pick(&choices, Some(Tier::Strong)), None);
        assert_eq!(rules.pick(&choices, Some(Tier::Medium)), Some(0));
        assert_eq!(above(Tier::Small), Tier::Medium);
        assert_eq!(above(Tier::Strong), Tier::Strong);

        // With nothing strong at all, the strongest there is will do.
        let small = Pair {
            tier: Tier::Small,
            ..pairs[0].clone()
        };
        let only = [Choice {
            pair: &small,
            free: true,
            busy: 0,
        }];
        assert_eq!(rules.pick(&only, Some(Tier::Strong)), Some(0));
    }

    #[test]
    fn the_least_busy_harness_wins_among_equals() {
        let m = means(Vec::new());
        let a = Pair {
            harness: HarnessKind::OpenCode,
            provider: Some("local".into()),
            model: "m".into(),
            tier: Tier::Medium,
            context_window: Some(1),
            tools: None,
            abilities: None,
        };
        let b = Pair {
            harness: HarnessKind::Builtin,
            ..a.clone()
        };
        let choices = [
            Choice {
                pair: &a,
                free: true,
                busy: 2,
            },
            Choice {
                pair: &b,
                free: true,
                busy: 0,
            },
        ];
        assert_eq!(Rules.pick(&choices, None), Some(1));
        assert!(m.pairs().is_empty());
    }

    #[test]
    fn a_run_is_started_from_its_harness_entry() {
        let m = means(vec![harness(HarnessKind::Builtin, 1)]);
        let taken = m.take(None).unwrap();
        assert_eq!(taken.spec.max_seconds, 60);
        match &taken.spec.adapter {
            Adapter::Builtin(b) => {
                assert_eq!(
                    (b.provider.as_str(), b.model.as_str()),
                    ("claude", "sonnet-api")
                );
                assert_eq!(
                    (b.max_steps, b.max_tokens, b.context_window),
                    (9, 300, 200_000)
                );
                assert_eq!(b.tools, BUILTIN_TOOLS);
            }
            other => panic!("{other:?}"),
        }
    }
}
