use crate::context::TrueflowContext;
use crate::store::{
    Attestation, AttestationKind, BlockState, Canonicalization, FileStore, Identity, Record,
    RepoRef, ReviewStore, VcsSystem, Verdict,
};
use crate::vcs;
use anyhow::{Context, Result};
use log::info;
use std::io::Write;
use std::process::{Command, Stdio};
use std::time::{SystemTime, UNIX_EPOCH};
use uuid::Uuid;

fn sign_data(data: &str, key_id: Option<&str>) -> Result<String> {
    let mut cmd = Command::new("gpg");
    cmd.arg("--detach-sign").arg("--armor");

    if let Some(kid) = key_id {
        cmd.arg("--local-user").arg(kid);
    }

    let mut child = cmd
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .context("Failed to spawn gpg")?;

    {
        let stdin = child.stdin.as_mut().context("Failed to open gpg stdin")?;
        stdin.write_all(data.as_bytes())?;
    }

    let output = child.wait_with_output()?;

    if !output.status.success() {
        return Err(anyhow::anyhow!("GPG signing failed"));
    }

    let sig = String::from_utf8(output.stdout)?;
    Ok(sig.trim().to_string())
}

fn export_public_key(key_id: Option<&str>) -> Result<String> {
    let mut cmd = Command::new("gpg");
    cmd.arg("--armor").arg("--export");

    if let Some(kid) = key_id {
        cmd.arg(kid);
    }

    let output = cmd.output().context("Failed to run gpg export")?;

    if !output.status.success() {
        return Err(anyhow::anyhow!("GPG export failed"));
    }

    let key = String::from_utf8(output.stdout)?;
    Ok(key.trim().to_string())
}

#[derive(Debug, Clone)]
pub struct MarkParams {
    pub fingerprint: String,
    pub verdict: Verdict,
    pub check: String,
    pub note: Option<String>,
    pub path: Option<String>,
    pub line: Option<u32>,
}

pub fn run(_context: &TrueflowContext, params: MarkParams) -> Result<()> {
    info!(
        "mark start (fingerprint={}, verdict={}, check={}, note_present={}, path={:?}, line={:?})",
        &params.fingerprint,
        &params.verdict,
        &params.check,
        params.note.is_some(),
        params.path.as_deref(),
        params.line
    );
    let store = FileStore::new()?;

    // We still use git config for Identity if available, but fall back gracefully
    let (email, signing_key) = match vcs::git_config_from_workdir() {
        Ok(config) => (config.email, config.signing_key),
        Err(_) => ("unknown@localhost".to_string(), None),
    };

    let now = SystemTime::now().duration_since(UNIX_EPOCH)?.as_secs() as i64;

    let identity = Identity::Email {
        email: email.clone(),
    };

    let repo_snapshot = vcs::snapshot_from_workdir();
    let revision = repo_snapshot
        .repo_ref_revision
        .clone()
        .unwrap_or_else(|| "unknown".to_string());

    let repo_ref = RepoRef::Vcs {
        system: VcsSystem::Git,
        revision,
    };

    let block_state: BlockState =
        vcs::block_state_for_path(&repo_snapshot, params.path.as_deref(), &params.fingerprint)
            .into();

    let MarkParams {
        fingerprint,
        verdict,
        check,
        note,
        path,
        line,
    } = params;

    let mut record = Record {
        id: Uuid::new_v4().to_string(),
        version: crate::store::CURRENT_VERSION,
        fingerprint: fingerprint.clone(),
        check: check.clone(),
        verdict: verdict.clone(),
        identity,
        repo_ref,
        block_state,
        timestamp: now,
        path_hint: path,
        line_hint: line,
        note,
        tags: None,
        attestations: None,
    };

    if signing_key.is_some() {
        let payload = record.signing_payload()?;
        let signature = sign_data(&payload, signing_key.as_deref())?;
        let public_key = export_public_key(signing_key.as_deref())?;
        record.attestations = Some(vec![Attestation {
            kind: AttestationKind::Pgp,
            canonicalization: Canonicalization::JcsV1,
            signature,
            public_key,
        }]);
    }

    store.append(record)?;
    info!(
        "mark recorded (fingerprint={}, check={}, verdict={})",
        fingerprint,
        check,
        verdict.as_str()
    );

    let signed_msg = if signing_key.is_some() {
        " (Signed)"
    } else {
        ""
    };
    info!(
        "Recorded verdict '{}' for {} by {}{}",
        verdict, fingerprint, email, signed_msg
    );
    Ok(())
}
