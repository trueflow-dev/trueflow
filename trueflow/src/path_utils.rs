use crate::repo_path::normalize_repo_path_string;
use std::path::{Path, PathBuf};

pub fn normalize_path_str(path: &str) -> String {
    normalize_repo_path_string(path)
}

pub fn workdir_prefix_for_repo_root(repo_root: &Path, cwd: &Path) -> Option<String> {
    let repo_root = canonicalize_or_original(repo_root);
    let cwd = canonicalize_or_original(cwd);
    let relative = cwd.strip_prefix(&repo_root).ok()?;
    let relative = normalize_path_str(relative.to_string_lossy().as_ref());
    if relative.is_empty() || relative == "." {
        None
    } else {
        Some(relative)
    }
}

pub fn current_workdir_prefix_for_repo_root(repo_root: &Path) -> Option<String> {
    let cwd = std::env::current_dir().ok()?;
    workdir_prefix_for_repo_root(repo_root, &cwd)
}

pub fn repo_relative_path_for_diff(path: &str, workdir_prefix: Option<&str>) -> String {
    let normalized_path = normalize_path_str(path);
    let Some(prefix) = workdir_prefix
        .map(normalize_path_str)
        .filter(|value| !value.is_empty())
    else {
        return normalized_path;
    };

    if normalized_path.is_empty()
        || normalized_path == prefix
        || normalized_path.starts_with(&format!("{prefix}/"))
    {
        return normalized_path;
    }

    format!("{prefix}/{normalized_path}")
}

pub fn tree_path_candidates_for_repo_path(
    repo_relative_path: &str,
    workdir_prefix: Option<&str>,
) -> Vec<String> {
    let normalized_path = normalize_path_str(repo_relative_path);
    if normalized_path.is_empty() {
        return Vec::new();
    }

    let mut candidates = vec![normalized_path.clone()];
    let Some(prefix) = workdir_prefix
        .map(normalize_path_str)
        .filter(|value| !value.is_empty())
    else {
        return candidates;
    };

    let prefixed_root = format!("{prefix}/");
    if let Some(stripped) = normalized_path.strip_prefix(&prefixed_root) {
        let stripped = normalize_path_str(stripped);
        if !stripped.is_empty() && !candidates.contains(&stripped) {
            candidates.push(stripped);
        }
    }

    candidates
}

pub fn candidate_repo_paths_for_hint(
    path_hint: &str,
    workdir_prefix: Option<&str>,
    repo_workdir: Option<&Path>,
) -> Vec<String> {
    let normalized = normalize_path_str(path_hint);
    if normalized.is_empty() {
        return Vec::new();
    }

    let mut candidates = vec![normalized.clone()];
    if let Some(prefix) = workdir_prefix
        .map(normalize_path_str)
        .filter(|value| !value.is_empty())
        && normalized != prefix
        && !normalized.starts_with(&format!("{prefix}/"))
    {
        candidates.push(format!("{prefix}/{normalized}"));
    }

    if let Some(workdir) = repo_workdir {
        let hinted_path = Path::new(path_hint);
        if hinted_path.is_absolute() {
            let canonical_workdir = canonicalize_or_original(workdir);
            let canonical_hint = canonicalize_or_original(hinted_path);
            if let Ok(relative) = canonical_hint.strip_prefix(&canonical_workdir) {
                let relative = normalize_path_str(relative.to_string_lossy().as_ref());
                if !relative.is_empty() && !candidates.contains(&relative) {
                    candidates.push(relative);
                }
            }
        }
    }

    candidates
}

pub fn path_matches_workdir_prefix(path: &str, prefix: &str) -> bool {
    let normalized_path = normalize_path_str(path);
    let normalized_prefix = normalize_path_str(prefix);
    normalized_path == normalized_prefix
        || normalized_path.starts_with(&format!("{normalized_prefix}/"))
}

fn canonicalize_or_original(path: &Path) -> PathBuf {
    path.canonicalize().unwrap_or_else(|_| path.to_path_buf())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalize_path_str_handles_windows_and_relative_prefixes() {
        assert_eq!(normalize_path_str("./src\\lib.rs"), "src/lib.rs");
        assert_eq!(
            normalize_path_str("src\\nested\\mod.rs"),
            "src/nested/mod.rs"
        );
    }

    #[test]
    fn repo_relative_path_for_diff_applies_workdir_prefix() {
        assert_eq!(
            repo_relative_path_for_diff("src/lib.rs", Some("pkg")),
            "pkg/src/lib.rs"
        );
        assert_eq!(
            repo_relative_path_for_diff("pkg/src/lib.rs", Some("pkg")),
            "pkg/src/lib.rs"
        );
    }

    #[test]
    fn tree_path_candidates_for_repo_path_strips_workdir_prefix() {
        assert_eq!(
            tree_path_candidates_for_repo_path("pkg/src/lib.rs", Some("pkg")),
            vec!["pkg/src/lib.rs".to_string(), "src/lib.rs".to_string()]
        );
    }

    #[test]
    fn candidate_repo_paths_for_hint_expands_subdir_input() {
        assert_eq!(
            candidate_repo_paths_for_hint("src/lib.rs", Some("pkg"), None),
            vec!["src/lib.rs".to_string(), "pkg/src/lib.rs".to_string()]
        );
    }

    #[test]
    fn path_matches_workdir_prefix_checks_exact_or_descendant() {
        assert!(path_matches_workdir_prefix("pkg/src/lib.rs", "pkg"));
        assert!(path_matches_workdir_prefix("pkg", "pkg"));
        assert!(!path_matches_workdir_prefix("other/src/lib.rs", "pkg"));
    }
}
