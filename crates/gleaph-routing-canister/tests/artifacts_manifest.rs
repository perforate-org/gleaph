//! The pinned Glev manifest must agree with the canister's runtime revision and wire version.

const MANIFEST: &str = include_str!("../artifacts.toml");

fn line_value<'a>(text: &'a str, key: &str) -> &'a str {
    let prefix = format!("{key} = \"");
    text.lines()
        .find_map(|line| line.trim().strip_prefix(&prefix))
        .and_then(|rest| rest.strip_suffix('"'))
        .unwrap_or_else(|| panic!("missing {key} in artifacts.toml"))
}

#[test]
fn manifest_pins_the_runtime_and_matches_the_canister() {
    let revision = line_value(MANIFEST, "runtime_revision");
    assert!(revision.len() == 40 && revision.chars().all(|ch| ch.is_ascii_hexdigit()));

    let cargo = include_str!("../Cargo.toml");
    let pinned = cargo
        .lines()
        .find(|line| line.contains("glev-general-router") && line.contains("rev = \""))
        .and_then(|line| line.split("rev = \"").nth(1))
        .and_then(|rest| rest.split('"').next())
        .expect("Cargo.toml pins glev-general-router by revision");
    assert_eq!(revision, pinned, "manifest revision must match Cargo.toml");

    let wire = MANIFEST
        .lines()
        .filter_map(|line| line.trim().strip_prefix("version = "))
        .filter_map(|rest| rest.parse::<u32>().ok())
        .next()
        .expect("wire version");
    assert_eq!(wire, gleaph_routing_wire::WIRE_VERSION);
}
