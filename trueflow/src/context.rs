use anyhow::{Context, Result};
use std::path::PathBuf;

use crate::cli::Cli;
use crate::store::FileStore;

pub struct TrueflowContext {
    pub invocation: Cli,
}

impl TrueflowContext {
    pub fn new(invocation: Cli) -> Self {
        Self { invocation }
    }

    pub fn trueflow_dir(&self) -> Result<PathBuf> {
        let store = FileStore::new()?;
        let db_path = store.db_path();
        db_path
            .parent()
            .context("Failed to resolve .trueflow directory")
            .map(|path| path.to_path_buf())
    }
}
