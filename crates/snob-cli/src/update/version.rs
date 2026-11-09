//! Versions as releases are tagged: `X.Y.Z`, with or without a leading `v`.
//!
//! Three numbers and nothing else. A pre-release (`0.10.0-rc.1`) is not a
//! version an update moves to, and GitHub's "latest release" never names one;
//! a build whose own version is one never updates, rather than guess where it
//! stands.

use std::fmt;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) struct Version(u64, u64, u64);

impl Version {
    /// `text` as a version, or `None` when it is anything but three numbers.
    pub(crate) fn parse(text: &str) -> Option<Self> {
        let text = text.trim();
        let text = text.strip_prefix('v').unwrap_or(text);
        let mut numbers = text.split('.').map(|part| {
            // `u64::from_str` takes a leading `+`, which no tag has.
            (!part.is_empty() && part.bytes().all(|b| b.is_ascii_digit()))
                .then(|| part.parse::<u64>().ok())
                .flatten()
        });
        let version = Self(numbers.next()??, numbers.next()??, numbers.next()??);
        numbers.next().is_none().then_some(version)
    }

    /// This build's version, `None` for a pre-release build.
    pub(crate) fn current() -> Option<Self> {
        Self::parse(env!("CARGO_PKG_VERSION"))
    }
}

impl fmt::Display for Version {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}.{}.{}", self.0, self.1, self.2)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_tag_reads_with_or_without_its_v() {
        assert_eq!(Version::parse("v0.9.0"), Some(Version(0, 9, 0)));
        assert_eq!(Version::parse("0.10.2"), Some(Version(0, 10, 2)));
        assert_eq!(Version::parse(" 1.2.3\n"), Some(Version(1, 2, 3)));
    }

    #[test]
    fn anything_but_three_numbers_is_no_version() {
        for not in [
            "",
            "v",
            "1.2",
            "1.2.3.4",
            "1.2.3-rc.1",
            "1.+2.3",
            "1..3",
            "a.b.c",
        ] {
            assert_eq!(Version::parse(not), None, "{not:?}");
        }
    }

    /// Compared as numbers, so 0.10 comes after 0.9.
    #[test]
    fn versions_compare_as_numbers() {
        assert!(Version::parse("0.10.0") > Version::parse("0.9.9"));
        assert!(Version::parse("1.0.0") > Version::parse("0.99.99"));
    }

    #[test]
    fn this_build_has_a_version() {
        assert!(Version::current().is_some());
    }
}
