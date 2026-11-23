//! Embed the app's files, those of `ENSEMBLE_UI_DIST` when it names a folder
//! (the Nix package builds the app and names it), so the one executable
//! serves them. Without it `ensemble-ui` is built with none, and serves the
//! folder `--dist` names.

use std::path::{Path, PathBuf};

fn main() {
    println!("cargo:rerun-if-env-changed=ENSEMBLE_UI_DIST");
    let mut files = Vec::new();
    if let Some(dist) = std::env::var_os("ENSEMBLE_UI_DIST").filter(|d| !d.is_empty()) {
        let dist = PathBuf::from(dist);
        println!("cargo:rerun-if-changed={}", dist.display());
        walk(&dist, &dist, &mut files);
        files.sort();
    }
    let entries: String = files
        .iter()
        .map(|(path, file)| format!("    ({path:?}, include_bytes!({file:?})),\n"))
        .collect();
    let out = PathBuf::from(std::env::var_os("OUT_DIR").expect("OUT_DIR")).join("files.rs");
    std::fs::write(
        out,
        format!("/// The app's files, each by its path.\npub static FILES: &[(&str, &[u8])] = &[\n{entries}];\n"),
    )
    .expect("write files.rs");
}

/// Every file under `dir`, by its path under `root`, with `/` between names.
fn walk(root: &Path, dir: &Path, files: &mut Vec<(String, String)>) {
    let entries = std::fs::read_dir(dir)
        .unwrap_or_else(|e| panic!("ENSEMBLE_UI_DIST {}: {e}", dir.display()));
    for entry in entries {
        let path = entry.expect("a folder entry").path();
        println!("cargo:rerun-if-changed={}", path.display());
        if path.is_dir() {
            walk(root, &path, files);
        } else {
            let relative = path.strip_prefix(root).expect("under the root");
            let name: Vec<String> = relative
                .components()
                .map(|c| c.as_os_str().to_string_lossy().into_owned())
                .collect();
            files.push((name.join("/"), path.display().to_string()));
        }
    }
}
