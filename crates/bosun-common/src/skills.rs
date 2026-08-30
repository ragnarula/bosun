use std::collections::BTreeMap;
use std::path::Path;
use std::path::PathBuf;

use serde::Deserialize;
use serde::Serialize;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Skill {
    pub name: String,
    pub description: String,
}

/// One discovered skill together with the directory holding its SKILL.md,
/// keyed by the parsed name so a frontmatter name can be looked up even when
/// it differs from the directory name.
struct SkillDir {
    skill: Skill,
    dir: PathBuf,
}

/// Scans `<skills_root>/<name>/SKILL.md` and returns the skills with parsed
/// metadata, sorted by name. Missing, hidden, or unreadable entries are
/// skipped.
pub fn parse_skill_dir(skills_root: &Path) -> Vec<Skill> {
    scan_skills_dir(skills_root)
        .into_values()
        .map(|entry| entry.skill)
        .collect()
}

/// Returns the full text of the skill named `name`, matching against the
/// *parsed* name (the frontmatter may name the skill differently from its
/// directory). None when no skill matches.
pub fn read_skill_markdown(skills_root: &Path, name: &str) -> Option<String> {
    let dir = scan_skills_dir(skills_root).remove(name)?.dir;
    let bytes = std::fs::read(dir.join("SKILL.md")).ok()?;
    Some(String::from_utf8_lossy(&bytes).into_owned())
}

/// Inserts every `<skills_root>/<name>/SKILL.md` into a map keyed by the
/// parsed name, skipping entries that are not directories, are hidden, or
/// fail to read.
fn scan_skills_dir(skills_root: &Path) -> BTreeMap<String, SkillDir> {
    let mut by_name = BTreeMap::new();
    let Ok(entries) = std::fs::read_dir(skills_root) else {
        return by_name;
    };
    for entry in entries.flatten() {
        let dir = entry.path();
        let Some(name) = dir.file_name().and_then(|name| name.to_str()) else {
            continue;
        };
        if name.starts_with('.') || !dir.is_dir() {
            continue;
        }
        let Some(entry) = skill_in_dir(&dir) else {
            continue;
        };
        by_name.insert(entry.skill.name.clone(), entry);
    }
    by_name
}

fn skill_in_dir(dir: &Path) -> Option<SkillDir> {
    let name = dir.file_name()?.to_str()?.to_string();
    let bytes = std::fs::read(dir.join("SKILL.md")).ok()?;
    let content = String::from_utf8_lossy(&bytes).into_owned();
    Some(SkillDir {
        skill: parse_skill(&name, &content),
        dir: dir.to_path_buf(),
    })
}

/// Parses the `---`-delimited frontmatter block at the top of SKILL.md for
/// `name:` and `description:` keys. A missing key falls back to the directory
/// name and the first non-empty body line.
fn parse_skill(dir_name: &str, content: &str) -> Skill {
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
        name: name.unwrap_or_else(|| dir_name.to_string()),
        description: description.unwrap_or(fallback_description),
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
    fn discovers_a_skill_with_frontmatter() {
        let root = tempdir().unwrap();
        let skills_root = root.path().join("skills");
        write_skill(
            &skills_root,
            "my-skill",
            "---\nname: my-skill\ndescription: Does things\n---\n\nBody text",
        );

        let skills = parse_skill_dir(&skills_root);
        assert_eq!(skills.len(), 1);
        assert_eq!(skills[0].name, "my-skill");
        assert_eq!(skills[0].description, "Does things");
        assert_eq!(
            read_skill_markdown(&skills_root, "my-skill").unwrap(),
            "---\nname: my-skill\ndescription: Does things\n---\n\nBody text"
        );
    }

    #[test]
    fn a_skill_without_frontmatter_uses_the_directory_name_and_first_line() {
        let root = tempdir().unwrap();
        let skills_root = root.path().join("skills");
        write_skill(&skills_root, "plain-skill", "\nDo the thing.\n\nDetails.\n");

        let skills = parse_skill_dir(&skills_root);
        assert_eq!(skills.len(), 1);
        assert_eq!(skills[0].name, "plain-skill");
        assert_eq!(skills[0].description, "Do the thing.");
    }

    #[test]
    fn skills_are_discovered_sorted_by_name() {
        let root = tempdir().unwrap();
        let skills_root = root.path().join("skills");
        write_skill(&skills_root, "zeta", "...\n");
        write_skill(&skills_root, "alpha", "...\n");

        let skills = parse_skill_dir(&skills_root);
        let names: Vec<&str> = skills.iter().map(|skill| skill.name.as_str()).collect();
        assert_eq!(names, ["alpha", "zeta"]);
    }

    #[test]
    fn hidden_absent_and_non_directory_entries_are_skipped() {
        let root = tempdir().unwrap();
        let skills_root = root.path().join("skills");
        assert!(
            parse_skill_dir(&skills_root).is_empty(),
            "a missing skills dir discovers nothing"
        );

        write_skill(&skills_root, ".hidden", "---\nname: .hidden\n---\n");
        write_skill(&skills_root, "visible", "---\nname: visible\n---\n");
        fs::write(skills_root.join("SKILL.md"), "not a directory").unwrap();

        let skills = parse_skill_dir(&skills_root);
        let names: Vec<&str> = skills.iter().map(|skill| skill.name.as_str()).collect();
        assert_eq!(names, ["visible"]);
    }

    #[test]
    fn read_skill_markdown_matches_the_frontmatter_name() {
        let root = tempdir().unwrap();
        let skills_root = root.path().join("skills");
        write_skill(
            &skills_root,
            "directory-name",
            "---\nname: parsed-name\ndescription: Named in frontmatter\n---\n\nBody",
        );

        assert_eq!(
            read_skill_markdown(&skills_root, "parsed-name").unwrap(),
            "---\nname: parsed-name\ndescription: Named in frontmatter\n---\n\nBody"
        );
        assert_eq!(read_skill_markdown(&skills_root, "directory-name"), None);
        assert_eq!(read_skill_markdown(&skills_root, "absent"), None);
    }
}
