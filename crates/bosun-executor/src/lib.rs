//! Executes tool calls against a session's working copy, one process per
//! session, over a local HTTP API.

pub mod server;
pub mod tools;

#[cfg(test)]
mod tests {
    #[test]
    fn crate_scaffold_compiles() {}
}
