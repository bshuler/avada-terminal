//! The wiring tests for the marketplace pipeline: tracks G6, G7 and G10.
//!
//! These live beside `mod.rs` rather than inside it because they are proofs *about* the
//! pipeline rather than parts of it, and six hundred lines of them buried under the module
//! they test makes the module itself hard to read end to end.
//!
//! They keep their `use super::super::*` rather than naming what they need: each one drives
//! the pipeline through whatever seam its track added, and a private helper is as fair game
//! here as it was when these modules sat one level up.

// ---- track G6 resolver

#[cfg(test)]
mod resolver_tests {
    use super::super::testing::{files_state, manifest_for, rig, wait, FakeCargo, FILES, GIT};
    use super::super::*;
    use crate::install::resolver::Defaults;
    use std::time::Duration;

    /// The whole G6 seam through the real install pipeline: the plan picks a version
    /// (not simply the newest tag), a dependency install does not claim a default, a
    /// hand install does, a pin that breaks an enabled module is refused with the
    /// nearest working version offered, and uninstalling forgets the default.
    #[tokio::test]
    async fn the_plan_picks_versions_claims_defaults_and_guards_pins() {
        let r = rig(
            "g6",
            files_state(),
            FakeCargo::Builds,
            Duration::from_secs(300),
        )
        .await;
        for (tag, provided) in [("v1.0.0", "1.0.0"), ("v1.2.0", "1.2.0")] {
            // `repo` re-clones the bare mirror from the same working tree, so dropping
            // it leaves one repository carrying both tags.
            let _ = std::fs::remove_dir_all(r.fixtures.root.join(format!("{FILES}.git")));
            r.fixtures.repo(
                FILES,
                tag,
                &manifest_for(
                    FILES,
                    provided,
                    "kind = \"source\"",
                    &format!(
                        "[[provides]]\nshape = \"avada.files.tree\"\nversion = \"{provided}\"\n"
                    ),
                ),
            );
        }
        // The newest tag (v1.2.0) does NOT satisfy this: only resolution finds v1.0.0.
        r.fixtures.repo(
            GIT,
            "v1.0.0",
            &manifest_for(
                GIT,
                "1.0.0",
                "kind = \"source\"",
                "[[requires]]\nshape = \"avada.files.tree\"\nversion = \">=1, <1.2\"\n\
                 provider = \"acme/avada-files\"\n",
            ),
        );

        let mut req = InstallRequest::new(GIT);
        req.workspace = Some("g6ws".into());
        let job = r.mp.install(req).unwrap();
        let done = wait(&r.mp, &job.id).await;
        assert_eq!(done.phase, Phase::Done, "{done:?}");
        let list = r.mp.installed().unwrap();
        assert_eq!(list.len(), 2, "{list:?}");
        let files = list
            .iter()
            .find(|i| i.module.as_deref() == Some(FILES))
            .unwrap();
        assert_eq!(
            files.tag.as_deref(),
            Some("v1.0.0"),
            "the resolver kept the version the requirement allows, not the newest tag"
        );
        assert_eq!(files.kind, Some(InstallKind::Dependency));

        // A dependency install never claims a shape.
        let paths = r.mp.store().paths();
        assert_eq!(Defaults::load(paths).unwrap().get("avada.files.tree"), None);

        // By hand it does.
        let mut req = InstallRequest::new(FILES);
        req.tag = Some("v1.2.0".into());
        let done = wait(&r.mp, &r.mp.install(req).unwrap().id).await;
        assert_eq!(done.phase, Phase::Done, "{done:?}");
        let id = ModuleId::new(FILES).unwrap();
        assert_eq!(
            Defaults::load(paths).unwrap().get("avada.files.tree"),
            Some(&id)
        );

        // GIT is enabled in g6ws, so a pin to 1.2.0 breaks it and is refused.
        r.mp.set_enabled("g6ws", FILES, true).unwrap();
        let e = r.mp.pin("g6ws", FILES, "1.2.0").unwrap_err();
        let text = e.to_string();
        assert_eq!(e.http_status(), 409);
        assert!(text.contains(GIT), "{text}");
        assert!(
            text.contains("1.0.0"),
            "the nearest working version: {text}"
        );
        assert!(r.mp.pins("g6ws").unwrap().is_empty());

        // The version the resolver already chose pins fine, and unpins again.
        assert_eq!(
            r.mp.pin("g6ws", FILES, "1.0.0").unwrap().get(&id),
            Some(&Version::new(1, 0, 0))
        );
        assert_eq!(r.mp.pins("g6ws").unwrap().len(), 1);
        assert!(r.mp.unpin("g6ws", FILES).unwrap().is_empty());

        // A version nobody installed cannot be pinned at all.
        let e = r.mp.pin("g6ws", FILES, "9.9.9").unwrap_err();
        assert!(e.to_string().contains("not installed"), "{e}");

        // Uninstalling every version forgets the default and the workspace state.
        r.mp.uninstall(FILES, "1.0.0").unwrap();
        assert_eq!(
            Defaults::load(paths).unwrap().get("avada.files.tree"),
            Some(&id),
            "one version left, the default stands"
        );
        r.mp.uninstall(FILES, "1.2.0").unwrap();
        assert_eq!(Defaults::load(paths).unwrap().get("avada.files.tree"), None);
    }
}

// ---- end track G6 resolver

// ---- track G7 policy

/// The notarization gate seen from the pipeline: a fake verifier gives each of the four
/// verdicts, and the install either refuses, warns or runs.
///
/// The [`policy`] module tests the matrix exhaustively; these tests only prove the
/// wiring — that the verifier is consulted at all, that a refusal fails the job and
/// installs nothing, and that what was decided is remembered next to the artifact.
#[cfg(test)]
mod policy_wiring {
    use super::super::testing::{files_state, manifest_for, rig, wait, FakeCargo, FILES};
    use super::super::*;
    use crate::install::RecordStatus;
    use crate::policy::{Context, RecordedVerdict, Verdict};
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::Duration;

    /// Answers with one fixed verdict and counts the asking.
    #[derive(Debug)]
    struct FakeVerifier {
        verdict: Verdict,
        calls: AtomicUsize,
    }

    impl FakeVerifier {
        fn new(verdict: Verdict) -> Arc<Self> {
            Arc::new(FakeVerifier {
                verdict,
                calls: AtomicUsize::new(0),
            })
        }

        /// How many artifacts were put to this verifier.
        fn calls(&self) -> usize {
            self.calls.load(Ordering::SeqCst)
        }
    }

    impl Verifier for FakeVerifier {
        fn name(&self) -> &'static str {
            "fake"
        }

        fn verify(&self, binary: &Path, _ctx: &Context) -> Verdict {
            // The gate must run on the artifact itself, not on a path that might exist.
            assert!(binary.is_file(), "{} is not a file", binary.display());
            self.calls.fetch_add(1, Ordering::SeqCst);
            self.verdict.clone()
        }
    }

    fn write_policy(root: &Path, json: &str) {
        std::fs::write(Policy::path_under(root), json).expect("policy.json");
    }

    /// The one installed artifact and the verdict recorded beside it.
    fn installed_verdict(mp: &Marketplace) -> (PathBuf, Option<RecordedVerdict>) {
        let records = mp.store.records().expect("records");
        let installed = records
            .into_iter()
            .find_map(|s| match s {
                RecordStatus::Ok(i) => Some(i),
                RecordStatus::Broken { .. } => None,
            })
            .expect("one installed module");
        let verdict = policy::read_verdict(&installed.binary);
        (installed.binary.clone(), verdict)
    }

    async fn install_under(
        name: &str,
        policy_json: &str,
        verdict: Verdict,
    ) -> (Job, super::super::testing::Rig, Arc<FakeVerifier>) {
        let r = rig(
            name,
            files_state(),
            FakeCargo::Builds,
            Duration::from_secs(60),
        )
        .await;
        r.fixtures.repo(
            FILES,
            "v1.0.0",
            &manifest_for(FILES, "1.0.0", "kind = \"source\"", ""),
        );
        write_policy(&r.root, policy_json);
        let fake = FakeVerifier::new(verdict);
        r.mp.set_verifier(Arc::clone(&fake) as Arc<dyn Verifier>);
        let job = r.mp.install(InstallRequest::new(FILES)).expect("job");
        let done = wait(&r.mp, &job.id).await;
        (done, r, fake)
    }

    #[tokio::test]
    async fn a_refusal_fails_the_job_and_installs_nothing() {
        let (done, r, fake) = install_under(
            "g7-refuse",
            r#"{"locally_built":"require-signature"}"#,
            Verdict::Unsigned,
        )
        .await;
        assert_eq!(done.phase, Phase::Failed, "{done:?}");
        let err = done.error.as_deref().unwrap_or_default();
        assert!(err.contains("requires a signature"), "{err}");
        assert!(err.contains("unsigned"), "{err}");
        assert!(
            r.mp.installed().expect("installed").is_empty(),
            "a refused artifact must not be recorded"
        );
        assert_eq!(fake.calls(), 1, "the artifact is assessed exactly once");
    }

    #[tokio::test]
    async fn a_warning_installs_and_is_remembered_beside_the_binary() {
        let (done, r, fake) =
            install_under("g7-warn", r#"{"locally_built":"warn"}"#, Verdict::Unsigned).await;
        assert_eq!(done.phase, Phase::Done, "{done:?}");
        assert!(
            done.log_tail.iter().any(|l| l.contains("notarization")),
            "the warning must reach the job log: {:?}",
            done.log_tail
        );
        let (_, recorded) = installed_verdict(&r.mp);
        let recorded = recorded.expect("a verdict beside the binary");
        assert_eq!(recorded.decision, "warn");
        assert_eq!(recorded.verdict, "unsigned");
        assert_eq!(recorded.verifier, "fake");
        assert_eq!(recorded.source, "built");
        assert!(!recorded.refused());
        assert_eq!(fake.calls(), 1);
    }

    #[tokio::test]
    async fn a_trusted_artifact_installs_and_records_a_run() {
        let (done, r, fake) = install_under(
            "g7-trusted",
            r#"{"locally_built":"require-signature"}"#,
            Verdict::Trusted {
                by: "Acme Software Ltd".into(),
            },
        )
        .await;
        assert_eq!(done.phase, Phase::Done, "{done:?}");
        let (binary, recorded) = installed_verdict(&r.mp);
        let recorded = recorded.expect("a verdict beside the binary");
        assert_eq!(recorded.decision, "run");
        assert!(
            recorded.detail.contains("Acme Software Ltd"),
            "{recorded:?}"
        );
        assert_eq!(crate::policy::recorded_refusal(&binary), None);
        assert_eq!(fake.calls(), 1);
    }

    #[tokio::test]
    async fn the_default_policy_lets_a_locally_built_module_through_unsigned() {
        // No policy.json at all — the shipped default. A module the host compiled
        // itself from a verified commit must still install with no signature anywhere.
        let (done, r, fake) = install_under("g7-default", "{}", Verdict::Unsigned).await;
        assert_eq!(done.phase, Phase::Done, "{done:?}");
        let (_, recorded) = installed_verdict(&r.mp);
        assert_eq!(recorded.expect("recorded").decision, "run");
        assert_eq!(fake.calls(), 1, "the default policy still asks");
    }

    #[tokio::test]
    async fn a_recorded_refusal_stops_the_spawn_path() {
        // The install-time refusal is what makes a module `Broken` later: `verify_hash`
        // is the host's gate, and it consults the recorded verdict before hashing.
        let (done, r, _fake) = install_under("g7-spawn", "{}", Verdict::Unsigned).await;
        assert_eq!(done.phase, Phase::Done, "{done:?}");
        let records = r.mp.store.records().expect("records");
        let installed = records
            .into_iter()
            .find_map(|s| match s {
                RecordStatus::Ok(i) => Some(i),
                RecordStatus::Broken { .. } => None,
            })
            .expect("one installed module");

        // As installed, it spawns.
        crate::module::spawn::verify_hash(&installed.binary, installed.rights())
            .expect("a run verdict does not block the spawn");

        // Now record the refusal the policy would have reached.
        let refused = RecordedVerdict::new(
            &Verdict::Unsigned,
            &Decision::Refuse {
                reason: "unsigned (built here from deadbeef)".into(),
            },
            &policy::Source::Built {
                commit: "deadbeef".into(),
            },
            "fake",
        );
        policy::write_verdict(&installed.binary, &refused).expect("write");
        match crate::module::spawn::verify_hash(&installed.binary, installed.rights()) {
            Err(crate::module::spawn::SpawnError::Notarized { reason }) => {
                assert!(reason.contains("unsigned"), "{reason}")
            }
            other => panic!("expected a notarization refusal, got {other:?}"),
        }

        // Removing the sidecar returns the artifact to the pre-G7 hash check and no
        // further: the file can add a refusal, never remove one.
        std::fs::remove_file(policy::verdict_path(&installed.binary)).expect("remove");
        crate::module::spawn::verify_hash(&installed.binary, installed.rights())
            .expect("no sidecar is the status quo");
    }

    #[tokio::test]
    async fn a_malformed_policy_is_logged_and_falls_back_to_the_strict_default() {
        // Fail closed, and say so. The default allows a locally built module, so the
        // install still succeeds — but the complaint has to reach the job log.
        let (done, _r, _fake) =
            install_under("g7-malformed", "{ not json", Verdict::Unsigned).await;
        assert_eq!(done.phase, Phase::Done, "{done:?}");
        assert!(
            done.log_tail
                .iter()
                .any(|l| l.contains("malformed") && l.contains("default policy")),
            "{:?}",
            done.log_tail
        );
    }
}

// ---- end track G7 policy

// ---- track G10 commercial

/// The commercial half of the install pipeline, exercised from a build that does not
/// have the private crate.
///
/// That is the whole point of making the edition a value and the artifact step a trait:
/// a free build can install a fake precompiled loader, declare itself commercial, and
/// drive the prebuilt path end to end. When `avada-commercial` lands, the only thing
/// these tests do not cover is the body of its two implementations.
#[cfg(test)]
mod commercial_wiring {
    use super::super::testing::{files_state, manifest_for, rig, wait, FakeCargo, FILES};
    use super::super::*;
    use crate::edition::{Edition, COMMERCIAL_URL};
    use crate::marketplace::loader::{LoadError, LoadRequest, Loader};
    use crate::policy::{Context, Verdict, Verifier};
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::Duration;

    const PAID: &str = "acme/avada-paid";
    const PREBUILT: &str =
        "kind = \"binary\"\ncommercial = true\nissuer = \"https://license.example\"";

    /// Stands in for the private crate's downloader: writes a file where the pipeline
    /// expects the artifact and records what it was asked for.
    #[derive(Debug, Default)]
    struct FakeLoader {
        calls: Mutex<Vec<String>>,
        fail: Option<String>,
    }

    impl FakeLoader {
        fn new() -> Arc<Self> {
            Arc::new(FakeLoader::default())
        }

        fn refusing(why: &str) -> Arc<Self> {
            Arc::new(FakeLoader {
                calls: Mutex::new(Vec::new()),
                fail: Some(why.to_string()),
            })
        }

        fn calls(&self) -> Vec<String> {
            self.calls.lock().expect("calls").clone()
        }
    }

    impl Loader for FakeLoader {
        fn name(&self) -> &'static str {
            "fake-prebuilt"
        }

        fn serves(&self, kind: DistributionKind) -> bool {
            kind == DistributionKind::Binary
        }

        fn load(
            &self,
            req: &LoadRequest<'_>,
            log: &dyn Fn(&str),
            progress: &dyn Fn(u8),
        ) -> Result<PathBuf, LoadError> {
            self.calls.lock().expect("calls").push(format!(
                "{} {} {}",
                req.id.as_str(),
                req.tag,
                req.commit
            ));
            if let Some(why) = &self.fail {
                return Err(LoadError::Refused(why.clone()));
            }
            log("downloading the signed artifact");
            progress(70);
            let out = req.scratch.join("downloaded");
            std::fs::write(&out, "prebuilt artifact\n").expect("write artifact");
            Ok(out)
        }
    }

    /// Trusts everything, so the prebuilt policy default (`require-signature`) passes
    /// and these tests are about the loader, not about G7.
    #[derive(Debug)]
    struct TrustingVerifier(AtomicUsize);

    impl Verifier for TrustingVerifier {
        fn name(&self) -> &'static str {
            "fake-trusting"
        }

        fn verify(&self, binary: &Path, _ctx: &Context) -> Verdict {
            assert!(binary.is_file(), "{} is not a file", binary.display());
            self.0.fetch_add(1, Ordering::SeqCst);
            Verdict::Trusted {
                by: "Fake Publisher".into(),
            }
        }
    }

    /// A rig with one prebuilt commercial fixture repo published.
    async fn prebuilt_rig(name: &str) -> super::super::testing::Rig {
        let r = rig(
            name,
            files_state(),
            FakeCargo::Builds,
            Duration::from_secs(60),
        )
        .await;
        r.fixtures
            .repo(PAID, "v1.0.0", &manifest_for(PAID, "1.0.0", PREBUILT, ""));
        r
    }

    #[tokio::test]
    async fn the_free_edition_refuses_a_prebuilt_commercial_module_by_name() {
        let r = prebuilt_rig("g10-free").await;
        assert_eq!(r.mp.edition(), Edition::Free, "the test build is free");
        let job = r.mp.install(InstallRequest::new(PAID)).expect("job");
        let done = wait(&r.mp, &job.id).await;
        assert_eq!(done.phase, Phase::Failed, "{done:?}");
        let err = done.error.unwrap_or_default();
        assert!(err.contains(PAID), "{err}");
        assert!(err.contains("prebuilt binary"), "{err}");
        // A refusal that does not say where to get the thing is half an answer.
        assert!(err.contains(COMMERCIAL_URL), "{err}");
        assert!(r.mp.installed().expect("installed").is_empty());
    }

    #[tokio::test]
    async fn the_commercial_edition_installs_what_the_loader_produced() {
        let r = prebuilt_rig("g10-install").await;
        let loader = FakeLoader::new();
        r.mp.install_loader(Arc::clone(&loader) as Arc<dyn Loader>);
        r.mp.set_verifier(Arc::new(TrustingVerifier(AtomicUsize::new(0))) as Arc<dyn Verifier>);
        r.mp.set_edition(Edition::Commercial);

        let job = r.mp.install(InstallRequest::new(PAID)).expect("job");
        let done = wait(&r.mp, &job.id).await;
        assert_eq!(done.phase, Phase::Done, "{done:?}");

        // The loader ran instead of cargo, on the tag and commit the resolver settled.
        let calls = loader.calls();
        assert_eq!(calls.len(), 1, "{calls:?}");
        assert!(
            calls[0].starts_with(&format!("{PAID} v1.0.0 ")),
            "{calls:?}"
        );
        assert!(
            done.log_tail
                .iter()
                .any(|l| l.contains("fake-prebuilt produced")),
            "{:?}",
            done.log_tail
        );
        assert!(
            !done.log_tail.iter().any(|l| l.contains("Compiling")),
            "cargo must not have run: {:?}",
            done.log_tail
        );

        // Past the loader the artifact is an artifact: hashed, notarized, recorded.
        let id = ModuleId::new(PAID).unwrap();
        let installed = r.mp.store().record(&id).unwrap().expect("active record");
        assert_eq!(
            std::fs::read_to_string(&installed.binary).unwrap(),
            "prebuilt artifact\n"
        );
        assert_eq!(
            installed.rights().artifact_sha256,
            hash_file(&installed.binary).unwrap()
        );
        let recorded = policy::read_verdict(&installed.binary).expect("verdict beside it");
        assert_eq!(recorded.source, "prebuilt", "{recorded:?}");
        assert_eq!(recorded.verdict, "trusted");
        assert_eq!(recorded.decision, "run");
    }

    #[tokio::test]
    async fn a_commercial_build_with_no_prebuilt_loader_says_so() {
        // The free build's one loader serves source only, so a commercial edition that
        // never installed the private one has nothing to run — and must say which
        // edition and which kind rather than falling back to cargo.
        let r = prebuilt_rig("g10-noloader").await;
        r.mp.set_edition(Edition::Commercial);
        let job = r.mp.install(InstallRequest::new(PAID)).expect("job");
        let done = wait(&r.mp, &job.id).await;
        assert_eq!(done.phase, Phase::Failed, "{done:?}");
        let err = done.error.unwrap_or_default();
        assert!(err.contains("commercial edition"), "{err}");
        assert!(err.contains("prebuilt"), "{err}");
        assert!(r.mp.installed().expect("installed").is_empty());
    }

    #[tokio::test]
    async fn a_loader_refusal_fails_the_job_and_installs_nothing() {
        let r = prebuilt_rig("g10-refuse").await;
        let loader = FakeLoader::refusing("no entitlement for this seat");
        r.mp.install_loader(Arc::clone(&loader) as Arc<dyn Loader>);
        r.mp.set_edition(Edition::Commercial);
        let job = r.mp.install(InstallRequest::new(PAID)).expect("job");
        let done = wait(&r.mp, &job.id).await;
        assert_eq!(done.phase, Phase::Failed, "{done:?}");
        assert!(
            done.error.unwrap_or_default().contains("no entitlement"),
            "the loader's own words reach the user"
        );
        assert_eq!(loader.calls().len(), 1);
        assert!(r.mp.installed().expect("installed").is_empty());
    }

    #[tokio::test]
    async fn the_source_path_is_unchanged_by_the_commercial_edition() {
        // The commercial build is a superset, not a fork: a source module still goes
        // through cargo when the precompiled loader is installed beside it.
        let r = rig(
            "g10-source",
            files_state(),
            FakeCargo::Builds,
            Duration::from_secs(60),
        )
        .await;
        r.fixtures.repo(
            FILES,
            "v1.0.0",
            &manifest_for(FILES, "1.0.0", "kind = \"source\"", ""),
        );
        let loader = FakeLoader::new();
        r.mp.install_loader(Arc::clone(&loader) as Arc<dyn Loader>);
        r.mp.set_edition(Edition::Commercial);
        let job = r.mp.install(InstallRequest::new(FILES)).expect("job");
        let done = wait(&r.mp, &job.id).await;
        assert_eq!(done.phase, Phase::Done, "{done:?}");
        assert!(loader.calls().is_empty(), "the prebuilt loader was asked");
        assert!(
            done.log_tail.iter().any(|l| l.contains("cargo produced")),
            "{:?}",
            done.log_tail
        );
    }
}

// ---- end track G10 commercial
