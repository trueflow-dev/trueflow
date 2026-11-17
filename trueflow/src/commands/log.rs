use std::cmp::Reverse;
use std::fmt::Write as _;
use std::io::{BufWriter, Write};

use anyhow::Result;
use chrono::{DateTime, Utc};
use clap::Args;

use crate::context::TrueflowContext;
use crate::repo_path::RepoPath;
use crate::store::{
    FileStore, Identity, Record, RepoRef, ReviewCheck, ReviewStore, ReviewTargetRef, Verdict,
};
use crate::targets::workdir_prefix_from_git_root;

pub const DEFAULT_LOG_LIMIT: usize = 16;

#[derive(Debug, Clone, Args)]
pub struct LogArgs {
    /// Maximum number of most-recent records to display
    #[arg(short = 'n', long, default_value_t = DEFAULT_LOG_LIMIT, conflicts_with = "all")]
    pub limit: usize,

    /// Display all matching records without truncating
    #[arg(long)]
    pub all: bool,

    /// Emit matching records as a JSON array in reverse-chronological order
    #[arg(long)]
    pub json: bool,

    /// Filter to records with these verdicts (approved, rejected, comment)
    #[arg(long)]
    pub verdict: Vec<Verdict>,

    /// Filter to records with these review checks (e.g. review, declaration, security)
    #[arg(long)]
    pub check: Vec<ReviewCheck>,

    /// Filter to records matching a file or directory path
    #[arg(long)]
    pub path: Option<RepoPath>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LogLimit {
    Bounded(usize),
    All,
}

impl LogLimit {
    pub fn from_flags(all: bool, limit: usize) -> Self {
        match all {
            true => Self::All,
            false => Self::Bounded(limit),
        }
    }
}

#[derive(Debug, Clone, Default)]
pub struct LogQuery {
    pub verdicts: Vec<Verdict>,
    pub checks: Vec<ReviewCheck>,
    pub path: Option<RepoPath>,
    pub limit: Option<LogLimit>,
}

#[derive(Debug, Clone)]
pub struct LogSelection<'a> {
    pub records: Vec<&'a Record>,
    pub total_matching: usize,
}

pub fn run(_context: &TrueflowContext, args: &LogArgs) -> Result<()> {
    let store = FileStore::open_readonly()?;
    let records = store.read_history()?;

    let resolved_path = resolve_filter_path(args.path.as_ref());
    let query = LogQuery {
        verdicts: args.verdict.clone(),
        checks: args.check.clone(),
        path: resolved_path,
        limit: Some(LogLimit::from_flags(args.all, args.limit)),
    };
    let selection = select_log_records(&records, &query);

    let stdout = std::io::stdout();
    let mut writer = BufWriter::new(stdout.lock());
    if args.json {
        serde_json::to_writer_pretty(&mut writer, &selection.records)?;
        writer.write_all(b"\n")?;
    } else {
        let rendered = render_log_text(&selection);
        writer.write_all(rendered.as_bytes())?;
    }
    writer.flush()?;
    Ok(())
}

fn resolve_filter_path(raw_path: Option<&RepoPath>) -> Option<RepoPath> {
    let raw_path = raw_path?;
    let prefix = workdir_prefix_from_git_root().and_then(|p| RepoPath::new(p).ok());
    Some(match prefix {
        Some(prefix) => raw_path.resolve_from_prefix(&prefix),
        None => raw_path.clone(),
    })
}

pub fn select_log_records<'a>(records: &'a [Record], query: &LogQuery) -> LogSelection<'a> {
    let mut matching: Vec<(usize, &'a Record)> = records
        .iter()
        .enumerate()
        .filter(|(_, record)| record_matches_query(record, query))
        .collect();

    matching
        .sort_by_key(|(append_index, record)| (Reverse(record.timestamp), Reverse(*append_index)));

    let total_matching = matching.len();
    let limit = query.limit.unwrap_or(LogLimit::Bounded(DEFAULT_LOG_LIMIT));
    let shown: Vec<&'a Record> = match limit {
        LogLimit::All => matching.into_iter().map(|(_, record)| record).collect(),
        LogLimit::Bounded(max_items) => matching
            .into_iter()
            .take(max_items)
            .map(|(_, record)| record)
            .collect(),
    };

    LogSelection {
        records: shown,
        total_matching,
    }
}

fn record_matches_query(record: &Record, query: &LogQuery) -> bool {
    if !query.verdicts.is_empty() && !query.verdicts.contains(&record.verdict) {
        return false;
    }
    if !query.checks.is_empty() && !query.checks.contains(&record.check) {
        return false;
    }
    if let Some(filter_path) = query.path.as_ref()
        && !record_matches_path(record, filter_path)
    {
        return false;
    }
    true
}

fn record_matches_path(record: &Record, filter_path: &RepoPath) -> bool {
    if filter_path.is_root() {
        return true;
    }
    record_primary_path(record)
        .is_some_and(|candidate| path_equals_or_within(candidate, filter_path))
}

fn record_primary_path(record: &Record) -> Option<&RepoPath> {
    record
        .path_hint
        .as_ref()
        .or_else(|| record.declaration_locator.as_ref().map(|loc| &loc.path))
}

fn path_equals_or_within(candidate: &RepoPath, filter: &RepoPath) -> bool {
    candidate == filter
        || candidate
            .as_str()
            .strip_prefix(filter.as_str())
            .is_some_and(|suffix| suffix.starts_with('/'))
}

pub fn render_log_text(selection: &LogSelection<'_>) -> String {
    if selection.records.is_empty() {
        return "No review records found.\n".to_string();
    }

    let mut out = String::new();
    for record in &selection.records {
        let timestamp = format_timestamp(record.timestamp);
        let verdict = record.verdict.as_str();
        let check = record.check.as_str();
        let target = format_target_summary(record);
        let identity = format_identity(&record.identity);
        let _ = write!(
            out,
            "{timestamp}  {verdict:<8}  [{check}]  {target}  by {identity}"
        );

        if let Some(rev) = format_repo_revision(&record.repo_ref) {
            let _ = write!(out, " @ {rev}");
        }
        if let Some(deleted_rev) = deleted_in_revision(record) {
            let _ = write!(out, " [deleted in {deleted_rev}]");
        }
        if record
            .attestations
            .as_ref()
            .is_some_and(|attestations| !attestations.is_empty())
        {
            out.push_str(" [signed]");
        }
        out.push('\n');

        if let Some(note) = record.note.as_deref()
            && !note.trim().is_empty()
        {
            for line in note.lines() {
                let _ = writeln!(out, "    {line}");
            }
        }
    }

    if selection.total_matching > selection.records.len() {
        let _ = writeln!(
            out,
            "(showing {} of {} matching records; pass --all for full history)",
            selection.records.len(),
            selection.total_matching
        );
    }

    out
}

fn format_timestamp(timestamp: i64) -> String {
    match DateTime::<Utc>::from_timestamp(timestamp, 0) {
        Some(dt) => dt.format("%Y-%m-%d %H:%M:%SZ").to_string(),
        None => timestamp.to_string(),
    }
}

fn short_hex(value: &str) -> &str {
    let end = value.len().min(8);
    &value[..end]
}

fn format_target_summary(record: &Record) -> String {
    match &record.target {
        ReviewTargetRef::Block { hash } => {
            let short = short_hex(hash.as_str());
            match (&record.path_hint, record.line_hint) {
                (Some(path), Some(line)) => {
                    format!("block {short} ({}:L{})", path, line.saturating_add(1))
                }
                (Some(path), None) => format!("block {short} ({path})"),
                (None, _) => format!("block {short}"),
            }
        }
        ReviewTargetRef::File { hash } => {
            let short = short_hex(hash.as_str());
            match &record.path_hint {
                Some(path) => format!("file {short} ({path})"),
                None => format!("file {short}"),
            }
        }
        ReviewTargetRef::Tree { hash } => {
            let short = short_hex(hash.as_str());
            match &record.path_hint {
                Some(path) => format!("tree {short} ({path})"),
                None => format!("tree {short}"),
            }
        }
        ReviewTargetRef::Declaration { hash } => {
            let short = short_hex(hash.as_str());
            match &record.declaration_locator {
                Some(locator) => format!(
                    "declaration {short} ({} :: {})",
                    locator.path,
                    locator.declaration_key.as_str()
                ),
                None => format!("declaration {short}"),
            }
        }
    }
}

fn format_identity(identity: &Identity) -> &str {
    match identity {
        Identity::Email { email } => email.as_str(),
    }
}

fn format_repo_revision(repo_ref: &RepoRef) -> Option<&str> {
    match repo_ref {
        RepoRef::Vcs { revision, .. } => Some(short_hex(revision.as_str())),
        RepoRef::Unknown => None,
    }
}

fn deleted_in_revision(record: &Record) -> Option<&str> {
    record
        .declaration_deletion
        .as_ref()
        .map(|deletion| short_hex(deletion.head_revision.as_str()))
        .or_else(|| {
            record
                .block_deletion
                .as_ref()
                .map(|deletion| short_hex(deletion.head_revision.as_str()))
        })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::hashing::TreeHash;
    use crate::store::{
        Attestation, AttestationKind, BlockState, Canonicalization, CommitId, VcsSystem,
    };

    fn sample_record(id: &str, timestamp: i64, path: &str, verdict: Verdict) -> Record {
        let mut record = Record::new(
            ReviewTargetRef::Block {
                hash: TreeHash::new("abcdef0123456789"),
            },
            ReviewCheck::review(),
            verdict,
            Identity::Email {
                email: "dev@example.com".to_string(),
            },
            RepoRef::Vcs {
                system: VcsSystem::Git,
                revision: CommitId::new("1111111111111111111111111111111111111111").unwrap(),
            },
            BlockState::Committed,
        );
        record.id = id.to_string();
        record.timestamp = timestamp;
        record.path_hint = Some(RepoPath::new(path).unwrap());
        record.line_hint = Some(0);
        record
    }

    #[test]
    fn path_filter_matches_exact_file_and_directory_prefix_only_on_segment_boundary() {
        let records = vec![
            sample_record("a", 10, "src/lib.rs", Verdict::Approved),
            sample_record("b", 11, "src_extra/lib.rs", Verdict::Approved),
        ];
        let query = LogQuery {
            path: Some(RepoPath::new("src").unwrap()),
            ..LogQuery::default()
        };
        let selected = select_log_records(&records, &query);
        assert_eq!(selected.records.len(), 1);
        assert_eq!(selected.records[0].id, "a");
    }

    #[test]
    fn render_log_text_includes_signed_badge_when_attested() {
        let mut record = sample_record("signed", 1_700_000_000, "src/lib.rs", Verdict::Approved);
        record.attestations = Some(vec![Attestation {
            kind: AttestationKind::Pgp,
            canonicalization: Canonicalization::JcsV1,
            signature: "sig".to_string(),
            public_key: "pub".to_string(),
        }]);
        let records = vec![record];
        let selection = select_log_records(&records, &LogQuery::default());
        let text = render_log_text(&selection);
        assert!(text.contains("[signed]"), "unexpected text: {text}");
    }
}
