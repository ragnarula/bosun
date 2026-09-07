//! Skill discovery: the working copy's skills are fetched from the node's
//! executor, while remote skill packages come from the store.

use std::time::Duration;

use anyhow::Context;
pub use bosun_common::skills::Skill;
use serde_json::Value;
use serde_json::json;
use tokio::sync::mpsc;
use uuid::Uuid;

use crate::agent_loop::ToolExecutor;

/// The executor answers skill calls on the node; a hung node must not stall
/// a turn, so the round trip is bounded.
const SKILL_CALL_TIMEOUT: Duration = Duration::from_secs(15);

/// Fetches the working copy's skills from the node's executor.
pub async fn fetch_working_skills(
    tools: &dyn ToolExecutor,
    session_id: &str,
) -> Result<Vec<Skill>, anyhow::Error> {
    // The first turn can race the node's tunnel registration, so a transient
    // failure is retried briefly before giving up.
    let mut last_error = None;
    for _ in 0..SKILL_FETCH_ATTEMPTS {
        match fetch_working_skills_once(tools, session_id).await {
            Ok(skills) => return Ok(skills),
            Err(error) => {
                last_error = Some(error);
                tokio::time::sleep(Duration::from_millis(200)).await;
            }
        }
    }
    Err(last_error.unwrap_or_else(|| anyhow::anyhow!("failed to fetch skills")))
}

const SKILL_FETCH_ATTEMPTS: usize = 4;

async fn fetch_working_skills_once(
    tools: &dyn ToolExecutor,
    session_id: &str,
) -> Result<Vec<Skill>, anyhow::Error> {
    let (delta_tx, _delta_rx) = mpsc::unbounded_channel();
    let outcome = tokio::time::timeout(
        SKILL_CALL_TIMEOUT,
        tools.call(
            session_id.to_string(),
            Uuid::new_v4().to_string(),
            "skills".to_string(),
            Value::Null,
            delta_tx,
        ),
    )
    .await
    .context("the node did not answer the skills request")??;
    if outcome.is_error {
        anyhow::bail!(
            "the node failed to list the working copy's skills: {}",
            outcome.content
        );
    }
    let skills = outcome
        .content
        .get("skills")
        .ok_or_else(|| anyhow::anyhow!("the node's skills response has no \"skills\" field"))?;
    Ok(serde_json::from_value(skills.clone())?)
}

/// Reads one working-copy skill's instructions from the node. Ok(None) when
/// the node does not know the skill.
pub async fn read_working_skill(
    tools: &dyn ToolExecutor,
    session_id: &str,
    name: &str,
) -> Result<Option<String>, anyhow::Error> {
    let (delta_tx, _delta_rx) = mpsc::unbounded_channel();
    let outcome = tokio::time::timeout(
        SKILL_CALL_TIMEOUT,
        tools.call(
            session_id.to_string(),
            Uuid::new_v4().to_string(),
            "skill/read".to_string(),
            json!({ "name": name }),
            delta_tx,
        ),
    )
    .await
    .context("the node did not answer the skill read")??;
    if outcome.is_error {
        return Ok(None);
    }
    let content = outcome
        .content
        .get("content")
        .and_then(Value::as_str)
        .ok_or_else(|| anyhow::anyhow!("the node's skill response has no \"content\" string"))?
        .to_string();
    Ok(Some(content))
}

/// How a `skill` call's `name` resolved against the session's remote
/// packages. `Exact` and `Short` carry the package address the handler
/// loads; `Ambiguous` and `None` describe the misses it reports.
pub enum RemoteResolution {
    /// `name` was the full address of one remote package.
    Exact { address: String },
    /// `name` matched exactly one remote package's short name; the matched
    /// package's address.
    Short { address: String },
    /// `name` is the short name of several remote packages; every candidate
    /// address.
    Ambiguous { addresses: Vec<String> },
    /// No remote package matched.
    None,
}

/// Resolves the `skill` tool's `name` against the remote packages. A full
/// address (`github.com/...`) matches a package's address exactly and never
/// falls back to a short-name match; any other name is matched by short
/// name, which is unique, ambiguous, or unknown.
pub fn resolve_remote(remote: &[Skill], name: &str) -> RemoteResolution {
    if name.starts_with("github.com/") {
        let Some(matched) = remote
            .iter()
            .find(|skill| skill.package.as_deref() == Some(name))
        else {
            return RemoteResolution::None;
        };
        let address = matched
            .package
            .as_deref()
            .expect("an advertised remote package names its address")
            .to_string();
        return RemoteResolution::Exact { address };
    }
    let mut addresses = Vec::new();
    for skill in remote {
        if skill.name == name {
            addresses.push(
                skill
                    .package
                    .as_deref()
                    .expect("an advertised remote package names its address")
                    .to_string(),
            );
        }
    }
    match addresses.len() {
        0 => RemoteResolution::None,
        1 => RemoteResolution::Short {
            address: addresses[0].clone(),
        },
        _ => RemoteResolution::Ambiguous { addresses },
    }
}

/// The reference paths the instructions body mentions, in the paths' input
/// order: a path counts when the body contains it verbatim or contains its
/// last path segment, and no path is listed twice.
pub fn referenced_paths(instructions: &str, paths: &[String]) -> Vec<String> {
    paths
        .iter()
        .filter(|path| {
            let last_segment = path
                .split('/')
                .next_back()
                .expect("a reference path has a last segment");
            instructions.contains(path.as_str()) || instructions.contains(last_segment)
        })
        .cloned()
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn remote_package(name: &str, address: &str) -> Skill {
        Skill {
            name: name.into(),
            description: "does things".into(),
            package: Some(address.into()),
        }
    }

    #[test]
    fn resolve_remote_matches_a_full_address_exactly() {
        let remote = vec![
            remote_package("checkout", "github.com/owner/acme/tools/skills/checkout"),
            remote_package("deploy", "github.com/owner/acme/tools/skills/deploy"),
        ];
        let RemoteResolution::Exact { address } =
            resolve_remote(&remote, "github.com/owner/acme/tools/skills/checkout")
        else {
            panic!("a full address must resolve to the matching package");
        };
        assert_eq!(address, "github.com/owner/acme/tools/skills/checkout");
    }

    #[test]
    fn resolve_remote_resolves_a_unique_short_name_to_its_address() {
        let remote = vec![
            remote_package("checkout", "github.com/owner/acme/tools/skills/checkout"),
            remote_package("deploy", "github.com/owner/acme/tools/skills/deploy"),
        ];
        let RemoteResolution::Short { address } = resolve_remote(&remote, "deploy") else {
            panic!("a unique short name must resolve to the matched package");
        };
        assert_eq!(address, "github.com/owner/acme/tools/skills/deploy");
    }

    #[test]
    fn resolve_remote_reports_an_ambiguous_short_name_with_every_candidate_address() {
        let remote = vec![
            remote_package("checkout", "github.com/corp/tools/skills/checkout"),
            remote_package("checkout", "github.com/owner/acme/tools/skills/checkout"),
        ];
        let RemoteResolution::Ambiguous { addresses } = resolve_remote(&remote, "checkout") else {
            panic!("a shared short name must resolve as ambiguous");
        };
        assert_eq!(
            addresses,
            [
                "github.com/corp/tools/skills/checkout",
                "github.com/owner/acme/tools/skills/checkout",
            ]
        );
    }

    #[test]
    fn resolve_remote_finds_nothing_for_an_unknown_name() {
        let remote = vec![remote_package(
            "checkout",
            "github.com/owner/acme/tools/skills/checkout",
        )];
        assert!(matches!(
            resolve_remote(&remote, "nope"),
            RemoteResolution::None
        ));
    }

    #[test]
    fn a_full_address_never_falls_back_to_a_short_name() {
        // A package whose *name* looks like a full address is not reachable
        // by that name: a full address only matches a package's address.
        let remote = vec![
            remote_package(
                "github.com/owner/wrong/skills/checkout",
                "github.com/owner/acme/tools/skills/checkout",
            ),
            remote_package("deploy", "github.com/owner/acme/tools/skills/deploy"),
        ];
        assert!(matches!(
            resolve_remote(&remote, "github.com/owner/wrong/skills/checkout"),
            RemoteResolution::None
        ));
    }

    #[test]
    fn referenced_paths_keeps_paths_present_verbatim_in_the_body() {
        let paths = vec!["guides/CHECKLIST.md".into(), "docs/RUNBOOK.md".into()];
        assert_eq!(
            referenced_paths("Read guides/CHECKLIST.md first.", &paths),
            ["guides/CHECKLIST.md"]
        );
    }

    #[test]
    fn referenced_paths_matches_a_path_by_its_last_segment_alone() {
        let paths = vec!["guides/FORMS.md".into(), "docs/RUNBOOK.md".into()];
        assert_eq!(
            referenced_paths("Fill in FORMS.md before continuing.", &paths),
            ["guides/FORMS.md"]
        );
    }

    #[test]
    fn referenced_paths_returns_empty_when_the_body_references_no_path() {
        let paths = vec!["guides/CHECKLIST.md".into()];
        assert_eq!(
            referenced_paths("Just do the thing.", &paths),
            Vec::<String>::new()
        );
    }

    #[test]
    fn referenced_paths_keeps_the_input_order() {
        let paths = vec![
            "z/LAST.md".into(),
            "a/FIRST.md".into(),
            "m/MIDDLE.md".into(),
        ];
        assert_eq!(
            referenced_paths("Do a/FIRST.md, then m/MIDDLE.md and LAST.md.", &paths),
            ["z/LAST.md", "a/FIRST.md", "m/MIDDLE.md"],
            "the paths keep their input order, not the body's mention order"
        );
    }

    #[test]
    fn referenced_paths_does_not_duplicate_a_path_that_matches_both_ways() {
        let paths = vec!["guides/ASK.md".into(), "docs/OTHER.md".into()];
        assert_eq!(
            referenced_paths("Read guides/ASK.md and ASK.md again.", &paths),
            ["guides/ASK.md"]
        );
    }
}
