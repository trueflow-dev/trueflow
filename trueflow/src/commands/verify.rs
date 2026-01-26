use crate::store::{AttestationKind, Canonicalization, FileStore, Record, ReviewStore};
use anyhow::{Context, Result};
use log::info;
use std::fs;
use std::path::PathBuf;
use std::process::{Command, Stdio};

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

pub fn run(all: bool, id: Option<String>) -> Result<()> {
    let store = FileStore::new()?;
    let records = store.read_history()?;

    let filtered = filter_records(records, all, id.as_deref())?;

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

    println!("Attested: {}", attested);
    println!("Unattested: {}", unattested);
    println!("Invalid: {}", invalid);

    if invalid > 0 {
        anyhow::bail!("Signature verification failed");
    }

    Ok(())
}

fn filter_records(records: Vec<Record>, all: bool, id: Option<&str>) -> Result<Vec<Record>> {
    if all && id.is_some() {
        anyhow::bail!("Use --all or --id, not both");
    }

    if !all && id.is_none() {
        anyhow::bail!("Provide --all or --id");
    }

    if let Some(target) = id {
        return Ok(records
            .into_iter()
            .filter(|record| record.id == target)
            .collect());
    }

    Ok(records)
}
