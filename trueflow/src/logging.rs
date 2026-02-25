use anyhow::{Context, Result};
use clap::ValueEnum;
use std::fs::{self, OpenOptions};
use std::path::PathBuf;
use tracing::level_filters::LevelFilter;
use tracing_subscriber::fmt;
use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::util::SubscriberInitExt;

use crate::store::FileStore;

#[derive(Copy, Clone, Debug, ValueEnum)]
#[clap(rename_all = "lower")]
pub enum LoggingMode {
    File,
    Stderr,
}

pub fn init_logging(mode: LoggingMode, debug: bool) -> Result<()> {
    let level = if debug {
        LevelFilter::DEBUG
    } else {
        LevelFilter::WARN
    };

    let mut log_warning = None;
    match mode {
        LoggingMode::Stderr => init_stderr_subscriber(level)?,
        LoggingMode::File => match create_log_file() {
            Ok(log_file) => init_file_subscriber(level, log_file)?,
            Err(err) => {
                init_stderr_subscriber(level)?;
                log_warning = Some(err);
            }
        },
    }

    if let Some(err) = log_warning {
        tracing::warn!(error = %err, "Failed to open log file, using stderr");
    }
    Ok(())
}

fn init_stderr_subscriber(level: LevelFilter) -> Result<()> {
    tracing_subscriber::registry()
        .with(level)
        .with(
            fmt::layer()
                .with_writer(std::io::stderr)
                .with_target(true)
                .with_thread_ids(true)
                .with_thread_names(true)
                .compact(),
        )
        .try_init()?;
    Ok(())
}

fn init_file_subscriber(level: LevelFilter, log_file: std::fs::File) -> Result<()> {
    tracing_subscriber::registry()
        .with(level)
        .with(
            fmt::layer()
                .with_writer(std::sync::Mutex::new(log_file))
                .with_ansi(false)
                .with_target(true)
                .with_thread_ids(true)
                .with_thread_names(true)
                .compact(),
        )
        .try_init()?;
    Ok(())
}

fn create_log_file() -> Result<std::fs::File> {
    let store = FileStore::new()?;
    let db_path = store.db_path();
    let trueflow_dir = db_path
        .parent()
        .context("Failed to resolve .trueflow directory")?;

    let log_dir = trueflow_dir.join("logs");
    fs::create_dir_all(&log_dir)?;

    let date = chrono::Local::now().format("%Y-%m-%d").to_string();
    let log_path: PathBuf = log_dir.join(format!("{date}.log"));

    let file = OpenOptions::new()
        .create(true)
        .append(true)
        .open(log_path)?;

    Ok(file)
}
