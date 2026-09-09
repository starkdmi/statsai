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
/// Counts only when the path ends with `/SKILL.md` and the grandparent directory
/// is named `skills`, or the path contains `/.agents/skills/`.
#[must_use]
pub fn classify_skill_path(path: &str) -> Option<ClassifiedSkill> {
    let normalized = path.replace('\\', "/");
    if !normalized.ends_with("/SKILL.md") && !normalized.ends_with("/skill.md") {
        return None;
    }
    let without_file = normalized
        .strip_suffix("/SKILL.md")
        .or_else(|| normalized.strip_suffix("/skill.md"))?;
    let (parent, name) = without_file.rsplit_once('/')?;
    if name.is_empty() {
        return None;
    }
    let grandparent = parent.rsplit_once('/').map(|(rest, _)| rest);
    let grandparent_is_skills = parent.ends_with("/skills") || parent.ends_with("/skills/");
    let agents_skills = normalized.contains("/.agents/skills/");
    let system_skills = normalized.contains("/skills/.system/");
    if !grandparent_is_skills && !agents_skills && !system_skills {
        return None;
    }
    let _ = grandparent;

    if normalized.contains("/skills/.system/") {
        return Some(ClassifiedSkill {
            name: name.to_string(),
            catalog: SkillCatalog::System,
            plugin: None,
        });
    }
    if normalized.contains("/.agents/skills/") {
        return Some(ClassifiedSkill {
            name: name.to_string(),
            catalog: SkillCatalog::Project,
            plugin: None,
        });
    }
    if normalized.contains("/.codex/skills/") {
        return Some(ClassifiedSkill {
            name: name.to_string(),
            catalog: SkillCatalog::User,
            plugin: None,
        });
    }
    if let Some(plugin) = plugin_from_skill_path(without_file, name) {
        return Some(ClassifiedSkill {
            name: name.to_string(),
            catalog: SkillCatalog::Plugin,
            plugin: Some(plugin),
        });
    }
    Some(ClassifiedSkill {
        name: name.to_string(),
        catalog: SkillCatalog::User,
        plugin: None,
    })
}

/// `/<plugin>/<version>/skills/<name>` immediately before the skill name.
fn plugin_from_skill_path(without_file: &str, skill_name: &str) -> Option<String> {
    let suffix = format!("/skills/{skill_name}");
    let prefix = without_file.strip_suffix(&suffix)?;
    let (plugin_root, version) = prefix.rsplit_once('/')?;
    if version.is_empty() || version.starts_with('.') {
        return None;
    }
    let plugin = plugin_root.rsplit_once('/')?.1;
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

        let plugin = classify_skill_path(
            "/home/fixture/.codex/plugins/acme-plugin/1.0.0/skills/example-skill/SKILL.md",
        )
        .expect("plugin skill");
        assert_eq!(plugin.name, "example-skill");
        assert_eq!(plugin.catalog, SkillCatalog::Plugin);
        assert_eq!(plugin.plugin.as_deref(), Some("acme-plugin"));

        let system = classify_skill_path("/opt/codex/skills/.system/example-skill/SKILL.md")
            .expect("system skill");
        assert_eq!(system.catalog, SkillCatalog::System);
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
