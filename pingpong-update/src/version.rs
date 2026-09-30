//! A version as a release's tag and `Cargo.toml` write it: `0.6.0`, `v0.6.0`,
//! `0.7.0-rc.1`. Only the ordering matters here: is that release newer than
//! this build?

use std::cmp::Ordering;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Version {
    numbers: [u32; 3],
    /// What follows a `-`: a pre-release comes before the release it leads
    /// to.
    pre: Option<String>,
}

impl Version {
    /// `None` for anything that is not numbers separated by dots (a tag named
    /// otherwise is not a release of this program).
    pub fn parse(text: &str) -> Option<Version> {
        let text = text.trim();
        let text = text.strip_prefix(['v', 'V']).unwrap_or(text);
        // Build metadata (`+…`) says nothing about order.
        let text = text.split('+').next()?;
        let (numbers, pre) = match text.split_once('-') {
            Some((n, p)) if !p.is_empty() => (n, Some(p.to_string())),
            Some(_) => return None,
            None => (text, None),
        };
        let mut out = [0u32; 3];
        let mut parts = numbers.split('.');
        for (i, slot) in out.iter_mut().enumerate() {
            match parts.next() {
                Some(p) => *slot = p.parse().ok()?,
                // "1" and "1.2" are 1.0.0 and 1.2.0; nothing at all is not
                // a version.
                None if i > 0 => break,
                None => return None,
            }
        }
        if parts.next().is_some() {
            return None;
        }
        Some(Version { numbers: out, pre })
    }
}

impl Ord for Version {
    fn cmp(&self, other: &Self) -> Ordering {
        self.numbers
            .cmp(&other.numbers)
            .then_with(|| match (&self.pre, &other.pre) {
                (None, None) => Ordering::Equal,
                (None, Some(_)) => Ordering::Greater,
                (Some(_), None) => Ordering::Less,
                (Some(a), Some(b)) => pre_release_order(a, b),
            })
    }
}

impl PartialOrd for Version {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

/// Semantic Versioning's rule for what follows the `-`: dot-separated parts,
/// numbers compared as numbers and before words, fewer parts first.
fn pre_release_order(a: &str, b: &str) -> Ordering {
    let (mut a, mut b) = (a.split('.'), b.split('.'));
    loop {
        match (a.next(), b.next()) {
            (None, None) => return Ordering::Equal,
            (None, Some(_)) => return Ordering::Less,
            (Some(_), None) => return Ordering::Greater,
            (Some(x), Some(y)) => {
                let order = match (x.parse::<u64>(), y.parse::<u64>()) {
                    (Ok(x), Ok(y)) => x.cmp(&y),
                    (Ok(_), Err(_)) => Ordering::Less,
                    (Err(_), Ok(_)) => Ordering::Greater,
                    (Err(_), Err(_)) => x.cmp(y),
                };
                if order != Ordering::Equal {
                    return order;
                }
            }
        }
    }
}

impl std::fmt::Display for Version {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let [major, minor, patch] = self.numbers;
        write!(f, "{major}.{minor}.{patch}")?;
        match &self.pre {
            Some(pre) => write!(f, "-{pre}"),
            None => Ok(()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn v(text: &str) -> Version {
        Version::parse(text).unwrap_or_else(|| panic!("{text} is a version"))
    }

    #[test]
    fn a_tag_and_a_cargo_version_are_the_same_version() {
        assert_eq!(v("v0.6.0"), v("0.6.0"));
        assert_eq!(v(" V1.2.3\n"), v("1.2.3"));
        assert_eq!(v("1.2"), v("1.2.0"));
        assert_eq!(v("0.6.0+build.7"), v("0.6.0"));
        assert_eq!(v("v0.7.0-rc.1").to_string(), "0.7.0-rc.1");
    }

    #[test]
    fn numbers_are_compared_as_numbers_not_as_text() {
        assert!(v("0.10.0") > v("0.9.9"));
        assert!(v("1.0.0") > v("0.99.99"));
        assert!(v("0.6.1") > v("0.6.0"));
        assert!(v("0.6.0") == v("0.6.0"));
    }

    #[test]
    fn a_pre_release_comes_before_its_release() {
        assert!(v("0.7.0-rc.1") < v("0.7.0"));
        assert!(v("0.7.0-rc.1") > v("0.6.9"));
        assert!(v("0.7.0-rc.2") > v("0.7.0-rc.1"));
        assert!(v("0.7.0-rc.10") > v("0.7.0-rc.9"));
        assert!(v("0.7.0-beta") < v("0.7.0-rc"));
        assert!(v("0.7.0-rc") < v("0.7.0-rc.1"));
    }

    #[test]
    fn what_is_not_a_version_is_refused() {
        for text in [
            "", "v", "nightly", "1.x", "1.2.3.4", "1..2", "0.6.0-", "-1.0",
        ] {
            assert_eq!(Version::parse(text), None, "{text:?}");
        }
    }
}
