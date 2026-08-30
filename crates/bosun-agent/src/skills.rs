//! Skill discovery: the working copy's skills are fetched from the node's
//! executor, while the control plane's injected skills are read from its own
//! data directory.

use std::collections::BTreeMap;
use std::path::Path;
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

/// The control plane's injected skills, from its own data directory.
pub fn injected_skills(dir: Option<&Path>) -> Vec<Skill> {
    match dir {
        Some(dir) => bosun_common::skills::parse_skill_dir(dir),
        None => Vec::new(),
    }
}

pub fn read_injected_skill(dir: Option<&Path>, name: &str) -> Option<String> {
    dir.and_then(|dir| bosun_common::skills::read_skill_markdown(dir, name))
}

/// Working-copy skills shadow injected ones with the same name; sorted.
pub fn merge_skills(working: Vec<Skill>, injected: Vec<Skill>) -> Vec<Skill> {
    let mut by_name = BTreeMap::new();
    // The injected skills are inserted first, so a working-copy skill with
    // the same name replaces the injected one on the later insert.
    for skill in injected {
        by_name.insert(skill.name.clone(), skill);
    }
    for skill in working {
        by_name.insert(skill.name.clone(), skill);
    }
    by_name.into_values().collect()
}

#[cfg(test)]
mod tests {
    use std::fs;

    use tempfile::tempdir;

    use super::*;

    /// Writes a skill's directory and SKILL.md.
    fn write_skill(root: &Path, name: &str, content: &str) {
        let dir = root.join(name);
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join("SKILL.md"), content).unwrap();
    }

    #[test]
    fn injected_skills_and_merge_work() {
        let dir = tempdir().unwrap();
        let skills_root = dir.path().join("injected");
        write_skill(
            &skills_root,
            "zeta",
            "---\nname: zeta\ndescription: zeta skill\n---\n\nZeta body",
        );
        write_skill(
            &skills_root,
            "alpha",
            "---\nname: alpha\ndescription: alpha skill\n---\n\nAlpha body",
        );

        let injected = injected_skills(Some(&skills_root));
        assert_eq!(injected.len(), 2);
        assert_eq!(injected[0].name, "alpha");
        assert_eq!(injected[0].description, "alpha skill");
        assert_eq!(injected[1].name, "zeta");
        assert_eq!(injected[1].description, "zeta skill");
        assert!(injected_skills(None).is_empty());

        let merged = merge_skills(
            vec![Skill {
                name: "alpha".into(),
                description: "working alpha".into(),
            }],
            injected.clone(),
        );
        assert_eq!(merged.len(), 2);
        assert_eq!(
            merged[0].description, "working alpha",
            "a working-copy skill shadows the injected one with the same name"
        );
        assert_eq!(merged[1].name, "zeta");

        assert!(
            read_injected_skill(Some(&skills_root), "alpha")
                .unwrap()
                .contains("Alpha body")
        );
        assert_eq!(read_injected_skill(Some(&skills_root), "absent"), None);
        assert_eq!(read_injected_skill(None, "alpha"), None);
    }
}
