//! End-to-end: the REAL shipped `rs/modules/operating-model/` (its `avada.toml` and
//! skills, read from the source tree) driven through the real [`Materializer`] into a
//! scratch project root. Covers `docs/operating-model-module.md` section 7.

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Path, PathBuf};

use avada_core::skills::{Materializer, ModuleInput, Plan, Request, Tools, HOST_ID};
use avada_module_sdk::caps::Capability;
use avada_module_sdk::manifest::{Manifest, ModuleId};

const ID: &str = "bshuler/avada-operating-model";
const SHAPE: &str = "avada.agent.operating_model";

fn shipped_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../modules/operating-model")
}

/// What `orchestrator::module_input` does with an install record, with the shipped
/// directory standing in for the installed version directory.
fn input_from(dir: &Path, enabled: bool) -> ModuleInput {
    let text = fs::read_to_string(dir.join("avada.toml")).expect("avada.toml");
    let m = Manifest::parse(&text).expect("shipped manifest parses");
    ModuleInput {
        id: m.module.id.clone(),
        name: m.module.name.clone(),
        version: m.module.version.to_string(),
        version_dir: dir.to_path_buf(),
        skills: m.skills.clone(),
        accepted: BTreeSet::from([Capability::SkillsMaterialize]),
        enabled,
        provides: m.provides.iter().map(|p| p.shape.clone()).collect(),
    }
}

struct Fx {
    tmp: PathBuf,
    root: PathBuf,
}

impl Fx {
    fn new(tag: &str) -> Fx {
        let tmp = std::env::temp_dir().join(format!("avada-opmodel-{tag}-{}", uuid::Uuid::new_v4()));
        let root = tmp.join("proj");
        fs::create_dir_all(&root).unwrap();
        Fx { tmp, root }
    }
    fn plan(&self, mods: &[ModuleInput], tools: &Tools, defaults: &BTreeMap<String, ModuleId>, retired: &[ModuleId]) -> Plan {
        let roots = [self.root.clone()];
        Materializer::new().plan(&Request {
            modules: mods,
            roots: &roots,
            home: None,
            tools,
            schema: None,
            workspace: None,
            defaults,
            retired,
        })
    }
    fn run(&self, mods: &[ModuleInput], tools: &Tools, defaults: &BTreeMap<String, ModuleId>) -> Plan {
        let p = self.plan(mods, tools, defaults, &[]);
        let a = Materializer::new().apply(&p);
        assert!(a.ok(), "{:?}", a.failed);
        p
    }
    fn read(&self, f: &str) -> String {
        fs::read_to_string(self.root.join(f)).unwrap_or_else(|e| panic!("{f}: {e}"))
    }
}

impl Drop for Fx {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.tmp);
    }
}

fn tools() -> Tools {
    Tools::only(["claude-code", "aider", "gemini"])
}

fn none() -> BTreeMap<String, ModuleId> {
    BTreeMap::new()
}

fn fence_of(id: &str, body: &str) -> String {
    format!("<!-- avada:module={id} -->\n{body}\n<!-- /avada:module={id} -->\n")
}

/// The section 5.1 text, read from the shipped rule (its body after the front matter).
fn rule_body() -> String {
    let t = fs::read_to_string(shipped_dir().join("skills/operating-model/SKILL.md")).unwrap();
    let rest = t.strip_prefix("---\n").unwrap();
    let (_, body) = rest.split_once("\n---\n").unwrap();
    body.trim_end_matches('\n').to_string()
}

#[test]
fn fence_lands_in_agents_md_with_the_section_5_1_text() {
    let fx = Fx::new("fence");
    let m = input_from(&shipped_dir(), true);
    assert_eq!(m.id.as_str(), ID);
    assert_eq!(m.provides, vec![SHAPE.to_string()]);
    let plan = fx.run(&[m], &tools(), &none());
    assert!(plan.errors.is_empty(), "{:?}", plan.errors);
    let agents = fx.read("AGENTS.md");
    assert_eq!(agents, fence_of(ID, &rule_body()));
    for needle in [
        "# Operating model",
        "Applies only when `AVADA_ROLE` is set",
        "## NEEDS HUMAN",
        "REPORT.md",
        "Do not deploy, publish, spend, or irreversibly delete",
        "Playbooks: your role's playbook skill",
    ] {
        assert!(agents.contains(needle), "missing {needle:?}: {agents}");
    }
}

#[test]
fn claude_md_gets_only_the_agents_import_and_other_tools_follow_their_form() {
    let fx = Fx::new("adapters");
    fx.run(&[input_from(&shipped_dir(), true)], &tools(), &none());
    assert_eq!(fx.read("CLAUDE.md"), fence_of(HOST_ID, "@AGENTS.md"));
    assert!(!fx.read("CLAUDE.md").contains("Operating model"));
    assert_eq!(fx.read("GEMINI.md"), fence_of(HOST_ID, "@AGENTS.md"));
    // CONVENTIONS.md has no import syntax: the rule is inlined.
    let conv = fx.read("CONVENTIONS.md");
    assert!(conv.contains(&rule_body()), "{conv}");
}

#[test]
fn user_text_outside_the_fence_is_byte_identical_and_second_plan_is_empty() {
    let fx = Fx::new("idem");
    let before = "# Mine\r\nkeep\ttabs  \n\nno trailing newline";
    let after_user = "\n\nUSER TAIL é\n";
    fs::write(fx.root.join("AGENTS.md"), before).unwrap();
    let m = input_from(&shipped_dir(), true);
    fx.run(std::slice::from_ref(&m), &tools(), &none());
    let t = fx.read("AGENTS.md");
    assert!(t.starts_with(before), "{t:?}");
    // User appends after the fence; a rerun must not touch it.
    fs::write(fx.root.join("AGENTS.md"), format!("{t}{after_user}")).unwrap();
    let again = fx.plan(std::slice::from_ref(&m), &tools(), &none(), &[]);
    assert!(again.is_empty(), "{:?} {:?}", again.writes, again.removals);
    assert!(fx.read("AGENTS.md").ends_with(after_user));
}

#[test]
fn version_bump_rewrites_only_the_fence() {
    let fx = Fx::new("bump");
    let v1 = input_from(&shipped_dir(), true);
    fs::write(fx.root.join("AGENTS.md"), "HEAD\n").unwrap();
    fx.run(std::slice::from_ref(&v1), &tools(), &none());
    let t = fx.read("AGENTS.md");
    fs::write(fx.root.join("AGENTS.md"), format!("{t}TAIL\n")).unwrap();

    // The upgrade ships changed rule text: a scratch copy of the shipped tree with one
    // line edited, under a new version directory.
    let v2dir = fx.tmp.join("v2");
    copy_dir(&shipped_dir(), &v2dir);
    let rule = v2dir.join("skills/operating-model/SKILL.md");
    let text = fs::read_to_string(&rule).unwrap();
    fs::write(&rule, text.replace("# Operating model", "# Operating model v2")).unwrap();
    let mut v2 = input_from(&v2dir, true);
    v2.version = "0.2.0".into();
    let plan = fx.run(&[v2], &tools(), &none());
    assert!(!plan.is_empty());
    let n = fx.read("AGENTS.md");
    assert!(n.starts_with("HEAD\n") && n.ends_with("TAIL\n"), "{n}");
    assert!(n.contains("# Operating model v2"), "{n}");
    assert_eq!(n.matches("avada:module=").count(), 2, "one fence: open and close");
}

fn copy_dir(from: &Path, to: &Path) {
    fs::create_dir_all(to).unwrap();
    for e in fs::read_dir(from).unwrap() {
        let e = e.unwrap();
        let name = e.file_name();
        if name == "target" {
            continue;
        }
        let dst = to.join(&name);
        if e.file_type().unwrap().is_dir() {
            copy_dir(&e.path(), &dst);
        } else {
            fs::copy(e.path(), dst).unwrap();
        }
    }
}

#[test]
fn disable_removes_only_the_fence_and_reenable_keeps_user_bytes() {
    let fx = Fx::new("disable");
    fs::write(fx.root.join("AGENTS.md"), "mine\n").unwrap();
    fs::write(fx.root.join("CLAUDE.md"), "claude mine\n").unwrap();
    fx.run(&[input_from(&shipped_dir(), true)], &tools(), &none());
    fx.run(&[input_from(&shipped_dir(), false)], &tools(), &none());
    assert_eq!(fx.read("AGENTS.md"), "mine\n");
    assert_eq!(fx.read("CLAUDE.md"), "claude mine\n");
    assert!(!fx.root.join("GEMINI.md").exists(), "fence-only file is deleted");
    fx.run(&[input_from(&shipped_dir(), true)], &tools(), &none());
    assert!(fx.read("AGENTS.md").starts_with("mine\n"));
    assert!(fx.read("CLAUDE.md").starts_with("claude mine\n"));
}

#[test]
fn uninstall_sweeps_the_fence_via_the_retired_list_and_a_foreign_fence_survives() {
    let fx = Fx::new("retire");
    let foreign = fence_of("bshuler/avada-orchestrator", "FOREIGN");
    fs::write(fx.root.join("AGENTS.md"), format!("mine\n{foreign}")).unwrap();
    fx.run(&[input_from(&shipped_dir(), true)], &tools(), &none());
    assert!(fx.read("AGENTS.md").contains("# Operating model"));
    let id = ModuleId::new(ID).unwrap();
    let plan = fx.plan(&[], &tools(), &none(), std::slice::from_ref(&id));
    Materializer::new().apply(&plan);
    let t = fx.read("AGENTS.md");
    assert!(!t.contains("Operating model") && !t.contains(ID), "{t}");
    assert!(t.starts_with("mine\n") && t.contains(&foreign), "{t}");
    // The host's own import fences are swept with it (only the ones this run owns).
    assert!(!fx.root.join("CLAUDE.md").exists(), "import-only file is deleted");
}

#[test]
fn unterminated_fence_leaves_the_file_untouched_and_records_an_error() {
    let fx = Fx::new("unterminated");
    let broken = format!("mine\n<!-- avada:module={ID} -->\nhalf\n");
    fs::write(fx.root.join("AGENTS.md"), &broken).unwrap();
    let plan = fx.run(&[input_from(&shipped_dir(), true)], &Tools::only(["claude-code"]), &none());
    assert_eq!(fx.read("AGENTS.md"), broken);
    assert!(plan.errors.iter().any(|e| e.error.contains("unterminated")), "{plan:?}");
}

#[test]
fn a_disabled_module_writes_nothing_at_user_scope_and_enabled_scope_is_project_only() {
    let fx = Fx::new("scope");
    let home = fx.tmp.join("home");
    fs::create_dir_all(&home).unwrap();
    let roots = [fx.root.clone()];
    let plan_for = |m: &[ModuleInput]| {
        Materializer::new().plan(&Request {
            modules: m,
            roots: &roots,
            home: Some(&home),
            tools: &tools(),
            schema: None,
            workspace: None,
            defaults: &none(),
            retired: &[],
        })
    };
    let off = plan_for(&[input_from(&shipped_dir(), false)]);
    assert!(off.writes.iter().all(|w| !w.path.starts_with(&home)), "{:?}", off.writes);
    let on = plan_for(&[input_from(&shipped_dir(), true)]);
    assert!(on.writes.iter().any(|w| w.path.starts_with(&fx.root)));
}

/// A second provider of the same exclusive shape; switching the default swaps the fence.
#[test]
fn swap_to_a_second_provider_of_the_shape_swaps_the_fence() {
    let fx = Fx::new("swap");
    let other = fx.tmp.join("other");
    fs::create_dir_all(other.join("skills/rule")).unwrap();
    fs::write(
        other.join("skills/rule/SKILL.md"),
        "---\nname: rule\ndescription: \"d\"\nkind: rule\nactivation: always\n---\nOTHER-TEXT\n",
    )
    .unwrap();
    let mut b = input_from(&shipped_dir(), true);
    b.id = ModuleId::new("acme/other-model").unwrap();
    b.version_dir = other;
    let a = input_from(&shipped_dir(), true);
    let mods = [a, b];
    let d = |id: &str| BTreeMap::from([(SHAPE.to_string(), ModuleId::new(id).unwrap())]);
    fs::write(fx.root.join("AGENTS.md"), "mine\n").unwrap();
    fx.run(&mods, &tools(), &d(ID));
    let t = fx.read("AGENTS.md");
    assert!(t.contains("# Operating model") && !t.contains("OTHER-TEXT"), "{t}");
    fx.run(&mods, &tools(), &d("acme/other-model"));
    let t = fx.read("AGENTS.md");
    assert!(t.contains("OTHER-TEXT") && !t.contains("# Operating model"), "{t}");
    assert!(t.starts_with("mine\n"));
    assert_eq!(t.matches("avada:module=").count(), 2, "{t}");
}

#[test]
fn the_agents_md_block_stays_small() {
    let fx = Fx::new("size");
    fx.run(&[input_from(&shipped_dir(), true)], &tools(), &none());
    let t = fx.read("AGENTS.md");
    assert!(t.len() < 2048, "AGENTS.md block is {} bytes; context cost grew", t.len());
    assert!(t.lines().count() <= 20, "{} lines", t.lines().count());
}
