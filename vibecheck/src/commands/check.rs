use anyhow::{Result, bail};
use crate::diff_logic::get_unreviewed_changes;

pub fn run() -> Result<()> {
    let unreviewed_changes = get_unreviewed_changes()?;
    
    if unreviewed_changes.is_empty() {
        println!("All clear! No unreviewed changes found.");
        Ok(())
    } else {
        println!("Found {} unreviewed change(s):", unreviewed_changes.len());
        for change in &unreviewed_changes {
            println!("  {} ({}:{}) - {}", change.fingerprint, change.file, change.line, change.status);
        }
        bail!("CI Check Failed: Unreviewed code detected.");
    }
}
