//! Which edition this build is (docs/modules-fanout-plan.md §2 "Free vs commercial",
//! track G10).
//!
//! The free edition compiles every module from source and can neither license nor
//! notarize a binary someone else compiled. The commercial edition additionally loads
//! precompiled, notarized artifacts and swaps stricter policy implementations in. That
//! is one Cargo feature, `commercial`, and it deliberately pulls in **no crate**: the
//! private `avada-commercial` crate depends on *this* one rather than the other way
//! round, so the free build never has a git dependency it cannot resolve offline, and
//! there is no dependency cycle to break. What the feature changes here is the
//! *policy* — which modules may install, and whether a missing precompiled loader is a
//! bug or the expected state.
//!
//! Every decision that turns on the edition takes it as an argument rather than reading
//! `cfg!` at the point of use. A free build can then test the commercial branch, which
//! is the only way the commercial branch is tested at all before the private crate
//! exists.

/// Which build this is.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Edition {
    /// Source-compiled modules only, no licensing, no prebuilt artifacts.
    Free,
    /// Compiled or precompiled, licensed, notarized against a stricter policy.
    Commercial,
}

/// Where a free build sends someone who wants a module it may not install.
pub const COMMERCIAL_URL: &str = "https://avada.to/commercial";

impl Edition {
    /// The edition this binary was compiled as.
    pub const CURRENT: Edition = if cfg!(feature = "commercial") {
        Edition::Commercial
    } else {
        Edition::Free
    };

    /// [`Edition::CURRENT`], as a function for the call sites that read better that way.
    pub const fn current() -> Edition {
        Edition::CURRENT
    }

    /// The wire/log name.
    pub const fn name(self) -> &'static str {
        match self {
            Edition::Free => "free",
            Edition::Commercial => "commercial",
        }
    }

    /// Whether prebuilt and commercial modules are allowed at all.
    pub const fn is_commercial(self) -> bool {
        matches!(self, Edition::Commercial)
    }
}

impl std::fmt::Display for Edition {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.name())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_feature_and_the_edition_say_the_same_thing() {
        assert_eq!(Edition::CURRENT, Edition::current());
        assert_eq!(
            Edition::CURRENT.is_commercial(),
            cfg!(feature = "commercial")
        );
        // The gates build without the feature, so this is what they are proving about.
        #[cfg(not(feature = "commercial"))]
        assert_eq!(Edition::CURRENT, Edition::Free);
    }

    #[test]
    fn the_names_are_the_ones_logs_and_json_use() {
        assert_eq!(Edition::Free.to_string(), "free");
        assert_eq!(Edition::Commercial.to_string(), "commercial");
        assert_eq!(
            serde_json::to_string(&Edition::Commercial).unwrap(),
            "\"commercial\""
        );
    }
}
