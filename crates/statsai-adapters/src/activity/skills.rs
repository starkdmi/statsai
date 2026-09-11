//! Skill-path classifier. Paths are read in memory and never persisted.

use statsai_core::SkillCatalog;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClassifiedSkill {
    pub name: String,
    pub catalog: SkillCatalog,
    pub plugin: Option<String>,
}

/// Codex structured `read` of `.../skills/<name>/SKILL.md`.
///
/// Counts only when the path ends with `/SKILL.md` and a parent directory is
/// named `skills`, or the path contains `.agents/skills` or `skills/.system`
/// as path segments. Relative and absolute forms classify identically.
#[must_use]
pub fn classify_skill_path(path: &str) -> Option<ClassifiedSkill> {
    let normalized = path.replace('\\', "/");
    if !normalized.ends_with("/SKILL.md") && !normalized.ends_with("/skill.md") {
        return None;
    }
    let without_file = normalized
        .strip_suffix("/SKILL.md")
        .or_else(|| normalized.strip_suffix("/skill.md"))?;
    let segments: Vec<&str> = without_file
        .split('/')
        .filter(|part| !part.is_empty())
        .collect();
    if segments.len() < 2 {
        return None;
    }
    let name = *segments.last()?;
    if name.is_empty() {
        return None;
    }
    let parent_is_skills = segments.get(segments.len().saturating_sub(2)) == Some(&"skills");
    let agents_skills = contains_segment_seq(&segments, &[".agents", "skills"]);
    let system_skills = contains_segment_seq(&segments, &["skills", ".system"]);
    if !parent_is_skills && !agents_skills && !system_skills {
        return None;
    }

    if system_skills {
        return Some(ClassifiedSkill {
            name: name.to_string(),
            catalog: SkillCatalog::System,
            plugin: None,
        });
    }
    if agents_skills {
        return Some(ClassifiedSkill {
            name: name.to_string(),
            catalog: SkillCatalog::Project,
            plugin: None,
        });
    }
    if contains_segment_seq(&segments, &[".codex", "skills"]) {
        return Some(ClassifiedSkill {
            name: name.to_string(),
            catalog: SkillCatalog::User,
            plugin: None,
        });
    }
    if let Some(plugin) = plugin_from_skill_segments(&segments, name) {
        return Some(ClassifiedSkill {
            name: name.to_string(),
            catalog: SkillCatalog::Plugin,
            plugin: Some(plugin),
        });
    }
    Some(ClassifiedSkill {
        name: name.to_string(),
        catalog: SkillCatalog::Unknown,
        plugin: None,
    })
}

fn contains_segment_seq(segments: &[&str], needle: &[&str]) -> bool {
    if needle.is_empty() || segments.len() < needle.len() {
        return false;
    }
    segments
        .windows(needle.len())
        .any(|window| window == needle)
}

/// `/<plugin>/<version>/skills/<name>` immediately before the skill name.
fn plugin_from_skill_segments(segments: &[&str], skill_name: &str) -> Option<String> {
    let len = segments.len();
    if len < 4 {
        return None;
    }
    if segments[len - 1] != skill_name || segments[len - 2] != "skills" {
        return None;
    }
    let version = segments[len - 3];
    let plugin = segments[len - 4];
    if version.is_empty() || version.starts_with('.') {
        return None;
    }
    if plugin.is_empty() || plugin.starts_with('.') || plugin == "skills" {
        return None;
    }
    Some(plugin.to_string())
}

/// Claude `Skill` tool input. `plugin:skill` form sets plugin ownership.
#[must_use]
pub fn classify_claude_skill_input(input: &str) -> ClassifiedSkill {
    let trimmed = input.trim();
    if let Some((plugin, skill)) = trimmed.split_once(':') {
        if !plugin.is_empty() && !skill.is_empty() {
            return ClassifiedSkill {
                name: skill.to_string(),
                catalog: SkillCatalog::Plugin,
                plugin: Some(plugin.to_string()),
            };
        }
    }
    ClassifiedSkill {
        name: trimmed.to_string(),
        catalog: SkillCatalog::User,
        plugin: None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classifies_codex_and_agents_skill_paths() {
        let user = classify_skill_path("/home/fixture/.codex/skills/example-skill/SKILL.md")
            .expect("user skill");
        assert_eq!(user.name, "example-skill");
        assert_eq!(user.catalog, SkillCatalog::User);

        let project = classify_skill_path("/fixture/project/.agents/skills/example-skill/SKILL.md")
            .expect("project skill");
        assert_eq!(project.name, "example-skill");
        assert_eq!(project.catalog, SkillCatalog::Project);

        let relative_project = classify_skill_path(".agents/skills/rust-skills/SKILL.md")
            .expect("relative project skill");
        assert_eq!(relative_project.name, "rust-skills");
        assert_eq!(relative_project.catalog, SkillCatalog::Project);

        let plugin = classify_skill_path(
            "/home/fixture/.codex/plugins/acme-plugin/1.0.0/skills/example-skill/SKILL.md",
        )
        .expect("plugin skill");
        assert_eq!(plugin.name, "example-skill");
        assert_eq!(plugin.catalog, SkillCatalog::Plugin);
        assert_eq!(plugin.plugin.as_deref(), Some("acme-plugin"));

        let cached_plugin = classify_skill_path(
            "/home/fixture/.codex/plugins/cache/control-in-app-browser/0.1.0/skills/visualize/SKILL.md",
        )
        .expect("cached plugin skill");
        assert_eq!(cached_plugin.catalog, SkillCatalog::Plugin);
        assert_eq!(
            cached_plugin.plugin.as_deref(),
            Some("control-in-app-browser")
        );

        let system = classify_skill_path("/opt/codex/skills/.system/example-skill/SKILL.md")
            .expect("system skill");
        assert_eq!(system.catalog, SkillCatalog::System);
    }

    #[test]
    fn unclassifiable_skills_paths_are_unknown_not_user() {
        let classified = classify_skill_path("/tmp/skills/mystery/SKILL.md").expect("unknown");
        assert_eq!(classified.catalog, SkillCatalog::Unknown);
        assert_eq!(classified.name, "mystery");
    }

    #[test]
    fn ignores_arbitrary_skill_md_files() {
        assert!(classify_skill_path("/fixture/project/SKILL.md").is_none());
        assert!(classify_skill_path("/fixture/docs/SKILL.md").is_none());
        assert!(classify_skill_path("/fixture/skills.md").is_none());
    }

    #[test]
    fn classifies_claude_skill_input() {
        let user = classify_claude_skill_input("example-skill");
        assert_eq!(user.catalog, SkillCatalog::User);
        assert_eq!(user.name, "example-skill");
        let plugin = classify_claude_skill_input("acme-plugin:example-skill");
        assert_eq!(plugin.catalog, SkillCatalog::Plugin);
        assert_eq!(plugin.plugin.as_deref(), Some("acme-plugin"));
        assert_eq!(plugin.name, "example-skill");
    }
}
