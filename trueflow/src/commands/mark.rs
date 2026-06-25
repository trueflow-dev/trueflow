use crate::context::TrueflowContext;
use crate::gpg::{GpgClient, SigningMode};
use crate::path_utils;
use crate::repo_path::RepoPath;
use crate::scanner::ScanOptions;
use crate::store::{
    Attestation, AttestationKind, BlockState, Canonicalization, CommentAnchor, CommentScope,
    FileStore, Identity, Record, RepoRef, ReviewCheck, ReviewStore, ReviewTargetKind, VcsSystem,
    Verdict,
};
use crate::tree::{self, TreeNodeKind};
use crate::vcs;
use anyhow::{Context, Result};
use tracing::info;

const NONINTERACTIVE_SIGNING_FAILURE_CONTEXT: &str = "non-interactive GPG signing failed";

pub(crate) fn is_noninteractive_signing_failure(error: &anyhow::Error) -> bool {
    error
        .chain()
        .any(|cause| cause.to_string() == NONINTERACTIVE_SIGNING_FAILURE_CONTEXT)
}

fn with_noninteractive_signing_context<T>(result: Result<T>, mode: SigningMode) -> Result<T> {
    match mode {
        SigningMode::Interactive => result,
        SigningMode::NonInteractive => result.context(NONINTERACTIVE_SIGNING_FAILURE_CONTEXT),
    }
}

#[derive(Debug, Clone)]
pub struct MarkParams {
    pub fingerprint: String,
    pub target_kind: Option<ReviewTargetKind>,
    pub verdict: Verdict,
    pub check: ReviewCheck,
    pub note: Option<String>,
    pub path: Option<RepoPath>,
    pub line: Option<u32>,
    pub comment_scope: Option<CommentScope>,
    pub comment_context: Option<String>,
    pub comment_anchor: Option<CommentAnchor>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TerminalSuspendRequirement {
    NotRequired,
    Required,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct RuntimeConfig {
    email: String,
    signing_key: Option<String>,
    terminal_suspend_requirement: TerminalSuspendRequirement,
}

fn runtime_config_from_git_config(config: Option<vcs::GitConfig>) -> RuntimeConfig {
    match config {
        Some(config) => {
            let terminal_suspend_requirement =
                suspend_policy_for_signing_key(config.signing_key.as_deref());
            RuntimeConfig {
                email: config.email,
                signing_key: config.signing_key,
                terminal_suspend_requirement,
            }
        }
        None => RuntimeConfig {
            email: "unknown@localhost".to_string(),
            signing_key: None,
            terminal_suspend_requirement: TerminalSuspendRequirement::NotRequired,
        },
    }
}

pub(crate) fn suspend_policy_for_signing_key(
    signing_key: Option<&str>,
) -> TerminalSuspendRequirement {
    match signing_key {
        Some(_) => TerminalSuspendRequirement::Required,
        None => TerminalSuspendRequirement::NotRequired,
    }
}

pub fn terminal_suspend_requirement_from_workdir() -> TerminalSuspendRequirement {
    runtime_config_from_git_config(vcs::git_config_from_workdir().ok()).terminal_suspend_requirement
}

fn normalize_path_hint_from_workdir(path: Option<RepoPath>) -> Option<RepoPath> {
    let path = path?;

    let prefix = vcs::git_root_from_workdir()
        .ok()
        .flatten()
        .and_then(|repo_root| path_utils::current_workdir_prefix_for_repo_root(&repo_root))
        .and_then(|prefix| RepoPath::new(prefix).ok());

    Some(match prefix {
        Some(prefix) => path.resolve_from_prefix(&prefix),
        None => path,
    })
}

pub fn run(context: &TrueflowContext, params: MarkParams) -> Result<()> {
    run_with_signing_mode(context, params, SigningMode::Interactive)
}

pub(crate) fn run_with_noninteractive_signing(
    context: &TrueflowContext,
    params: MarkParams,
) -> Result<()> {
    run_with_signing_mode(context, params, SigningMode::NonInteractive)
}

fn run_with_signing_mode(
    _context: &TrueflowContext,
    params: MarkParams,
    signing_mode: SigningMode,
) -> Result<()> {
    info!(
        "mark start (fingerprint={}, verdict={}, check={}, note_present={}, path={:?}, line={:?}, comment_scope={:?}, comment_context_present={}, comment_anchor_present={}, signing_mode={:?})",
        &params.fingerprint,
        &params.verdict,
        &params.check,
        params.note.is_some(),
        params.path.as_ref().map(RepoPath::as_str),
        params.line,
        params.comment_scope,
        params.comment_context.is_some(),
        params.comment_anchor.is_some(),
        signing_mode,
    );
    let store = FileStore::new()?;

    let runtime_config = runtime_config_from_git_config(vcs::git_config_from_workdir().ok());
    let should_sign = matches!(
        runtime_config.terminal_suspend_requirement,
        TerminalSuspendRequirement::Required
    );
    let email = runtime_config.email;
    let signing_key = runtime_config.signing_key;
    let gpg = GpgClient::new();

    let identity = Identity::Email {
        email: email.clone(),
    };

    let repo_snapshot = vcs::snapshot_from_workdir();
    let repo_ref = match repo_snapshot.repo_ref_revision.clone() {
        Some(revision) => RepoRef::Vcs {
            system: VcsSystem::Git,
            revision,
        },
        None => RepoRef::Unknown,
    };

    let MarkParams {
        fingerprint,
        target_kind,
        verdict,
        check,
        note,
        path,
        line,
        comment_scope,
        comment_context,
        comment_anchor,
    } = params;

    let path_hint = normalize_path_hint_from_workdir(path);
    let target_kind = infer_target_kind(target_kind, &fingerprint, path_hint.as_ref(), line)?;
    let target = target_kind.parse_target(&fingerprint)?;
    let block_state: BlockState = vcs::block_state_for_path(
        &repo_snapshot,
        path_hint.as_ref().map(RepoPath::as_str),
        target.lookup_key(),
    )
    .into();

    let mut record = Record::new(
        target,
        check.clone(),
        verdict.clone(),
        identity,
        repo_ref,
        block_state,
    );
    record.path_hint = path_hint;
    record.line_hint = line;
    record.note = note;
    record.comment_scope = comment_scope;
    record.comment_context = comment_context;
    record.comment_anchor = comment_anchor;

    if should_sign {
        let payload = record.signing_payload()?;
        let signature = with_noninteractive_signing_context(
            gpg.sign_detached(&payload, signing_key.as_deref(), signing_mode),
            signing_mode,
        )?;
        let public_key = with_noninteractive_signing_context(
            gpg.export_public_key(signing_key.as_deref()),
            signing_mode,
        )?;
        record.attestations = Some(vec![Attestation {
            kind: AttestationKind::Pgp,
            canonicalization: Canonicalization::JcsV1,
            signature,
            public_key,
        }]);
    }

    store.append(&record)?;
    info!(
        "mark recorded (fingerprint={}, check={}, verdict={})",
        fingerprint,
        check.as_str(),
        verdict.as_str()
    );

    let signed_msg = if should_sign { " (Signed)" } else { "" };
    info!("Recorded verdict '{verdict}' for {fingerprint} by {email}{signed_msg}");
    Ok(())
}

fn infer_target_kind(
    explicit: Option<ReviewTargetKind>,
    fingerprint: &str,
    path_hint: Option<&RepoPath>,
    line_hint: Option<u32>,
) -> Result<ReviewTargetKind> {
    if let Some(kind) = explicit {
        return Ok(kind);
    }

    if let Some(kind) =
        infer_target_kind_from_current_tree_location(fingerprint, path_hint, line_hint)?
    {
        return Ok(kind);
    }

    if let Some(kind) = infer_target_kind_from_current_tree(fingerprint)? {
        return Ok(kind);
    }

    Ok(ReviewTargetKind::Block)
}

fn infer_target_kind_from_current_tree_location(
    fingerprint: &str,
    path_hint: Option<&RepoPath>,
    line_hint: Option<u32>,
) -> Result<Option<ReviewTargetKind>> {
    let Some(path_hint) = path_hint else {
        return Ok(None);
    };
    let scan_result = match crate::scanner::scan_directory(".", &ScanOptions::default()) {
        Ok(scan_result) => scan_result,
        Err(_) => return Ok(None),
    };
    let tree = tree::build_tree_from_files(&scan_result.files);

    if let Some(line_hint) = line_hint {
        for node in tree.nodes() {
            if node.kind != TreeNodeKind::Block
                || node.path != *path_hint
                || node.hash.as_str() != fingerprint
            {
                continue;
            }
            let Some(block) = node.block.as_ref() else {
                continue;
            };
            if u32::try_from(block.start_line).ok() == Some(line_hint) {
                return Ok(Some(ReviewTargetKind::Block));
            }
        }
    }

    for node in tree.nodes() {
        if node.path != *path_hint || node.hash.as_str() != fingerprint {
            continue;
        }
        let kind = match node.kind {
            TreeNodeKind::Root | TreeNodeKind::Directory => ReviewTargetKind::Tree,
            TreeNodeKind::File => ReviewTargetKind::File,
            TreeNodeKind::Block => ReviewTargetKind::Block,
        };
        return Ok(Some(kind));
    }

    Ok(None)
}

fn infer_target_kind_from_current_tree(fingerprint: &str) -> Result<Option<ReviewTargetKind>> {
    let scan_result = match crate::scanner::scan_directory(".", &ScanOptions::default()) {
        Ok(scan_result) => scan_result,
        Err(_) => return Ok(None),
    };
    let tree = tree::build_tree_from_files(&scan_result.files);

    for node in tree.nodes() {
        if node.hash.as_str() != fingerprint {
            continue;
        }
        let kind = match node.kind {
            TreeNodeKind::Root | TreeNodeKind::Directory => ReviewTargetKind::Tree,
            TreeNodeKind::File => ReviewTargetKind::File,
            TreeNodeKind::Block => ReviewTargetKind::Block,
        };
        return Ok(Some(kind));
    }

    Ok(None)
}

#[cfg(test)]
mod tests {
    use super::*;
    use anyhow::anyhow;

    #[test]
    fn noninteractive_signing_failure_predicate_matches_context_chain() {
        let error = anyhow!("gpg failed").context(NONINTERACTIVE_SIGNING_FAILURE_CONTEXT);

        assert!(is_noninteractive_signing_failure(&error));
    }

    #[test]
    fn noninteractive_signing_failure_predicate_rejects_unrelated_errors() {
        let error = anyhow!("store append failed");

        assert!(!is_noninteractive_signing_failure(&error));
    }

    #[test]
    fn runtime_config_without_signing_key_does_not_require_terminal_suspend() {
        let config = runtime_config_from_git_config(Some(vcs::GitConfig {
            email: "reviewer@example.com".to_string(),
            signing_key: None,
        }));

        assert_eq!(config.email, "reviewer@example.com");
        assert_eq!(config.signing_key, None);
        assert_eq!(
            config.terminal_suspend_requirement,
            TerminalSuspendRequirement::NotRequired
        );
    }

    #[test]
    fn runtime_config_with_signing_key_requires_terminal_suspend() {
        let config = runtime_config_from_git_config(Some(vcs::GitConfig {
            email: "reviewer@example.com".to_string(),
            signing_key: Some("ABC123".to_string()),
        }));

        assert_eq!(config.email, "reviewer@example.com");
        assert_eq!(config.signing_key.as_deref(), Some("ABC123"));
        assert_eq!(
            config.terminal_suspend_requirement,
            TerminalSuspendRequirement::Required
        );
    }

    #[test]
    fn runtime_config_without_git_config_falls_back_to_unsigned_defaults() {
        let config = runtime_config_from_git_config(None);

        assert_eq!(config.email, "unknown@localhost");
        assert_eq!(config.signing_key, None);
        assert_eq!(
            config.terminal_suspend_requirement,
            TerminalSuspendRequirement::NotRequired
        );
    }
}
