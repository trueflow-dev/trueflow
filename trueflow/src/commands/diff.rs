use crate::block::BlockKind;
use crate::commands::review::{self, ReviewRequest};
use crate::context::TrueflowContext;
use crate::review_scope::ReviewScope;
use anyhow::Result;

pub fn run(_context: &TrueflowContext, json: bool) -> Result<()> {
    let request: ReviewRequest = ReviewScope::MainDiff.to_review_request()?;
    review::run_request(json, request, &[] as &[BlockKind], &[] as &[BlockKind])
}
