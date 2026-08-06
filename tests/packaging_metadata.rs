//! Invariants of the packaging metadata.
//!
//! The crate version is duplicated across five files that ship to four different
//! distribution channels, and the MCP Registry silently accepts a `server.json`
//! right up until it rejects the publish. Both failure modes — a stale version
//! and an over-long description — surface only at release time, after the tag is
//! already cut. These assert them at `cargo test` time instead.

use serde_json::Value;
use std::path::{Path, PathBuf};

/// The registry's limit on `server.json`'s `description`, which it enforces with
/// a 422 at publish time and nowhere earlier.
const REGISTRY_DESCRIPTION_LIMIT: usize = 100;

fn root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

fn read_json(relative: &str) -> Value {
    let path: PathBuf = root().join(relative);
    let text = std::fs::read_to_string(&path)
        .unwrap_or_else(|e| panic!("cannot read {}: {e}", path.display()));
    serde_json::from_str(&text)
        .unwrap_or_else(|e| panic!("{} is not valid JSON: {e}", path.display()))
}

#[test]
fn every_packaging_file_carries_the_crate_version() {
    let version = env!("CARGO_PKG_VERSION");

    let checks: Vec<(&str, String)> = vec![
        (
            "server.json",
            read_json("server.json")["version"].as_str().unwrap().into(),
        ),
        (
            "packaging/mcpb/manifest.json",
            read_json("packaging/mcpb/manifest.json")["version"]
                .as_str()
                .unwrap()
                .into(),
        ),
        (
            "plugins/appstore-mcp/.claude-plugin/plugin.json",
            read_json("plugins/appstore-mcp/.claude-plugin/plugin.json")["version"]
                .as_str()
                .unwrap()
                .into(),
        ),
        (
            ".claude-plugin/marketplace.json",
            read_json(".claude-plugin/marketplace.json")["plugins"][0]["version"]
                .as_str()
                .unwrap()
                .into(),
        ),
    ];

    for (file, found) in checks {
        assert_eq!(
            found, version,
            "{file} says {found}, Cargo.toml says {version} — bump them together"
        );
    }
}

#[test]
fn the_registry_description_is_within_the_length_it_will_accept() {
    let server = read_json("server.json");
    let description = server["description"].as_str().expect("description");
    assert!(
        description.len() <= REGISTRY_DESCRIPTION_LIMIT,
        "server.json description is {} chars; the MCP Registry rejects anything over {} \
         with a 422 at publish time: {description:?}",
        description.len(),
        REGISTRY_DESCRIPTION_LIMIT
    );
    assert!(!description.trim().is_empty());
}

#[test]
fn the_release_asset_url_points_at_this_version() {
    // The publish workflow rewrites this from the tag, but a stale value in the
    // committed file means the repo disagrees with what was actually published.
    let version = env!("CARGO_PKG_VERSION");
    let server = read_json("server.json");
    let identifier = server["packages"][0]["identifier"].as_str().expect("identifier");
    assert!(
        identifier.contains(&format!("/v{version}/")),
        "server.json package identifier does not reference v{version}: {identifier}"
    );
}

#[test]
fn the_mcpb_manifest_declares_every_platform_binary_the_release_builds() {
    let manifest = read_json("packaging/mcpb/manifest.json");
    let config = &manifest["server"]["mcp_config"];
    assert_eq!(config["command"], "server/appstore-mcp-macos");
    assert_eq!(
        config["platform_overrides"]["win32"]["command"],
        "server/appstore-mcp-windows.exe"
    );
    assert_eq!(
        config["platform_overrides"]["linux"]["command"],
        "server/appstore-mcp-linux"
    );
}

#[test]
fn the_changelog_has_an_entry_for_this_version() {
    let version = env!("CARGO_PKG_VERSION");
    let path: &Path = &root().join("CHANGELOG.md");
    let changelog = std::fs::read_to_string(path).expect("CHANGELOG.md");
    assert!(
        changelog.contains(&format!("## [{version}]")),
        "CHANGELOG.md has no `## [{version}]` section"
    );
}
