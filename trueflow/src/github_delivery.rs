use crate::github::{GitHubInlineComment, ResolvedPullRequestRef};
use crate::store::CommitId;
use anyhow::{Context, Result, anyhow, bail};
use fs2::FileExt;
use serde::{Deserialize, Serialize};
use std::collections::HashSet;
use std::fmt;
use std::fs::{self, File, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use uuid::Uuid;

const GITHUB_DELIVERY_LEDGER_VERSION: u32 = 2;
pub const GITHUB_DELIVERY_LEDGER_FILE: &str = "github_delivery.json";
pub const GITHUB_DELIVERY_LEDGER_LOCK_FILE: &str = "github_delivery.lock";

/// A durable, opaque identity for a delivery mutation or its marked comment.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct GitHubDeliveryOperationId(Uuid);

impl GitHubDeliveryOperationId {
    pub fn new() -> Self {
        Self(Uuid::new_v4())
    }
}

impl Default for GitHubDeliveryOperationId {
    fn default() -> Self {
        Self::new()
    }
}

impl fmt::Display for GitHubDeliveryOperationId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(formatter)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GitHubDeliveryIntentStatus {
    Prepared,
    InFlight,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GitHubDeliveryComment {
    pub record_id: String,
    pub operation_id: GitHubDeliveryOperationId,
    pub comment: GitHubInlineComment,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum GitHubDeliveryIntent {
    CreatePendingReview {
        pr: ResolvedPullRequestRef,
        head_sha: CommitId,
        review_body: String,
        comments: Vec<GitHubDeliveryComment>,
    },
    AppendReviewThread {
        pr: ResolvedPullRequestRef,
        head_sha: CommitId,
        review_id: u64,
        review_node_id: String,
        review_url: String,
        comment: GitHubDeliveryComment,
    },
}

impl GitHubDeliveryIntent {
    pub fn pr(&self) -> &ResolvedPullRequestRef {
        match self {
            Self::CreatePendingReview { pr, .. } | Self::AppendReviewThread { pr, .. } => pr,
        }
    }

    pub fn head_sha(&self) -> &CommitId {
        match self {
            Self::CreatePendingReview { head_sha, .. }
            | Self::AppendReviewThread { head_sha, .. } => head_sha,
        }
    }

    pub fn comments(&self) -> &[GitHubDeliveryComment] {
        match self {
            Self::CreatePendingReview { comments, .. } => comments,
            Self::AppendReviewThread { comment, .. } => std::slice::from_ref(comment),
        }
    }

    pub fn is_create(&self) -> bool {
        matches!(self, Self::CreatePendingReview { .. })
    }

    fn validate(&self, parent_operation_id: GitHubDeliveryOperationId) -> Result<()> {
        let comments = self.comments();
        if comments.is_empty() {
            bail!("a GitHub delivery intent must contain at least one comment");
        }

        let mut record_ids = HashSet::new();
        let mut comment_operation_ids = HashSet::new();
        for comment in comments {
            validate_comment(comment)?;
            if !record_ids.insert(comment.record_id.as_str()) {
                bail!(
                    "delivery intent contains duplicate record ID {}",
                    comment.record_id
                );
            }
            if !comment_operation_ids.insert(comment.operation_id) {
                bail!(
                    "delivery intent contains duplicate comment operation ID {}",
                    comment.operation_id
                );
            }
        }

        match self {
            Self::CreatePendingReview { review_body, .. } => {
                if review_body.trim().is_empty() {
                    bail!("create-pending-review intent has a blank marked review body");
                }
                if comment_operation_ids.contains(&parent_operation_id) {
                    bail!(
                        "create-pending-review operation ID {parent_operation_id} must differ from comment operation IDs"
                    );
                }
            }
            Self::AppendReviewThread {
                review_id,
                review_node_id,
                review_url,
                comment,
                ..
            } => {
                if *review_id == 0 {
                    bail!("append-review-thread intent has a zero review database ID");
                }
                validate_nonblank("append-review-thread review node ID", review_node_id)?;
                validate_nonblank("append-review-thread review URL", review_url)?;
                if parent_operation_id != comment.operation_id {
                    bail!("append-review-thread operation ID must equal its comment operation ID");
                }
            }
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GitHubDeliveryOperation {
    pub id: GitHubDeliveryOperationId,
    pub status: GitHubDeliveryIntentStatus,
    pub intent: GitHubDeliveryIntent,
}

impl GitHubDeliveryOperation {
    pub fn prepared(id: GitHubDeliveryOperationId, intent: GitHubDeliveryIntent) -> Self {
        Self {
            id,
            status: GitHubDeliveryIntentStatus::Prepared,
            intent,
        }
    }

    fn persistent_operation_ids(&self) -> Vec<GitHubDeliveryOperationId> {
        if self.intent.is_create() {
            std::iter::once(self.id)
                .chain(
                    self.intent
                        .comments()
                        .iter()
                        .map(|comment| comment.operation_id),
                )
                .collect()
        } else {
            vec![self.id]
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GitHubDeliveryCommentReceipt {
    pub record_id: String,
    pub operation_id: GitHubDeliveryOperationId,
    pub thread_node_id: Option<String>,
    pub comment_node_id: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GitHubDeliveryPendingReviewReceipt {
    pub review_id: u64,
    pub review_node_id: String,
    pub html_url: String,
    pub comments: Vec<GitHubDeliveryCommentReceipt>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GitHubDeliveryPendingReview {
    pub pr: ResolvedPullRequestRef,
    pub head_sha: CommitId,
    pub review_id: u64,
    pub review_node_id: String,
    pub html_url: String,
    pub create_operation_id: Option<GitHubDeliveryOperationId>,
    pub comments: Vec<GitHubDeliveryCommentReceipt>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GitHubDeliveryTerminalReason {
    Submitted,
    Deleted,
    Missing,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GitHubDeliveryTerminalReview {
    pub review: GitHubDeliveryPendingReview,
    pub reason: GitHubDeliveryTerminalReason,
}

/// The v2 write-ahead delivery journal.  Active work is retained until a validated
/// acknowledgement is folded into a pending or terminal receipt.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GitHubDeliveryLedger {
    version: u32,
    active_operations: Vec<GitHubDeliveryOperation>,
    pending_reviews: Vec<GitHubDeliveryPendingReview>,
    terminal_reviews: Vec<GitHubDeliveryTerminalReview>,
}

impl Default for GitHubDeliveryLedger {
    fn default() -> Self {
        Self {
            version: GITHUB_DELIVERY_LEDGER_VERSION,
            active_operations: Vec::new(),
            pending_reviews: Vec::new(),
            terminal_reviews: Vec::new(),
        }
    }
}

impl GitHubDeliveryLedger {
    pub fn active_operations(&self) -> &[GitHubDeliveryOperation] {
        &self.active_operations
    }

    pub fn pending_reviews(&self) -> &[GitHubDeliveryPendingReview] {
        &self.pending_reviews
    }

    pub fn terminal_reviews(&self) -> &[GitHubDeliveryTerminalReview] {
        &self.terminal_reviews
    }

    pub fn operation(
        &self,
        operation_id: &GitHubDeliveryOperationId,
    ) -> Option<&GitHubDeliveryOperation> {
        self.active_operations
            .iter()
            .find(|operation| operation.id == *operation_id)
    }

    pub fn active_operations_for(
        &self,
        pr: &ResolvedPullRequestRef,
    ) -> impl Iterator<Item = &GitHubDeliveryOperation> {
        self.active_operations
            .iter()
            .filter(move |operation| operation.intent.pr() == pr)
    }

    pub fn excluded_record_ids(&self, pr: &ResolvedPullRequestRef) -> HashSet<String> {
        self.record_entries()
            .filter(|entry| entry.pr == pr)
            .map(|entry| entry.record_id.to_owned())
            .collect()
    }

    pub fn excluded_record_ids_for_head(
        &self,
        pr: &ResolvedPullRequestRef,
        head_sha: &CommitId,
    ) -> HashSet<String> {
        self.record_entries()
            .filter(|entry| entry.pr == pr && entry.head_sha == head_sha)
            .map(|entry| entry.record_id.to_owned())
            .collect()
    }

    /// Inserts a fully materialized intent. Its `Prepared` status is the only state
    /// from which an operation can later be cancelled without risking duplicate I/O.
    pub fn prepare(&mut self, operation: GitHubDeliveryOperation) -> Result<()> {
        self.validate()?;
        if operation.status != GitHubDeliveryIntentStatus::Prepared {
            bail!(
                "only Prepared delivery operations may be inserted; {} is {:?}",
                operation.id,
                operation.status
            );
        }
        operation.intent.validate(operation.id)?;

        let known_operation_ids = self.operation_ids()?;
        for operation_id in operation.persistent_operation_ids() {
            if known_operation_ids.contains(&operation_id) {
                bail!("duplicate delivery operation ID {operation_id}");
            }
        }

        let reserved_records = self.record_keys();
        for comment in operation.intent.comments() {
            let key = (
                operation.intent.pr().clone(),
                operation.intent.head_sha().clone(),
                comment.record_id.clone(),
            );
            if reserved_records.contains(&key) {
                bail!(
                    "record ID {} is already reserved for this pull request and head",
                    comment.record_id
                );
            }
        }

        self.active_operations.push(operation);
        Ok(())
    }

    /// Marks a durable operation as possibly dispatched. Callers must persist this
    /// transition before allowing `gh` to receive any request bytes.
    pub fn transition_to_in_flight(
        &mut self,
        operation_id: &GitHubDeliveryOperationId,
    ) -> Result<()> {
        let Some(operation) = self
            .active_operations
            .iter_mut()
            .find(|operation| operation.id == *operation_id)
        else {
            bail!("cannot mark unknown delivery operation {operation_id} in flight");
        };
        if operation.status != GitHubDeliveryIntentStatus::Prepared {
            bail!(
                "cannot mark delivery operation {operation_id} in flight from {:?}",
                operation.status
            );
        }
        operation.status = GitHubDeliveryIntentStatus::InFlight;
        Ok(())
    }

    /// Removes an un-sent intent and returns it for immediate replanning.
    pub fn cancel_prepared(
        &mut self,
        operation_id: &GitHubDeliveryOperationId,
    ) -> Result<GitHubDeliveryIntent> {
        let Some(index) = self
            .active_operations
            .iter()
            .position(|operation| operation.id == *operation_id)
        else {
            bail!("cannot cancel unknown delivery operation {operation_id}");
        };
        if self.active_operations[index].status != GitHubDeliveryIntentStatus::Prepared {
            bail!("cannot cancel delivery operation {operation_id} after it is in flight");
        }
        Ok(self.active_operations.remove(index).intent)
    }

    pub fn accept_create(
        &mut self,
        operation_id: &GitHubDeliveryOperationId,
        receipt: GitHubDeliveryPendingReviewReceipt,
    ) -> Result<()> {
        let operation = self
            .operation(operation_id)
            .cloned()
            .ok_or_else(|| anyhow!("cannot accept unknown delivery operation {operation_id}"))?;
        ensure_in_flight(&operation)?;
        let GitHubDeliveryIntent::CreatePendingReview {
            pr,
            head_sha,
            comments,
            ..
        } = &operation.intent
        else {
            bail!("delivery operation {operation_id} is not a pending-review create");
        };

        validate_pending_review_receipt(&receipt)?;
        validate_comment_receipts(comments, &receipt.comments)?;
        if self
            .pending_reviews
            .iter()
            .chain(
                self.terminal_reviews
                    .iter()
                    .map(|terminal| &terminal.review),
            )
            .any(|review| review.pr == *pr && review.review_id == receipt.review_id)
        {
            bail!(
                "review {} for {} already has a durable delivery receipt",
                receipt.review_id,
                pr
            );
        }

        let index = self
            .active_operations
            .iter()
            .position(|active| active.id == *operation_id)
            .ok_or_else(|| anyhow!("delivery operation {operation_id} disappeared"))?;
        self.active_operations.remove(index);
        self.pending_reviews.push(GitHubDeliveryPendingReview {
            pr: pr.clone(),
            head_sha: head_sha.clone(),
            review_id: receipt.review_id,
            review_node_id: receipt.review_node_id,
            html_url: receipt.html_url,
            create_operation_id: Some(*operation_id),
            comments: receipt.comments,
        });
        self.validate()
    }

    pub fn accept_append(
        &mut self,
        operation_id: &GitHubDeliveryOperationId,
        receipt: GitHubDeliveryCommentReceipt,
    ) -> Result<()> {
        let operation = self
            .operation(operation_id)
            .cloned()
            .ok_or_else(|| anyhow!("cannot accept unknown delivery operation {operation_id}"))?;
        ensure_in_flight(&operation)?;
        let GitHubDeliveryIntent::AppendReviewThread {
            pr,
            head_sha,
            review_id,
            review_node_id,
            comment,
            ..
        } = &operation.intent
        else {
            bail!("delivery operation {operation_id} is not a review-thread append");
        };

        validate_comment_receipts(
            std::slice::from_ref(comment),
            std::slice::from_ref(&receipt),
        )?;
        let pending_index = self
            .pending_reviews
            .iter()
            .position(|review| {
                review.pr == *pr
                    && review.head_sha == *head_sha
                    && review.review_id == *review_id
                    && review.review_node_id == *review_node_id
            })
            .ok_or_else(|| {
                anyhow!(
                    "append delivery operation {operation_id} has no matching pending review {review_id}"
                )
            })?;
        if self.pending_reviews[pending_index]
            .comments
            .iter()
            .any(|existing| existing.operation_id == receipt.operation_id)
        {
            bail!(
                "pending review {} already contains comment operation {}",
                review_id,
                receipt.operation_id
            );
        }

        let operation_index = self
            .active_operations
            .iter()
            .position(|active| active.id == *operation_id)
            .ok_or_else(|| anyhow!("delivery operation {operation_id} disappeared"))?;
        self.active_operations.remove(operation_index);
        self.pending_reviews[pending_index].comments.push(receipt);
        self.validate()
    }

    /// Converts an accepted pending review to a tombstone without releasing any
    /// record or operation identity for redelivery.
    pub fn tombstone_pending_review(
        &mut self,
        pr: &ResolvedPullRequestRef,
        review_id: u64,
        reason: GitHubDeliveryTerminalReason,
    ) -> Result<()> {
        let Some(index) = self
            .pending_reviews
            .iter()
            .position(|review| review.pr == *pr && review.review_id == review_id)
        else {
            bail!("cannot tombstone unknown pending review {review_id} for {pr}");
        };
        let review = self.pending_reviews.remove(index);
        self.terminal_reviews
            .push(GitHubDeliveryTerminalReview { review, reason });
        self.validate()
    }

    fn validate(&self) -> Result<()> {
        if self.version != GITHUB_DELIVERY_LEDGER_VERSION {
            bail!(
                "GitHub delivery ledger version {} is unsupported; expected version {}; delivery cannot continue safely until remote state is resolved",
                self.version,
                GITHUB_DELIVERY_LEDGER_VERSION
            );
        }

        for operation in &self.active_operations {
            operation.intent.validate(operation.id)?;
        }
        for review in &self.pending_reviews {
            validate_pending_review(review)?;
        }
        for terminal in &self.terminal_reviews {
            validate_pending_review(&terminal.review)?;
        }

        let mut review_keys = HashSet::new();
        for review in self.pending_reviews.iter().chain(
            self.terminal_reviews
                .iter()
                .map(|terminal| &terminal.review),
        ) {
            if !review_keys.insert((review.pr.clone(), review.review_id)) {
                bail!(
                    "duplicate durable review receipt {} for {}",
                    review.review_id,
                    review.pr
                );
            }
        }

        let _ = self.operation_ids()?;
        let mut records = HashSet::new();
        for entry in self.record_entries() {
            if !records.insert((entry.pr.clone(), entry.head_sha.clone(), entry.record_id)) {
                bail!(
                    "record ID {} appears more than once for {} at {}",
                    entry.record_id,
                    entry.pr,
                    entry.head_sha
                );
            }
        }
        Ok(())
    }

    fn operation_ids(&self) -> Result<HashSet<GitHubDeliveryOperationId>> {
        let mut ids = HashSet::new();
        for operation in &self.active_operations {
            for operation_id in operation.persistent_operation_ids() {
                if !ids.insert(operation_id) {
                    bail!("duplicate delivery operation ID {operation_id}");
                }
            }
        }
        for review in self.pending_reviews.iter().chain(
            self.terminal_reviews
                .iter()
                .map(|terminal| &terminal.review),
        ) {
            if let Some(operation_id) = review.create_operation_id
                && !ids.insert(operation_id)
            {
                bail!("duplicate delivery operation ID {operation_id}");
            }
            for comment in &review.comments {
                if !ids.insert(comment.operation_id) {
                    bail!("duplicate delivery operation ID {}", comment.operation_id);
                }
            }
        }
        Ok(ids)
    }

    fn record_keys(&self) -> HashSet<(ResolvedPullRequestRef, CommitId, String)> {
        self.record_entries()
            .map(|entry| {
                (
                    entry.pr.clone(),
                    entry.head_sha.clone(),
                    entry.record_id.to_owned(),
                )
            })
            .collect()
    }

    fn record_entries(&self) -> impl Iterator<Item = DeliveryRecordEntry<'_>> {
        self.active_operations
            .iter()
            .flat_map(|operation| {
                operation
                    .intent
                    .comments()
                    .iter()
                    .map(move |comment| DeliveryRecordEntry {
                        pr: operation.intent.pr(),
                        head_sha: operation.intent.head_sha(),
                        record_id: &comment.record_id,
                    })
            })
            .chain(self.pending_reviews.iter().flat_map(|review| {
                review
                    .comments
                    .iter()
                    .map(move |comment| DeliveryRecordEntry {
                        pr: &review.pr,
                        head_sha: &review.head_sha,
                        record_id: &comment.record_id,
                    })
            }))
            .chain(self.terminal_reviews.iter().flat_map(|terminal| {
                terminal
                    .review
                    .comments
                    .iter()
                    .map(move |comment| DeliveryRecordEntry {
                        pr: &terminal.review.pr,
                        head_sha: &terminal.review.head_sha,
                        record_id: &comment.record_id,
                    })
            }))
    }
}

struct DeliveryRecordEntry<'a> {
    pr: &'a ResolvedPullRequestRef,
    head_sha: &'a CommitId,
    record_id: &'a str,
}

fn ensure_in_flight(operation: &GitHubDeliveryOperation) -> Result<()> {
    if operation.status != GitHubDeliveryIntentStatus::InFlight {
        bail!(
            "delivery operation {} cannot be accepted from {:?}; it must be InFlight",
            operation.id,
            operation.status
        );
    }
    Ok(())
}

fn validate_comment(comment: &GitHubDeliveryComment) -> Result<()> {
    validate_nonblank("delivery record ID", &comment.record_id)?;
    validate_nonblank("delivery comment body", &comment.comment.body)?;
    if comment.comment.start_line.is_some() != comment.comment.start_side.is_some() {
        bail!(
            "delivery comment {} must specify start_line and start_side together",
            comment.record_id
        );
    }
    if let Some(start_line) = comment.comment.start_line
        && start_line > comment.comment.line
    {
        bail!(
            "delivery comment {} starts after its ending line",
            comment.record_id
        );
    }
    Ok(())
}

fn validate_pending_review_receipt(receipt: &GitHubDeliveryPendingReviewReceipt) -> Result<()> {
    if receipt.review_id == 0 {
        bail!("accepted review receipt has a zero review database ID");
    }
    validate_nonblank("accepted review receipt node ID", &receipt.review_node_id)?;
    validate_nonblank("accepted review receipt URL", &receipt.html_url)?;
    Ok(())
}

fn validate_pending_review(review: &GitHubDeliveryPendingReview) -> Result<()> {
    if review.review_id == 0 {
        bail!("accepted review receipt has a zero review database ID");
    }
    validate_nonblank("accepted review receipt node ID", &review.review_node_id)?;
    validate_nonblank("accepted review receipt URL", &review.html_url)?;
    if review.comments.is_empty() {
        bail!(
            "accepted review {} has no comment receipts",
            review.review_id
        );
    }
    let mut records = HashSet::new();
    let mut operations = HashSet::new();
    for comment in &review.comments {
        validate_comment_receipt(comment)?;
        if !records.insert(comment.record_id.as_str()) {
            bail!(
                "accepted review {} has duplicate record ID {}",
                review.review_id,
                comment.record_id
            );
        }
        if !operations.insert(comment.operation_id) {
            bail!(
                "accepted review {} has duplicate comment operation ID {}",
                review.review_id,
                comment.operation_id
            );
        }
    }
    Ok(())
}

fn validate_comment_receipts(
    expected: &[GitHubDeliveryComment],
    actual: &[GitHubDeliveryCommentReceipt],
) -> Result<()> {
    if expected.len() != actual.len() {
        bail!(
            "accepted comment receipt count {} does not match intent count {}",
            actual.len(),
            expected.len()
        );
    }
    let expected = expected
        .iter()
        .map(|comment| (comment.record_id.as_str(), comment.operation_id))
        .collect::<HashSet<_>>();
    let mut actual_ids = HashSet::new();
    for receipt in actual {
        validate_comment_receipt(receipt)?;
        let key = (receipt.record_id.as_str(), receipt.operation_id);
        if !actual_ids.insert(key) {
            bail!(
                "accepted receipts duplicate record ID {} and operation ID {}",
                receipt.record_id,
                receipt.operation_id
            );
        }
        if !expected.contains(&key) {
            bail!(
                "accepted receipt record ID {} and operation ID {} does not match its intent",
                receipt.record_id,
                receipt.operation_id
            );
        }
    }
    Ok(())
}

fn validate_comment_receipt(receipt: &GitHubDeliveryCommentReceipt) -> Result<()> {
    validate_nonblank("accepted comment receipt record ID", &receipt.record_id)?;
    if let Some(thread_node_id) = &receipt.thread_node_id {
        validate_nonblank("accepted comment receipt thread node ID", thread_node_id)?;
    }
    if let Some(comment_node_id) = &receipt.comment_node_id {
        validate_nonblank("accepted comment receipt comment node ID", comment_node_id)?;
    }
    Ok(())
}

fn validate_nonblank(label: &str, value: &str) -> Result<()> {
    if value.trim().is_empty() {
        bail!("{label} must not be blank");
    }
    Ok(())
}

/// Owns the stable lock-file location for one repository's `.trueflow` directory.
#[derive(Debug, Clone)]
pub struct GitHubDeliveryLedgerStore {
    directory: PathBuf,
}

impl GitHubDeliveryLedgerStore {
    pub fn for_directory(directory: impl Into<PathBuf>) -> Self {
        Self {
            directory: directory.into(),
        }
    }

    /// Acquires the persistent lock before loading the ledger. The returned session
    /// intentionally retains the lock while callers reconcile, plan, dispatch, and
    /// persist state transitions.
    pub fn lock(&self) -> Result<GitHubDeliveryLedgerSession> {
        fs::create_dir_all(&self.directory).with_context(|| {
            format!(
                "failed to create GitHub delivery directory {}",
                self.directory.display()
            )
        })?;
        let lock_path = self.directory.join(GITHUB_DELIVERY_LEDGER_LOCK_FILE);
        let lock_file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(&lock_path)
            .with_context(|| {
                format!(
                    "failed to open GitHub delivery lock {}",
                    lock_path.display()
                )
            })?;
        lock_file.lock_exclusive().with_context(|| {
            format!(
                "failed to lock GitHub delivery state at {}",
                lock_path.display()
            )
        })?;

        let ledger_path = self.directory.join(GITHUB_DELIVERY_LEDGER_FILE);
        let ledger = load_ledger(&ledger_path)?;
        Ok(GitHubDeliveryLedgerSession {
            ledger,
            ledger_path,
            _lock_file: lock_file,
        })
    }
}

/// A locked load/mutate/save session. Dropping it releases the separate lock file.
#[derive(Debug)]
pub struct GitHubDeliveryLedgerSession {
    ledger: GitHubDeliveryLedger,
    ledger_path: PathBuf,
    _lock_file: File,
}

impl GitHubDeliveryLedgerSession {
    pub fn ledger(&self) -> &GitHubDeliveryLedger {
        &self.ledger
    }

    pub fn ledger_mut(&mut self) -> &mut GitHubDeliveryLedger {
        &mut self.ledger
    }

    /// Atomically persists the current v2 document without releasing this session's
    /// lock. On a failure before rename, the prior document remains untouched.
    pub fn save(&self) -> Result<()> {
        write_ledger_atomically(&self.ledger_path, &self.ledger)
    }
}

#[derive(Deserialize)]
struct GitHubDeliveryLedgerVersion {
    version: Option<u32>,
}

fn load_ledger(path: &Path) -> Result<GitHubDeliveryLedger> {
    let raw = match fs::read_to_string(path) {
        Ok(raw) => raw,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Ok(GitHubDeliveryLedger::default());
        }
        Err(error) => {
            return Err(error).with_context(|| {
                format!("failed to read GitHub delivery ledger {}", path.display())
            });
        }
    };
    let header: GitHubDeliveryLedgerVersion = serde_json::from_str(&raw)
        .with_context(|| format!("failed to parse GitHub delivery ledger {}", path.display()))?;
    let version = header.version.ok_or_else(|| {
        anyhow!(
            "GitHub delivery ledger has no version; delivery cannot continue safely until remote state is resolved"
        )
    })?;
    if version != GITHUB_DELIVERY_LEDGER_VERSION {
        bail!(
            "GitHub delivery ledger version {version} is unsupported; expected version {GITHUB_DELIVERY_LEDGER_VERSION}; delivery cannot continue safely until remote state is resolved"
        );
    }
    let ledger: GitHubDeliveryLedger = serde_json::from_str(&raw)
        .with_context(|| format!("failed to parse GitHub delivery ledger {}", path.display()))?;
    ledger.validate()?;
    Ok(ledger)
}

fn write_ledger_atomically(path: &Path, ledger: &GitHubDeliveryLedger) -> Result<()> {
    write_ledger_atomically_inner(path, ledger, || Ok(()))
}

#[cfg(test)]
fn write_ledger_atomically_before_rename(
    path: &Path,
    ledger: &GitHubDeliveryLedger,
    before_rename: impl FnOnce() -> Result<()>,
) -> Result<()> {
    write_ledger_atomically_inner(path, ledger, before_rename)
}

fn write_ledger_atomically_inner(
    path: &Path,
    ledger: &GitHubDeliveryLedger,
    before_rename: impl FnOnce() -> Result<()>,
) -> Result<()> {
    ledger.validate()?;
    let parent = path.parent().ok_or_else(|| {
        anyhow!(
            "GitHub delivery ledger path has no parent: {}",
            path.display()
        )
    })?;
    fs::create_dir_all(parent).with_context(|| {
        format!(
            "failed to create GitHub delivery directory {}",
            parent.display()
        )
    })?;

    let serialized = format!("{}\n", serde_json::to_string_pretty(ledger)?);
    let temporary_path = parent.join(format!(".github_delivery.{}.tmp", Uuid::new_v4()));
    let mut temporary = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&temporary_path)
        .with_context(|| {
            format!(
                "failed to create temporary GitHub delivery ledger {}",
                temporary_path.display()
            )
        })?;
    temporary
        .write_all(serialized.as_bytes())
        .with_context(|| format!("failed to write {}", temporary_path.display()))?;
    temporary
        .flush()
        .with_context(|| format!("failed to flush {}", temporary_path.display()))?;
    temporary
        .sync_all()
        .with_context(|| format!("failed to sync {}", temporary_path.display()))?;

    // The hook exists only to make the pre-rename preservation contract testable.
    // A failure here deliberately leaves both the old document and an ignored,
    // uniquely named temporary file in place.
    before_rename()?;
    fs::rename(&temporary_path, path).with_context(|| {
        format!(
            "failed to atomically replace GitHub delivery ledger {}",
            path.display()
        )
    })?;
    sync_parent_directory(parent)
}

#[cfg(unix)]
fn sync_parent_directory(parent: &Path) -> Result<()> {
    let directory = File::open(parent).with_context(|| {
        format!(
            "failed to open GitHub delivery directory {}",
            parent.display()
        )
    })?;
    match directory.sync_all() {
        Ok(()) => Ok(()),
        // Some Unix filesystems do not support syncing directory descriptors. The
        // file itself is already synced; permit this documented platform limitation.
        Err(error) if error.kind() == std::io::ErrorKind::InvalidInput => Ok(()),
        Err(error) => Err(error).with_context(|| {
            format!(
                "failed to sync GitHub delivery directory {}",
                parent.display()
            )
        }),
    }
}

#[cfg(not(unix))]
fn sync_parent_directory(_parent: &Path) -> Result<()> {
    // Windows does not expose a portable directory fsync through std::fs.
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::github::{GitHubCommentSide, GitHubInlineComment, ResolvedPullRequestRef};
    use crate::repo_path::RepoPath;
    use crate::store::CommitId;
    use crate::test_git::temp_test_dir;
    use anyhow::Result;
    use std::fs::{self, File};
    use std::io::Write;
    use std::process::Command;
    use std::thread;
    use std::time::{Duration, Instant};

    fn pr() -> ResolvedPullRequestRef {
        ResolvedPullRequestRef {
            host: "github.com".to_string(),
            owner: "jmqd".to_string(),
            repo: "trueflow".to_string(),
            number: 11,
        }
    }

    fn head() -> CommitId {
        CommitId::new("1111111111111111111111111111111111111111").unwrap()
    }

    fn comment(
        record_id: impl Into<String>,
        operation_id: GitHubDeliveryOperationId,
    ) -> GitHubDeliveryComment {
        GitHubDeliveryComment {
            record_id: record_id.into(),
            operation_id,
            comment: GitHubInlineComment {
                path: RepoPath::new("src/lib.rs").unwrap(),
                line: 8,
                side: GitHubCommentSide::Right,
                start_line: None,
                start_side: None,
                body: "<!-- trueflow:delivery-comment -->\nA marked comment".to_string(),
            },
        }
    }

    fn create_intent(
        record_id: impl Into<String>,
        comment_operation_id: GitHubDeliveryOperationId,
    ) -> GitHubDeliveryIntent {
        GitHubDeliveryIntent::CreatePendingReview {
            pr: pr(),
            head_sha: head(),
            review_body: "<!-- trueflow:pending-review -->\nMarked review".to_string(),
            comments: vec![comment(record_id, comment_operation_id)],
        }
    }

    fn create_operation(record_id: impl Into<String>) -> GitHubDeliveryOperation {
        let operation_id = GitHubDeliveryOperationId::new();
        GitHubDeliveryOperation::prepared(
            operation_id,
            create_intent(record_id, GitHubDeliveryOperationId::new()),
        )
    }

    fn accepted_comment(
        record_id: impl Into<String>,
        operation_id: GitHubDeliveryOperationId,
    ) -> GitHubDeliveryCommentReceipt {
        GitHubDeliveryCommentReceipt {
            record_id: record_id.into(),
            operation_id,
            thread_node_id: Some("PRRT_1".to_string()),
            comment_node_id: Some("PRRC_1".to_string()),
        }
    }

    #[test]
    fn rejects_non_v2_ledger_versions_without_resetting_the_document() -> Result<()> {
        let directory = temp_test_dir("github_delivery_reject_version");
        fs::create_dir_all(&directory)?;
        let ledger_path = directory.join(GITHUB_DELIVERY_LEDGER_FILE);
        let original = r#"{"version":1,"pull_requests":[]}"#;
        let mut file = File::create(&ledger_path)?;
        file.write_all(original.as_bytes())?;
        file.sync_all()?;

        let error = match GitHubDeliveryLedgerStore::for_directory(&directory).lock() {
            Err(error) => error,
            Ok(_) => anyhow::bail!("v1 ledger must fail closed"),
        };

        assert!(
            error.to_string().contains("version 1"),
            "unexpected error: {error:#}"
        );
        assert!(
            error.to_string().contains("cannot continue safely"),
            "unexpected error: {error:#}"
        );
        assert_eq!(fs::read_to_string(&ledger_path)?, original);
        Ok(())
    }

    #[test]
    fn prepared_in_flight_and_accepted_entries_exclude_their_head_scoped_record() -> Result<()> {
        let mut ledger = GitHubDeliveryLedger::default();
        let create_operation_id = GitHubDeliveryOperationId::new();
        let comment_operation_id = GitHubDeliveryOperationId::new();
        let operation = GitHubDeliveryOperation::prepared(
            create_operation_id,
            create_intent("record-1", comment_operation_id),
        );

        ledger.prepare(operation)?;
        assert!(
            ledger
                .excluded_record_ids_for_head(&pr(), &head())
                .contains("record-1")
        );

        ledger.transition_to_in_flight(&create_operation_id)?;
        assert!(
            ledger
                .excluded_record_ids_for_head(&pr(), &head())
                .contains("record-1")
        );

        ledger.accept_create(
            &create_operation_id,
            GitHubDeliveryPendingReviewReceipt {
                review_id: 42,
                review_node_id: "PRR_42".to_string(),
                html_url: "https://example.test/reviews/42".to_string(),
                comments: vec![accepted_comment("record-1", comment_operation_id)],
            },
        )?;
        assert!(
            ledger
                .excluded_record_ids_for_head(&pr(), &head())
                .contains("record-1")
        );
        assert!(ledger.active_operations().is_empty());
        assert_eq!(ledger.pending_reviews().len(), 1);
        Ok(())
    }

    #[test]
    fn append_intent_uses_the_same_durable_identity_for_transition_and_receipt() -> Result<()> {
        let mut ledger = GitHubDeliveryLedger::default();
        let create_operation_id = GitHubDeliveryOperationId::new();
        let create_comment_operation_id = GitHubDeliveryOperationId::new();
        ledger.prepare(GitHubDeliveryOperation::prepared(
            create_operation_id,
            create_intent("record-1", create_comment_operation_id),
        ))?;
        ledger.transition_to_in_flight(&create_operation_id)?;
        ledger.accept_create(
            &create_operation_id,
            GitHubDeliveryPendingReviewReceipt {
                review_id: 42,
                review_node_id: "PRR_42".to_string(),
                html_url: "https://example.test/reviews/42".to_string(),
                comments: vec![accepted_comment("record-1", create_comment_operation_id)],
            },
        )?;

        let append_operation_id = GitHubDeliveryOperationId::new();
        ledger.prepare(GitHubDeliveryOperation::prepared(
            append_operation_id,
            GitHubDeliveryIntent::AppendReviewThread {
                pr: pr(),
                head_sha: head(),
                review_id: 42,
                review_node_id: "PRR_42".to_string(),
                review_url: "https://example.test/reviews/42".to_string(),
                comment: comment("record-2", append_operation_id),
            },
        ))?;
        assert!(
            ledger
                .excluded_record_ids_for_head(&pr(), &head())
                .contains("record-2")
        );
        ledger.transition_to_in_flight(&append_operation_id)?;
        ledger.accept_append(
            &append_operation_id,
            accepted_comment("record-2", append_operation_id),
        )?;

        assert!(ledger.active_operations().is_empty());
        assert_eq!(ledger.pending_reviews()[0].comments.len(), 2);
        assert!(
            ledger
                .excluded_record_ids_for_head(&pr(), &head())
                .contains("record-2")
        );
        Ok(())
    }

    #[test]
    fn only_prepared_intents_can_be_cancelled_and_replanned() -> Result<()> {
        let mut ledger = GitHubDeliveryLedger::default();
        let prepared_id = GitHubDeliveryOperationId::new();
        ledger.prepare(GitHubDeliveryOperation::prepared(
            prepared_id,
            create_intent("record-1", GitHubDeliveryOperationId::new()),
        ))?;

        let cancelled = ledger.cancel_prepared(&prepared_id)?;
        assert!(cancelled.is_create());
        assert!(
            !ledger
                .excluded_record_ids_for_head(&pr(), &head())
                .contains("record-1")
        );
        ledger.prepare(create_operation("record-1"))?;

        let in_flight_id = ledger.active_operations()[0].id;
        ledger.transition_to_in_flight(&in_flight_id)?;
        let error = match ledger.cancel_prepared(&in_flight_id) {
            Err(error) => error,
            Ok(_) => anyhow::bail!("in-flight intent may already have reached GitHub"),
        };
        assert!(error.to_string().contains("after it is in flight"));
        Ok(())
    }

    #[test]
    fn rejects_duplicate_record_and_operation_identities() -> Result<()> {
        let mut ledger = GitHubDeliveryLedger::default();
        let first_operation_id = GitHubDeliveryOperationId::new();
        let first_comment_operation_id = GitHubDeliveryOperationId::new();
        ledger.prepare(GitHubDeliveryOperation::prepared(
            first_operation_id,
            create_intent("record-1", first_comment_operation_id),
        ))?;

        let duplicate_record = match ledger.prepare(GitHubDeliveryOperation::prepared(
            GitHubDeliveryOperationId::new(),
            create_intent("record-1", GitHubDeliveryOperationId::new()),
        )) {
            Err(error) => error,
            Ok(()) => anyhow::bail!("active record must not be planned twice"),
        };
        assert!(
            duplicate_record
                .to_string()
                .contains("already reserved for this pull request and head")
        );

        let duplicate_operation = match ledger.prepare(GitHubDeliveryOperation::prepared(
            first_operation_id,
            create_intent("record-2", GitHubDeliveryOperationId::new()),
        )) {
            Err(error) => error,
            Ok(()) => anyhow::bail!("operation ID must remain globally unique"),
        };
        assert!(
            duplicate_operation
                .to_string()
                .contains("duplicate delivery operation ID")
        );
        Ok(())
    }

    #[test]
    fn terminal_tombstones_retain_accepted_record_exclusions() -> Result<()> {
        let mut ledger = GitHubDeliveryLedger::default();
        let create_operation_id = GitHubDeliveryOperationId::new();
        let comment_operation_id = GitHubDeliveryOperationId::new();
        ledger.prepare(GitHubDeliveryOperation::prepared(
            create_operation_id,
            create_intent("record-1", comment_operation_id),
        ))?;
        ledger.transition_to_in_flight(&create_operation_id)?;
        ledger.accept_create(
            &create_operation_id,
            GitHubDeliveryPendingReviewReceipt {
                review_id: 42,
                review_node_id: "PRR_42".to_string(),
                html_url: "https://example.test/reviews/42".to_string(),
                comments: vec![accepted_comment("record-1", comment_operation_id)],
            },
        )?;

        ledger.tombstone_pending_review(&pr(), 42, GitHubDeliveryTerminalReason::Missing)?;

        assert!(ledger.pending_reviews().is_empty());
        assert_eq!(ledger.terminal_reviews().len(), 1);
        assert!(
            ledger
                .excluded_record_ids_for_head(&pr(), &head())
                .contains("record-1")
        );
        Ok(())
    }

    #[test]
    fn failed_pre_rename_save_preserves_the_previous_valid_document() -> Result<()> {
        let directory = temp_test_dir("github_delivery_preserve_save_failure");
        let ledger_path = directory.join(GITHUB_DELIVERY_LEDGER_FILE);
        let mut original_ledger = GitHubDeliveryLedger::default();
        original_ledger.prepare(create_operation("record-1"))?;
        write_ledger_atomically(&ledger_path, &original_ledger)?;
        let original = fs::read_to_string(&ledger_path)?;

        let mut replacement_ledger = original_ledger.clone();
        replacement_ledger.prepare(create_operation("record-2"))?;
        let error =
            match write_ledger_atomically_before_rename(&ledger_path, &replacement_ledger, || {
                Err(anyhow::anyhow!("injected pre-rename failure"))
            }) {
                Err(error) => error,
                Ok(()) => anyhow::bail!("injected failure must stop before rename"),
            };

        assert!(error.to_string().contains("injected pre-rename failure"));
        assert_eq!(fs::read_to_string(&ledger_path)?, original);
        assert_eq!(
            GitHubDeliveryLedgerStore::for_directory(&directory)
                .lock()?
                .ledger()
                .excluded_record_ids_for_head(&pr(), &head())
                .len(),
            1
        );
        Ok(())
    }

    #[test]
    fn lock_serializes_separate_processes_through_the_persistent_lock_file() -> Result<()> {
        let directory = temp_test_dir("github_delivery_lock_serialization");
        fs::create_dir_all(&directory)?;
        let attempted = directory.join("child-attempted");
        let acquired = directory.join("child-acquired");
        let store = GitHubDeliveryLedgerStore::for_directory(&directory);
        let session = store.lock()?;

        let mut child = Command::new(std::env::current_exe()?)
            .args([
                "--exact",
                "github_delivery::tests::lock_serialization_child",
                "--nocapture",
            ])
            .env("TRUEFLOW_GITHUB_DELIVERY_LOCK_CHILD_DIR", &directory)
            .env("TRUEFLOW_GITHUB_DELIVERY_LOCK_CHILD_ATTEMPTED", &attempted)
            .env("TRUEFLOW_GITHUB_DELIVERY_LOCK_CHILD_ACQUIRED", &acquired)
            .spawn()?;

        wait_for_file(&attempted)?;
        thread::sleep(Duration::from_millis(100));
        assert!(
            !acquired.exists(),
            "second process acquired github_delivery.lock while first session held it"
        );
        drop(session);

        assert!(child.wait()?.success(), "lock child failed");
        assert!(acquired.exists(), "child did not acquire released lock");
        assert!(directory.join(GITHUB_DELIVERY_LEDGER_LOCK_FILE).is_file());
        Ok(())
    }

    #[test]
    fn lock_serialization_child() -> Result<()> {
        let Ok(directory) = std::env::var("TRUEFLOW_GITHUB_DELIVERY_LOCK_CHILD_DIR") else {
            return Ok(());
        };
        let attempted = std::env::var("TRUEFLOW_GITHUB_DELIVERY_LOCK_CHILD_ATTEMPTED")?;
        let acquired = std::env::var("TRUEFLOW_GITHUB_DELIVERY_LOCK_CHILD_ACQUIRED")?;
        let mut attempted_file = File::create(attempted)?;
        attempted_file.write_all(b"attempting")?;
        attempted_file.sync_all()?;

        let _session = GitHubDeliveryLedgerStore::for_directory(directory).lock()?;
        let mut acquired_file = File::create(acquired)?;
        acquired_file.write_all(b"acquired")?;
        acquired_file.sync_all()?;
        Ok(())
    }

    fn wait_for_file(path: &std::path::Path) -> Result<()> {
        let deadline = Instant::now() + Duration::from_secs(5);
        while !path.exists() {
            if Instant::now() >= deadline {
                anyhow::bail!("timed out waiting for {path:?}");
            }
            thread::sleep(Duration::from_millis(10));
        }
        Ok(())
    }
}
