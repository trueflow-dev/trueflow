use std::collections::HashSet;
use std::io::{BufWriter, Write};

use anyhow::{Result, bail};
use clap::Args;
use serde::Serialize;

use crate::context::TrueflowContext;
use crate::feedback_export::{self, FeedbackCursorUpdateGuard};
use crate::store::{CommitId, FileStore, Identity, Record, RepoRef, ReviewStore};
use crate::vcs;

#[derive(Debug, Clone, Args)]
pub struct ResetArgs {
    /// Preview which records would be removed without mutating `.trueflow/reviews.jsonl`
    #[arg(long)]
    pub dry_run: bool,

    /// Emit structured JSON summary
    #[arg(long)]
    pub json: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ResetReport {
    pub dry_run: bool,
    pub identity: Identity,
    pub revision: CommitId,
    pub removed_count: usize,
    pub remaining_count: usize,
    pub removed_record_ids: Vec<String>,
}

#[derive(Debug)]
pub struct ResetPartition {
    pub surviving: Vec<Record>,
    pub removed_ids: Vec<String>,
}

pub fn run(_context: &TrueflowContext, args: &ResetArgs) -> Result<()> {
    let repo_snapshot = vcs::snapshot_from_workdir();
    let Some(repo) = repo_snapshot.repo() else {
        bail!("trueflow reset requires a Git repository");
    };
    let Some(current_revision) = repo_snapshot.repo_ref_revision.clone() else {
        bail!("trueflow reset requires a resolved HEAD commit (HEAD is unborn or unresolved)");
    };
    let git_cfg = vcs::git_config_from_repo(repo);
    let current_identity = Identity::Email {
        email: git_cfg.email,
    };

    let store = FileStore::open_readonly()?;
    let report = if args.dry_run {
        let records = store.read_history()?;
        let partition = partition_records_for_reset(records, &current_identity, &current_revision);
        ResetReport {
            dry_run: true,
            identity: current_identity,
            revision: current_revision,
            removed_count: partition.removed_ids.len(),
            remaining_count: partition.surviving.len(),
            removed_record_ids: partition.removed_ids,
        }
    } else {
        let cursor_path = feedback_export::feedback_cursor_path(&store);
        let cursor_guard = FeedbackCursorUpdateGuard::acquire_if_present(&cursor_path)?;
        let (report, prepared_cursor) = store.rewrite_history_with(|records| {
            let removed_set: HashSet<&str> = records
                .iter()
                .filter(|record| {
                    record_matches_reset_scope(record, &current_identity, &current_revision)
                })
                .map(|record| record.id.as_str())
                .collect();

            if removed_set.is_empty() {
                let remaining_count = records.len();
                return Ok((
                    None,
                    (
                        ResetReport {
                            dry_run: false,
                            identity: current_identity.clone(),
                            revision: current_revision.clone(),
                            removed_count: 0,
                            remaining_count,
                            removed_record_ids: Vec::new(),
                        },
                        None,
                    ),
                ));
            }

            let prepared = match cursor_guard.as_ref() {
                Some(guard) => {
                    guard.prepare_reconciled_cursor_after_removal(&records, &removed_set)?
                }
                None => None,
            };

            let partition =
                partition_records_for_reset(records, &current_identity, &current_revision);
            let report = ResetReport {
                dry_run: false,
                identity: current_identity.clone(),
                revision: current_revision.clone(),
                removed_count: partition.removed_ids.len(),
                remaining_count: partition.surviving.len(),
                removed_record_ids: partition.removed_ids,
            };
            Ok((Some(partition.surviving), (report, prepared)))
        })?;

        if let (Some(guard), Some(next_cursor)) = (cursor_guard, prepared_cursor.as_ref()) {
            guard.write_prepared(next_cursor)?;
        }
        report
    };

    let stdout = std::io::stdout();
    let mut writer = BufWriter::new(stdout.lock());
    if args.json {
        serde_json::to_writer_pretty(&mut writer, &report)?;
        writer.write_all(b"\n")?;
    } else {
        let rendered = render_reset_text(&report);
        writer.write_all(rendered.as_bytes())?;
    }
    writer.flush()?;
    Ok(())
}

pub fn record_effective_revision(record: &Record) -> Option<&CommitId> {
    if let Some(deletion) = &record.declaration_deletion {
        return Some(&deletion.head_revision);
    }
    if let Some(deletion) = &record.block_deletion {
        return Some(&deletion.head_revision);
    }
    match &record.repo_ref {
        RepoRef::Vcs { revision, .. } => Some(revision),
        RepoRef::Unknown => None,
    }
}

pub fn record_matches_reset_scope(
    record: &Record,
    identity: &Identity,
    revision: &CommitId,
) -> bool {
    &record.identity == identity && record_effective_revision(record) == Some(revision)
}

pub fn partition_records_for_reset(
    records: Vec<Record>,
    identity: &Identity,
    revision: &CommitId,
) -> ResetPartition {
    let mut surviving = Vec::with_capacity(records.len());
    let mut removed_ids = Vec::new();
    for record in records {
        if record_matches_reset_scope(&record, identity, revision) {
            removed_ids.push(record.id);
        } else {
            surviving.push(record);
        }
    }
    ResetPartition {
        surviving,
        removed_ids,
    }
}

pub fn render_reset_text(report: &ResetReport) -> String {
    let Identity::Email { email } = &report.identity;
    let rev = &report.revision.as_str()[..report.revision.as_str().len().min(8)];
    match (report.dry_run, report.removed_count) {
        (true, 0) => format!(
            "Dry run: no review records for {email} at {rev} would be removed ({} historical/other record(s) retained).\n",
            report.remaining_count
        ),
        (true, count) => format!(
            "Dry run: would remove {count} review record(s) for {email} at {rev} ({} record(s) retained).\n",
            report.remaining_count
        ),
        (false, 0) => format!(
            "No review records found for {email} at {rev} ({} record(s) retained).\n",
            report.remaining_count
        ),
        (false, count) => format!(
            "Removed {count} review record(s) for {email} at {rev} ({} record(s) retained).\n",
            report.remaining_count
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::hashing::TreeHash;
    use crate::store::{
        BlockDeletion, BlockState, ReviewCheck, ReviewTargetRef, VcsSystem, Verdict,
    };

    fn sample_record(id: &str, email: &str, rev: &str) -> Record {
        let mut record = Record::new(
            ReviewTargetRef::Block {
                hash: TreeHash::new("abcdef0123456789"),
            },
            ReviewCheck::review(),
            Verdict::Approved,
            Identity::Email {
                email: email.to_string(),
            },
            RepoRef::Vcs {
                system: VcsSystem::Git,
                revision: CommitId::new(rev).unwrap(),
            },
            BlockState::Committed,
        );
        record.id = id.to_string();
        record
    }

    #[test]
    fn effective_revision_prefers_deletion_head_over_repo_ref() {
        let rev_base = "1111111111111111111111111111111111111111";
        let rev_head = "2222222222222222222222222222222222222222";
        let mut record = sample_record("del", "alice@example.com", rev_base);
        record.block_deletion = Some(BlockDeletion {
            head_revision: CommitId::new(rev_head).unwrap(),
        });

        let alice = Identity::Email {
            email: "alice@example.com".to_string(),
        };
        assert!(!record_matches_reset_scope(
            &record,
            &alice,
            &CommitId::new(rev_base).unwrap()
        ));
        assert!(record_matches_reset_scope(
            &record,
            &alice,
            &CommitId::new(rev_head).unwrap()
        ));
    }
}
