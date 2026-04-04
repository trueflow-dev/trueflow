use crate::commands::review;
use crate::context::TrueflowContext;
use anyhow::{Result, bail};
use tracing::{info, warn};

pub fn run(_context: &TrueflowContext) -> Result<()> {
    let summary = review::collect_main_diff_summary()?;
    let unreviewed_block_count: usize = summary.files.iter().map(|file| file.blocks.len()).sum();

    if summary.files.is_empty() {
        info!("All clear! No unreviewed blocks found in main diff.");
        Ok(())
    } else {
        warn!(
            "Found {} unreviewed block(s) across {} file(s) in main diff.",
            unreviewed_block_count,
            summary.files.len()
        );
        for file in &summary.files {
            warn!("  {} ({} block(s))", file.path, file.blocks.len());
        }
        bail!("CI Check Failed: Unreviewed code detected.");
    }
}
