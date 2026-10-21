use crate::commands::review::{self, ReviewSummary};
use crate::context::TrueflowContext;
use anyhow::{Result, bail};
use tracing::{info, warn};

pub fn run(_context: &TrueflowContext) -> Result<()> {
    let summary = review::collect_main_diff_summary()?;

    if summary.files.is_empty() {
        info!("All clear! No unreviewed blocks found in main diff.");
        return Ok(());
    }

    // stdout stays silent for scripts; the listing goes to stderr and the log.
    for line in unreviewed_report_lines(&summary) {
        eprintln!("{line}");
        warn!("{line}");
    }
    bail!("CI Check Failed: Unreviewed code detected.");
}

fn unreviewed_report_lines(summary: &ReviewSummary) -> Vec<String> {
    let unreviewed_block_count: usize = summary.files.iter().map(|file| file.blocks.len()).sum();
    std::iter::once(format!(
        "Found {} unreviewed block(s) across {} file(s) in main diff.",
        unreviewed_block_count,
        summary.files.len()
    ))
    .chain(
        summary
            .files
            .iter()
            .map(|file| format!("  {} ({} block(s))", file.path, file.blocks.len())),
    )
    .collect()
}
