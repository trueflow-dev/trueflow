use crate::commands::review;
use crate::context::TrueflowContext;
use anyhow::Result;

pub fn run(_context: &TrueflowContext, json: bool) -> Result<()> {
    let summary = review::collect_main_diff_summary()?;
    review::print_review_summary(summary, json)
}
