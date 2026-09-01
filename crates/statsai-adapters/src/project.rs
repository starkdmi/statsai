use statsai_core::{display_path, hash_text, normalize_git_remote, path_hash, ProjectInfo};
use std::collections::HashMap;
use std::path::{Path, PathBuf};

pub(crate) type ProjectContextCacheKey = (Option<PathBuf>, Option<String>, Option<String>);
pub(crate) type ProjectContextCache = HashMap<ProjectContextCacheKey, Option<ProjectInfo>>;

#[derive(Debug, Clone, Default)]
pub(crate) struct ProjectContext {
    pub(crate) project_label: Option<String>,
    pub(crate) repo_remote_hash: Option<String>,
    pub(crate) repo_label: Option<String>,
    pub(crate) branch_hash: Option<String>,
    pub(crate) branch_label: Option<String>,
    pub(crate) path_hash: Option<String>,
    pub(crate) path_label: Option<String>,
}

impl ProjectContext {
    pub(crate) fn into_project_info(self) -> Option<ProjectInfo> {
        let identity_key = if let Some(path_hash) = self.path_hash.as_deref() {
            format!(
                "path:{path_hash}:repo:{}",
                self.repo_remote_hash.as_deref().unwrap_or("none")
            )
        } else {
            let repo_remote_hash = self.repo_remote_hash.as_deref()?;
            format!("repo:{repo_remote_hash}")
        };

        Some(ProjectInfo {
            project_id: format!("project_{}", &hash_text(&identity_key)[..24]),
            project_label: self.project_label,
            repo_remote_hash: self.repo_remote_hash,
            repo_label: self.repo_label,
            branch_hash: self.branch_hash,
            branch_label: self.branch_label,
            path_hash: self.path_hash,
            path_label: self.path_label,
        })
    }
}

pub(crate) fn resolve_project_context_cached(
    project_path: Option<PathBuf>,
    repository_url: Option<String>,
    branch: Option<String>,
    cache: &mut ProjectContextCache,
) -> Option<ProjectInfo> {
    let cache_key = (project_path.clone(), repository_url.clone(), branch.clone());
    if let Some(project) = cache.get(&cache_key) {
        return project.clone();
    }
    let project = resolve_project_context(project_path, repository_url, branch);
    cache.insert(cache_key, project.clone());
    project
}

pub(crate) fn resolve_project_context(
    project_path: Option<PathBuf>,
    repository_url: Option<String>,
    branch: Option<String>,
) -> Option<ProjectInfo> {
    let git = project_path
        .as_deref()
        .and_then(read_git_repository_metadata);
    let normalized_remote = repository_url
        .as_deref()
        .and_then(normalize_git_remote)
        .or_else(|| {
            git.as_ref()
                .and_then(|metadata| metadata.normalized_remote.clone())
        });
    let repo_remote_hash = normalized_remote.as_ref().map(|remote| hash_text(remote));
    let repo_label = normalized_remote
        .as_deref()
        .map(repo_label_from_normalized_remote)
        .or_else(|| {
            git.as_ref()
                .and_then(|metadata| metadata.repo_label.clone())
        });
    let branch_label = branch.or_else(|| {
        git.as_ref()
            .and_then(|metadata| metadata.branch_label.clone())
    });
    let branch_hash = branch_label.as_ref().map(|branch| hash_text(branch));
    let project_label = project_path
        .as_deref()
        .and_then(project_label_from_path)
        .or_else(|| repo_label.clone());
    let path_hash_value = project_path.as_deref().map(path_hash);
    let path_label = project_path.as_deref().map(display_path);

    ProjectContext {
        project_label,
        repo_remote_hash,
        repo_label,
        branch_hash,
        branch_label,
        path_hash: path_hash_value,
        path_label,
    }
    .into_project_info()
}

pub(crate) fn project_context_from_path_fallback(root: &Path, path: &Path) -> Option<ProjectInfo> {
    let project_key = project_key_from_path(root, path)?;
    if matches!(project_key.as_str(), "sessions" | "archived_sessions") {
        return None;
    }
    let project_path = root.join(&project_key);
    ProjectContext {
        project_label: Some(project_key),
        path_hash: Some(path_hash(&project_path)),
        path_label: Some(display_path(&project_path)),
        ..ProjectContext::default()
    }
    .into_project_info()
}

#[derive(Debug, Clone, Default)]
pub(crate) struct GitRepositoryMetadata {
    normalized_remote: Option<String>,
    repo_label: Option<String>,
    branch_label: Option<String>,
}

pub(crate) fn read_git_repository_metadata(path: &Path) -> Option<GitRepositoryMetadata> {
    let repo_root = find_git_repo_root(path)?;
    let git_dir = git_dir_for_repo_root(&repo_root)?;
    let common_dir = git_common_dir(&git_dir).unwrap_or_else(|| git_dir.clone());
    let config_path = if git_dir.join("config").is_file() {
        git_dir.join("config")
    } else {
        common_dir.join("config")
    };
    let remote = read_git_remote_url(&config_path);
    let normalized_remote = remote.as_deref().and_then(normalize_git_remote);
    let repo_label = normalized_remote
        .as_deref()
        .map(repo_label_from_normalized_remote)
        .or_else(|| project_label_from_path(&repo_root));

    Some(GitRepositoryMetadata {
        normalized_remote,
        repo_label,
        branch_label: read_git_head_branch(&git_dir),
    })
}

pub(crate) fn find_git_repo_root(path: &Path) -> Option<PathBuf> {
    let mut current = if path.is_dir() {
        path.to_path_buf()
    } else {
        path.parent()?.to_path_buf()
    };
    loop {
        if current.join(".git").exists() {
            return Some(current);
        }
        if !current.pop() {
            return None;
        }
    }
}

pub(crate) fn git_dir_for_repo_root(repo_root: &Path) -> Option<PathBuf> {
    let dot_git = repo_root.join(".git");
    if dot_git.is_dir() {
        return Some(dot_git);
    }
    let text = std::fs::read_to_string(dot_git).ok()?;
    let gitdir = text.trim().strip_prefix("gitdir:")?.trim();
    let path = PathBuf::from(gitdir);
    if path.is_absolute() {
        Some(path)
    } else {
        Some(repo_root.join(path))
    }
}

pub(crate) fn git_common_dir(git_dir: &Path) -> Option<PathBuf> {
    let text = std::fs::read_to_string(git_dir.join("commondir")).ok()?;
    let value = text.trim();
    if value.is_empty() {
        return None;
    }
    let path = PathBuf::from(value);
    if path.is_absolute() {
        Some(path)
    } else {
        Some(git_dir.join(path))
    }
}

pub(crate) fn read_git_remote_url(config_path: &Path) -> Option<String> {
    let text = std::fs::read_to_string(config_path).ok()?;
    let mut current_remote: Option<String> = None;
    let mut first_remote_url: Option<String> = None;
    let mut origin_remote_url: Option<String> = None;

    for line in text.lines() {
        let trimmed = line.trim();
        if trimmed.starts_with("[remote \"") && trimmed.ends_with("\"]") {
            current_remote = trimmed
                .trim_start_matches("[remote \"")
                .trim_end_matches("\"]")
                .split('"')
                .next()
                .map(ToOwned::to_owned);
            continue;
        }
        if trimmed.starts_with('[') {
            current_remote = None;
            continue;
        }
        let Some(remote_name) = current_remote.as_deref() else {
            continue;
        };
        let Some((key, value)) = trimmed.split_once('=') else {
            continue;
        };
        if key.trim() != "url" {
            continue;
        }
        let url = value.trim().to_string();
        if first_remote_url.is_none() {
            first_remote_url = Some(url.clone());
        }
        if remote_name == "origin" {
            origin_remote_url = Some(url);
        }
    }

    origin_remote_url.or(first_remote_url)
}

pub(crate) fn read_git_head_branch(git_dir: &Path) -> Option<String> {
    let text = std::fs::read_to_string(git_dir.join("HEAD")).ok()?;
    let head = text.trim();
    head.strip_prefix("ref: refs/heads/").map(ToOwned::to_owned)
}

pub(crate) fn repo_label_from_normalized_remote(remote: &str) -> String {
    let parts: Vec<&str> = remote.split('/').filter(|part| !part.is_empty()).collect();
    if parts.len() >= 3 {
        format!("{}/{}", parts[parts.len() - 2], parts[parts.len() - 1])
    } else {
        remote.to_string()
    }
}

pub(crate) fn project_label_from_path(path: &Path) -> Option<String> {
    path.file_name()
        .and_then(|name| name.to_str())
        .filter(|name| !name.is_empty())
        .map(ToOwned::to_owned)
        .or_else(|| {
            let display = display_path(path);
            (!display.is_empty()).then_some(display)
        })
}

pub(crate) fn project_key_from_path(root: &Path, path: &Path) -> Option<String> {
    let relative = path.strip_prefix(root).ok()?;
    relative
        .components()
        .next()
        .and_then(|component| component.as_os_str().to_str())
        .filter(|part| !part.is_empty())
        .map(ToOwned::to_owned)
}

#[test]
fn project_context_requires_path_or_repo_identity() {
    let project = ProjectContext {
        project_label: Some("scratch".to_string()),
        ..ProjectContext::default()
    }
    .into_project_info();

    assert_eq!(project, None);
}
