//! Structural checks that keep README.md and skills/README.md from drifting
//! away from the binary and the repository layout.
//!
//! - `readme_version_block_matches_crate_version`: the README's console
//!   block starting with the line "$ npu --version" must show exactly
//!   "npu `CARGO_PKG_VERSION`" on the next line -- copy-pasted output, never
//!   reconstructed by hand (see CLAUDE.md: "quote the binary verbatim").
//! - `readme_prose_has_no_stray_version_number`: the README must not contain
//!   a "Version X.Y.Z" sentence outside of quoted binary output; version
//!   numbers rot the moment they are typed in prose.
//! - `skills_readme_table_links_match_skill_directories`: the skill links in
//!   skills/README.md's table are exactly the set of subdirectories of
//!   skills/.
//! - `every_schema_key_is_documented`: every key a committed schema under
//!   schemas/ accepts is quoted somewhere under docs/.
#![allow(clippy::expect_used)] // allowed in tests (see Cargo.toml [lints.clippy]).

use std::collections::BTreeSet;
use std::path::PathBuf;

fn repo_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

#[test]
fn readme_version_block_matches_crate_version() {
    let readme =
        std::fs::read_to_string(repo_root().join("README.md")).expect("README.md must be readable");

    let version_line = readme
        .lines()
        .skip_while(|line| line.trim() != "$ npu --version")
        .nth(1);

    assert_eq!(
        version_line.map(str::trim),
        Some(format!("npu {}", env!("CARGO_PKG_VERSION")).as_str()),
        "the README's `$ npu --version` block must show the real, current output"
    );
}

#[test]
fn readme_prose_has_no_stray_version_number() {
    let readme =
        std::fs::read_to_string(repo_root().join("README.md")).expect("README.md must be readable");

    let bytes = readme.as_bytes();
    let needle = b"Version ";

    let found = bytes.windows(needle.len() + 3).any(|window| {
        &window[..needle.len()] == needle && looks_like_semver(&window[needle.len()..])
    });

    assert!(
        !found,
        "README.md must not contain a `Version X.Y.Z` sentence in prose"
    );
}

/// `d.d` immediately after "Version ", e.g. "0.1" in "Version 0.1.0" -- good
/// enough to catch the pattern this test guards against without a regex
/// crate (see CLAUDE.md: twelve dependencies, deliberately).
fn looks_like_semver(rest: &[u8]) -> bool {
    rest.len() >= 3 && rest[0].is_ascii_digit() && rest[1] == b'.' && rest[2].is_ascii_digit()
}

#[test]
fn skills_readme_table_links_match_skill_directories() {
    let skills_dir = repo_root().join("skills");
    let readme = std::fs::read_to_string(skills_dir.join("README.md"))
        .expect("skills/README.md must be readable");

    let linked: BTreeSet<String> = readme
        .match_indices("](npu-")
        .filter_map(|(idx, _)| {
            let rest = &readme[idx + 2..];
            let end = rest.find('/')?;
            Some(rest[..end].to_string())
        })
        .collect();

    let on_disk: BTreeSet<String> = std::fs::read_dir(&skills_dir)
        .expect("skills/ must be readable")
        .filter_map(Result::ok)
        .filter(|entry| entry.path().is_dir())
        .filter_map(|entry| entry.file_name().into_string().ok())
        .filter(|name| name.starts_with("npu-"))
        .collect();

    assert_eq!(
        linked, on_disk,
        "skills/README.md's table must link exactly the npu-* subdirectories of skills/"
    );
}

/// Every property name declared anywhere in a JSON Schema.
fn schema_keys(value: &serde_json::Value, found: &mut BTreeSet<String>) {
    match value {
        serde_json::Value::Object(map) => {
            if let Some(serde_json::Value::Object(properties)) = map.get("properties") {
                found.extend(properties.keys().cloned());
            }
            map.values().for_each(|child| schema_keys(child, found));
        }
        serde_json::Value::Array(items) => items.iter().for_each(|child| schema_keys(child, found)),
        _ => {}
    }
}

/// Every key the committed schemas accept is documented somewhere under
/// `docs/`, quoted as code: a key the binary reads but no page names is one
/// a user can only find by reading the source.
#[test]
fn every_schema_key_is_documented() {
    let docs: String = std::fs::read_dir(repo_root().join("docs"))
        .expect("docs/ must be readable")
        .filter_map(Result::ok)
        .map(|entry| std::fs::read_to_string(entry.path()).expect("a readable doc page"))
        .collect();

    for kind in ["backend", "model", "command", "test"] {
        let path = repo_root().join("schemas").join(format!("{kind}.json"));
        let schema: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&path).expect("a committed schema"))
                .expect("a JSON schema");
        let mut found = BTreeSet::new();
        schema_keys(&schema, &mut found);
        let undocumented: Vec<&String> = found
            .iter()
            .filter(|key| {
                !docs.contains(&format!("`{key}`"))
                    && !docs.contains(&format!("`{key} ="))
                    && !docs.contains(&format!("[{key}]"))
                    && !docs.contains(&format!("[{key}."))
                    && !docs.contains(&format!(".{key}]"))
                    && !docs.contains(&format!("\n{key} ="))
            })
            .collect();
        assert!(
            undocumented.is_empty(),
            "{}: keys missing from docs/: {undocumented:?}",
            path.display()
        );
    }
}
