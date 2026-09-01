use std::cmp::Ordering;

use semver::Version;

/// The binary version, shared by every crate in the workspace.
pub const VERSION: &str = env!("CARGO_PKG_VERSION");

/// Orders two version strings as semver. Returns None when either string is
/// not a valid semver version.
pub fn compare(a: &str, b: &str) -> Option<Ordering> {
    let a = Version::parse(a).ok()?;
    let b = Version::parse(b).ok()?;
    Some(a.cmp(&b))
}

#[cfg(test)]
mod tests {
    use std::cmp::Ordering;

    use super::*;

    #[test]
    fn equal_versions_compare_equal() {
        assert_eq!(compare("0.5.5", "0.5.5"), Some(Ordering::Equal));
    }

    #[test]
    fn newer_versions_compare_greater() {
        assert_eq!(compare("0.6.0", "0.5.5"), Some(Ordering::Greater));
        assert_eq!(compare("1.0.0", "0.9.9"), Some(Ordering::Greater));
        assert_eq!(compare("0.5.10", "0.5.9"), Some(Ordering::Greater));
    }

    #[test]
    fn older_versions_compare_less() {
        assert_eq!(compare("0.5.5", "0.6.0"), Some(Ordering::Less));
    }

    #[test]
    fn prerelease_versions_sort_before_releases() {
        assert_eq!(compare("0.5.5-alpha.1", "0.5.5"), Some(Ordering::Less));
    }

    #[test]
    fn unparsable_versions_compare_to_nothing() {
        assert_eq!(compare("banana", "0.5.5"), None);
        assert_eq!(compare("0.5.5", ""), None);
    }
}
