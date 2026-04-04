use crate::commands::review;
use crate::context::TrueflowContext;
use anyhow::{Result, bail};
use tracing::{info, warn};

pub fn run(_context: &TrueflowContext) -> Result<()> {
    let summary = review::collect_main_diff_summary()?;

    if summary.files.is_empty() {
        info!("All clear! No unreviewed blocks found.");
        Ok(())
    } else {
        warn!(
            "Found {} unreviewed file(s) covering {} block(s).",
            summary.files.len(),
            summary.total_blocks
        );
        for file in &summary.files {
            warn!("  {} ({} block(s))", file.path, file.blocks.len());
        }
        bail!("CI Check Failed: Unreviewed code detected.");
    }
}
