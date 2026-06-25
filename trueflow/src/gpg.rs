use anyhow::{Context, Result, anyhow};
use std::collections::HashSet;
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SigningMode {
    Interactive,
    NonInteractive,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct GpgCommandOutput {
    success: bool,
    stdout: Vec<u8>,
    stderr: Vec<u8>,
}

impl GpgCommandOutput {
    #[cfg(test)]
    fn success(stdout: impl Into<Vec<u8>>) -> Self {
        Self {
            success: true,
            stdout: stdout.into(),
            stderr: Vec::new(),
        }
    }

    #[cfg(test)]
    fn failure(stderr: impl Into<Vec<u8>>) -> Self {
        Self {
            success: false,
            stdout: Vec::new(),
            stderr: stderr.into(),
        }
    }
}

pub(crate) trait GpgRunner {
    fn sign_detached(
        &self,
        data: &str,
        key_id: Option<&str>,
        mode: SigningMode,
    ) -> Result<GpgCommandOutput>;

    fn export_public_key(&self, key_id: Option<&str>) -> Result<GpgCommandOutput>;

    fn import_public_key(&self, homedir: &Path, key_path: &Path) -> Result<GpgCommandOutput>;

    fn verify_signature(
        &self,
        homedir: &Path,
        signature_path: &Path,
        payload_path: &Path,
    ) -> Result<GpgCommandOutput>;
}

#[derive(Debug, Clone, Copy)]
pub(crate) struct SystemGpgRunner;

impl GpgRunner for SystemGpgRunner {
    fn sign_detached(
        &self,
        data: &str,
        key_id: Option<&str>,
        mode: SigningMode,
    ) -> Result<GpgCommandOutput> {
        let mut cmd = Command::new("gpg");
        if matches!(mode, SigningMode::NonInteractive) {
            cmd.arg("--batch")
                .arg("--no-tty")
                .arg("--pinentry-mode")
                .arg("error");
        }
        cmd.arg("--detach-sign").arg("--armor");

        if let Some(key_id) = key_id {
            cmd.arg("--local-user").arg(key_id);
        }

        let mut child = cmd
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .context("Failed to spawn gpg")?;

        {
            let stdin = child.stdin.as_mut().context("Failed to open gpg stdin")?;
            stdin.write_all(data.as_bytes())?;
        }

        let output = child.wait_with_output()?;
        Ok(GpgCommandOutput {
            success: output.status.success(),
            stdout: output.stdout,
            stderr: output.stderr,
        })
    }

    fn export_public_key(&self, key_id: Option<&str>) -> Result<GpgCommandOutput> {
        let mut cmd = Command::new("gpg");
        cmd.arg("--batch")
            .arg("--no-tty")
            .arg("--armor")
            .arg("--export");

        if let Some(key_id) = key_id {
            cmd.arg(key_id);
        }

        let output = cmd.output().context("Failed to run gpg export")?;
        Ok(GpgCommandOutput {
            success: output.status.success(),
            stdout: output.stdout,
            stderr: output.stderr,
        })
    }

    fn import_public_key(&self, homedir: &Path, key_path: &Path) -> Result<GpgCommandOutput> {
        let mut import = Command::new("gpg");
        import
            .arg("--batch")
            .arg("--no-tty")
            .arg("--homedir")
            .arg(homedir)
            .arg("--import")
            .arg(key_path)
            .stdout(Stdio::null())
            .stderr(Stdio::piped());

        let output = import.output().context("Failed to import gpg public key")?;
        Ok(GpgCommandOutput {
            success: output.status.success(),
            stdout: output.stdout,
            stderr: output.stderr,
        })
    }

    fn verify_signature(
        &self,
        homedir: &Path,
        signature_path: &Path,
        payload_path: &Path,
    ) -> Result<GpgCommandOutput> {
        let mut verify = Command::new("gpg");
        verify
            .arg("--batch")
            .arg("--no-tty")
            .arg("--homedir")
            .arg(homedir)
            .arg("--verify")
            .arg(signature_path)
            .arg(payload_path)
            .stdout(Stdio::null())
            .stderr(Stdio::piped());

        let output = verify.output().context("Failed to verify gpg signature")?;
        Ok(GpgCommandOutput {
            success: output.status.success(),
            stdout: output.stdout,
            stderr: output.stderr,
        })
    }
}

pub(crate) struct GpgClient<R = SystemGpgRunner> {
    runner: R,
    verify_home: Option<PathBuf>,
    imported_public_keys: HashSet<String>,
}

impl GpgClient<SystemGpgRunner> {
    pub(crate) fn new() -> Self {
        Self::with_runner(SystemGpgRunner)
    }
}

impl<R: GpgRunner> GpgClient<R> {
    fn with_runner(runner: R) -> Self {
        Self {
            runner,
            verify_home: None,
            imported_public_keys: HashSet::new(),
        }
    }

    pub(crate) fn sign_detached(
        &self,
        data: &str,
        key_id: Option<&str>,
        mode: SigningMode,
    ) -> Result<String> {
        let output = self.runner.sign_detached(data, key_id, mode)?;
        ensure_success("GPG signing failed", output).and_then(output_to_trimmed_utf8)
    }

    pub(crate) fn export_public_key(&self, key_id: Option<&str>) -> Result<String> {
        let output = self.runner.export_public_key(key_id)?;
        ensure_success("GPG export failed", output).and_then(output_to_trimmed_utf8)
    }

    pub(crate) fn verify(
        &mut self,
        payload: &str,
        signature: &str,
        public_key: &str,
    ) -> Result<bool> {
        if !self.ensure_public_key_imported(public_key)? {
            return Ok(false);
        }

        let verify_home = self.ensure_verify_home()?;
        let signature_path = verify_home.join("signature.asc");
        let payload_path = verify_home.join("payload.txt");

        fs::write(&signature_path, signature)?;
        fs::write(&payload_path, payload)?;

        let output = self
            .runner
            .verify_signature(&verify_home, &signature_path, &payload_path)?;
        Ok(output.success)
    }

    fn ensure_public_key_imported(&mut self, public_key: &str) -> Result<bool> {
        if self.imported_public_keys.contains(public_key) {
            return Ok(true);
        }

        let verify_home = self.ensure_verify_home()?;
        let key_path = verify_home.join("pubkey.asc");
        fs::write(&key_path, public_key)?;
        let output = self.runner.import_public_key(&verify_home, &key_path)?;
        if !output.success {
            return Ok(false);
        }

        self.imported_public_keys.insert(public_key.to_string());
        Ok(true)
    }

    fn ensure_verify_home(&mut self) -> Result<PathBuf> {
        if let Some(verify_home) = self.verify_home.as_ref() {
            return Ok(verify_home.clone());
        }

        let verify_home = std::env::temp_dir()
            .join("trueflow-gpg-verify")
            .join(uuid::Uuid::new_v4().to_string());
        fs::create_dir_all(&verify_home)?;
        self.verify_home = Some(verify_home.clone());
        Ok(verify_home)
    }
}

impl<R> Drop for GpgClient<R> {
    fn drop(&mut self) {
        if let Some(verify_home) = self.verify_home.as_ref() {
            let _ = fs::remove_dir_all(verify_home);
        }
    }
}

fn ensure_success(context: &str, output: GpgCommandOutput) -> Result<Vec<u8>> {
    if output.success {
        Ok(output.stdout)
    } else {
        Err(gpg_error(context, &output.stderr))
    }
}

fn output_to_trimmed_utf8(output: Vec<u8>) -> Result<String> {
    Ok(String::from_utf8(output)?.trim().to_string())
}

fn gpg_error(context: &str, stderr: &[u8]) -> anyhow::Error {
    let stderr = String::from_utf8_lossy(stderr);
    let stderr = stderr.trim();
    if stderr.is_empty() {
        anyhow!(context.to_string())
    } else {
        anyhow!("{context}: {stderr}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;
    use std::rc::Rc;

    #[derive(Default)]
    struct FakeGpgRunner {
        sign_output: RefCell<Option<GpgCommandOutput>>,
        export_output: RefCell<Option<GpgCommandOutput>>,
        imported_keys: RefCell<Vec<String>>,
        verified_payloads: RefCell<Vec<String>>,
    }

    impl GpgRunner for Rc<FakeGpgRunner> {
        fn sign_detached(
            &self,
            _data: &str,
            _key_id: Option<&str>,
            _mode: SigningMode,
        ) -> Result<GpgCommandOutput> {
            Ok(self
                .sign_output
                .borrow_mut()
                .take()
                .unwrap_or_else(|| GpgCommandOutput::success("signature\n")))
        }

        fn export_public_key(&self, _key_id: Option<&str>) -> Result<GpgCommandOutput> {
            Ok(self
                .export_output
                .borrow_mut()
                .take()
                .unwrap_or_else(|| GpgCommandOutput::success("public key\n")))
        }

        fn import_public_key(&self, _homedir: &Path, key_path: &Path) -> Result<GpgCommandOutput> {
            self.imported_keys
                .borrow_mut()
                .push(fs::read_to_string(key_path)?);
            Ok(GpgCommandOutput::success(Vec::new()))
        }

        fn verify_signature(
            &self,
            _homedir: &Path,
            _signature_path: &Path,
            payload_path: &Path,
        ) -> Result<GpgCommandOutput> {
            self.verified_payloads
                .borrow_mut()
                .push(fs::read_to_string(payload_path)?);
            Ok(GpgCommandOutput::success(Vec::new()))
        }
    }

    #[test]
    fn sign_detached_trims_armored_signature_output() {
        let runner = Rc::new(FakeGpgRunner::default());
        runner
            .sign_output
            .borrow_mut()
            .replace(GpgCommandOutput::success("signature\n\n"));
        let client = GpgClient::with_runner(Rc::clone(&runner));

        let signature = client
            .sign_detached("payload", Some("ABC123"), SigningMode::NonInteractive)
            .unwrap();

        assert_eq!(signature, "signature");
    }

    #[test]
    fn export_public_key_reports_gpg_stderr_on_failure() {
        let runner = Rc::new(FakeGpgRunner::default());
        runner
            .export_output
            .borrow_mut()
            .replace(GpgCommandOutput::failure("no public key\n"));
        let client = GpgClient::with_runner(Rc::clone(&runner));

        let error = client.export_public_key(Some("missing")).unwrap_err();

        assert_eq!(error.to_string(), "GPG export failed: no public key");
    }

    #[test]
    fn verify_imports_each_public_key_once() {
        let runner = Rc::new(FakeGpgRunner::default());
        let mut client = GpgClient::with_runner(Rc::clone(&runner));

        assert!(
            client
                .verify("payload one", "signature one", "public key")
                .unwrap()
        );
        assert!(
            client
                .verify("payload two", "signature two", "public key")
                .unwrap()
        );

        assert_eq!(runner.imported_keys.borrow().as_slice(), ["public key"]);
        assert_eq!(
            runner.verified_payloads.borrow().as_slice(),
            ["payload one", "payload two"]
        );
    }
}
