//! The pinned Glev artifact manifest must agree with the canister's runtime revision and wire
//! version; network verification lives in `fetch_glev_artifacts.py` and the routing-artifacts CI.

const MANIFEST: &str = include_str!("../artifacts.toml");

fn line_value<'a>(text: &'a str, key: &str) -> &'a str {
    let prefix = format!("{key} = \"");
    text.lines()
        .find_map(|line| line.trim().strip_prefix(&prefix))
        .and_then(|rest| rest.strip_suffix('"'))
        .unwrap_or_else(|| panic!("missing {key} in artifacts.toml"))
}

fn is_sha256(value: &str) -> bool {
    value.len() == 64 && value.chars().all(|ch| ch.is_ascii_hexdigit())
}

#[test]
fn manifest_pins_release_assets_and_matches_the_canister() {
    let revision = line_value(MANIFEST, "runtime_revision");
    assert!(revision.len() == 40 && revision.chars().all(|ch| ch.is_ascii_hexdigit()));

    let cargo = include_str!("../Cargo.toml");
    let pinned = cargo
        .lines()
        .find(|line| line.contains("glev-infer") && line.contains("rev = \""))
        .and_then(|line| line.split("rev = \"").nth(1))
        .and_then(|rest| rest.split('"').next())
        .expect("Cargo.toml pins glev-infer by revision");
    assert_eq!(revision, pinned, "manifest revision must match Cargo.toml");

    let pack = line_value(MANIFEST, "file");
    assert!(pack.starts_with("glev-") && pack.ends_with(".pack"));
    let tokenizer = MANIFEST
        .lines()
        .filter_map(|line| line.trim().strip_prefix("file = \""))
        .filter_map(|rest| rest.strip_suffix('"'))
        .find(|value| value.ends_with(".json"))
        .expect("tokenizer asset");
    assert!(tokenizer.starts_with("glev-") && tokenizer.ends_with(".json"));

    let hashes: Vec<&str> = MANIFEST
        .lines()
        .filter_map(|line| line.trim().strip_prefix("sha256 = \""))
        .filter_map(|rest| rest.strip_suffix('"'))
        .collect();
    assert_eq!(hashes.len(), 2, "pack and tokenizer must each pin a sha256");
    for hash in hashes {
        assert!(is_sha256(hash), "invalid sha256: {hash}");
    }

    let wire = MANIFEST
        .lines()
        .filter_map(|line| line.trim().strip_prefix("version = "))
        .filter_map(|rest| rest.parse::<u32>().ok())
        .next()
        .expect("wire version");
    assert_eq!(wire, gleaph_routing_wire::WIRE_VERSION);
}
