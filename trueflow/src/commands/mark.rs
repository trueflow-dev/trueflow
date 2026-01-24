use crate::context::TrueflowContext;
use crate::store::{FileStore, Identity, Record, ReviewStore, Verdict};
use anyhow::{Context, Result};
use git2::Repository;
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

pub fn run(
    _context: &TrueflowContext,
    fingerprint: &str,
    verdict: &str,
    check: &str,
    note: Option<&str>,
    path_hint: Option<&str>,
    line_hint: Option<u32>,
) -> Result<()> {
    info!(
        "mark start (fingerprint={}, verdict={}, check={}, note_present={}, path_hint={:?}, line_hint={:?})",
        fingerprint,
        verdict,
        check,
        note.is_some(),
        path_hint,
        line_hint
    );
    let store = FileStore::new()?;

    let verdict: Verdict = verdict.parse()?;

    // We still use git config for Identity if available, but fall back gracefully
    let (email, signing_key) = if let Ok(repo) = Repository::discover(".") {
        let config = repo.config()?;
        let email = config
            .get_string("user.email")
            .unwrap_or_else(|_| "unknown@localhost".to_string());
        let key = config.get_string("user.signingkey").ok();
        (email, key)
    } else {
        ("unknown@localhost".to_string(), None)
    };

    let now = SystemTime::now().duration_since(UNIX_EPOCH)?.as_secs() as i64;

    // Prepare data to sign (Fingerprint + Verdict + Timestamp provides replay protection)
    let payload = format!("{}:{}:{}", fingerprint, verdict.as_str(), now);

    // Attempt signing if configured
    let signature = if signing_key.is_some() {
        Some(sign_data(&payload, signing_key.as_deref())?)
    } else {
        None
    };

    let identity = Identity::Email {
        email: email.clone(),
        signature,
    };

    let record = Record {
        id: Uuid::new_v4().to_string(),
        fingerprint: fingerprint.to_string(),
        check: check.to_string(),
        verdict: verdict.clone(),
        identity,
        timestamp: now,
        path_hint: path_hint.map(|s| s.to_string()),
        line_hint,
        note: note.map(|s| s.to_string()),
        tags: None,
    };

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
    println!(
        "Recorded verdict '{}' for {} by {}{}",
        verdict, fingerprint, email, signed_msg
    );
    Ok(())
}
