//! A record of every run: on which harness and model, when, how long, how many
//! tokens, whether it worked. One line of JSON per run, appended to the
//! runner's `records.jsonl`, so `ensemble-runner stats` can say what each
//! harness and model costs and how it is doing.

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::io::Write;
use std::path::{Path, PathBuf};

const FILE: &str = "records.jsonl";

/// One run.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Record {
    /// The harness that ran it.
    pub harness: String,
    /// The model, as the harness names it.
    pub model: String,
    /// Seconds since the epoch.
    pub started: u64,
    pub seconds: f64,
    /// Tokens, when the harness or the server said. Absent is not zero.
    pub prompt_tokens: Option<u64>,
    pub completion_tokens: Option<u64>,
    pub steps: u32,
    pub tool_calls: u32,
    pub ok: bool,
    /// Why it failed, in a few words.
    pub error: Option<String>,
}

pub fn path(dir: &Path) -> PathBuf {
    dir.join(FILE)
}

/// Add `record` to the file in `dir`. A failure to write is reported once on
/// stderr and never fails the run.
pub fn append(dir: &Path, record: &Record) {
    let write = || -> std::io::Result<()> {
        std::fs::create_dir_all(dir)?;
        let mut line = serde_json::to_string(record).expect("a record serializes");
        line.push('\n');
        // One write of one line, so runs in parallel do not interleave.
        std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(path(dir))?
            .write_all(line.as_bytes())
    };
    if let Err(e) = write() {
        eprintln!("ensemble: cannot record a run: {e}");
    }
}

/// Every record in `dir`; a line that is not one is skipped.
pub fn read(dir: &Path) -> Vec<Record> {
    std::fs::read_to_string(path(dir))
        .unwrap_or_default()
        .lines()
        .filter_map(|l| serde_json::from_str(l).ok())
        .collect()
}

/// What one harness has done on one model.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Summary {
    pub harness: String,
    pub model: String,
    pub runs: u32,
    pub failures: u32,
    pub total_seconds: f64,
    pub mean_seconds: f64,
    /// Summed over the runs that reported tokens; `None` when none did.
    pub prompt_tokens: Option<u64>,
    pub completion_tokens: Option<u64>,
}

/// Per harness and model, for the runs that started at or after `since`.
pub fn summarize(records: &[Record], since: Option<u64>) -> Vec<Summary> {
    let mut by: BTreeMap<(&str, &str), Summary> = BTreeMap::new();
    for r in records
        .iter()
        .filter(|r| since.is_none_or(|s| r.started >= s))
    {
        let s = by.entry((&r.harness, &r.model)).or_insert_with(|| Summary {
            harness: r.harness.clone(),
            model: r.model.clone(),
            runs: 0,
            failures: 0,
            total_seconds: 0.0,
            mean_seconds: 0.0,
            prompt_tokens: None,
            completion_tokens: None,
        });
        s.runs += 1;
        s.failures += u32::from(!r.ok);
        s.total_seconds += r.seconds;
        let add = |total: &mut Option<u64>, n: Option<u64>| {
            if let Some(n) = n {
                *total = Some(total.unwrap_or(0) + n);
            }
        };
        add(&mut s.prompt_tokens, r.prompt_tokens);
        add(&mut s.completion_tokens, r.completion_tokens);
    }
    let mut out: Vec<Summary> = by.into_values().collect();
    for s in &mut out {
        s.mean_seconds = s.total_seconds / f64::from(s.runs.max(1));
    }
    out
}

/// `30m`, `12h`, `7d` or `2w` as seconds.
pub fn parse_duration(text: &str) -> Result<u64, String> {
    let text = text.trim();
    let (digits, unit) = text.split_at(text.len().saturating_sub(1));
    let n: u64 = digits
        .parse()
        .map_err(|_| format!("'{text}' is not a duration such as 30m, 12h, 7d or 2w"))?;
    let unit = match unit {
        "m" => 60,
        "h" => 3600,
        "d" => 86_400,
        "w" => 604_800,
        _ => {
            return Err(format!(
                "'{text}' is not a duration such as 30m, 12h, 7d or 2w"
            ));
        }
    };
    Ok(n * unit)
}

fn tokens(n: Option<u64>) -> String {
    n.map_or("-".into(), |n| n.to_string())
}

/// The summaries as a table.
pub fn table(summaries: &[Summary]) -> String {
    if summaries.is_empty() {
        return "No runs recorded.\n".into();
    }
    let width = summaries
        .iter()
        .map(|s| s.harness.len() + 4 + s.model.len())
        .max()
        .unwrap_or(3)
        .max(3);
    let mut out = format!(
        "{:<width$}  {:>5}  {:>8}  {:>9}  {:>8}  {:>10}  {:>10}\n",
        "run", "runs", "failures", "total s", "mean s", "tokens in", "tokens out"
    );
    for s in summaries {
        out.push_str(&format!(
            "{:<width$}  {:>5}  {:>8}  {:>9.1}  {:>8.1}  {:>10}  {:>10}\n",
            format!("{} on {}", s.harness, s.model),
            s.runs,
            s.failures,
            s.total_seconds,
            s.mean_seconds,
            tokens(s.prompt_tokens),
            tokens(s.completion_tokens)
        ));
    }
    out
}

pub fn json(summaries: &[Summary]) -> String {
    serde_json::to_string_pretty(summaries).expect("summaries serialize")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::workspace::testing::TempDir;

    fn run(
        harness: &str,
        started: u64,
        seconds: f64,
        tokens: Option<(u64, u64)>,
        ok: bool,
    ) -> Record {
        Record {
            harness: harness.into(),
            model: "m".into(),
            started,
            seconds,
            prompt_tokens: tokens.map(|t| t.0),
            completion_tokens: tokens.map(|t| t.1),
            steps: 1,
            tool_calls: 0,
            ok,
            error: (!ok).then(|| "boom".into()),
        }
    }

    #[test]
    fn records_are_appended_and_read_back_and_a_bad_line_is_skipped() {
        let tmp = TempDir::new();
        append(&tmp.0, &run("codex", 1, 2.0, Some((3, 4)), true));
        append(&tmp.0, &run("builtin", 2, 1.0, None, false));
        let mut text = std::fs::read_to_string(path(&tmp.0)).unwrap();
        text.push_str("not json\n");
        std::fs::write(path(&tmp.0), text).unwrap();
        let all = read(&tmp.0);
        assert_eq!(all.len(), 2);
        assert_eq!(all[1].error.as_deref(), Some("boom"));
        assert!(read(&tmp.0.join("nowhere")).is_empty());
    }

    #[test]
    fn a_summary_sums_runs_and_keeps_unknown_tokens_unknown() {
        let records = [
            run("opencode", 100, 10.0, Some((5, 1)), true),
            run("opencode", 200, 20.0, None, false),
            run("builtin", 200, 4.0, None, true),
            run("opencode", 50, 99.0, Some((1000, 1000)), true),
        ];
        let all = summarize(&records, None);
        let a = &all[1];
        assert_eq!((a.harness.as_str(), a.runs, a.failures), ("opencode", 3, 1));
        assert_eq!(a.total_seconds, 129.0);
        assert_eq!(a.mean_seconds, 43.0);
        assert_eq!(
            (a.prompt_tokens, a.completion_tokens),
            (Some(1005), Some(1001))
        );
        assert_eq!(
            (all[0].prompt_tokens, all[0].completion_tokens),
            (None, None)
        );

        let recent = summarize(&records, Some(100));
        assert_eq!(recent[1].runs, 2);
        assert_eq!(recent[1].prompt_tokens, Some(5));
    }

    #[test]
    fn a_table_shows_a_dash_for_tokens_nobody_reported() {
        let text = table(&summarize(&[run("builtin", 1, 4.0, None, true)], None));
        let row = text.lines().nth(1).unwrap();
        assert!(
            row.starts_with("builtin on m") && row.trim_end().ends_with("-"),
            "{text}"
        );
        let last: Vec<&str> = row.split_whitespace().rev().take(2).collect();
        assert_eq!(last, ["-", "-"], "unknown is not zero: {row}");
        assert_eq!(table(&[]), "No runs recorded.\n");
        let parsed: serde_json::Value = serde_json::from_str(&json(&summarize(
            &[run("builtin", 1, 4.0, None, true)],
            None,
        )))
        .unwrap();
        assert_eq!(parsed[0]["prompt_tokens"], serde_json::Value::Null);
    }

    #[test]
    fn durations_are_a_number_and_a_unit() {
        assert_eq!(parse_duration("30m").unwrap(), 1800);
        assert_eq!(parse_duration("12h").unwrap(), 43_200);
        assert_eq!(parse_duration("7d").unwrap(), 604_800);
        assert_eq!(parse_duration("2w").unwrap(), 1_209_600);
        assert!(
            parse_duration("soon").is_err()
                && parse_duration("5").is_err()
                && parse_duration("").is_err()
        );
    }
}
