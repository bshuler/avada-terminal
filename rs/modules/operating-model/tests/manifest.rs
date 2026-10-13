//! The embedded manifest parses and validates, and the three shipped units exist.

use avada_module_sdk::manifest::Manifest;

#[test]
fn manifest_parses_and_provides_the_shape() {
    let m = Manifest::parse(include_str!("../avada.toml")).expect("manifest parses");
    assert_eq!(m.module.id.to_string(), "bshuler/avada-operating-model");
    assert_eq!(m.provides.len(), 1);
    assert!(m.contributions.is_empty());
}

#[test]
fn the_three_units_ship() {
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("skills");
    for name in ["operating-model", "om-orchestrator", "om-builder"] {
        assert!(root.join(name).join("SKILL.md").is_file(), "{name}");
    }
    let rule = std::fs::read_to_string(root.join("operating-model/SKILL.md")).unwrap();
    assert!(rule.contains("kind: rule") && rule.contains("activation: always"));
    assert!(rule.contains("## NEEDS HUMAN") && !rule.contains("tools:"));
}
