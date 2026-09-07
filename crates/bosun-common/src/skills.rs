use std::collections::BTreeMap;
use std::path::Path;
use std::path::PathBuf;

use serde::Deserialize;
use serde::Serialize;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Skill {
    pub name: String,
    pub description: String,
    /// The package address when this skill is a remote package; None for a
    /// working-copy skill.
    #[serde(default)]
    pub package: Option<String>,
}

/// A skill package's stored shape: metadata, instructions, and reference
/// chunks under one immutable address. `sha` is the provenance of the repo
/// commit the package was indexed from, not part of the address.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SkillPackage {
    pub address: String,
    pub repo: String,
    pub name: String,
    pub description: String,
    pub when_to_use: Option<String>,
    pub license: Option<String>,
    pub author: Option<String>,
    pub instructions: String,
    pub references: Vec<SkillReference>,
    pub sha: String,
}

/// One reference chunk inside a package, stored one row per file and read by
/// `package#path`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SkillReference {
    pub path: String,
    pub content: String,
}

/// A skill repo row as listed: the tracked ref, the commit currently indexed
/// (`sha`), and the package count the management UI shows per repo.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SkillRepo {
    pub repo: String,
    pub host: String,
    pub r#ref: Option<String>,
    pub sha: Option<String>,
    pub enabled: bool,
    pub added_at_secs: i64,
    pub updated_at_secs: Option<i64>,
    pub last_error: Option<String>,
    pub package_count: i64,
}

/// A package's advertisement: the full address, the short name, and the
/// description the loop and the web pane show.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SkillAd {
    pub address: String,
    pub name: String,
    pub description: String,
}

impl From<SkillAd> for Skill {
    fn from(ad: SkillAd) -> Self {
        Skill {
            name: ad.name,
            description: ad.description,
            package: Some(ad.address),
        }
    }
}

/// The metadata keys a SKILL.md frontmatter may carry, with the description
/// fallback already applied. `name` is the frontmatter name when present and
/// None otherwise; the caller decides the fallback.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SkillMetadata {
    pub name: Option<String>,
    pub description: String,
    pub when_to_use: Option<String>,
    pub license: Option<String>,
    pub author: Option<String>,
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
    let (frontmatter, _) = frontmatter_split(content);
    let mut name = None;
    if let Some(frontmatter) = frontmatter {
        for line in frontmatter.lines() {
            let Some((key, value)) = line.split_once(':') else {
                continue;
            };
            if key.trim() == "name" {
                name = Some(value.trim().to_string());
            }
        }
    }
    let (metadata, _) = parse_skill_metadata(content);
    Skill {
        name: name.unwrap_or_else(|| dir_name.to_string()),
        description: metadata.description,
        package: None,
    }
}

/// Parses the `---`-delimited frontmatter block at the top of SKILL.md and
/// returns the metadata plus the body after the block. A missing
/// `description` falls back to the first non-empty body line; a file without
/// a frontmatter block — or with an unclosed one — yields metadata with the
/// defaults and the whole content as the body.
pub fn parse_skill_metadata(content: &str) -> (SkillMetadata, String) {
    let (frontmatter, body) = frontmatter_split(content);
    let mut name = None;
    let mut description = None;
    let mut when_to_use = None;
    let mut license = None;
    let mut author = None;
    if let Some(frontmatter) = frontmatter {
        for line in frontmatter.lines() {
            let Some((key, value)) = line.split_once(':') else {
                continue;
            };
            let value = value.trim();
            match key.trim() {
                "name" => name = Some(value.to_string()),
                "description" => description = Some(value.to_string()),
                "when_to_use" => when_to_use = Some(value.to_string()),
                "license" => license = Some(value.to_string()),
                "author" => author = Some(value.to_string()),
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
    (
        SkillMetadata {
            name,
            description: description.unwrap_or(fallback_description),
            when_to_use,
            license,
            author,
        },
        body.to_string(),
    )
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

#[test]
fn an_ad_without_a_package_field_is_a_working_copy_skill() {
    let skill: Skill = serde_json::from_str(r#"{"name":"plain","description":"does things"}"#)
        .expect("an executor skill payload without a package field still parses");
    assert_eq!(skill.name, "plain");
    assert_eq!(skill.description, "does things");
    assert_eq!(skill.package, None);
}

#[test]
fn a_package_field_round_trips_through_json() {
    let skill = Skill {
        name: "checkout".into(),
        description: "does checkout".into(),
        package: Some("github.com/owner/acme/tools/skills/checkout".into()),
    };
    let parsed: Skill = serde_json::from_value(serde_json::to_value(&skill).unwrap()).unwrap();
    assert_eq!(skill, parsed);
}

#[test]
fn a_skill_ad_maps_to_a_remote_package_skill() {
    let ad = SkillAd {
        address: "github.com/owner/acme/tools/skills/checkout".into(),
        name: "checkout".into(),
        description: "does checkout".into(),
    };
    let skill = Skill::from(ad);
    assert_eq!(skill.name, "checkout");
    assert_eq!(skill.description, "does checkout");
    assert_eq!(
        skill.package.as_deref(),
        Some("github.com/owner/acme/tools/skills/checkout")
    );
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

    #[test]
    fn parses_full_frontmatter_metadata_and_the_body() {
        let (metadata, body) = parse_skill_metadata(
            "---\nname: Doer\ndescription: Does the thing\nwhen_to_use: When needed\nlicense: MIT\nauthor: Ada\n---\n\nInstructions here.\n",
        );
        assert_eq!(metadata.name.as_deref(), Some("Doer"));
        assert_eq!(metadata.description, "Does the thing");
        assert_eq!(metadata.when_to_use.as_deref(), Some("When needed"));
        assert_eq!(metadata.license.as_deref(), Some("MIT"));
        assert_eq!(metadata.author.as_deref(), Some("Ada"));
        assert_eq!(body, "\n\nInstructions here.\n");
    }

    #[test]
    fn metadata_description_falls_back_to_the_first_body_line() {
        let (metadata, _) =
            parse_skill_metadata("---\nwhen_to_use: With care\n---\n\nFallback line.\n\nRest.\n");
        assert_eq!(metadata.description, "Fallback line.");
        assert_eq!(metadata.when_to_use.as_deref(), Some("With care"));
        assert_eq!(metadata.name, None);
        assert_eq!(metadata.license, None);
        assert_eq!(metadata.author, None);
    }

    #[test]
    fn a_file_without_frontmatter_is_all_body() {
        let (metadata, body) = parse_skill_metadata("Plain text\nwith a fallback line.\n");
        assert_eq!(metadata.description, "Plain text");
        assert_eq!(metadata.name, None);
        assert_eq!(metadata.when_to_use, None);
        assert_eq!(metadata.license, None);
        assert_eq!(metadata.author, None);
        assert_eq!(body, "Plain text\nwith a fallback line.\n");
    }

    #[test]
    fn an_unclosed_frontmatter_block_is_not_frontmatter() {
        let content = "---\ndescription: never closed\n\nthe rest";
        let (metadata, body) = parse_skill_metadata(content);
        assert_eq!(metadata.description, "---");
        assert_eq!(metadata.when_to_use, None);
        assert_eq!(body, content);
    }

    #[test]
    fn an_empty_body_leaves_the_description_empty() {
        let (metadata, body) = parse_skill_metadata("---\n---\n");
        assert_eq!(metadata.description, "");
        assert_eq!(body, "\n");
    }
}
