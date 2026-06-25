use crate::{
    ReviewRecordOverrides, TestRepo, json_array, read_review_records, run_git_output,
    write_review_records,
};
use anyhow::{Context, Result, anyhow};
use trueflow::block::Block;
use trueflow::commands::feedback::{FeedbackCollectionParams, collect_feedback_json_values};
use trueflow::commands::review::ReviewRequest;
use trueflow::feedback_since::FeedbackSinceExpr;
use trueflow::repo_path::RepoPath;
use trueflow::store::{
    BlockState, CommitId, Identity, Record, RepoRef, ReviewCheck, ReviewTargetKind,
    ReviewTargetRef, VcsSystem, Verdict,
};
use trueflow::targets::ReviewTarget;

pub struct FeedbackScenario {
    repo: TestRepo,
}

impl FeedbackScenario {
    pub fn new(name: &str) -> Result<Self> {
        Ok(Self {
            repo: TestRepo::new(name)?,
        })
    }

    pub fn repo(&self) -> &TestRepo {
        &self.repo
    }

    pub fn write(&self, path: &str, content: &str) -> Result<()> {
        self.repo.write(path, content)
    }

    pub fn commit_all(&self, message: &str) -> Result<String> {
        self.repo.commit_all(message)?;
        self.head_revision()
    }

    pub fn head_revision(&self) -> Result<String> {
        Ok(run_git_output(&self.repo.path, &["rev-parse", "HEAD"])?
            .trim()
            .to_string())
    }

    pub fn reviews(&self) -> Result<Vec<Record>> {
        read_review_records(&self.reviews_path())
    }

    pub fn write_reviews(&self, records: &[Record]) -> Result<()> {
        write_review_records(&self.trueflow_dir(), records)
    }

    pub fn feedback_json(&self, extra_args: &[&str]) -> Result<Vec<serde_json::Value>> {
        let mut args = vec!["feedback", "--format", "json"];
        args.extend_from_slice(extra_args);
        let output = self.repo.run(&args)?;
        json_array(&output)
    }

    pub fn feedback_json_in_process(&self, extra_args: &[&str]) -> Result<Vec<serde_json::Value>> {
        let request = parse_feedback_json_request(extra_args)?;
        crate::with_current_dir(&self.repo.path, || {
            collect_feedback_json_values(FeedbackCollectionParams {
                since: request.since.as_ref(),
                targets: &request.targets,
                include_approved: request.include_approved,
                only: &[],
                exclude: &[],
            })
        })
    }

    pub fn review_block(&self, path: &str, verdict: &str) -> Result<Record> {
        self.review_block_with_overrides(path, verdict, &ReviewRecordOverrides::default())
    }

    pub fn review_block_in_process(&self, path: &str, verdict: &str) -> Result<Record> {
        self.review_block_in_process_with_overrides(
            path,
            verdict,
            &ReviewRecordOverrides::default(),
        )
    }

    pub fn review_block_in_process_with_overrides(
        &self,
        path: &str,
        verdict: &str,
        overrides: &ReviewRecordOverrides<'_>,
    ) -> Result<Record> {
        let block = self.review_block_from_summary(path)?;
        let cli_verdict = overrides.verdict.unwrap_or(verdict);
        let revision = self.head_revision()?;
        let mut record = Record::new(
            ReviewTargetRef::Block {
                hash: block.hash.clone(),
            },
            ReviewCheck::review(),
            parse_verdict(cli_verdict)?,
            Identity::Email {
                email: "test@example.com".to_string(),
            },
            RepoRef::Vcs {
                system: VcsSystem::Git,
                revision: CommitId::new(revision)?,
            },
            BlockState::Committed,
        );
        record.path_hint = Some(RepoPath::new(path)?);
        record.line_hint = Some(u32::try_from(block.start_line).with_context(|| {
            format!(
                "start_line {} should fit into u32 for synthetic feedback record",
                block.start_line
            )
        })?);
        apply_review_record_overrides(&mut record, overrides)?;

        let mut records = self.reviews()?;
        records.push(record.clone());
        self.write_reviews(&records)?;
        Ok(record)
    }

    pub fn review_block_with_overrides(
        &self,
        path: &str,
        verdict: &str,
        overrides: &ReviewRecordOverrides<'_>,
    ) -> Result<Record> {
        let block = self.review_block_info(path)?;
        let cli_verdict = overrides.verdict.unwrap_or(verdict);
        let mut args = vec![
            "mark".to_string(),
            "--fingerprint".to_string(),
            block.hash,
            "--verdict".to_string(),
            cli_verdict.to_string(),
            "--path".to_string(),
            path.to_string(),
            "--line".to_string(),
            block.start_line.to_string(),
            "--quiet".to_string(),
        ];
        if let Some(check) = overrides.check {
            args.push("--check".to_string());
            args.push(check.to_string());
        }
        if let Some(note) = overrides.note {
            args.push("--note".to_string());
            args.push(note.to_string());
        }
        let arg_refs = args.iter().map(String::as_str).collect::<Vec<_>>();
        self.repo.run(&arg_refs)?;

        let mut records = self.reviews()?;
        let record = records
            .last_mut()
            .context("expected review record after mark")?;
        apply_review_record_overrides(record, overrides)?;
        let record = record.clone();
        self.write_reviews(&records)?;
        Ok(record)
    }

    fn review_block_from_summary(&self, path: &str) -> Result<Block> {
        let summary = self
            .repo
            .review_summary(ReviewRequest::AllFiles, &[], &[])?;
        let file = summary
            .files
            .iter()
            .find(|file| file.path.as_str().trim_start_matches("./") == path)
            .with_context(|| format!("missing review output for {path}"))?;
        file.blocks
            .first()
            .cloned()
            .context("expected at least one block")
    }

    fn review_block_info(&self, path: &str) -> Result<ReviewBlockInfo> {
        let output = self.repo.run(&["review", "--all", "--json"])?;
        let files = json_array(&output)?;
        let file = files
            .iter()
            .find(|file| path_matches(file, path))
            .with_context(|| format!("missing review output for {path}"))?;
        let blocks = file["blocks"]
            .as_array()
            .context("blocks should be array")?;
        let block = blocks.first().context("expected at least one block")?;
        let hash = block["hash"]
            .as_str()
            .context("hash should be string")?
            .to_string();
        let start_line = block["start_line"]
            .as_u64()
            .context("start_line should be integer")?;
        let start_line = u32::try_from(start_line)
            .with_context(|| format!("start_line {start_line} should fit into u32 for mark CLI"))?;
        Ok(ReviewBlockInfo { hash, start_line })
    }

    fn trueflow_dir(&self) -> std::path::PathBuf {
        self.repo.path.join(".trueflow")
    }

    fn reviews_path(&self) -> std::path::PathBuf {
        self.trueflow_dir().join("reviews.jsonl")
    }
}

struct ReviewBlockInfo {
    hash: String,
    start_line: u32,
}

struct FeedbackJsonRequest {
    since: Option<FeedbackSinceExpr>,
    targets: Vec<ReviewTarget>,
    include_approved: bool,
}

fn parse_feedback_json_request(extra_args: &[&str]) -> Result<FeedbackJsonRequest> {
    let mut since = None;
    let mut targets = Vec::new();
    let mut include_approved = false;
    let mut index = 0;
    while index < extra_args.len() {
        match extra_args[index] {
            "--since" => {
                let value = extra_args
                    .get(index + 1)
                    .copied()
                    .context("--since requires a value")?;
                since = Some(FeedbackSinceExpr::new(value)?);
                index += 2;
            }
            "--target" => {
                let value = extra_args
                    .get(index + 1)
                    .copied()
                    .context("--target requires a value")?;
                targets.push(ReviewTarget::from_cli(value)?);
                index += 2;
            }
            "--include-approved" => {
                include_approved = true;
                index += 1;
            }
            other => return Err(anyhow!("unsupported in-process feedback arg: {other}")),
        }
    }

    Ok(FeedbackJsonRequest {
        since,
        targets,
        include_approved,
    })
}

fn path_matches(file: &serde_json::Value, expected: &str) -> bool {
    file["path"]
        .as_str()
        .is_some_and(|path| path.trim_start_matches("./") == expected)
}

fn apply_review_record_overrides(
    record: &mut Record,
    overrides: &ReviewRecordOverrides<'_>,
) -> Result<()> {
    if let Some(id) = overrides.id {
        record.id = id.to_string();
    }
    if let Some(check) = overrides.check {
        record.check = ReviewCheck::new(check)?;
    }
    if let Some(verdict) = overrides.verdict {
        record.verdict = parse_verdict(verdict)?;
    }
    if let Some(email) = overrides.email {
        record.identity = Identity::Email {
            email: email.to_string(),
        };
    }
    if let Some(timestamp) = overrides.timestamp {
        record.timestamp = timestamp;
    }
    if let Some(repo_revision) = overrides.repo_revision {
        record.repo_ref = RepoRef::Vcs {
            system: VcsSystem::Git,
            revision: CommitId::new(repo_revision)?,
        };
    }
    if let Some(block_state) = overrides.block_state {
        record.block_state = parse_block_state(block_state)?;
    }
    if let Some(target_kind) = overrides.target_kind {
        let lookup_key = record.target.lookup_key().to_string();
        record.target = parse_target_kind(target_kind)?.parse_target(&lookup_key)?;
    }
    if let Some(path_hint) = overrides.path_hint {
        record.path_hint = Some(RepoPath::new(path_hint)?);
    }
    if let Some(line_hint) = overrides.line_hint {
        record.line_hint = Some(line_hint);
    }
    if let Some(note) = overrides.note {
        record.note = Some(note.to_string());
    }
    if let Some(attestations) = &overrides.attestations {
        record.attestations = if attestations.is_null() {
            None
        } else {
            Some(serde_json::from_value(attestations.clone())?)
        };
    }
    Ok(())
}

fn parse_verdict(raw: &str) -> Result<Verdict> {
    match raw {
        "approved" => Ok(Verdict::Approved),
        "rejected" => Ok(Verdict::Rejected),
        "comment" | "question" => Ok(Verdict::Comment),
        _ => Err(anyhow!("unknown verdict: {raw}")),
    }
}

fn parse_block_state(raw: &str) -> Result<BlockState> {
    match raw {
        "committed" => Ok(BlockState::Committed),
        "uncommitted" => Ok(BlockState::Uncommitted),
        "unknown" => Ok(BlockState::Unknown),
        _ => Err(anyhow!("unknown block state: {raw}")),
    }
}

fn parse_target_kind(raw: &str) -> Result<ReviewTargetKind> {
    match raw {
        "block" => Ok(ReviewTargetKind::Block),
        "file" => Ok(ReviewTargetKind::File),
        "tree" => Ok(ReviewTargetKind::Tree),
        _ => Err(anyhow!("unknown review target kind: {raw}")),
    }
}
