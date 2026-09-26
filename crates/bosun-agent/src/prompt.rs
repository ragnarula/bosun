//! The layered system prompt: the fixed harness contract, the persona role,
//! and the session's live context.

use bosun_common::session::SessionState;
use serde_json::Value;

use crate::skills::Skill;

/// The fixed harness contract, included at compile time. It is the first text
/// of every request and no persona or untrusted input changes it.
pub const HARNESS_CONTRACT: &str = include_str!("prompts/harness.md");

/// One child session in the per-wake manifest: id, persona, state, and its
/// last authored message to this session, when it has authored one.
#[derive(Debug, Clone)]
pub(crate) struct LiveChild {
    pub(crate) id: String,
    pub(crate) persona: Option<String>,
    pub(crate) state: SessionState,
    pub(crate) last_authored: Option<String>,
}

/// How many characters of a skill's description the advertisement shows; the
/// full text loads on demand through the `skill` tool.
const SKILL_DESCRIPTION_CAP: usize = 500;

/// One skill as the system prompt advertises it: the name with its
/// provenance — the package address for a remote package, "working copy"
/// otherwise — and the description truncated to the advertisement cap.
fn skill_ad_line(skill: &Skill) -> String {
    let provenance = skill.package.as_deref().unwrap_or("working copy");
    let description: String = skill
        .description
        .chars()
        .take(SKILL_DESCRIPTION_CAP)
        .collect();
    format!("- {} ({}): {}", skill.name, provenance, description)
}

/// A session state as the manifest renders it: the wire-format names the
/// store uses.
fn state_name(state: SessionState) -> &'static str {
    match state {
        SessionState::Creating => "creating",
        SessionState::Running => "running",
        SessionState::WaitingForInput => "waiting_for_input",
        SessionState::Interrupted => "interrupted",
        SessionState::Stopped => "stopped",
    }
}

/// Builds the system prompt in layers: the fixed harness contract first, then
/// the persona's role text when it has one, then the parts of the session's
/// context that hold for the whole session — the repo-standard files present
/// in the working copy, the persona catalog for spawn-capable sessions, and
/// skill advertisements. The system prompt is never stored.
///
/// Everything here must stay the same from one request to the next. Providers
/// cache a request's prefix, and the system prompt is the start of that prefix:
/// a change here makes the provider read the whole thread again at full price.
/// Context that changes during a session belongs in [`session_context`].
pub(crate) fn system_prompt(
    persona: Option<&str>,
    repo_standards: &[String],
    skills: &[Skill],
    catalog: Option<Vec<(String, String)>>,
) -> String {
    let mut prompt = HARNESS_CONTRACT.to_string();
    if let Some(persona) = persona {
        prompt.push_str("\n\n");
        prompt.push_str(persona);
    }
    if !repo_standards.is_empty() {
        prompt.push_str(&format!(
            "\n\nRepo standards present: {}. The contents are not in this context; \
             read the files with the file tools when your task needs them.",
            repo_standards.join(", ")
        ));
    }
    if let Some(catalog) = catalog {
        prompt.push_str("\n\nPersonas you may spawn:");
        for (name, description) in catalog {
            if description.is_empty() {
                prompt.push_str(&format!("\n- {name}"));
            } else {
                prompt.push_str(&format!("\n- {name}: {description}"));
            }
        }
    }
    if !skills.is_empty() {
        prompt.push_str("\n\nSkills available in this session:");
        for skill in skills {
            prompt.push_str(&format!("\n{}", skill_ad_line(skill)));
        }
    }
    prompt
}

/// The heading of the session context block. It says who wrote the block,
/// because the block is appended to a message someone else wrote.
const SESSION_CONTEXT_HEADING: &str = "[session context] Written by the harness for this request: \
     the session's state as it stands now. It is not part of the message it is attached to, \
     and it replaces any earlier session context.";

/// The parts of the session's context that change during a session: the todo
/// list, and the manifest of children whose state or latest authored event
/// this wake is reacting to. `None` when there is neither.
///
/// The serializers append this block to the end of the request, after the
/// thread, so a change to it leaves the cached prefix of the thread intact.
pub(crate) fn session_context(todos: &[Value], live: &[LiveChild]) -> Option<String> {
    if todos.is_empty() && live.is_empty() {
        return None;
    }
    let mut context = SESSION_CONTEXT_HEADING.to_string();
    if !todos.is_empty() {
        context.push_str("\n\nCurrent todo list:");
        for (index, todo) in todos.iter().enumerate() {
            let content = todo["content"].as_str().unwrap_or_default();
            let status = todo["status"].as_str().unwrap_or_default();
            context.push_str(&format!("\n{index}. [{status}] {content}"));
        }
    }
    if !live.is_empty() {
        context.push_str("\n\nLive children:");
        for child in live {
            let persona = child.persona.as_deref().unwrap_or("default");
            let last = child.last_authored.as_deref().unwrap_or("none");
            context.push_str(&format!(
                "\n- {} (persona: {persona}, state: {}, last message: {last})",
                child.id,
                state_name(child.state)
            ));
        }
    }
    Some(context)
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    fn skill(name: &str) -> Skill {
        Skill {
            name: name.into(),
            description: "Does things".into(),
            package: None,
        }
    }

    fn child(id: &str) -> LiveChild {
        LiveChild {
            id: id.into(),
            persona: Some("coder".into()),
            state: SessionState::Running,
            last_authored: None,
        }
    }

    #[test]
    fn the_harness_contract_is_within_the_length_cap() {
        // The cap keeps the fixed contract small, so it stays readable as
        // prose and cheap to send on every request.
        assert!(!HARNESS_CONTRACT.is_empty());
        assert!(
            HARNESS_CONTRACT.chars().count() <= 4000,
            "the contract is {} characters, over the 4000 cap",
            HARNESS_CONTRACT.chars().count()
        );
    }

    #[test]
    fn a_persona_body_follows_the_contract() {
        let persona = "You are a reviewer.";
        let prompt = system_prompt(Some(persona), &[], &[], None);
        assert!(prompt.starts_with(&format!("{HARNESS_CONTRACT}\n\n{persona}")));
    }

    #[test]
    fn no_persona_leaves_the_contract_alone() {
        let prompt = system_prompt(None, &[], &[], None);
        assert_eq!(prompt, HARNESS_CONTRACT);
    }

    #[test]
    fn context_blocks_follow_the_persona() {
        let prompt = system_prompt(
            Some("You are a reviewer."),
            &["STANDARDS.md".into()],
            &[skill("checkout")],
            Some(vec![("coder".into(), "writes code".into())]),
        );
        let persona_at = prompt.find("You are a reviewer.").expect("persona text");
        let standards_at = prompt.find("Repo standards present:").expect("standards");
        let catalog_at = prompt.find("Personas you may spawn:").expect("catalog");
        let skills_at = prompt
            .find("Skills available in this session:")
            .expect("skills");
        assert!(
            persona_at < standards_at && standards_at < catalog_at && catalog_at < skills_at,
            "the context blocks follow the persona in order: {prompt}"
        );
    }

    #[test]
    fn a_session_with_no_todos_and_no_live_children_has_no_session_context() {
        assert_eq!(session_context(&[], &[]), None);
    }

    #[test]
    fn the_session_context_names_its_author_then_lists_todos_then_children() {
        let context = session_context(
            &[json!({ "content": "write tests", "status": "todo" })],
            &[child("child-1")],
        )
        .expect("a context with a todo and a child");
        assert!(context.starts_with(SESSION_CONTEXT_HEADING));
        let todos_at = context.find("0. [todo] write tests").expect("todos");
        let live_at = context
            .find("- child-1 (persona: coder, state: running, last message: none)")
            .expect("live children");
        assert!(todos_at < live_at, "todos come before children: {context}");
    }

    #[test]
    fn skill_ad_line_renders_working_copy_provenance() {
        let skill = Skill {
            name: "my-skill".into(),
            description: "Does things".into(),
            package: None,
        };
        assert_eq!(
            skill_ad_line(&skill),
            "- my-skill (working copy): Does things"
        );
    }

    #[test]
    fn skill_ad_line_renders_the_package_address_for_a_remote_skill() {
        let skill = Skill {
            name: "checkout".into(),
            description: "does checkout".into(),
            package: Some("github.com/owner/acme/tools/skills/checkout".into()),
        };
        assert_eq!(
            skill_ad_line(&skill),
            "- checkout (github.com/owner/acme/tools/skills/checkout): does checkout"
        );
    }

    #[test]
    fn skill_ad_line_truncates_the_description_at_the_cap() {
        let skill = Skill {
            name: "wordy".into(),
            description: "x".repeat(600),
            package: None,
        };
        assert_eq!(
            skill_ad_line(&skill),
            format!("- wordy (working copy): {}", "x".repeat(500)),
            "the advertisement keeps only the first 500 characters"
        );
        let unicode = Skill {
            name: "accented".into(),
            description: "é".repeat(600),
            package: None,
        };
        assert_eq!(
            skill_ad_line(&unicode),
            format!("- accented (working copy): {}", "é".repeat(500)),
            "the truncation cuts at a character boundary"
        );
    }
}
