use std::error::Error;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::process::Command;

fn repo_root() -> Result<PathBuf, Box<dyn Error>> {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .map(Path::to_path_buf)
        .ok_or_else(|| {
            io::Error::new(io::ErrorKind::NotFound, "crate should live under repo root").into()
        })
}

fn forbidden_task_tracker_name() -> String {
    ["be", "ads"].concat()
}

fn tracked_paths(root: &Path) -> Result<Vec<PathBuf>, Box<dyn Error>> {
    let output = Command::new("git")
        .arg("-C")
        .arg(root)
        .arg("ls-files")
        .arg("-z")
        .output()?;

    if !output.status.success() {
        return Ok(Vec::new());
    }

    Ok(output
        .stdout
        .split(|byte| *byte == b'\0')
        .filter(|path| !path.is_empty())
        .map(|path| PathBuf::from(String::from_utf8_lossy(path).into_owned()))
        .collect())
}

#[test]
fn tracked_repository_files_do_not_reference_rejected_task_tracker() -> Result<(), Box<dyn Error>> {
    let root = repo_root()?;
    let token = forbidden_task_tracker_name();

    for path in tracked_paths(&root)? {
        let full_path = root.join(&path);
        if !full_path.is_file() {
            continue;
        }

        let bytes = fs::read(&full_path)?;
        let content = String::from_utf8_lossy(&bytes).to_ascii_lowercase();
        assert!(
            !content.contains(&token),
            "{} references rejected task tracker {token:?}",
            path.display()
        );
    }

    Ok(())
}
