//! The commercial edition's licence check at launch.
//!
//! The decision is `avada_core::license::host::gate`; this file is only the app's side of it:
//! where the licence store lives, what the user is shown on a refusal, and the
//! `--install-license <file>` command that puts a licence there. A free build returns
//! from [`enforce`] before it touches the store or the network.

use avada_core::edition::Edition;
use avada_core::license::host::{self, HOST_ISSUER};
use avada_core::license::{Gate, LicenseService};

/// The major version of this build, which a licence's `max_major` is compared against.
fn build_major() -> u64 {
    env!("CARGO_PKG_VERSION_MAJOR").parse().unwrap_or(0)
}

fn service() -> LicenseService {
    LicenseService::under(&avada_core::persistence::lockfile::modules_root())
}

/// What a launch should do.
#[derive(Debug, PartialEq, Eq)]
pub enum Launch {
    Run,
    /// Run, and say why (the licence is in its grace window).
    RunWithNotice(String),
    /// Do not run; show this text.
    Refused(String),
}

/// Decide whether this build may start. `edition` and `issuer` are arguments so the commercial
/// path is testable from a free build.
pub async fn decide(edition: Edition, service: &LicenseService, issuer: &str) -> Launch {
    match host::gate(edition, service, issuer, build_major()).await {
        Gate::Run => Launch::Run,
        Gate::RunWithBanner(why) => Launch::RunWithNotice(why),
        Gate::Refuse(why) => Launch::Refused(host::refusal_text(&why)),
    }
}

/// Gate this process. Returns `false` after telling the user why, when it must not start.
/// Needs a Tokio runtime entered (the dialog's portal backend wants one).
pub fn enforce(rt: &tokio::runtime::Runtime) -> bool {
    if !Edition::CURRENT.is_commercial() {
        return true;
    }
    match rt.block_on(decide(Edition::CURRENT, &service(), HOST_ISSUER)) {
        Launch::Run => true,
        Launch::RunWithNotice(why) => {
            tracing::warn!(%why, "licence is in its grace window");
            true
        }
        Launch::Refused(text) => {
            eprintln!("{text}");
            let _ = rfd::MessageDialog::new()
                .set_level(rfd::MessageLevel::Error)
                .set_title("Avada Terminal needs a licence")
                .set_description(text)
                .set_buttons(rfd::MessageButtons::Ok)
                .show();
            false
        }
    }
}

/// The file named by `--install-license <file>`, if that flag is present.
/// `Some(None)` is the flag with nothing after it.
pub fn install_arg(argv: &[String]) -> Option<Option<&str>> {
    let i = argv.iter().position(|a| a == "--install-license")?;
    Some(argv.get(i + 1).map(String::as_str))
}

/// Run `--install-license`. Prints the outcome and returns the process exit code.
pub fn run_install(file: Option<&str>) -> i32 {
    let Some(file) = file else {
        eprintln!("usage: avada --install-license <file>");
        return 2;
    };
    let rt = match tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
    {
        Ok(rt) => rt,
        Err(e) => {
            eprintln!("cannot start: {e}");
            return 1;
        }
    };
    match rt.block_on(host::install(
        &service(),
        HOST_ISSUER,
        std::path::Path::new(file),
    )) {
        Ok(s) => {
            println!(
                "Installed a licence for {} issued to {} (state: {}).",
                s.product, s.licensee, s.state
            );
            0
        }
        Err(e) => {
            eprintln!("Could not install that licence: {e}");
            1
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use avada_core::license::MemoryLicenseStore;

    fn argv(a: &[&str]) -> Vec<String> {
        a.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn the_flag_and_its_file_are_found_or_reported_missing() {
        assert_eq!(install_arg(&argv(&["avada"])), None);
        assert_eq!(
            install_arg(&argv(&["avada", "--install-license", "a.jwt"])),
            Some(Some("a.jwt"))
        );
        assert_eq!(
            install_arg(&argv(&["avada", "--install-license"])),
            Some(None)
        );
    }

    #[test]
    fn a_missing_file_argument_is_a_usage_error() {
        assert_eq!(run_install(None), 2);
    }

    #[tokio::test]
    async fn a_free_launch_runs_and_an_unlicensed_commercial_one_is_refused_with_the_way_out() {
        let svc = LicenseService::new(
            std::sync::Arc::new(MemoryLicenseStore::new()),
            std::sync::Arc::new(avada_core::license::stub_issuer::StubIssuer::new(
                "https://issuer.test",
            )),
            std::sync::Arc::new(|| 1_700_000_000),
        );
        assert_eq!(
            decide(Edition::Free, &svc, "https://issuer.test").await,
            Launch::Run
        );
        match decide(Edition::Commercial, &svc, "https://issuer.test").await {
            Launch::Refused(t) => {
                assert!(t.contains("--install-license"), "{t}");
                assert!(t.contains(host::GET_A_LICENSE_URL), "{t}");
            }
            other => panic!("expected a refusal, got {other:?}"),
        }
    }
}
