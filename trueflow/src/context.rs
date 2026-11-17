use anyhow::Result;
use std::path::PathBuf;

use crate::cli::Cli;
use crate::store::StoreLocation;

pub struct TrueflowContext {
    pub invocation: Cli,
}

impl TrueflowContext {
    pub fn new(invocation: Cli) -> Self {
        Self { invocation }
    }

    pub fn trueflow_dir(&self) -> Result<PathBuf> {
        Ok(StoreLocation::discover_readonly()?.trueflow_dir())
    }
}
