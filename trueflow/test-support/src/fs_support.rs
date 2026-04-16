use anyhow::{Context, Result};
use std::ffi::OsStr;
use std::fs;
use std::path::Path;

const FIXTURE_PLACEHOLDER: &str = ".fixture-root";

pub fn copy_dir_all(src: &Path, dst: &Path) -> Result<()> {
    fs::create_dir_all(dst)
        .with_context(|| format!("failed to create fixture dir {}", dst.display()))?;

    for entry in fs::read_dir(src)
        .with_context(|| format!("failed to read fixture dir {}", src.display()))?
    {
        let entry = entry?;
        if entry.file_name() == OsStr::new(FIXTURE_PLACEHOLDER) {
            continue;
        }

        let file_type = entry.file_type()?;
        let target = dst.join(entry.file_name());
        if file_type.is_dir() {
            copy_dir_all(&entry.path(), &target)?;
        } else if file_type.is_file() {
            fs::copy(entry.path(), &target).with_context(|| {
                format!(
                    "failed to copy fixture file {} -> {}",
                    entry.path().display(),
                    target.display()
                )
            })?;
        }
    }

    Ok(())
}
