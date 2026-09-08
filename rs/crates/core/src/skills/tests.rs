//! Materializer tests. Every test builds its own module tree, project root and
//! home under a scratch directory; nothing reads the real machine.

use std::collections::BTreeSet;
use std::fs;
use std::path::{Path, PathBuf};

use avada_module_sdk::caps::Capability;
use avada_module_sdk::descriptor::{ModuleSummary, RouteDescriptor, SchemaDocument, Verb};
use avada_module_sdk::manifest::{ModuleId, SkillsSection};

use super::*;

fn scratch(tag: &str) -> PathBuf {
    let p = std::env::temp_dir().join(format!("avada-skills-{tag}-{}", uuid::Uuid::new_v4()));
    fs::create_dir_all(&p).unwrap();
    p
}

fn write(p: &Path, s: &str) {
    fs::create_dir_all(p.parent().unwrap()).unwrap();
    fs::write(p, s).unwrap();
}

fn read(p: &Path) -> String {
    fs::read_to_string(p).unwrap_or_else(|e| panic!("{}: {e}", p.display()))
}

fn seg(base: &Path, s: &str) -> PathBuf {
    s.split('/').fold(base.to_path_buf(), |p, x| p.join(x))
}

/// A module rooted at `dir` with `[skills] paths = ["skills"]`, accepted and enabled.
fn module(dir: &Path, id: &str) -> ModuleInput {
    ModuleInput {
        id: ModuleId::new(id).unwrap(),
        name: id.rsplit('/').next().unwrap().to_string(),
        version: "1.2.3".into(),
        version_dir: dir.to_path_buf(),
        skills: SkillsSection {
            paths: vec!["skills".into()],
        },
        accepted: BTreeSet::from([Capability::SkillsMaterialize]),
        enabled: true,
    }
}

/// Write `skills/[<tool>/]<name>/SKILL.md` under a module dir.
fn unit(dir: &Path, tool: Option<&str>, name: &str, extra_fm: &str, body: &str) -> PathBuf {
    let mut p = dir.join("skills");
    if let Some(t) = tool {
        p = p.join(t);
    }
    let p = p.join(name);
    write(
        &p.join("SKILL.md"),
        &format!("---\nname: {name}\ndescription: \"About {name}\"\n{extra_fm}---\n{body}"),
    );
    p
}

const RULE: &str = "kind: rule\nactivation: always\n";
const SKILL: &str = "";

struct Fx {
    tmp: PathBuf,
    module_dir: PathBuf,
    root: PathBuf,
    home: PathBuf,
    modules: Vec<ModuleInput>,
}

fn fixture(tag: &str) -> Fx {
    let tmp = scratch(tag);
    let module_dir = tmp.join("mod");
    let root = tmp.join("proj");
    let home = tmp.join("home");
    fs::create_dir_all(&root).unwrap();
    fs::create_dir_all(&home).unwrap();
    let modules = vec![module(&module_dir, "acme/tools")];
    Fx {
        tmp,
        module_dir,
        root,
        home,
        modules,
    }
}

impl Fx {
    fn plan(&self, tools: &Tools, home: bool, schema: Option<&SchemaDocument>) -> Plan {
        let roots = [self.root.clone()];
        Materializer::new().plan(&Request {
            modules: &self.modules,
            roots: &roots,
            home: home.then_some(self.home.as_path()),
            tools,
            schema,
            workspace: Some("ws"),
        })
    }

    fn run(&self, tools: &Tools, home: bool, schema: Option<&SchemaDocument>) -> Plan {
        let plan = self.plan(tools, home, schema);
        let applied = Materializer::new().apply(&plan);
        assert!(applied.ok(), "{:?}", applied.failed);
        plan
    }

    fn done(self) {
        let _ = fs::remove_dir_all(&self.tmp);
    }
}

fn claude() -> Tools {
    Tools::only(["claude-code"])
}

fn schema() -> SchemaDocument {
    let route = |method: &str, path: &str, verb: Verb, summary: &str, module: Option<&str>| {
        RouteDescriptor {
            method: method.into(),
            path: path.into(),
            verb,
            capability: None,
            summary: summary.into(),
            params: vec![],
            scope: Default::default(),
            module: module.map(|m| ModuleId::new(m).unwrap()),
            response: None,
        }
    };
    SchemaDocument {
        contract_version: 1,
        product: "Avada Terminal".into(),
        host_version: "9.9.9".into(),
        routes: vec![
            route(
                "panes.output",
                "/panes/{id}/output",
                Verb::Get,
                "Read a pane",
                None,
            ),
            route(
                "acme.tools.zap",
                "/m/acme/tools/zap",
                Verb::Post,
                "Zap it",
                Some("acme/tools"),
            ),
        ],
        rpcs: vec![],
        modules: vec![
            ModuleSummary {
                id: ModuleId::new("acme/tools").unwrap(),
                name: "Tools".into(),
                version: "1.2.3".into(),
                running: true,
            },
            ModuleSummary {
                id: ModuleId::new("acme/other").unwrap(),
                name: "Other".into(),
                version: "0.1.0".into(),
                running: false,
            },
        ],
    }
}

// ---- shared layer + Claude Code -------------------------------------------------

#[test]
fn plan_then_apply_is_idempotent_and_sorted() {
    let fx = fixture("idem");
    unit(
        &fx.module_dir,
        None,
        "conventions",
        RULE,
        "Always run fmt.\n",
    );
    let d = unit(
        &fx.module_dir,
        None,
        "deploy",
        SKILL,
        "# Deploy\n\nRun the script.\n",
    );
    write(&d.join("scripts").join("go.sh"), "#!/bin/sh\necho go\n");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(
            d.join("scripts").join("go.sh"),
            fs::Permissions::from_mode(0o755),
        )
        .unwrap();
    }
    write(&d.join("references").join("api.md"), "# API\n");

    let tools = claude().with("aider");
    let plan = fx.run(&tools, true, None);
    assert!(plan.errors.is_empty(), "{:?}", plan.errors);
    let paths = plan.written_paths();
    let mut sorted = paths.clone();
    sorted.sort();
    assert_eq!(paths, sorted, "plan is sorted");
    let expect: Vec<PathBuf> = [
        "proj/.agents/skills/acme-tools-deploy/SKILL.md",
        "proj/.agents/skills/acme-tools-deploy/references/api.md",
        "proj/.agents/skills/acme-tools-deploy/scripts/go.sh",
        "proj/.claude/skills/acme-tools-deploy/SKILL.md",
        "proj/.claude/skills/acme-tools-deploy/references/api.md",
        "proj/.claude/skills/acme-tools-deploy/scripts/go.sh",
        "proj/AGENTS.md",
        "proj/CLAUDE.md",
        "proj/CONVENTIONS.md",
        "home/.claude/CLAUDE.md",
        "home/.claude/skills/acme-tools-deploy/SKILL.md",
        "home/.claude/skills/acme-tools-deploy/references/api.md",
        "home/.claude/skills/acme-tools-deploy/scripts/go.sh",
    ]
    .iter()
    .map(|s| seg(&fx.tmp, s))
    .collect();
    let mut expect_sorted = expect.clone();
    expect_sorted.sort();
    assert_eq!(
        paths,
        expect_sorted
            .iter()
            .map(PathBuf::as_path)
            .collect::<Vec<_>>()
    );

    // Content.
    let agents = read(&fx.root.join("AGENTS.md"));
    assert_eq!(
        agents,
        "<!-- avada:module=acme/tools -->\nAlways run fmt.\n<!-- /avada:module=acme/tools -->\n"
    );
    let claude_md = read(&fx.root.join("CLAUDE.md"));
    assert_eq!(
        claude_md,
        "<!-- avada:module=avada/modules -->\n@AGENTS.md\n<!-- /avada:module=avada/modules -->\n"
    );
    let conventions = read(&fx.root.join("CONVENTIONS.md"));
    assert!(conventions.contains("Always run fmt."));
    let skill = read(&seg(&fx.root, ".agents/skills/acme-tools-deploy/SKILL.md"));
    assert!(skill.starts_with("---\nname: acme-tools-deploy\ndescription: \"About deploy\"\n---\n<!-- avada:module=acme/tools -->\n# Deploy"));
    assert!(skill.ends_with("<!-- /avada:module=acme/tools -->\n"));
    let home_md = read(&seg(&fx.home, ".claude/CLAUDE.md"));
    assert!(
        home_md.contains("Always run fmt."),
        "user scope inlines rules"
    );
    assert!(!fx.home.join(".claude").join("settings.json").exists());
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = fs::metadata(seg(
            &fx.root,
            ".agents/skills/acme-tools-deploy/scripts/go.sh",
        ))
        .unwrap()
        .permissions()
        .mode();
        assert!(mode & 0o111 != 0, "scripts are executable");
    }

    // Aider has no skills dir: the skill is skipped there, with a reason.
    assert!(plan.skipped.iter().any(|s| s.tool == "aider"
        && s.unit.name == "deploy"
        && s.reason.contains("no on-demand skills")));

    // Second run: nothing to do.
    let again = fx.plan(&tools, true, None);
    assert!(again.is_empty(), "{again:?}");
    fx.done();
}

#[test]
fn fence_splice_preserves_user_content_before_and_after() {
    let fx = fixture("fence");
    unit(&fx.module_dir, None, "conventions", RULE, "Rule v1.\n");
    let user_agents = "# My project\n\nHand-written intro.\n\n<!-- avada:module=acme/tools -->\nstale\n<!-- /avada:module=acme/tools -->\n\nHand-written outro.\n";
    write(&fx.root.join("AGENTS.md"), user_agents);
    let user_claude = "# Claude notes\nNo fence here at all";
    write(&fx.root.join("CLAUDE.md"), user_claude);

    fx.run(&claude(), false, None);
    let agents = read(&fx.root.join("AGENTS.md"));
    assert_eq!(
        agents,
        "# My project\n\nHand-written intro.\n\n<!-- avada:module=acme/tools -->\nRule v1.\n<!-- /avada:module=acme/tools -->\n\nHand-written outro.\n",
        "the fence is replaced in place; every other byte is preserved"
    );
    let claude_md = read(&fx.root.join("CLAUDE.md"));
    assert_eq!(
        claude_md,
        "# Claude notes\nNo fence here at all\n<!-- avada:module=avada/modules -->\n@AGENTS.md\n<!-- /avada:module=avada/modules -->\n"
    );

    // Regenerate with a changed rule: still only the fence moves.
    unit(&fx.module_dir, None, "conventions", RULE, "Rule v2.\n");
    fx.run(&claude(), false, None);
    let agents = read(&fx.root.join("AGENTS.md"));
    assert!(agents.starts_with(
        "# My project\n\nHand-written intro.\n\n<!-- avada:module=acme/tools -->\nRule v2.\n"
    ));
    assert!(agents.ends_with("\n\nHand-written outro.\n"));

    // Disable: fences go, user text comes back byte-identical (minus the stale block).
    let mut fx = fx;
    fx.modules[0].enabled = false;
    fx.run(&claude(), false, None);
    assert_eq!(
        read(&fx.root.join("AGENTS.md")),
        "# My project\n\nHand-written intro.\n\n\nHand-written outro.\n"
    );
    assert_eq!(
        read(&fx.root.join("CLAUDE.md")),
        "# Claude notes\nNo fence here at all\n"
    );
    fx.done();
}

#[test]
fn disable_removes_exactly_our_dirs_and_leaves_user_skills() {
    let fx = fixture("disable");
    unit(&fx.module_dir, None, "deploy", SKILL, "body\n");
    unit(&fx.module_dir, None, "conventions", RULE, "rule\n");
    let user_skill = seg(&fx.root, ".claude/skills/my-own/SKILL.md");
    write(&user_skill, "---\nname: my-own\n---\nmine\n");
    let lookalike = seg(&fx.root, ".claude/skills/acme-tools-other/SKILL.md");
    write(
        &lookalike,
        "---\nname: acme-tools-other\n---\nno fence, so not ours\n",
    );
    fx.run(&claude(), false, None);
    assert!(seg(&fx.root, ".claude/skills/acme-tools-deploy/SKILL.md").is_file());

    let mut fx = fx;
    fx.modules[0].enabled = false;
    let plan = fx.run(&claude(), false, None);
    let removed = plan.removed_paths();
    assert!(removed.contains(&seg(&fx.root, ".claude/skills/acme-tools-deploy").as_path()));
    assert!(removed.contains(&seg(&fx.root, ".agents/skills/acme-tools-deploy").as_path()));
    assert!(
        removed.contains(&fx.root.join("AGENTS.md").as_path()),
        "an emptied fenced file goes"
    );
    assert!(removed.contains(&fx.root.join("CLAUDE.md").as_path()));
    assert!(!seg(&fx.root, ".claude/skills/acme-tools-deploy").exists());
    assert_eq!(read(&user_skill), "---\nname: my-own\n---\nmine\n");
    assert!(
        lookalike.is_file(),
        "a dir without our fence is never ours to delete"
    );
    assert!(fx.plan(&claude(), false, None).is_empty());
    fx.done();
}

#[test]
fn a_user_dir_with_our_name_but_no_fence_is_left_alone() {
    let fx = fixture("foreign");
    unit(&fx.module_dir, None, "deploy", SKILL, "ours\n");
    let theirs = seg(&fx.root, ".agents/skills/acme-tools-deploy/SKILL.md");
    write(&theirs, "---\nname: acme-tools-deploy\n---\ntheirs\n");
    let plan = fx.run(&claude(), false, None);
    assert_eq!(read(&theirs), "---\nname: acme-tools-deploy\n---\ntheirs\n");
    assert!(plan.skipped.iter().any(|s| s.tool == SHARED
        && s.unit.name == "deploy"
        && s.reason.contains("not written by Avada")));
    // The Claude Code copy is still made: that directory was free.
    assert!(seg(&fx.root, ".claude/skills/acme-tools-deploy/SKILL.md").is_file());
    fx.done();
}

#[test]
fn override_wins_for_its_tool_only() {
    let fx = fixture("override");
    unit(&fx.module_dir, None, "deploy", SKILL, "generic\n");
    unit(
        &fx.module_dir,
        Some("claude-code"),
        "deploy",
        SKILL,
        "claude flavour\n",
    );
    unit(&fx.module_dir, None, "conventions", RULE, "generic rule\n");
    unit(
        &fx.module_dir,
        Some("claude-code"),
        "conventions",
        RULE,
        "claude rule\n",
    );
    let plan = fx.run(&claude().with("aider"), false, None);
    assert!(plan.errors.is_empty(), "{:?}", plan.errors);
    assert!(
        read(&seg(&fx.root, ".agents/skills/acme-tools-deploy/SKILL.md")).contains("generic\n")
    );
    assert!(
        read(&seg(&fx.root, ".claude/skills/acme-tools-deploy/SKILL.md"))
            .contains("claude flavour\n")
    );
    assert!(read(&fx.root.join("AGENTS.md")).contains("generic rule"));
    // Claude's own rule differs from the shared one, so it is inlined next to the import.
    let claude_md = read(&fx.root.join("CLAUDE.md"));
    assert!(claude_md.contains("@AGENTS.md"));
    assert!(claude_md.contains("<!-- avada:module=acme/tools -->\nclaude rule\n"));
    assert!(!claude_md.contains("generic rule"));
    assert!(read(&fx.root.join("CONVENTIONS.md")).contains("generic rule"));
    fx.done();
}

#[test]
fn tools_allow_list_is_honoured() {
    let fx = fixture("allow");
    unit(
        &fx.module_dir,
        None,
        "claude-only",
        "tools: [claude-code]\n",
        "c\n",
    );
    unit(
        &fx.module_dir,
        None,
        "shared-only",
        "tools: [agents]\n",
        "s\n",
    );
    unit(
        &fx.module_dir,
        None,
        "claude-rule",
        "kind: rule\nactivation: always\ntools: [claude-code]\n",
        "only claude\n",
    );
    let plan = fx.run(&claude().with("aider"), false, None);
    assert!(plan.errors.is_empty(), "{:?}", plan.errors);
    assert!(!seg(&fx.root, ".agents/skills/acme-tools-claude-only").exists());
    assert!(seg(&fx.root, ".claude/skills/acme-tools-claude-only/SKILL.md").is_file());
    assert!(seg(&fx.root, ".agents/skills/acme-tools-shared-only/SKILL.md").is_file());
    assert!(!seg(&fx.root, ".claude/skills/acme-tools-shared-only").exists());
    assert!(
        !fx.root.join("AGENTS.md").exists(),
        "no shared rule, no AGENTS.md"
    );
    let claude_md = read(&fx.root.join("CLAUDE.md"));
    assert!(claude_md.contains("only claude"));
    assert!(
        !claude_md.contains("@AGENTS.md"),
        "nothing shared to import"
    );
    assert!(!fx.root.join("CONVENTIONS.md").exists());
    fx.done();
}

#[test]
fn glob_and_manual_units_are_written_in_claude_codes_native_forms() {
    let fx = fixture("glob");
    unit(
        &fx.module_dir,
        None,
        "rs",
        "kind: rule\nactivation: glob\nglobs: [\"*.rs\", \"Cargo.toml\"]\n",
        "x\n",
    );
    unit(
        &fx.module_dir,
        None,
        "manual",
        "kind: rule\nactivation: manual\n",
        "m\n",
    );
    unit(&fx.module_dir, None, "ok", RULE, "fine\n");
    let plan = fx.run(&claude(), true, None);
    assert!(plan.errors.is_empty(), "{:?}", plan.errors);
    assert_eq!(
        written(&fx, &plan),
        paths([
            "home/.claude/CLAUDE.md",
            "home/.claude/rules/acme-tools-rs.md",
            "home/.claude/skills/acme-tools-manual/SKILL.md",
            "proj/.claude/rules/acme-tools-rs.md",
            "proj/.claude/skills/acme-tools-manual/SKILL.md",
            "proj/AGENTS.md",
            "proj/CLAUDE.md",
        ])
    );
    // The shared file carries the glob rule in prose, for tools without a glob form.
    assert_eq!(
        read(&fx.root.join("AGENTS.md")),
        "<!-- avada:module=acme/tools -->\nfine\n\nApplies only when working on files matching \
         `*.rs`, `Cargo.toml`:\n\nx\n<!-- /avada:module=acme/tools -->\n"
    );
    // Claude Code gets the native, path-scoped file at both scopes.
    let native = "---\npaths:\n  - \"*.rs\"\n  - \"Cargo.toml\"\n---\n\
                  <!-- avada:module=acme/tools -->\nx\n<!-- /avada:module=acme/tools -->\n";
    assert_eq!(
        read(&seg(&fx.root, ".claude/rules/acme-tools-rs.md")),
        native
    );
    assert_eq!(
        read(&seg(&fx.home, ".claude/rules/acme-tools-rs.md")),
        native
    );
    // A manual unit is a skill only the user can invoke.
    let manual = read(&seg(&fx.root, ".claude/skills/acme-tools-manual/SKILL.md"));
    assert!(manual.starts_with(
        "---\nname: acme-tools-manual\ndescription: \"About manual\"\n\
         disable-model-invocation: true\n---\n<!-- avada:module=acme/tools -->\nm\n"
    ));
    // The shared layer has no manual flag: skipped there, with a readable reason.
    let shared_manual = plan
        .skipped
        .iter()
        .find(|s| s.unit.name == "manual" && s.tool == SHARED)
        .expect("shared manual skip");
    assert!(shared_manual.reason.contains("manual"));
    assert!(!plan.skipped.iter().any(|s| s.tool == "claude-code"));
    assert!(fx.plan(&claude(), true, None).is_empty());
    fx.done();
}

/// Paths a plan writes, relative to the fixture's scratch dir, `/`-joined.
fn written(fx: &Fx, plan: &Plan) -> BTreeSet<String> {
    plan.written_paths()
        .iter()
        .map(|p| {
            p.strip_prefix(&fx.tmp)
                .unwrap()
                .components()
                .map(|c| c.as_os_str().to_string_lossy().into_owned())
                .collect::<Vec<_>>()
                .join("/")
        })
        .collect()
}

fn paths<const N: usize>(list: [&str; N]) -> BTreeSet<String> {
    list.iter().map(|s| s.to_string()).collect()
}

/// One unit per activation: an always-on rule, a glob rule, a manual rule and a
/// model-picked skill.
fn four_units(fx: &Fx) {
    unit(&fx.module_dir, None, "always", RULE, "Always run fmt.\n");
    unit(
        &fx.module_dir,
        None,
        "rs",
        "kind: rule\nactivation: glob\nglobs: [\"*.rs\"]\n",
        "Rust rule.\n",
    );
    unit(
        &fx.module_dir,
        None,
        "manual",
        "kind: rule\nactivation: manual\n",
        "Manual steps.\n",
    );
    unit(&fx.module_dir, None, "deploy", SKILL, "# Deploy\n");
}

/// The shared layer's files for [`four_units`], which every per-tool run also
/// writes, plus the tool's own.
fn expect(tool_paths: &[&str]) -> BTreeSet<String> {
    [
        "proj/.agents/skills/acme-tools-deploy/SKILL.md",
        "proj/AGENTS.md",
    ]
    .iter()
    .chain(tool_paths)
    .map(|s| s.to_string())
    .collect()
}

/// Uninstalling the module removes exactly what was written, at both scopes.
fn assert_uninstall(mut fx: Fx, tools: &Tools, paths: &BTreeSet<String>) {
    fx.modules.clear();
    let plan = fx.run(tools, true, None);
    for p in paths {
        assert!(!seg(&fx.tmp, p).exists(), "{p} should be gone");
    }
    assert!(plan.writes.is_empty(), "{:?}", plan.written_paths());
    assert!(fx.plan(tools, true, None).is_empty());
    fx.done();
}

#[test]
fn cline_gets_rule_files_and_workflows_at_both_scopes() {
    let fx = fixture("cline");
    four_units(&fx);
    let tools = Tools::only(["cline"]);
    let plan = fx.run(&tools, true, None);
    assert!(plan.errors.is_empty(), "{:?}", plan.errors);
    let paths = written(&fx, &plan);
    assert_eq!(
        paths,
        expect(&[
            "home/Documents/Cline/Rules/acme-tools-always.md",
            "home/Documents/Cline/Rules/acme-tools-rs.md",
            "home/Documents/Cline/Workflows/acme-tools-manual.md",
            "proj/.clinerules/acme-tools-always.md",
            "proj/.clinerules/acme-tools-rs.md",
            "proj/.clinerules/workflows/acme-tools-manual.md",
        ])
    );
    assert_eq!(
        read(&seg(&fx.root, ".clinerules/acme-tools-always.md")),
        "<!-- avada:module=acme/tools -->\nAlways run fmt.\n<!-- /avada:module=acme/tools -->\n"
    );
    assert_eq!(
        read(&seg(&fx.root, ".clinerules/acme-tools-rs.md")),
        "---\npaths:\n  - \"*.rs\"\n---\n<!-- avada:module=acme/tools -->\nRust rule.\n\
         <!-- /avada:module=acme/tools -->\n"
    );
    assert_eq!(
        read(&seg(
            &fx.home,
            "Documents/Cline/Workflows/acme-tools-manual.md"
        )),
        "<!-- avada:module=acme/tools -->\nManual steps.\n<!-- /avada:module=acme/tools -->\n"
    );
    // Cline has no on-demand skills.
    assert!(plan.skipped.iter().any(|s| {
        s.tool == "cline" && s.unit.name == "deploy" && s.reason.contains("no on-demand")
    }));
    assert!(fx.plan(&tools, true, None).is_empty());
    assert_uninstall(fx, &tools, &paths);
}

#[test]
fn kiro_gets_a_steering_file_per_unit_with_inclusion_front_matter() {
    let fx = fixture("kiro");
    four_units(&fx);
    let tools = Tools::only(["kiro"]);
    let plan = fx.run(&tools, true, None);
    assert!(plan.errors.is_empty(), "{:?}", plan.errors);
    let paths = written(&fx, &plan);
    assert_eq!(
        paths,
        expect(&[
            "home/.kiro/steering/acme-tools-always.md",
            "home/.kiro/steering/acme-tools-deploy.md",
            "home/.kiro/steering/acme-tools-manual.md",
            "home/.kiro/steering/acme-tools-rs.md",
            "proj/.kiro/steering/acme-tools-always.md",
            "proj/.kiro/steering/acme-tools-deploy.md",
            "proj/.kiro/steering/acme-tools-manual.md",
            "proj/.kiro/steering/acme-tools-rs.md",
        ])
    );
    let at = |n: &str| read(&seg(&fx.root, &format!(".kiro/steering/acme-tools-{n}.md")));
    assert!(
        at("always").starts_with("---\ninclusion: always\n---\n<!-- avada:module=acme/tools -->\n")
    );
    assert!(
        at("rs").starts_with("---\ninclusion: fileMatch\nfileMatchPattern:\n  - \"*.rs\"\n---\n")
    );
    assert!(at("manual").starts_with("---\ninclusion: manual\n---\n"));
    assert!(at("deploy").starts_with(
        "---\ninclusion: auto\nname: \"acme-tools-deploy\"\ndescription: \"About deploy\"\n---\n"
    ));
    assert!(!plan.skipped.iter().any(|s| s.tool == "kiro"));
    assert!(fx.plan(&tools, true, None).is_empty());
    assert_uninstall(fx, &tools, &paths);
}

#[test]
fn augment_gets_typed_rules_and_only_always_on_rules_at_user_scope() {
    let fx = fixture("augment");
    four_units(&fx);
    let tools = Tools::only(["augment"]);
    let plan = fx.run(&tools, true, None);
    assert!(plan.errors.is_empty(), "{:?}", plan.errors);
    let paths = written(&fx, &plan);
    assert_eq!(
        paths,
        expect(&[
            "home/.augment/rules/acme-tools-always.md",
            "proj/.augment/rules/acme-tools-always.md",
            "proj/.augment/rules/acme-tools-deploy.md",
            "proj/.augment/rules/acme-tools-manual.md",
            "proj/.augment/rules/acme-tools-rs.md",
        ])
    );
    let at = |n: &str| read(&seg(&fx.root, &format!(".augment/rules/acme-tools-{n}.md")));
    assert!(at("always").starts_with("---\ntype: always_apply\n---\n"));
    assert!(at("rs").starts_with(
        "---\ntype: agent_requested\ndescription: \"About rs Applies to files matching *.rs.\"\n---\n"
    ));
    assert!(at("manual").starts_with("---\ntype: manual\n---\n"));
    assert!(at("deploy")
        .starts_with("---\ntype: agent_requested\ndescription: \"About deploy\"\n---\n"));
    // User rules are always-on whatever the front matter says: the rest is skipped.
    let user_skips: BTreeSet<&str> = plan
        .skipped
        .iter()
        .filter(|s| s.tool == "augment")
        .map(|s| s.unit.name.as_str())
        .collect();
    assert_eq!(user_skips, BTreeSet::from(["deploy", "manual", "rs"]));
    assert!(plan
        .skipped
        .iter()
        .filter(|s| s.tool == "augment")
        .all(|s| s.reason.contains("always-on only")));
    assert!(fx.plan(&tools, true, None).is_empty());
    assert_uninstall(fx, &tools, &paths);
}

#[test]
fn continue_gets_rules_with_globs_and_always_apply() {
    let fx = fixture("continue");
    four_units(&fx);
    let tools = Tools::only(["continue"]);
    let plan = fx.run(&tools, true, None);
    assert!(plan.errors.is_empty(), "{:?}", plan.errors);
    let paths = written(&fx, &plan);
    assert_eq!(
        paths,
        expect(&[
            "home/.continue/rules/acme-tools-always.md",
            "home/.continue/rules/acme-tools-deploy.md",
            "home/.continue/rules/acme-tools-manual.md",
            "home/.continue/rules/acme-tools-rs.md",
            "proj/.continue/rules/acme-tools-always.md",
            "proj/.continue/rules/acme-tools-deploy.md",
            "proj/.continue/rules/acme-tools-manual.md",
            "proj/.continue/rules/acme-tools-rs.md",
        ])
    );
    let at = |n: &str| {
        read(&seg(
            &fx.home,
            &format!(".continue/rules/acme-tools-{n}.md"),
        ))
    };
    assert!(at("always").starts_with("---\nname: \"acme-tools-always\"\nalwaysApply: true\n---\n"));
    assert!(at("rs").starts_with("---\nname: \"acme-tools-rs\"\nglobs:\n  - \"*.rs\"\n---\n"));
    assert!(at("manual").starts_with(
        "---\nname: \"acme-tools-manual\"\nalwaysApply: false\n\
         description: \"Only when the user asks for it by name: About manual\"\n---\n"
    ));
    assert!(at("deploy").starts_with(
        "---\nname: \"acme-tools-deploy\"\nalwaysApply: false\ndescription: \"About deploy\"\n---\n"
    ));
    assert!(!plan.skipped.iter().any(|s| s.tool == "continue"));
    assert!(fx.plan(&tools, true, None).is_empty());
    assert_uninstall(fx, &tools, &paths);
}

#[test]
fn aider_puts_glob_rules_in_prose_and_skips_manual_units() {
    let fx = fixture("aider");
    four_units(&fx);
    let plan = fx.run(&Tools::only(["aider"]), false, None);
    assert!(plan.errors.is_empty(), "{:?}", plan.errors);
    assert_eq!(written(&fx, &plan), expect(&["proj/CONVENTIONS.md"]));
    let conv = read(&fx.root.join("CONVENTIONS.md"));
    assert!(conv.contains(
        "Always run fmt.\n\nApplies only when working on files matching `*.rs`:\n\nRust rule."
    ));
    let reasons: BTreeSet<(&str, &str)> = plan
        .skipped
        .iter()
        .filter(|s| s.tool == "aider")
        .map(|s| (s.unit.name.as_str(), s.reason.as_str()))
        .collect();
    assert_eq!(
        reasons,
        BTreeSet::from([
            (
                "deploy",
                "this tool has no on-demand skills; only rules are written"
            ),
            ("manual", "this tool has no manual form"),
        ])
    );
    fx.done();
}

#[test]
fn a_legacy_single_file_clinerules_is_used_inline() {
    let mut fx = fixture("clinefile");
    write(&fx.root.join(".clinerules"), "Team rules.\n");
    four_units(&fx);
    let tools = Tools::only(["cline"]);
    let plan = fx.run(&tools, false, None);
    assert_eq!(written(&fx, &plan), expect(&["proj/.clinerules"]));
    let text = read(&fx.root.join(".clinerules"));
    assert!(text.starts_with("Team rules.\n<!-- avada:module=acme/tools -->\nAlways run fmt.\n"));
    assert!(text.contains("files matching `*.rs`"));
    assert!(plan.skipped.iter().any(|s| {
        s.tool == "cline" && s.unit.name == "manual" && s.reason.contains("no manual form")
    }));
    assert!(fx.plan(&tools, false, None).is_empty());
    fx.modules.clear();
    fx.run(&tools, false, None);
    assert_eq!(read(&fx.root.join(".clinerules")), "Team rules.\n");
    fx.done();
}

#[test]
fn rule_files_sweep_ours_and_leave_foreign_files_alone() {
    let fx = fixture("sweep");
    unit(&fx.module_dir, None, "always", RULE, "x\n");
    // A foreign file at the path we want, a foreign file beside it, and a stale
    // file of ours from an earlier run.
    write(
        &seg(&fx.root, ".kiro/steering/acme-tools-always.md"),
        "mine\n",
    );
    write(&seg(&fx.root, ".kiro/steering/team.md"), "team\n");
    write(
        &seg(&fx.root, ".kiro/steering/acme-tools-old.md"),
        "<!-- avada:module=acme/tools -->\nold\n<!-- /avada:module=acme/tools -->\n",
    );
    let tools = Tools::only(["kiro"]);
    let plan = fx.run(&tools, false, None);
    assert_eq!(
        read(&seg(&fx.root, ".kiro/steering/acme-tools-always.md")),
        "mine\n"
    );
    assert_eq!(read(&seg(&fx.root, ".kiro/steering/team.md")), "team\n");
    assert!(!seg(&fx.root, ".kiro/steering/acme-tools-old.md").exists());
    assert!(plan
        .skipped
        .iter()
        .any(|s| s.tool == "kiro" && s.reason.contains("not written by Avada")));
    assert!(fx.plan(&tools, false, None).is_empty());
    fx.done();
}

#[test]
fn a_disabled_tool_is_swept_but_not_written() {
    let fx = fixture("disabled");
    unit(&fx.module_dir, None, "always", RULE, "x\n");
    let on = Tools::only(["kiro"]);
    fx.run(&on, true, None);
    assert!(seg(&fx.root, ".kiro/steering/acme-tools-always.md").is_file());
    assert!(seg(&fx.home, ".kiro/steering/acme-tools-always.md").is_file());
    let off = on.clone().disable("kiro");
    assert!(off.detected("kiro") && !off.has("kiro"));
    let plan = fx.run(&off, true, None);
    assert!(plan
        .writes
        .iter()
        .all(|w| !w.path.to_string_lossy().contains(".kiro")));
    assert!(!seg(&fx.root, ".kiro/steering/acme-tools-always.md").exists());
    assert!(!seg(&fx.home, ".kiro/steering/acme-tools-always.md").exists());
    assert!(fx.plan(&off, true, None).is_empty());
    // Re-enabling writes it back.
    fx.run(&off.enable("kiro"), true, None);
    assert!(seg(&fx.root, ".kiro/steering/acme-tools-always.md").is_file());
    fx.done();
}

#[test]
fn a_glob_unit_without_globs_is_an_error() {
    let fx = fixture("noglobs");
    unit(
        &fx.module_dir,
        None,
        "rs",
        "kind: rule\nactivation: glob\n",
        "x\n",
    );
    let plan = fx.plan(&claude(), false, None);
    assert_eq!(plan.errors.len(), 1);
    assert!(plan.errors[0].error.contains("globs"));
    assert!(plan.writes.is_empty());
    fx.done();
}

// ---- size caps ------------------------------------------------------------------

#[test]
fn truncate_at_paragraph_prefers_blank_lines_then_line_breaks() {
    assert_eq!(truncate_at_paragraph("short", 10), None);
    assert_eq!(
        truncate_at_paragraph("aaa\n\nbbb\n\nccc", 9).as_deref(),
        Some("aaa")
    );
    assert_eq!(
        truncate_at_paragraph("aaa\n\nbbb\n\nccc", 12).as_deref(),
        Some("aaa\n\nbbb")
    );
    assert_eq!(
        truncate_at_paragraph("aaa\nbbb\nccc", 6).as_deref(),
        Some("aaa")
    );
    assert_eq!(truncate_at_paragraph("abcdef", 3).as_deref(), Some("abc"));
    assert_eq!(truncate_at_paragraph("ééé", 3).as_deref(), Some("é"));
}

#[test]
fn an_over_cap_unit_is_cut_with_a_notice_and_reported() {
    let fx = fixture("cap");
    let paragraph = "lorem ipsum ".repeat(90);
    let body: String = (0..4400)
        .map(|i| format!("Paragraph {i}. {paragraph}\n\n"))
        .collect();
    let cap = adapter("claude-code").unwrap().user.unwrap().cap.unwrap();
    assert!(body.len() > cap.bytes);
    unit(&fx.module_dir, None, "big", RULE, &body);
    let plan = fx.run(&claude(), true, None);
    assert!(plan.errors.is_empty(), "{:?}", plan.errors);
    assert_eq!(plan.truncated.len(), 1);
    let t = &plan.truncated[0];
    assert_eq!(
        (t.tool.as_str(), t.unit.name.as_str(), t.cap),
        ("claude-code", "big", cap.bytes)
    );
    assert_eq!(t.bytes, body.trim().len());
    let user = read(&seg(&fx.home, ".claude/CLAUDE.md"));
    assert!(user.len() <= cap.bytes);
    assert!(user.contains("> Avada truncated this unit at a paragraph boundary"));
    assert!(user.contains("Paragraph 0. "));
    assert!(!user.contains("Paragraph 4399. "));
    assert!(user.ends_with("<!-- /avada:module=acme/tools -->\n"));
    // The shared file has no documented cap: the full text is there.
    let shared = read(&fx.root.join("AGENTS.md"));
    assert!(shared.contains("Paragraph 4399. "));
    assert!(!shared.contains("Avada truncated"));
    assert!(fx.plan(&claude(), true, None).is_empty());
    fx.done();
}

// ---- gates ---------------------------------------------------------------------

#[test]
fn nothing_is_written_without_the_accepted_capability() {
    let mut fx = fixture("gate");
    unit(&fx.module_dir, None, "deploy", SKILL, "x\n");
    unit(&fx.module_dir, None, "conventions", RULE, "x\n");
    fx.modules[0].accepted.clear();
    let plan = fx.run(&claude(), true, None);
    assert!(plan.is_empty(), "{plan:?}");
    assert!(
        plan.errors.is_empty() && plan.skipped.is_empty(),
        "not even inspected"
    );
    assert!(fs::read_dir(&fx.root).unwrap().next().is_none());
    assert!(fs::read_dir(&fx.home).unwrap().next().is_none());
    fx.done();
}

#[test]
fn disabled_module_keeps_user_scope_but_not_project_scope() {
    let mut fx = fixture("enabled");
    unit(&fx.module_dir, None, "deploy", SKILL, "x\n");
    fx.modules[0].enabled = false;
    fx.run(&claude(), true, None);
    assert!(fs::read_dir(&fx.root).unwrap().next().is_none());
    assert!(seg(&fx.home, ".claude/skills/acme-tools-deploy/SKILL.md").is_file());
    fx.done();
}

#[test]
fn two_roots_and_a_duplicate_get_one_plan_each() {
    let fx = fixture("roots");
    unit(&fx.module_dir, None, "conventions", RULE, "x\n");
    let other = fx.tmp.join("proj2");
    fs::create_dir_all(&other).unwrap();
    let roots = [fx.root.clone(), other.clone(), fx.root.clone()];
    let plan = Materializer::new().plan(&Request {
        modules: &fx.modules,
        roots: &roots,
        home: None,
        tools: &Tools::none(),
        schema: None,
        workspace: None,
    });
    assert_eq!(plan.writes.len(), 2);
    assert!(plan
        .written_paths()
        .contains(&other.join("AGENTS.md").as_path()));
    fx.done();
}

// ---- index skill ----------------------------------------------------------------

#[test]
fn index_skill_lists_modules_and_cli_verbs() {
    let fx = fixture("index");
    unit(&fx.module_dir, None, "conventions", RULE, "x\n");
    let s = schema();
    let plan = fx.run(&claude(), false, Some(&s));
    assert!(plan.errors.is_empty());
    let shared = read(&seg(&fx.root, ".agents/skills/avada-modules/SKILL.md"));
    let claude_copy = read(&seg(&fx.root, ".claude/skills/avada-modules/SKILL.md"));
    assert_eq!(shared, claude_copy);
    assert!(shared.starts_with("---\nname: avada-modules\ndescription: \""));
    assert!(shared.contains("<!-- avada:module=avada/modules -->"));
    assert!(shared.contains("Workspace: ws."));
    assert!(shared.contains("| Tools | `acme/tools` | 1.2.3 | yes | yes |"));
    assert!(shared.contains("| Other | `acme/other` | 0.1.0 | unknown | no |"));
    assert!(shared.contains("- `avada panes output` — Read a pane (GET `/panes/{id}/output`)"));
    assert!(shared
        .contains("- `avada acme tools zap` — Zap it (POST `/m/acme/tools/zap`) [acme/tools]"));
    assert!(fx.plan(&claude(), false, Some(&s)).is_empty());
    // Without a schema the index is ours and stale, so it goes.
    let plan = fx.run(&claude(), false, None);
    assert!(plan
        .removed_paths()
        .contains(&seg(&fx.root, ".agents/skills/avada-modules").as_path()));
    fx.done();
}

// ---- validation -----------------------------------------------------------------

#[test]
fn frontmatter_errors_are_reported_per_unit_not_fatal() {
    let fx = fixture("errors");
    unit(&fx.module_dir, None, "good", SKILL, "ok\n");
    write(
        &seg(&fx.module_dir, "skills/broken/SKILL.md"),
        "no frontmatter at all\n",
    );
    write(
        &seg(&fx.module_dir, "skills/badkind/SKILL.md"),
        "---\nname: badkind\nkind: nope\n---\n",
    );
    let plan = fx.run(&claude(), false, None);
    assert_eq!(plan.errors.len(), 2, "{:?}", plan.errors);
    assert!(plan
        .errors
        .iter()
        .all(|e| e.module.as_str() == "acme/tools"));
    assert!(plan
        .errors
        .iter()
        .any(|e| e.path.ends_with(seg(Path::new("broken"), "SKILL.md"))
            && e.error.contains("frontmatter")));
    assert!(plan
        .errors
        .iter()
        .any(|e| e.error.contains("`kind` cannot be `nope`")));
    assert!(seg(&fx.root, ".agents/skills/acme-tools-good/SKILL.md").is_file());
    fx.done();
}

#[test]
fn name_and_description_limits() {
    let fx = fixture("limits");
    let m = ModuleId::new("acme/tools").unwrap();
    // Name must match its directory and be kebab-case.
    write(
        &seg(&fx.module_dir, "skills/deploy/SKILL.md"),
        "---\nname: Deploy\n---\n",
    );
    write(
        &seg(&fx.module_dir, "skills/other/SKILL.md"),
        "---\nname: deploy\ndescription: x\n---\n",
    );
    // Emitted name `acme-tools-<name>` must stay within 64.
    let long = "a".repeat(60);
    write(
        &seg(&fx.module_dir, &format!("skills/{long}/SKILL.md")),
        &format!("---\nname: {long}\ndescription: x\n---\n"),
    );
    // Description within 1024.
    let desc = "d".repeat(1025);
    write(
        &seg(&fx.module_dir, "skills/wordy/SKILL.md"),
        &format!("---\nname: wordy\ndescription: {desc}\n---\n"),
    );
    // A skill needs a description; a rule does not.
    write(
        &seg(&fx.module_dir, "skills/mute/SKILL.md"),
        "---\nname: mute\n---\nbody\n",
    );
    write(
        &seg(&fx.module_dir, "skills/quiet/SKILL.md"),
        "---\nname: quiet\nkind: rule\nactivation: always\n---\nbody\n",
    );
    // Unknown tool id.
    write(
        &seg(&fx.module_dir, "skills/where/SKILL.md"),
        "---\nname: where\ndescription: x\ntools: [vim]\n---\n",
    );
    let plan = fx.plan(&Tools::none(), false, None);
    let errs: Vec<String> = plan.errors.iter().map(|e| e.error.clone()).collect();
    assert_eq!(errs.len(), 6, "{errs:?}");
    assert!(errs.iter().any(|e| e.contains("not kebab-case")));
    assert!(errs
        .iter()
        .any(|e| e.contains("does not match its directory")));
    assert!(errs.iter().any(|e| e.contains("the limit is 64")));
    assert!(errs.iter().any(|e| e.contains("the limit is 1024")));
    assert!(errs.iter().any(|e| e.contains("needs a description")));
    assert!(errs.iter().any(|e| e.contains("`vim` is not a known tool")));
    assert!(plan
        .written_paths()
        .contains(&fx.root.join("AGENTS.md").as_path()));
    assert_eq!(emitted_name(&m, "deploy"), "acme-tools-deploy");
    assert_eq!(
        emitted_name(&ModuleId::new("Acme_Co/My.Repo").unwrap(), "x"),
        "acme-co-my-repo-x"
    );
    assert!(is_kebab("a-b1") && !is_kebab("a--b") && !is_kebab("-a") && !is_kebab("A"));
    fx.done();
}

#[test]
fn skills_paths_must_stay_inside_the_module() {
    let mut fx = fixture("paths");
    fx.modules[0].skills.paths = vec!["../elsewhere".into(), "missing".into()];
    let plan = fx.plan(&Tools::none(), false, None);
    assert_eq!(plan.errors.len(), 2);
    assert!(plan
        .errors
        .iter()
        .any(|e| e.error.contains("inside the module")));
    assert!(plan
        .errors
        .iter()
        .any(|e| e.error.contains("does not exist")));
    fx.done();
}

// ---- adapters + detection --------------------------------------------------------

#[test]
fn detection_is_data_driven_and_never_probes_the_real_machine() {
    let tmp = scratch("detect");
    let home = tmp.join("home");
    let bin = tmp.join("bin");
    fs::create_dir_all(home.join(".aider")).unwrap();
    fs::create_dir_all(seg(&home, "Documents/Cline")).unwrap();
    write(&bin.join("kiro"), "#!/bin/sh\n");
    write(&bin.join("cn.exe"), "MZ");
    let t = Tools::detect(&home, std::slice::from_ref(&bin));
    assert_eq!(
        t.ids().collect::<Vec<_>>(),
        vec!["aider", "cline", "continue", "kiro"]
    );
    assert!(Tools::detect(&tmp.join("nowhere"), &[])
        .ids()
        .next()
        .is_none());
    assert!(t
        .clone()
        .without("aider")
        .ids()
        .eq(["cline", "continue", "kiro"]));
    // Every row is probed: a home path or a binary each flips one id.
    for row in ADAPTERS {
        let h = tmp.join(format!("home-{}", row.id));
        write(&rel(&h, row.detect.home_paths[0]).join(".keep"), "");
        assert!(Tools::detect(&h, &[]).ids().eq([row.id]), "{}", row.id);
        let b = tmp.join(format!("bin-{}", row.id));
        write(&b.join(row.detect.binaries[0]), "");
        assert!(
            Tools::detect(&tmp.join("nowhere"), &[b]).ids().eq([row.id]),
            "{}",
            row.id
        );
    }
    let _ = fs::remove_dir_all(&tmp);
}

#[test]
fn a_disabled_tool_stays_detected_but_is_not_written_for() {
    let tmp = scratch("toggle");
    let home = tmp.join("home");
    fs::create_dir_all(home.join(".kiro")).unwrap();
    fs::create_dir_all(home.join(".augment")).unwrap();
    let t = Tools::detect_with(&home, &[], ["kiro", "cline"]);
    assert!(t.ids().eq(["augment"]));
    assert!(t.detected_ids().eq(["augment", "kiro"]));
    assert!(t.disabled_ids().eq(["cline", "kiro"]));
    assert!(t.detected("kiro") && !t.has("kiro"));
    assert!(!t.detected("cline") && !t.has("cline"));
    assert!(t.clone().enable("kiro").has("kiro"));
    assert!(!Tools::only(["kiro"]).disable("kiro").has("kiro"));
    assert!(Tools::only(["kiro"]).disable("kiro").ids().next().is_none());
    let _ = fs::remove_dir_all(&tmp);
}

#[test]
fn adapter_table_is_well_formed() {
    let ids: BTreeSet<&str> = ADAPTERS.iter().map(|a| a.id).collect();
    assert_eq!(ids.len(), ADAPTERS.len(), "unique ids");
    assert!(!ids.contains(SHARED));
    for a in ADAPTERS {
        assert!(is_kebab(a.id), "{}", a.id);
        assert_eq!(a.status, AdapterStatus::Implemented, "{}", a.id);
        assert!(
            a.project.is_some() || a.user.is_some(),
            "{} has nowhere to write",
            a.id
        );
        assert!(
            !a.detect.home_paths.is_empty() && !a.detect.binaries.is_empty(),
            "{} is undetectable",
            a.id
        );
        for l in [a.project, a.user].into_iter().flatten() {
            assert!(
                l.skills_dir.is_some() || l.rules_file.is_some() || l.rules_dir.is_some(),
                "{} has an empty layout",
                a.id
            );
            for segs in l
                .paths()
                .into_iter()
                .chain(a.detect.home_paths.iter().copied())
            {
                assert!(!segs.is_empty());
                assert!(
                    segs.iter().all(|s| !s.contains('/') && !s.contains('\\')),
                    "segments, not paths"
                );
            }
            if let Some(cap) = l.cap {
                assert!(cap.bytes > CAP_RESERVE && !cap.source.is_empty());
            }
            assert!(!l.manual_skills || l.skills_dir.is_some());
        }
    }
    assert_eq!(
        adapter("cline").unwrap().project.unwrap().manual_dir,
        Some(&[".clinerules", "workflows"][..])
    );
    assert!(
        adapter("augment")
            .unwrap()
            .user
            .unwrap()
            .rules_dir
            .unwrap()
            .always_only
    );
    assert!(known_tool_ids().contains(&"agents"));
}

#[test]
fn written_paths_are_built_from_segments() {
    let fx = fixture("cfg");
    unit(&fx.module_dir, None, "deploy", SKILL, "x\n");
    let plan = fx.plan(&claude(), true, None);
    let root_n = fx.root.components().count();
    for p in plan.written_paths() {
        let base = if p.starts_with(&fx.root) {
            &fx.root
        } else {
            &fx.home
        };
        assert!(p.starts_with(base), "{}", p.display());
        assert!(p.components().count() > root_n);
        let tail = p.strip_prefix(base).unwrap();
        for c in tail.components() {
            let s = c.as_os_str().to_string_lossy();
            assert!(!s.contains('/') && !s.contains('\\'), "{}", p.display());
        }
    }
    fx.done();
}

#[test]
fn render_skill_escapes_the_description() {
    let s = render_skill("a/b", "a-b-x", "say \"hi\"\nnow", "body\n\n");
    assert_eq!(
        s,
        "---\nname: a-b-x\ndescription: \"say \\\"hi\\\" now\"\n---\n<!-- avada:module=a/b -->\nbody\n<!-- /avada:module=a/b -->\n"
    );
    assert!(is_ours(&s));
    assert!(!is_ours("---\nname: x\n---\nplain"));
}

#[test]
fn apply_reports_failures_and_continues() {
    let tmp = scratch("apply");
    let blocker = tmp.join("file");
    fs::write(&blocker, "x").unwrap();
    let plan = Plan {
        writes: vec![
            FileWrite {
                path: blocker.join("child"),
                bytes: b"no".to_vec(),
                executable: false,
            },
            FileWrite {
                path: tmp.join("ok"),
                bytes: b"yes".to_vec(),
                executable: false,
            },
        ],
        removals: vec![Removal {
            path: tmp.join("never-existed"),
            dir: false,
        }],
        ..Plan::default()
    };
    let out = apply(&plan);
    assert_eq!(out.failed.len(), 1);
    assert_eq!(out.written, vec![tmp.join("ok")]);
    assert_eq!(out.removed.len(), 1, "a missing path counts as removed");
    let _ = fs::remove_dir_all(&tmp);
}
