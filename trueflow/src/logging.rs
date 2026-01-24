use anyhow::{Context, Result};
use clap::ValueEnum;
use std::fs::{self, OpenOptions};
use std::path::PathBuf;

use crate::store::FileStore;

#[derive(Copy, Clone, Debug, ValueEnum)]
#[clap(rename_all = "lower")]
pub enum LoggingMode {
    File,
    Stderr,
}

pub fn init_logging(mode: LoggingMode) -> Result<()> {
    let mut dispatch =
        fern::Dispatch::new()
            .level(log::LevelFilter::Info)
            .format(|out, message, record| {
                let timestamp = chrono::Local::now().format("%Y-%m-%d %H:%M:%S%.3f");
                let thread_id = format!("{:?}", std::thread::current().id());
                let module = record.module_path().unwrap_or(record.target());
                let line = record
                    .line()
                    .map(|line| line.to_string())
                    .unwrap_or_else(|| "?".to_string());
                out.finish(format_args!(
                    "[{}] [{}] [{}] [{}:{}] [{}]",
                    record.level(),
                    timestamp,
                    thread_id,
                    module,
                    line,
                    message
                ))
            });

    match mode {
        LoggingMode::Stderr => {
            dispatch = dispatch.chain(std::io::stderr());
        }
        LoggingMode::File => match create_log_file() {
            Ok(log_file) => {
                dispatch = dispatch.chain(log_file);
            }
            Err(err) => {
                eprintln!("Failed to open log file: {}", err);
                dispatch = dispatch.chain(std::io::stderr());
            }
        },
    }

    dispatch.apply()?;
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
    let log_path: PathBuf = log_dir.join(format!("{}.log", date));

    let file = OpenOptions::new()
        .create(true)
        .append(true)
        .open(log_path)?;

    Ok(file)
}
