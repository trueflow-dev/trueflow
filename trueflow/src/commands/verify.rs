use crate::store::{AttestationKind, Canonicalization, FileStore, Record, ReviewStore};
use anyhow::{Context, Result};
use std::fs;
use std::path::PathBuf;
use std::process::{Command, Stdio};
use tracing::info;

struct Verifier {
    temp_dir: PathBuf,
}

impl Verifier {
    fn new() -> Result<Self> {
        let temp_dir = std::env::temp_dir()
            .join("trueflow-gpg-verify")
            .join(uuid::Uuid::new_v4().to_string());
        fs::create_dir_all(&temp_dir)?;
        Ok(Self { temp_dir })
    }

    fn verify(&self, payload: &str, signature: &str, public_key: &str) -> Result<bool> {
        // We reuse the temp dir, but write files to unique paths or overwrite.
        let key_path = self.temp_dir.join("pubkey.asc");
        let sig_path = self.temp_dir.join("signature.asc");
        let payload_path = self.temp_dir.join("payload.txt");

        fs::write(&key_path, public_key)?;
        fs::write(&sig_path, signature)?;
        fs::write(&payload_path, payload)?;

        // Import key
        let mut import = Command::new("gpg");
        import
            .arg("--batch")
            .arg("--no-tty")
            .arg("--homedir")
            .arg(&self.temp_dir)
            .arg("--import")
            .arg(&key_path)
            .stdout(Stdio::null())
            .stderr(Stdio::null());

        let import_output = import.output().context("Failed to import gpg public key")?;
        if !import_output.status.success() {
            // If import fails, we can't verify.
            return Ok(false);
        }

        // Verify signature
        let mut verify = Command::new("gpg");
        verify
            .arg("--batch")
            .arg("--no-tty")
            .arg("--homedir")
            .arg(&self.temp_dir)
            .arg("--verify")
            .arg(&sig_path)
            .arg(&payload_path)
            .stdout(Stdio::null())
            .stderr(Stdio::null());

        let verify_output = verify.output().context("Failed to verify gpg signature")?;
        Ok(verify_output.status.success())
    }
}

impl Drop for Verifier {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.temp_dir);
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VerifySelection<'a> {
    All,
    Id(&'a str),
}

impl<'a> VerifySelection<'a> {
    pub fn from_clap_args(all: bool, id: Option<&'a str>) -> Self {
        match (all, id) {
            (true, None) => Self::All,
            (false, Some(id)) => Self::Id(id),
            (true, Some(_)) | (false, None) => {
                panic!("clap invariant violated: expected exactly one of --all or --id")
            }
        }
    }
}

pub fn run(selection: VerifySelection<'_>) -> Result<()> {
    let store = FileStore::new()?;
    let records = store.read_history()?;

    let filtered = filter_records(records, selection);

    let mut attested = 0;
    let mut unattested = 0;
    let mut invalid = 0;

    let verifier = Verifier::new()?;

    for record in filtered {
        let Some(attestations) = record.attestations.as_ref() else {
            unattested += 1;
            continue;
        };

        if attestations.is_empty() {
            unattested += 1;
            continue;
        }

        let payload = record.signing_payload()?;
        let mut record_invalid = false;
        let mut record_invalid_count = 0;

        for (index, attestation) in attestations.iter().enumerate() {
            if attestation.kind != AttestationKind::Pgp
                || attestation.canonicalization != Canonicalization::JcsV1
            {
                record_invalid = true;
                record_invalid_count += 1;
                eprintln!(
                    "INVALID ATTESTATION TYPE/CANON id={} attestation={}",
                    record.id, index
                );
                continue;
            }

            match verifier.verify(&payload, &attestation.signature, &attestation.public_key) {
                Ok(true) => {}
                Ok(false) => {
                    record_invalid = true;
                    record_invalid_count += 1;
                    eprintln!(
                        "SIGNATURE VERIFICATION FAILED id={} attestation={}",
                        record.id, index
                    );
                }
                Err(e) => {
                    record_invalid = true;
                    record_invalid_count += 1;
                    info!("attestation verification error: {e}");
                    eprintln!(
                        "SIGNATURE VERIFICATION ERROR id={} attestation={}: {}",
                        record.id, index, e
                    );
                }
            }
        }

        if record_invalid {
            invalid += record_invalid_count;
            continue;
        }

        attested += 1;
    }

    println!("Attested: {attested}");
    println!("Unattested: {unattested}");
    println!("Invalid: {invalid}");

    if invalid > 0 {
        anyhow::bail!("Signature verification failed");
    }

    Ok(())
}

fn filter_records(records: Vec<Record>, selection: VerifySelection<'_>) -> Vec<Record> {
    match selection {
        VerifySelection::All => records,
        VerifySelection::Id(target) => records
            .into_iter()
            .filter(|record| record.id == target)
            .collect(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::hashing::TreeHash;
    use crate::store::{
        BlockState, CommitId, Identity, RepoRef, ReviewCheck, ReviewTargetRef, VcsSystem, Verdict,
    };

    fn record(id: &str) -> Record {
        Record {
            id: id.to_string(),
            version: 1,
            target: ReviewTargetRef::Block {
                hash: TreeHash::parse(
                    "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef",
                )
                .unwrap(),
            },
            check: ReviewCheck::review(),
            verdict: Verdict::Approved,
            identity: Identity::Email {
                email: "test@example.com".to_string(),
            },
            repo_ref: RepoRef::Vcs {
                system: VcsSystem::Git,
                revision: CommitId::new("0123456789abcdef").unwrap(),
            },
            block_state: BlockState::Committed,
            timestamp: 1,
            path_hint: None,
            line_hint: None,
            note: None,
            tags: None,
            attestations: None,
        }
    }

    #[test]
    fn verify_selection_accepts_all_without_id() {
        assert_eq!(
            VerifySelection::from_clap_args(true, None),
            VerifySelection::All
        );
    }

    #[test]
    fn verify_selection_accepts_single_id_without_all() {
        assert_eq!(
            VerifySelection::from_clap_args(false, Some("abc")),
            VerifySelection::Id("abc")
        );
    }

    #[test]
    fn filter_records_uses_explicit_selection() {
        let records = vec![record("one"), record("two")];

        let filtered = filter_records(records, VerifySelection::Id("two"));

        assert_eq!(filtered.len(), 1);
        assert_eq!(filtered[0].id, "two");
    }
}
