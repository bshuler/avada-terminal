//! `semver::VersionReq` → `pubgrub::Ranges<Version>`.
//!
//! pubgrub reasons about sets of versions as unions of intervals; a Cargo-style
//! requirement is an intersection of comparators, each of which is one interval. The
//! translation follows the semver crate's own matching rules for every operator.
//! Pre-releases are the one place the two disagree: semver only lets a pre-release
//! match a comparator that names the same `major.minor.patch` with a pre-release,
//! whereas an interval `>=1.0.0, <2.0.0` contains `1.5.0-beta`. The resolver keeps
//! semver's rule by never offering a pre-release as a candidate unless it is asked
//! for exactly (see `provider.rs`), so the intervals here need not encode it.

use pubgrub::Ranges;
use semver::{Comparator, Op, Prerelease, Version, VersionReq};

/// The set of versions `req` accepts.
pub fn ranges_of(req: &VersionReq) -> Ranges<Version> {
    req.comparators
        .iter()
        .fold(Ranges::full(), |acc, c| acc.intersection(&comparator(c)))
}

/// `[lo, hi)` bumped at the level the comparator left unspecified.
fn comparator(c: &Comparator) -> Ranges<Version> {
    let lo = |pre: bool| {
        let mut v = Version::new(c.major, c.minor.unwrap_or(0), c.patch.unwrap_or(0));
        if pre {
            v.pre = c.pre.clone();
        }
        v
    };
    let next_major = Version::new(c.major + 1, 0, 0);
    let next_minor = |m: u64| Version::new(c.major, m + 1, 0);
    let next_patch = |m: u64, p: u64| Version::new(c.major, m, p + 1);
    match c.op {
        Op::Exact | Op::Wildcard => match (c.minor, c.patch) {
            (None, _) => Ranges::between(lo(false), next_major),
            (Some(m), None) => Ranges::between(lo(false), next_minor(m)),
            (Some(_), Some(_)) => Ranges::singleton(lo(true)),
        },
        Op::Greater => match (c.minor, c.patch) {
            (None, _) => Ranges::higher_than(next_major),
            (Some(m), None) => Ranges::higher_than(next_minor(m)),
            (Some(_), Some(_)) => Ranges::strictly_higher_than(lo(true)),
        },
        Op::GreaterEq => Ranges::higher_than(lo(true)),
        Op::Less => Ranges::strictly_lower_than(lo(true)),
        Op::LessEq => match (c.minor, c.patch) {
            (None, _) => Ranges::strictly_lower_than(next_major),
            (Some(m), None) => Ranges::strictly_lower_than(next_minor(m)),
            (Some(_), Some(_)) => Ranges::lower_than(lo(true)),
        },
        Op::Tilde => match (c.minor, c.patch) {
            (None, _) => Ranges::between(lo(false), next_major),
            (Some(m), _) => Ranges::between(lo(true), next_minor(m)),
        },
        Op::Caret => match (c.minor, c.patch) {
            (None, _) => Ranges::between(lo(false), next_major),
            (Some(m), None) if c.major == 0 => Ranges::between(lo(false), next_minor(m)),
            (Some(_), None) => Ranges::between(lo(false), next_major),
            (Some(m), Some(p)) if c.major == 0 && m == 0 => {
                Ranges::between(lo(true), next_patch(m, p))
            }
            (Some(m), Some(_)) if c.major == 0 => Ranges::between(lo(true), next_minor(m)),
            (Some(_), Some(_)) => Ranges::between(lo(true), next_major),
        },
        _ => Ranges::full(),
    }
}

/// Whether `v` is a pre-release (`1.0.0-rc.1`).
pub fn is_prerelease(v: &Version) -> bool {
    v.pre != Prerelease::EMPTY
}

/// The compatibility line a version belongs to under caret rules: `2` for `2.x.y`,
/// `0.3` for `0.3.y`. Two versions on different lines may be installed side by side.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Compat {
    /// The major.
    pub major: u64,
    /// The minor, only when the major is 0.
    pub minor: Option<u64>,
}

impl Compat {
    /// The line `v` is on.
    pub fn of(v: &Version) -> Compat {
        Compat {
            major: v.major,
            minor: (v.major == 0).then_some(v.minor),
        }
    }

    /// Every version on this line.
    pub fn ranges(&self) -> Ranges<Version> {
        match self.minor {
            Some(m) => Ranges::between(Version::new(0, m, 0), Version::new(0, m + 1, 0)),
            None => Ranges::between(
                Version::new(self.major, 0, 0),
                Version::new(self.major + 1, 0, 0),
            ),
        }
    }
}

impl std::fmt::Display for Compat {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self.minor {
            Some(m) => write!(f, "0.{m}.x"),
            None => write!(f, "{}.x", self.major),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn v(s: &str) -> Version {
        Version::parse(s).unwrap()
    }

    /// Every operator agrees with `VersionReq::matches` on a grid of plain versions.
    #[test]
    fn ranges_agree_with_semver_matching() {
        let reqs = [
            "*",
            "1",
            "1.2",
            "1.2.3",
            "=1.2.3",
            "=1.2",
            "=1",
            ">1.2.3",
            ">1.2",
            ">1",
            ">=1.2.3",
            ">=1.2",
            ">=1",
            "<1.2.3",
            "<1.2",
            "<1",
            "<=1.2.3",
            "<=1.2",
            "<=1",
            "~1.2.3",
            "~1.2",
            "~1",
            "^1.2.3",
            "^1.2",
            "^1",
            "^0.2.3",
            "^0.2",
            "^0.0.3",
            "^0.0",
            "^0",
            "1.*",
            "1.2.*",
            ">=1.2, <1.5",
            ">=0.1, <0.3",
            ">1, <3",
        ];
        let mut grid = Vec::new();
        for major in 0..4 {
            for minor in 0..4 {
                for patch in 0..5 {
                    grid.push(Version::new(major, minor, patch));
                }
            }
        }
        for text in reqs {
            let req = VersionReq::parse(text).unwrap();
            let ranges = ranges_of(&req);
            for ver in &grid {
                assert_eq!(
                    ranges.contains(ver),
                    req.matches(ver),
                    "`{text}` on {ver}: ranges {ranges}"
                );
            }
        }
    }

    #[test]
    fn ranges_render_readably() {
        assert_eq!(
            ranges_of(&VersionReq::parse("^1.2").unwrap()).to_string(),
            ">=1.2.0, <2.0.0"
        );
        assert_eq!(
            ranges_of(&VersionReq::parse("=1.2.3").unwrap()).to_string(),
            "1.2.3"
        );
        assert_eq!(ranges_of(&VersionReq::parse("*").unwrap()).to_string(), "*");
        assert_eq!(
            ranges_of(&VersionReq::parse(">=1, <1").unwrap()).to_string(),
            "∅"
        );
    }

    #[test]
    fn compat_lines_follow_caret_rules() {
        assert_eq!(Compat::of(&v("2.4.1")).to_string(), "2.x");
        assert_eq!(Compat::of(&v("0.3.9")).to_string(), "0.3.x");
        assert_eq!(Compat::of(&v("2.4.1")), Compat::of(&v("2.0.0")));
        assert_ne!(Compat::of(&v("0.3.9")), Compat::of(&v("0.4.0")));
        assert!(Compat::of(&v("2.4.1")).ranges().contains(&v("2.9.9")));
        assert!(!Compat::of(&v("2.4.1")).ranges().contains(&v("3.0.0")));
        assert!(Compat::of(&v("0.3.1")).ranges().contains(&v("0.3.7")));
        assert!(!Compat::of(&v("0.3.1")).ranges().contains(&v("0.4.0")));
        assert!(is_prerelease(&v("1.0.0-rc.1")) && !is_prerelease(&v("1.0.0")));
    }
}
