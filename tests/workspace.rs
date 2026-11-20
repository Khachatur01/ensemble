//! The crates of the workspace depend on each other in one direction only, so a
//! later module (memory, the hub, the link) is added as a crate on top, never
//! woven back into one below; and the runner is built of none of the hub's
//! own: it shares only the store of settings and sealed keys.
//! This reads each crate's `Cargo.toml` rather than asking cargo, so it needs
//! nothing but the files.

use std::path::Path;

/// Bottom to top: a crate may depend only on crates before it.
const ORDER: [&str; 11] = [
    "ensemble-core",
    "ensemble-ops",
    "ensemble-config",
    "ensemble-backends",
    "ensemble-store",
    "ensemble-knowledge",
    "ensemble-link",
    "ensemble-agents",
    "ensemble-tasks",
    "ensemble-runner",
    "ensemble-hub",
];

/// The crates only the hub has, which the runner never builds, by itself or
/// through another crate: the hub and its knowledge. The store, where both
/// keep their settings and sealed keys, they share.
const HUB_ONLY: [&str; 2] = ["ensemble-hub", "ensemble-knowledge"];

/// The workspace crates a manifest names, in any dependency table.
fn ensemble_dependencies(manifest: &str) -> Vec<String> {
    let mut found = Vec::new();
    let mut in_dependencies = false;
    for line in manifest.lines() {
        let line = line.trim();
        if line.starts_with('[') {
            in_dependencies = line.ends_with("dependencies]");
            continue;
        }
        if !in_dependencies {
            continue;
        }
        if let Some(name) = line.split(['=', '.', ' ']).next()
            && name.starts_with("ensemble-")
            && !found.iter().any(|f| f == name)
        {
            found.push(name.to_string());
        }
    }
    found
}

#[test]
fn every_crate_depends_only_on_crates_below_it() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"));
    let mut crates: Vec<String> = std::fs::read_dir(root.join("crates"))
        .unwrap()
        .filter_map(Result::ok)
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .collect();
    crates.sort();
    let mut known = ORDER.map(String::from).to_vec();
    known.sort();
    assert_eq!(
        crates, known,
        "a crate was added or removed: put it in ORDER, where its dependencies allow"
    );
    for (at, name) in ORDER.iter().enumerate() {
        let manifest =
            std::fs::read_to_string(root.join("crates").join(name).join("Cargo.toml")).unwrap();
        for dependency in ensemble_dependencies(&manifest) {
            let below = ORDER[..at].contains(&dependency.as_str());
            assert!(
                below,
                "{name} depends on {dependency}, which is not below it in {ORDER:?}"
            );
        }
    }
}

#[test]
fn the_runner_depends_on_nothing_of_the_hub() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"));
    let manifest = |name: &str| {
        std::fs::read_to_string(root.join("crates").join(name).join("Cargo.toml")).unwrap()
    };
    // Every workspace crate the runner depends on, through any other.
    let mut reached = vec!["ensemble-runner".to_string()];
    let mut at = 0;
    while at < reached.len() {
        for dependency in ensemble_dependencies(&manifest(&reached[at])) {
            if !reached.contains(&dependency) {
                reached.push(dependency);
            }
        }
        at += 1;
    }
    for name in &reached {
        assert!(
            !HUB_ONLY.contains(&name.as_str()),
            "the runner depends on {name}, which only the hub has: {reached:?}"
        );
    }
}

#[test]
fn the_manifest_reader_finds_every_kind_of_dependency() {
    let manifest = "[package]\nname = \"x\"\n\n[dependencies]\nensemble-core.workspace = true\nserde.workspace = true\n\n[target.'cfg(unix)'.dependencies]\nensemble-config = { workspace = true }\n\n[dev-dependencies]\nensemble-core = { workspace = true, features = [\"test-support\"] }\n\n[features]\ntest-support = [\"ensemble-tasks/test-support\"]\n";
    assert_eq!(
        ensemble_dependencies(manifest),
        ["ensemble-core", "ensemble-config"]
    );
}
