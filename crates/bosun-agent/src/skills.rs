//! Skill discovery: skills from the session's working copy and the control
//! plane's injected skills directory, with metadata parsed from SKILL.md
//! frontmatter.

use std::collections::BTreeMap;
use std::path::Path;
use std::path::PathBuf;

/// One discovered skill. `path` is the directory containing SKILL.md.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Skill {
    pub name: String,
    pub description: String,
    pub path: PathBuf,
}

/// Scans `<session_dir>/.agents/skills/*/SKILL.md` and, when given,
/// `<injected_dir>/*/SKILL.md`. A same-named skill in the working copy
/// shadows the injected one. Returns the skills sorted by name.
pub fn discover_skills(session_dir: &Path, injected_dir: Option<&Path>) -> Vec<Skill> {
    let mut by_name = BTreeMap::new();
    // The injected skills are scanned first, so a working-copy skill with the
    // same name replaces the injected one on the later insert.
    if let Some(injected_dir) = injected_dir {
        scan_skills_dir(&mut by_name, injected_dir);
    }
    scan_skills_dir(&mut by_name, &session_dir.join(".agents").join("skills"));
    by_name.into_values().collect()
}

/// The whole SKILL.md as text, lossily decoded. Errors when the file is
/// missing or unreadable.
pub fn skill_markdown(skill: &Skill) -> std::io::Result<String> {
    let bytes = std::fs::read(skill.path.join("SKILL.md"))?;
    Ok(String::from_utf8_lossy(&bytes).into_owned())
}

/// Inserts every `<skills_root>/<name>/SKILL.md` into `by_name`, skipping
/// entries that are not directories, are hidden, or fail to read.
fn scan_skills_dir(by_name: &mut BTreeMap<String, Skill>, skills_root: &Path) {
    let Ok(entries) = std::fs::read_dir(skills_root) else {
        return;
    };
    for entry in entries.flatten() {
        let dir = entry.path();
        let Some(name) = dir.file_name().and_then(|name| name.to_str()) else {
            continue;
        };
        if name.starts_with('.') || !dir.is_dir() {
            continue;
        }
        let Some(skill) = skill_in_dir(&dir) else {
            continue;
        };
        by_name.insert(skill.name.clone(), skill);
    }
}

fn skill_in_dir(dir: &Path) -> Option<Skill> {
    let name = dir.file_name()?.to_str()?.to_string();
    let placeholder = Skill {
        name: name.clone(),
        description: String::new(),
        path: dir.to_path_buf(),
    };
    let content = skill_markdown(&placeholder).ok()?;
    Some(parse_skill(&placeholder, &content))
}

/// Parses the `---`-delimited frontmatter block at the top of SKILL.md for
/// `name:` and `description:` keys. A missing key falls back to the directory
/// name and the first non-empty body line.
fn parse_skill(skill: &Skill, content: &str) -> Skill {
    let (frontmatter, body) = frontmatter_split(content);
    let mut name = None;
    let mut description = None;
    if let Some(frontmatter) = frontmatter {
        for line in frontmatter.lines() {
            let Some((key, value)) = line.split_once(':') else {
                continue;
            };
            let value = value.trim();
            match key.trim() {
                "name" => name = Some(value.to_string()),
                "description" => description = Some(value.to_string()),
                _ => {}
            }
        }
    }
    let fallback_description = body
        .lines()
        .map(str::trim)
        .find(|line| !line.is_empty())
        .unwrap_or_default()
        .to_string();
    Skill {
        name: name.unwrap_or_else(|| skill.name.clone()),
        description: description.unwrap_or(fallback_description),
        path: skill.path.clone(),
    }
}

/// Splits `content` into the frontmatter block and the body. Returns
/// `(None, content)` when the file does not start with `---` or never closes
/// the block.
fn frontmatter_split(content: &str) -> (Option<&str>, &str) {
    let Some(after_open) = content.strip_prefix("---") else {
        return (None, content);
    };
    let Some(end) = after_open.find("---") else {
        return (None, content);
    };
    (Some(&after_open[..end]), &after_open[end + 3..])
}

#[cfg(test)]
mod tests {
    use std::fs;

    use tempfile::tempdir;

    use super::*;

    /// Writes a skill's directory and SKILL.md, returning the directory.
    fn write_skill(root: &Path, name: &str, content: &str) -> PathBuf {
        let dir = root.join(name);
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join("SKILL.md"), content).unwrap();
        dir
    }

    #[test]
    fn discovers_a_session_skill_and_reads_its_markdown() {
        let dir = tempdir().unwrap();
        let skill_dir = write_skill(
            &dir.path().join(".agents").join("skills"),
            "my-skill",
            "---\nname: my-skill\ndescription: Does things\n---\n\nBody text",
        );

        let skills = discover_skills(dir.path(), None);
        assert_eq!(skills.len(), 1);
        assert_eq!(skills[0].name, "my-skill");
        assert_eq!(skills[0].description, "Does things");
        assert_eq!(skills[0].path, skill_dir);
        assert_eq!(
            skill_markdown(&skills[0]).unwrap(),
            "---\nname: my-skill\ndescription: Does things\n---\n\nBody text"
        );
    }

    #[test]
    fn a_skill_without_frontmatter_uses_the_directory_name_and_first_line() {
        let dir = tempdir().unwrap();
        write_skill(
            &dir.path().join(".agents").join("skills"),
            "plain-skill",
            "\nDo the thing.\n\nDetails.\n",
        );

        let skills = discover_skills(dir.path(), None);
        assert_eq!(skills.len(), 1);
        assert_eq!(skills[0].name, "plain-skill");
        assert_eq!(skills[0].description, "Do the thing.");
    }

    #[test]
    fn the_session_skill_shadows_the_injected_one_with_the_same_name() {
        let dir = tempdir().unwrap();
        let session_skill = write_skill(
            &dir.path().join(".agents").join("skills"),
            "shared",
            "---\nname: shared\ndescription: session version\n---\n",
        );
        write_skill(
            &dir.path().join("injected"),
            "shared",
            "---\nname: shared\ndescription: injected version\n---\n",
        );

        let skills = discover_skills(dir.path(), Some(&dir.path().join("injected")));
        assert_eq!(skills.len(), 1);
        assert_eq!(skills[0].description, "session version");
        assert_eq!(skills[0].path, session_skill);
    }

    #[test]
    fn injected_skills_are_discovered_sorted_by_name() {
        let dir = tempdir().unwrap();
        write_skill(&dir.path().join("injected"), "zeta", "...\n");
        write_skill(&dir.path().join("injected"), "alpha", "...\n");

        let skills = discover_skills(dir.path(), Some(&dir.path().join("injected")));
        let names: Vec<&str> = skills.iter().map(|skill| skill.name.as_str()).collect();
        assert_eq!(names, ["alpha", "zeta"]);
    }

    #[test]
    fn hidden_absent_and_non_directory_entries_are_skipped() {
        let dir = tempdir().unwrap();
        assert!(
            discover_skills(dir.path(), None).is_empty(),
            "a session without a skills dir discovers nothing"
        );
        assert!(
            discover_skills(dir.path(), Some(&dir.path().join("missing"))).is_empty(),
            "an absent injected dir is skipped"
        );

        let skills_root = dir.path().join(".agents").join("skills");
        write_skill(&skills_root, ".hidden", "---\nname: .hidden\n---\n");
        write_skill(&skills_root, "visible", "---\nname: visible\n---\n");
        fs::write(skills_root.join("SKILL.md"), "not a directory").unwrap();

        let skills = discover_skills(dir.path(), None);
        let names: Vec<&str> = skills.iter().map(|skill| skill.name.as_str()).collect();
        assert_eq!(names, ["visible"]);
    }
}
