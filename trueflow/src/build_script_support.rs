use std::path::{Path, PathBuf};

pub(crate) fn git_rerun_hint_paths(
    manifest_dir: Option<&Path>,
    head_contents: Option<&str>,
) -> Vec<PathBuf> {
    let Some(manifest_dir) = manifest_dir else {
        return Vec::new();
    };
    let Some(repo_root) = manifest_dir.parent() else {
        return Vec::new();
    };

    let git_dir = repo_root.join(".git");
    let mut paths = vec![git_dir.join("HEAD"), git_dir.join("packed-refs")];

    if let Some(reference) = git_head_reference(head_contents.unwrap_or_default()) {
        paths.push(git_dir.join(reference));
    }

    paths
}

pub(crate) fn git_head_reference(head_contents: &str) -> Option<&str> {
    head_contents
        .strip_prefix("ref: ")
        .map(str::trim)
        .filter(|reference| !reference.is_empty())
}

pub(crate) fn normalized_command_stdout(success: bool, stdout: &[u8]) -> Option<String> {
    if !success {
        return None;
    }

    let value = std::str::from_utf8(stdout).ok()?;
    let trimmed = value.trim();
    if trimmed.is_empty() {
        None
    } else {
        Some(trimmed.to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn git_rerun_hint_paths_are_empty_without_manifest_dir() {
        assert!(git_rerun_hint_paths(None, None).is_empty());
    }

    #[test]
    fn git_rerun_hint_paths_emit_default_git_targets_when_head_is_missing() {
        let paths = git_rerun_hint_paths(Some(Path::new("/tmp/repo/trueflow")), None);

        assert_eq!(
            paths,
            vec![
                PathBuf::from("/tmp/repo/.git/HEAD"),
                PathBuf::from("/tmp/repo/.git/packed-refs"),
            ]
        );
    }

    #[test]
    fn git_rerun_hint_paths_ignore_detached_head_contents() {
        let paths = git_rerun_hint_paths(
            Some(Path::new("/tmp/repo/trueflow")),
            Some("4d3adb33f4d3adb33f4d3adb33f"),
        );

        assert_eq!(
            paths,
            vec![
                PathBuf::from("/tmp/repo/.git/HEAD"),
                PathBuf::from("/tmp/repo/.git/packed-refs"),
            ]
        );
    }

    #[test]
    fn git_rerun_hint_paths_include_symbolic_ref_with_trailing_newline() {
        let paths = git_rerun_hint_paths(
            Some(Path::new("/tmp/repo/trueflow")),
            Some("ref: refs/heads/main\n"),
        );

        assert_eq!(
            paths,
            vec![
                PathBuf::from("/tmp/repo/.git/HEAD"),
                PathBuf::from("/tmp/repo/.git/packed-refs"),
                PathBuf::from("/tmp/repo/.git/refs/heads/main"),
            ]
        );
    }

    #[test]
    fn normalized_command_stdout_returns_none_on_failure() {
        assert_eq!(normalized_command_stdout(false, b"abc123\n"), None);
    }

    #[test]
    fn normalized_command_stdout_returns_none_on_empty_stdout() {
        assert_eq!(normalized_command_stdout(true, b"  \n\t"), None);
    }

    #[test]
    fn normalized_command_stdout_trims_successful_stdout() {
        assert_eq!(
            normalized_command_stdout(true, b"abc123\n"),
            Some("abc123".to_string())
        );
    }
}
