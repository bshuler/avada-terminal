//! **The Wave 0 pane kinds on disk** (`docs/modules-fanout-plan.md`, track W0).
//!
//! `workspace_kind_compat.rs` pins the original kinds; this file pins the four that
//! Wave 0 reserved — `Data`, `Table`, `Image` and `Module(..)` — under the same
//! contract, and adds the one rule that is new with modules: a `module:` value this
//! build cannot parse is **kept verbatim**, never rewritten. A workspace saved by a
//! build that understands a richer reference syntax must survive a round trip
//! through this one.

use hyperpanes_core::tools::kind::{ModulePaneRef, PaneKind, Version, META_KIND_KEY};
use hyperpanes_core::workspace::io::{read_workspace, write_workspace};
use hyperpanes_core::workspace::model::{GroupSpec, PaneSpec, WindowSpec, WorkspaceFile};

fn temp_file(tag: &str) -> std::path::PathBuf {
    std::env::temp_dir().join(format!(
        "hp-module-kind-compat-{}-{tag}.json",
        std::process::id()
    ))
}

fn pane_with_kind(kind: PaneKind) -> PaneSpec {
    let mut p = PaneSpec::default();
    p.set_pane_kind(&kind);
    p
}

fn module_pane(pin: Option<&str>) -> PaneKind {
    PaneKind::Module(
        ModulePaneRef::new(
            "acme/avada-files",
            "tree",
            pin.map(|v| Version::parse(v).unwrap()),
        )
        .unwrap(),
    )
}

#[test]
fn the_wave_0_kinds_survive_a_disk_round_trip_at_every_nesting_level() {
    let path = temp_file("nesting");
    let ws = WorkspaceFile {
        name: Some("w0".into()),
        panes: Some(vec![
            pane_with_kind(PaneKind::Data),
            pane_with_kind(module_pane(Some("1.2.0"))),
        ]),
        groups: Some(vec![GroupSpec {
            title: Some("g".into()),
            panes: vec![
                pane_with_kind(PaneKind::Table),
                pane_with_kind(module_pane(None)),
            ],
            ..Default::default()
        }]),
        windows: Some(vec![WindowSpec {
            groups: vec![GroupSpec {
                title: Some("w".into()),
                panes: vec![pane_with_kind(PaneKind::Image)],
                ..Default::default()
            }],
            ..Default::default()
        }]),
        ..Default::default()
    };
    assert!(write_workspace(&path, &ws), "write must succeed");
    let back = read_workspace(&path).unwrap();

    let top = back.panes.as_ref().unwrap();
    assert_eq!(top[0].pane_kind(), PaneKind::Data);
    assert_eq!(top[1].pane_kind(), module_pane(Some("1.2.0")));
    let g = &back.groups.as_ref().unwrap()[0].panes;
    assert_eq!(g[0].pane_kind(), PaneKind::Table);
    assert_eq!(g[1].pane_kind(), module_pane(None));
    assert_eq!(
        back.windows.as_ref().unwrap()[0].groups[0].panes[0].pane_kind(),
        PaneKind::Image
    );
    let _ = std::fs::remove_file(&path);
}

#[test]
fn the_wave_0_kinds_write_the_documented_meta_values() {
    // The strings are the contract with every other build and with the module SDK's
    // docs; a rename here is a silent data-loss bug there.
    for (kind, want) in [
        (PaneKind::Data, "view:data"),
        (PaneKind::Table, "view:table"),
        (PaneKind::Image, "view:image"),
        (module_pane(None), "module:acme/avada-files#tree"),
        (
            module_pane(Some("1.2.0")),
            "module:acme/avada-files#tree@1.2.0",
        ),
    ] {
        let p = pane_with_kind(kind.clone());
        let json = serde_json::to_value(&p).unwrap();
        assert_eq!(json["meta"][META_KIND_KEY], want, "{kind:?}");
    }
}

#[test]
fn a_module_reference_this_build_cannot_parse_is_kept_verbatim() {
    // A future build may extend the reference grammar. Whatever it writes must come
    // back out of this build byte-for-byte, not as `terminal` (which drops the key).
    let raw = r##"{
  "panes": [
    { "meta": { "pane.kind": "module:acme/avada-files#tree@2.0.0-beta.1+build.7?layout=wide" } },
    { "meta": { "pane.kind": "module:acme/avada-files" } }
  ]
}"##;
    let path = temp_file("verbatim");
    std::fs::write(&path, raw).unwrap();
    let ws = read_workspace(&path).unwrap();
    let panes = ws.panes.as_ref().unwrap();
    for p in panes {
        let k = p.pane_kind();
        assert!(matches!(k, PaneKind::Tool(_)), "{k:?} must stay opaque");
        assert!(!k.is_pty() || k.tool().is_none());
    }
    // …and a save keeps both strings.
    let out = temp_file("verbatim-out");
    assert!(write_workspace(&out, &ws));
    let text = std::fs::read_to_string(&out).unwrap();
    assert!(text.contains("module:acme/avada-files#tree@2.0.0-beta.1+build.7?layout=wide"));
    assert!(text.contains("\"module:acme/avada-files\""));
    let _ = std::fs::remove_file(&path);
    let _ = std::fs::remove_file(&out);
}

#[test]
fn a_pane_without_the_key_is_still_a_terminal() {
    let raw = r##"{ "panes": [ { "command": "htop", "meta": { "role": "x" } } ] }"##;
    let path = temp_file("nokey");
    std::fs::write(&path, raw).unwrap();
    let ws = read_workspace(&path).unwrap();
    assert_eq!(ws.panes.unwrap()[0].pane_kind(), PaneKind::Terminal);
    let _ = std::fs::remove_file(&path);
}
