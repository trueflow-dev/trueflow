use anyhow::{Context, Result};
use serde::Deserialize;
use std::collections::HashSet;
use std::path::{Path, PathBuf};
use tracing::warn;

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
    #[serde(default)]
    pub storage: StorageConfig,
}

#[derive(Debug, Deserialize)]
pub struct TuiConfig {
    #[serde(default = "default_confirm_batch")]
    pub confirm_batch: bool,
    #[serde(default = "default_tui_diff_focus_mode")]
    pub diff_focus_mode: TuiDiffFocusMode,
    #[serde(default = "default_diff_focus_context_lines")]
    pub diff_focus_context_lines: usize,
}

#[derive(Debug, Deserialize)]
pub struct StorageConfig {
    #[serde(default = "default_storage_branch")]
    pub branch: String,
}

#[derive(Debug, Clone, Copy, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum TuiDiffFocusMode {
    WholeBlock,
    ChangedWithContext,
}

impl Default for TuiConfig {
    fn default() -> Self {
        Self {
            confirm_batch: true,
            diff_focus_mode: default_tui_diff_focus_mode(),
            diff_focus_context_lines: default_diff_focus_context_lines(),
        }
    }
}

impl Default for StorageConfig {
    fn default() -> Self {
        Self {
            branch: default_storage_branch(),
        }
    }
}

fn default_confirm_batch() -> bool {
    true
}

fn default_tui_diff_focus_mode() -> TuiDiffFocusMode {
    TuiDiffFocusMode::WholeBlock
}

fn default_diff_focus_context_lines() -> usize {
    3
}

fn default_storage_branch() -> String {
    "trueflow-db".to_string()
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

    pub fn allows_block(&self, kind: BlockKind) -> bool {
        if self.exclude.contains(&kind) {
            return false;
        }
        match &self.only {
            Some(only) => only.contains(&kind),
            None => true,
        }
    }

    pub fn allows_subblock(&self, kind: BlockKind) -> bool {
        !self.exclude.contains(&kind)
    }

    pub fn only_contains(&self, kind: BlockKind) -> bool {
        self.only.as_ref().is_some_and(|only| only.contains(&kind))
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
                warn!("Ignoring unknown block kind '{value}': {err}");
            }
        }
    }
    kinds
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tui_config_defaults_to_whole_block_focus() {
        let cfg: TrueflowConfig = match toml::from_str("") {
            Ok(config) => config,
            Err(err) => panic!("parse config: {err}"),
        };
        assert!(cfg.tui.confirm_batch);
        assert_eq!(cfg.tui.diff_focus_mode, TuiDiffFocusMode::WholeBlock);
        assert_eq!(cfg.tui.diff_focus_context_lines, 3);
    }

    #[test]
    fn tui_config_parses_changed_with_context_focus() {
        let cfg: TrueflowConfig = match toml::from_str(
            "[tui]\ndiff_focus_mode = \"changed_with_context\"\ndiff_focus_context_lines = 5\n",
        ) {
            Ok(config) => config,
            Err(err) => panic!("parse config: {err}"),
        };
        assert_eq!(
            cfg.tui.diff_focus_mode,
            TuiDiffFocusMode::ChangedWithContext
        );
        assert_eq!(cfg.tui.diff_focus_context_lines, 5);
    }

    #[test]
    fn storage_branch_defaults_to_trueflow_db() {
        let cfg: TrueflowConfig = match toml::from_str("") {
            Ok(config) => config,
            Err(err) => panic!("parse config: {err}"),
        };
        assert_eq!(cfg.storage.branch, "trueflow-db");
    }

    #[test]
    fn storage_branch_parses_custom_value() {
        let cfg: TrueflowConfig = match toml::from_str("[storage]\nbranch = \"reviews/custom\"\n") {
            Ok(config) => config,
            Err(err) => panic!("parse config: {err}"),
        };
        assert_eq!(cfg.storage.branch, "reviews/custom");
    }
}
