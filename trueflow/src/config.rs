use anyhow::{Context, Result};
use log::warn;
use serde::Deserialize;
use std::collections::HashSet;
use std::path::{Path, PathBuf};

use crate::block::BlockKind;

const CONFIG_FILE_NAME: &str = "trueflow.toml";

#[derive(Debug, Default, Deserialize)]
pub struct TrueflowConfig {
    #[serde(default)]
    pub review: BlockFilterConfig,
    #[serde(default)]
    pub feedback: BlockFilterConfig,
    #[serde(default)]
    pub tui: TuiConfig,
}

#[derive(Debug, Deserialize)]
pub struct TuiConfig {
    #[serde(default = "default_confirm_batch")]
    pub confirm_batch: bool,
}

impl Default for TuiConfig {
    fn default() -> Self {
        Self {
            confirm_batch: true,
        }
    }
}

fn default_confirm_batch() -> bool {
    true
}

#[derive(Debug, Default, Deserialize)]
pub struct BlockFilterConfig {
    #[serde(default)]
    pub only: Vec<String>,
    #[serde(default)]
    pub exclude: Vec<String>,
}

impl BlockFilterConfig {
    pub fn resolve_filters(&self, cli_only: &[String], cli_exclude: &[String]) -> BlockFilters {
        let only_values = if cli_only.is_empty() {
            &self.only
        } else {
            cli_only
        };
        let exclude_values = if cli_exclude.is_empty() {
            &self.exclude
        } else {
            cli_exclude
        };
        BlockFilters::from_lists(only_values, exclude_values)
    }
}

#[derive(Debug, Clone, Default)]
pub struct BlockFilters {
    only: Option<HashSet<BlockKind>>,
    exclude: HashSet<BlockKind>,
}

impl BlockFilters {
    pub fn from_lists(only: &[String], exclude: &[String]) -> Self {
        let only_set = parse_block_kinds(only);
        let exclude_set = parse_block_kinds(exclude);
        let only = if only_set.is_empty() {
            None
        } else {
            Some(only_set)
        };
        Self {
            only,
            exclude: exclude_set,
        }
    }

    pub fn allows_block(&self, kind: &BlockKind) -> bool {
        if self.exclude.contains(kind) {
            return false;
        }
        match &self.only {
            Some(only) => only.contains(kind),
            None => true,
        }
    }

    pub fn allows_subblock(&self, kind: &BlockKind) -> bool {
        !self.exclude.contains(kind)
    }
}

pub fn load() -> Result<TrueflowConfig> {
    let current_dir = std::env::current_dir()?;
    let Some(path) = find_config_path(&current_dir) else {
        return Ok(TrueflowConfig::default());
    };

    let content = std::fs::read_to_string(&path)
        .with_context(|| format!("Failed to read config: {}", path.display()))?;
    let config = toml::from_str(&content)
        .with_context(|| format!("Failed to parse config: {}", path.display()))?;
    Ok(config)
}

fn find_config_path(start_dir: &Path) -> Option<PathBuf> {
    let mut current = Some(start_dir);
    while let Some(dir) = current {
        let candidate = dir.join(CONFIG_FILE_NAME);
        if candidate.is_file() {
            return Some(candidate);
        }
        current = dir.parent();
    }
    None
}

fn parse_block_kinds(values: &[String]) -> HashSet<BlockKind> {
    let mut kinds = HashSet::new();
    for value in values {
        match value.parse::<BlockKind>() {
            Ok(kind) => {
                kinds.insert(kind);
            }
            Err(err) => {
                warn!("Ignoring unknown block kind '{}': {}", value, err);
            }
        }
    }
    kinds
}
