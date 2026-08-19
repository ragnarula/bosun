use std::error::Error;

pub trait ErrorExt {
    fn display_chain(self) -> String;
}

impl<T> ErrorExt for T
where
    T: Error,
{
    fn display_chain(self) -> String {
        let mut result = self.to_string();
        let mut current = self.source();
        while let Some(err) = current {
            result.push_str("\n\t caused by: ");
            result.push_str(&err.to_string());
            current = err.source();
        }
        result
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Debug, thiserror::Error)]
    enum TestError {
        #[error("outer")]
        Outer(#[from] anyhow::Error),
    }

    #[test]
    fn display_chain_builds_full_chain() {
        let err = TestError::Outer(anyhow::Error::msg("inner problem"));
        let chain = err.display_chain();
        assert!(chain.starts_with("outer"));
        assert!(chain.contains("inner problem"));
    }
}
