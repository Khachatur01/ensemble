//! One setting of a YAML file, read or changed as text, so that its comments,
//! its order and everything else in it stay as they were: what `ensemble-runner
//! config`, `ensemble-hub config` and `ensemble-runner team config` do. A
//! setting is named by its path, its keys joined by dots, such as
//! `server.listen`. The caller checks the result by loading it before it is
//! written anywhere.
//!
//! Only block mappings are walked: a setting inside a list, or inside a
//! mapping written inline (`{ a: 1 }`), is set by setting the list or the
//! mapping whole.

use serde_norway::Value;

/// The value of the setting at `key`, as YAML: a scalar as it reads, a list or
/// mapping as a block. `None` when the file has no such setting.
pub fn get(text: &str, key: &str) -> Result<Option<String>, String> {
    let mut value: Value = serde_norway::from_str(text).map_err(|e| e.to_string())?;
    for part in path(key)? {
        match value {
            Value::Mapping(mut map) => match map.remove(part) {
                Some(v) => value = v,
                None => return Ok(None),
            },
            _ => return Ok(None),
        }
    }
    Ok(Some(match &value {
        Value::String(s) => s.clone(),
        Value::Null => "null".into(),
        Value::Bool(b) => b.to_string(),
        Value::Number(n) => n.to_string(),
        other => serde_norway::to_string(other)
            .map_err(|e| e.to_string())?
            .trim_end()
            .to_string(),
    }))
}

/// `text` with the setting at `key` set to `value`, which is written as it is
/// given when it reads as YAML (`laptop`, `8`, `true`, `[a, b]`, `{}`), and
/// quoted otherwise. A setting that is not there is added under the nearest
/// one that is, with the mappings it needs.
pub fn set(text: &str, key: &str, value: &str) -> Result<String, String> {
    let parts = path(key)?;
    let value = scalar(value);
    let lines: Vec<&str> = text.lines().collect();
    let keys = key_lines(&lines);
    let at = |p: &[&str]| keys.iter().position(|k| k.path == p);

    if let Some(i) = at(&parts) {
        let k = &keys[i];
        let end = block_end(&lines, k.line, k.indent);
        let mut line = format!("{}{}: {value}", " ".repeat(k.indent), k.key);
        if let Some(comment) = &k.comment {
            line.push_str(&format!("  {comment}"));
        }
        let mut out: Vec<String> = lines[..k.line].iter().map(|l| l.to_string()).collect();
        out.push(line);
        out.extend(lines[end..].iter().map(|l| l.to_string()));
        return Ok(join(out, text));
    }

    // The nearest setting that is there, and what is missing below it.
    let found = (1..parts.len())
        .rev()
        .find_map(|n| at(&parts[..n]).map(|i| (n, i)));
    let mut out: Vec<String> = lines.iter().map(|l| l.to_string()).collect();
    let (depth, insert_at, indent) = match found {
        None => (0, lines.len(), 0),
        Some((n, i)) => {
            let parent = &keys[i];
            match parent.value.as_str() {
                "" => {}
                "{}" => {
                    // An empty mapping written inline becomes a block.
                    let mut line = format!("{}{}:", " ".repeat(parent.indent), parent.key);
                    if let Some(comment) = &parent.comment {
                        line.push_str(&format!("  {comment}"));
                    }
                    out[parent.line] = line;
                }
                _ => {
                    return Err(format!(
                        "`{}` is written inline; set it whole, such as `{} '{{ … }}'`",
                        parts[..n].join("."),
                        parts[..n].join(".")
                    ));
                }
            }
            let end = block_end(&lines, parent.line, parent.indent);
            let child = keys
                .iter()
                .find(|k| k.line > parent.line && k.line < end && k.path.len() == n + 1)
                .map_or(parent.indent + 2, |k| k.indent);
            (n, end, child)
        }
    };
    let mut added = Vec::new();
    for (step, part) in parts[depth..].iter().enumerate() {
        let pad = " ".repeat(indent + 2 * step);
        if depth + step + 1 == parts.len() {
            added.push(format!("{pad}{part}: {value}"));
        } else {
            added.push(format!("{pad}{part}:"));
        }
    }
    out.splice(insert_at..insert_at, added);
    Ok(join(out, text))
}

/// `text` without the setting at `key`. A mapping it leaves empty is written
/// `{}`.
pub fn unset(text: &str, key: &str) -> Result<String, String> {
    let parts = path(key)?;
    let lines: Vec<&str> = text.lines().collect();
    let keys = key_lines(&lines);
    let k = keys
        .iter()
        .find(|k| k.path == parts)
        .ok_or_else(|| format!("there is no `{key}` to unset"))?;
    let end = block_end(&lines, k.line, k.indent);
    let mut out: Vec<String> = lines[..k.line].iter().map(|l| l.to_string()).collect();
    out.extend(lines[end..].iter().map(|l| l.to_string()));
    if parts.len() > 1
        && let Some(parent) = keys.iter().find(|p| p.path == parts[..parts.len() - 1])
    {
        let parent_end = block_end(&lines, parent.line, parent.indent);
        let left = (parent.line + 1..parent_end)
            .filter(|i| *i < k.line || *i >= end)
            .any(|i| !blank_or_comment(lines[i]));
        if !left {
            let mut line = format!("{}{}: {{}}", " ".repeat(parent.indent), parent.key);
            if let Some(comment) = &parent.comment {
                line.push_str(&format!("  {comment}"));
            }
            out[parent.line] = line;
        }
    }
    Ok(join(out, text))
}

/// `text` with every setting of `doc`, a YAML mapping, set in it: a mapping
/// setting by setting, so what `text` has beside them stays, and anything
/// else whole. Returns the result and the paths it set, such as
/// `server.listen`: what the Nix modules use to keep the settings declared in
/// Nix, and nothing more.
pub fn merge(text: &str, doc: &str) -> Result<(String, Vec<String>), String> {
    let doc: Value = serde_norway::from_str(doc).map_err(|e| e.to_string())?;
    let mut leaves = Vec::new();
    match doc {
        Value::Mapping(map) => leaves_of(&map, &mut Vec::new(), &mut leaves)?,
        Value::Null => {}
        _ => return Err("the settings to merge are not a mapping".into()),
    }
    let mut out = text.to_string();
    let mut paths = Vec::new();
    for (path, value) in leaves {
        let key = path.join(".");
        let value = serde_json::to_string(
            &serde_json::to_value(&value).map_err(|e| format!("{key}: {e}"))?,
        )
        .map_err(|e| format!("{key}: {e}"))?;
        out = set(&out, &key, &value)?;
        paths.push(key);
    }
    Ok((out, paths))
}

/// The settings of `map` that are not themselves non-empty mappings, by path.
fn leaves_of(
    map: &serde_norway::Mapping,
    at: &mut Vec<String>,
    out: &mut Vec<(Vec<String>, Value)>,
) -> Result<(), String> {
    for (key, value) in map {
        let key = key
            .as_str()
            .ok_or_else(|| format!("a key under `{}` is not a string", at.join(".")))?;
        at.push(key.to_string());
        match value {
            Value::Mapping(inner) if !inner.is_empty() => leaves_of(inner, at, out)?,
            _ => out.push((at.clone(), value.clone())),
        }
        at.pop();
    }
    Ok(())
}

/// Whether `key` is `managed`, or holds or is held by a setting that is.
pub fn overlaps(key: &str, managed: &str) -> bool {
    key == managed
        || managed.starts_with(&format!("{key}."))
        || key.starts_with(&format!("{managed}."))
}

/// A key's path: its keys, joined by dots.
fn path(key: &str) -> Result<Vec<&str>, String> {
    let parts: Vec<&str> = key.split('.').collect();
    if parts.iter().any(|p| p.trim().is_empty() || *p != p.trim()) {
        return Err(format!(
            "'{key}' is not a setting: name it by its keys joined by dots, such as server.listen"
        ));
    }
    Ok(parts)
}

/// `value` as it is written: as given when it is one line that reads as YAML
/// (a mapping only when written inline), else as a quoted string.
fn scalar(value: &str) -> String {
    let as_given = !value.contains('\n')
        && !value.contains(" #")
        && !value.trim().is_empty()
        && value == value.trim()
        && match serde_norway::from_str::<Value>(value) {
            Ok(Value::Mapping(_)) => value.starts_with('{'),
            Ok(Value::Sequence(_)) => value.starts_with('['),
            Ok(Value::Tagged(_)) => false,
            Ok(_) => true,
            Err(_) => false,
        };
    if as_given {
        value.to_string()
    } else {
        serde_json::to_string(value).expect("a string serializes")
    }
}

/// A line that sets a key of a block mapping.
struct KeyLine {
    line: usize,
    indent: usize,
    key: String,
    /// Its keys from the top; a key inside a list has a `-` among them, so
    /// that no dotted path names it.
    path: Vec<String>,
    /// What follows the colon, without a comment: empty for a block.
    value: String,
    comment: Option<String>,
}

fn key_lines(lines: &[&str]) -> Vec<KeyLine> {
    let mut out = Vec::new();
    // The keys and list items the current line is inside, by indent.
    let mut stack: Vec<(usize, String)> = Vec::new();
    for (i, line) in lines.iter().enumerate() {
        if blank_or_comment(line) {
            continue;
        }
        let indent = indent_of(line);
        while stack.last().is_some_and(|(at, _)| *at >= indent) {
            stack.pop();
        }
        let rest = line.trim_start();
        if let Some(item) = rest
            .strip_prefix("- ")
            .or(if rest == "-" { Some("") } else { None })
        {
            stack.push((indent, "-".into()));
            // A key on the item's own line holds what is indented under it.
            if let Some((key, _, _)) = split_key(item) {
                stack.push((indent + 2, key));
            }
            continue;
        }
        let Some((key, value, comment)) = split_key(rest) else {
            continue;
        };
        let mut path: Vec<String> = stack.iter().map(|(_, k)| k.clone()).collect();
        path.push(key.clone());
        stack.push((indent, key.clone()));
        out.push(KeyLine {
            line: i,
            indent,
            key,
            path,
            value,
            comment,
        });
    }
    out
}

/// `key: value  # comment`, split, when the line sets a key.
fn split_key(rest: &str) -> Option<(String, String, Option<String>)> {
    let (key, after) = if let Some(quoted) = rest.strip_prefix('"') {
        let end = quoted.find('"')?;
        let after = quoted[end + 1..].strip_prefix(':')?;
        (quoted[..end].to_string(), after)
    } else {
        let colon = rest
            .char_indices()
            .find(|(i, c)| {
                *c == ':' && rest[i + 1..].chars().next().is_none_or(char::is_whitespace)
            })
            .map(|(i, _)| i)?;
        let key = &rest[..colon];
        if key.is_empty() || key.contains(['#', '{', '[', '"', '\'']) {
            return None;
        }
        (key.trim_end().to_string(), &rest[colon + 1..])
    };
    if !after.is_empty() && !after.starts_with(char::is_whitespace) {
        return None;
    }
    let (value, comment) = split_comment(after.trim());
    Some((key, value, comment))
}

/// A value and the comment after it, outside quotes.
fn split_comment(text: &str) -> (String, Option<String>) {
    let mut quote = None;
    let bytes: Vec<char> = text.chars().collect();
    for (i, c) in bytes.iter().enumerate() {
        match (quote, c) {
            (None, '"' | '\'') => quote = Some(*c),
            (Some(q), c) if *c == q => quote = None,
            (None, '#') if i == 0 || bytes[i - 1].is_whitespace() => {
                let value: String = bytes[..i].iter().collect();
                let comment: String = bytes[i..].iter().collect();
                return (value.trim_end().to_string(), Some(comment));
            }
            _ => {}
        }
    }
    (text.to_string(), None)
}

/// One past the last line of what the key at `at`, indented `indent`, holds:
/// the lines indented further, without the blank lines and comments after
/// them, which belong to what follows.
fn block_end(lines: &[&str], at: usize, indent: usize) -> usize {
    let mut last = at;
    for (i, line) in lines.iter().enumerate().skip(at + 1) {
        if blank_or_comment(line) {
            continue;
        }
        if indent_of(line) <= indent {
            break;
        }
        last = i;
    }
    last + 1
}

fn blank_or_comment(line: &str) -> bool {
    let t = line.trim();
    t.is_empty() || t.starts_with('#')
}

fn indent_of(line: &str) -> usize {
    line.len() - line.trim_start().len()
}

fn join(lines: Vec<String>, original: &str) -> String {
    let mut text = lines.join("\n");
    if original.ends_with('\n') || original.is_empty() {
        text.push('\n');
    }
    text
}

#[cfg(test)]
mod tests {
    use super::*;

    const RUNNER: &str = "\
# This computer's runner.
version: 1
name: laptop        # its name
access: private
offers:
  # what it lends
  runs:
    agents: [web/coder]
  models:
    base_url: http://127.0.0.1:11434/v1
    serve: [qwen]

# the end
";

    #[test]
    fn a_setting_is_read_as_it_is() {
        assert_eq!(get(RUNNER, "name").unwrap().as_deref(), Some("laptop"));
        assert_eq!(get(RUNNER, "version").unwrap().as_deref(), Some("1"));
        assert_eq!(
            get(RUNNER, "offers.models.base_url").unwrap().as_deref(),
            Some("http://127.0.0.1:11434/v1")
        );
        assert_eq!(
            get(RUNNER, "offers.runs").unwrap().as_deref(),
            Some("agents:\n- web/coder")
        );
        assert_eq!(get(RUNNER, "hub").unwrap(), None);
        assert_eq!(get(RUNNER, "name.first").unwrap(), None);
        assert!(get(RUNNER, "a..b").is_err());
    }

    #[test]
    fn a_setting_is_changed_in_place_with_its_comment() {
        let out = set(RUNNER, "name", "gpu-box").unwrap();
        assert_eq!(
            out,
            RUNNER.replace(
                "name: laptop        # its name",
                "name: gpu-box  # its name"
            )
        );
        let out = set(RUNNER, "offers.models.base_url", "http://10.0.0.2:8888/v1").unwrap();
        assert!(
            out.contains("    base_url: http://10.0.0.2:8888/v1\n    serve: [qwen]\n"),
            "{out}"
        );
        assert!(out.starts_with("# This computer's runner.\n") && out.ends_with("# the end\n"));
    }

    #[test]
    fn a_block_set_whole_loses_what_it_held() {
        let out = set(RUNNER, "offers.runs", "{}").unwrap();
        assert!(out.contains("  runs: {}\n  models:\n"), "{out}");
        assert!(!out.contains("web/coder"), "{out}");
    }

    #[test]
    fn a_missing_setting_is_added_where_it_belongs() {
        let out = set(RUNNER, "hub", "ws://127.0.0.1:7700").unwrap();
        assert!(
            out.ends_with("# the end\nhub: ws://127.0.0.1:7700\n"),
            "{out}"
        );
        assert_eq!(
            get(&out, "hub").unwrap().as_deref(),
            Some("ws://127.0.0.1:7700")
        );

        let out = set(RUNNER, "offers.models.max_concurrent", "2").unwrap();
        assert!(
            out.contains("    serve: [qwen]\n    max_concurrent: 2\n\n# the end"),
            "{out}"
        );

        let out = set(
            "version: 1\noffers: {}  # none\n",
            "offers.runs.agents",
            "[web/coder]",
        )
        .unwrap();
        assert_eq!(
            out,
            "version: 1\noffers:  # none\n  runs:\n    agents: [web/coder]\n"
        );

        let err = set("offers: { runs: {} }\n", "offers.models", "{}").unwrap_err();
        assert!(err.contains("written inline"), "{err}");
    }

    #[test]
    fn a_value_that_is_not_plain_yaml_is_quoted() {
        let out = set("a: 1\n", "a", "two words: here").unwrap();
        assert_eq!(out, "a: \"two words: here\"\n");
        let out = set("a: 1\n", "a", "x # not a comment").unwrap();
        assert_eq!(out, "a: \"x # not a comment\"\n");
        assert_eq!(set("a: 1\n", "a", "[a, b]").unwrap(), "a: [a, b]\n");
        assert_eq!(set("a: 1\n", "a", "").unwrap(), "a: \"\"\n");
    }

    #[test]
    fn a_setting_is_taken_out_and_an_empty_mapping_says_so() {
        let out = unset(RUNNER, "offers.models").unwrap();
        assert!(
            out.contains("  runs:\n    agents: [web/coder]\n\n# the end"),
            "{out}"
        );
        let out = unset(&out, "offers.runs").unwrap();
        assert!(out.contains("offers: {}\n  # what it lends\n"), "{out}");
        assert!(unset(RUNNER, "hub").unwrap_err().contains("no `hub`"));
    }

    #[test]
    fn settings_are_merged_one_by_one_and_what_is_beside_them_stays() {
        let doc = "name: rogstrix\noffers:\n  models:\n    serve: [qwen, llama]\n  runs: {}\nhub: \"ws://127.0.0.1:7700\"\n";
        let (out, paths) = merge(RUNNER, doc).unwrap();
        assert_eq!(paths, ["name", "offers.models.serve", "offers.runs", "hub"]);
        assert!(out.contains("name: \"rogstrix\"  # its name\n"), "{out}");
        assert!(
            out.contains(
                "    base_url: http://127.0.0.1:11434/v1\n    serve: [\"qwen\",\"llama\"]\n"
            ),
            "{out}"
        );
        assert!(out.contains("  runs: {}\n"), "{out}");
        assert!(out.ends_with("hub: \"ws://127.0.0.1:7700\"\n"), "{out}");
        assert_eq!(get(&out, "access").unwrap().as_deref(), Some("private"));
        // Into nothing, it makes the file.
        let (made, _) = merge("", "version: 1\noffers: {}\n").unwrap();
        assert_eq!(made, "version: 1\noffers: {}\n");
        assert!(overlaps("offers", "offers.runs") && overlaps("offers.runs.agents", "offers.runs"));
        assert!(!overlaps("offers.models", "offers.runs") && !overlaps("name", "names"));
    }

    #[test]
    fn keys_inside_lists_are_not_walked() {
        let text = "try:\n  - backend: local\n    model: small\nwhen_none: fail\n";
        let out = set(text, "try.backend", "x").unwrap();
        assert!(out.contains("  - backend: local\n"), "{out}");
        assert_eq!(
            set(text, "when_none", "wait").unwrap(),
            text.replace("fail", "wait")
        );
    }
}
