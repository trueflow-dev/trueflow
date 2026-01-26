use crate::context::TrueflowContext;
use crate::diff_logic::get_unreviewed_changes;
use anyhow::{Result, bail};
use log::{info, warn};

pub fn run(_context: &TrueflowContext) -> Result<()> {
    let unreviewed_changes = get_unreviewed_changes()?;

    if unreviewed_changes.is_empty() {
        info!("All clear! No unreviewed changes found.");
        Ok(())
    } else {
        warn!("Found {} unreviewed change(s):", unreviewed_changes.len());
        for change in &unreviewed_changes {
            warn!(
                "  {} ({}:{}) - {}",
                change.fingerprint, change.file, change.line, change.status
            );
        }
        bail!("CI Check Failed: Unreviewed code detected.");
    }
}
