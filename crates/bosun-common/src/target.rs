/// The rustc target triple this binary was compiled for, set by `build.rs`
/// from Cargo's `TARGET` build-script variable. cargo-dist names artifacts by
/// the same triples, so this is the manifest key and the artifact-file suffix.
pub const TARGET: &str = env!("BOSUN_TARGET");

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn target_looks_like_a_rust_triple() {
        assert!(!TARGET.is_empty());
        assert!(!TARGET.contains(char::is_whitespace));
        assert!(TARGET.contains('-'));
    }
}
