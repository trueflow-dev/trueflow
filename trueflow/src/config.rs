use anyhow::{Context, Result, anyhow};
use ignore::gitignore::GitignoreBuilder;
use serde::{Deserialize, Deserializer};
use std::collections::HashSet;
use std::path::{Path, PathBuf};
use toml_edit::{DocumentMut, Entry, Item, Table, value};

use crate::block::BlockKind;
use crate::feedback_since::FeedbackSinceExpr;
use crate::repo_path::RepoPath;
use crate::scanner::{ScanCacheMode, ScanOptions};

const CONFIG_FILE_NAME: &str = "trueflow.toml";
const GLOBAL_CONFIG_FILE_NAME: &str = ".trueflow.toml";

#[derive(Debug, Default, Deserialize)]
pub struct TrueflowConfig {
    #[serde(default)]
    pub review: BlockFilterConfig,
    #[serde(default)]
    pub feedback: FeedbackConfig,
    #[serde(default)]
    pub tui: TuiConfig,
    #[serde(default)]
    pub scan: ScanConfig,
    #[serde(default)]
    pub ai: AiConfig,
}

#[derive(Debug, Clone, Deserialize)]
pub struct TuiConfig {
    #[serde(default = "default_confirm_batch_sub_blocks")]
    pub confirm_batch_sub_blocks: BatchConfirmPolicy,
    #[serde(default = "default_tui_diff_focus_mode")]
    pub diff_focus_mode: TuiDiffFocusMode,
    #[serde(default = "default_diff_focus_context_lines")]
    pub diff_focus_context_lines: usize,
    #[serde(default = "default_tui_diff_line_numbers")]
    pub diff_line_numbers: TuiDiffLineNumbers,
    #[serde(default)]
    pub keybinds: TuiKeybindsConfig,
    #[serde(default)]
    pub speed_read: TuiSpeedReadConfig,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TuiKeybindsConfig {
    pub scroll_up: char,
    pub scroll_down: char,
    pub prev: char,
    pub next: char,
    pub parent: char,
    pub child: char,
    pub approve: char,
    pub note: char,
    pub toggle_view: char,
    pub speed_read: char,
    pub root: char,
    pub recap_done: char,
    pub quit: char,
}

#[derive(Debug, Clone, Copy, Deserialize)]
struct TuiKeybindsConfigRepr {
    #[serde(
        default = "default_tui_keybind_scroll_up",
        deserialize_with = "deserialize_single_char"
    )]
    scroll_up: char,
    #[serde(
        default = "default_tui_keybind_scroll_down",
        deserialize_with = "deserialize_single_char"
    )]
    scroll_down: char,
    #[serde(
        default = "default_tui_keybind_prev",
        deserialize_with = "deserialize_single_char"
    )]
    prev: char,
    #[serde(
        default = "default_tui_keybind_next",
        deserialize_with = "deserialize_single_char"
    )]
    next: char,
    #[serde(
        default = "default_tui_keybind_parent",
        deserialize_with = "deserialize_single_char"
    )]
    parent: char,
    #[serde(
        default = "default_tui_keybind_child",
        deserialize_with = "deserialize_single_char"
    )]
    child: char,
    #[serde(
        default = "default_tui_keybind_approve",
        deserialize_with = "deserialize_single_char"
    )]
    approve: char,
    #[serde(
        default = "default_tui_keybind_note",
        deserialize_with = "deserialize_single_char"
    )]
    note: char,
    #[serde(
        default = "default_tui_keybind_toggle_view",
        deserialize_with = "deserialize_single_char"
    )]
    toggle_view: char,
    #[serde(
        default = "default_tui_keybind_speed_read",
        deserialize_with = "deserialize_single_char"
    )]
    speed_read: char,
    #[serde(
        default = "default_tui_keybind_root",
        deserialize_with = "deserialize_single_char"
    )]
    root: char,
    #[serde(
        default = "default_tui_keybind_recap_done",
        deserialize_with = "deserialize_single_char"
    )]
    recap_done: char,
    #[serde(
        default = "default_tui_keybind_quit",
        deserialize_with = "deserialize_single_char"
    )]
    quit: char,
}

impl From<TuiKeybindsConfigRepr> for TuiKeybindsConfig {
    fn from(repr: TuiKeybindsConfigRepr) -> Self {
        Self {
            scroll_up: repr.scroll_up,
            scroll_down: repr.scroll_down,
            prev: repr.prev,
            next: repr.next,
            parent: repr.parent,
            child: repr.child,
            approve: repr.approve,
            note: repr.note,
            toggle_view: repr.toggle_view,
            speed_read: repr.speed_read,
            root: repr.root,
            recap_done: repr.recap_done,
            quit: repr.quit,
        }
    }
}

impl<'de> Deserialize<'de> for TuiKeybindsConfig {
    fn deserialize<D>(deserializer: D) -> std::result::Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let config = Self::from(TuiKeybindsConfigRepr::deserialize(deserializer)?);
        validate_tui_keybinds(&config).map_err(serde::de::Error::custom)?;
        Ok(config)
    }
}

fn validate_tui_keybinds(config: &TuiKeybindsConfig) -> std::result::Result<(), String> {
    let keybinds = [
        ("scroll_up", config.scroll_up),
        ("scroll_down", config.scroll_down),
        ("prev", config.prev),
        ("next", config.next),
        ("parent", config.parent),
        ("child", config.child),
        ("approve", config.approve),
        ("note", config.note),
        ("toggle_view", config.toggle_view),
        ("speed_read", config.speed_read),
        ("root", config.root),
        ("recap_done", config.recap_done),
        ("quit", config.quit),
    ];

    for (left_index, (left_name, left_key)) in keybinds.iter().enumerate() {
        if let Some((right_name, _)) = keybinds
            .iter()
            .skip(left_index + 1)
            .find(|(_, right_key)| right_key == left_key)
        {
            return Err(format!(
                "duplicate TUI keybind '{left_key}' for {left_name} and {right_name}"
            ));
        }
    }

    Ok(())
}

#[derive(Debug, Clone, Deserialize)]
pub struct AiConfig {
    #[serde(default = "default_ai_enabled")]
    pub enabled: bool,
    #[serde(default = "default_ai_provider")]
    pub provider: AiProviderConfig,
    #[serde(default = "default_ai_model")]
    pub model: String,
    #[serde(default = "default_ai_max_context_lines")]
    pub max_context_lines: usize,
    #[serde(default = "default_ai_cache")]
    pub cache: bool,
}

#[derive(Debug, Clone, Copy, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum AiProviderConfig {
    Auto,
    Anthropic,
    OpenAi,
    ClaudeCli,
    CodexCli,
    None,
}

#[derive(Debug, Clone, Deserialize)]
pub struct ScanConfig {
    #[serde(default = "default_scan_use_cache")]
    pub use_cache: bool,
    #[serde(default = "default_scan_write_cache")]
    pub write_cache: bool,
    #[serde(default)]
    pub cache_dir: Option<PathBuf>,
    #[serde(default)]
    pub ignore_names: Vec<String>,
    #[serde(default, deserialize_with = "deserialize_ignore_globs")]
    pub ignore_globs: Vec<String>,
    #[serde(default, deserialize_with = "deserialize_repo_paths")]
    pub ignore_path_prefixes: Vec<RepoPath>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct FeedbackConfig {
    #[serde(flatten)]
    pub filters: BlockFilterConfig,
    #[serde(default = "default_feedback_since")]
    pub default_since: FeedbackSinceExpr,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BatchConfirmPolicy {
    Never,
    Threshold(usize),
}

impl Default for BatchConfirmPolicy {
    fn default() -> Self {
        Self::Threshold(2)
    }
}

impl<'de> Deserialize<'de> for BatchConfirmPolicy {
    fn deserialize<D>(deserializer: D) -> std::result::Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        #[derive(Deserialize)]
        #[serde(untagged)]
        enum Repr {
            Keyword(String),
            Threshold(usize),
        }

        match Repr::deserialize(deserializer)? {
            Repr::Keyword(keyword) => match keyword.as_str() {
                "never" => Ok(BatchConfirmPolicy::Never),
                _ => Err(serde::de::Error::custom(
                    "confirm_batch_sub_blocks must be \"never\" or an integer threshold >= 1",
                )),
            },
            Repr::Threshold(0) => Err(serde::de::Error::custom(
                "confirm_batch_sub_blocks threshold must be at least 1",
            )),
            Repr::Threshold(threshold) => Ok(BatchConfirmPolicy::Threshold(threshold)),
        }
    }
}

impl BatchConfirmPolicy {
    pub fn should_confirm(self, count: usize) -> bool {
        match self {
            BatchConfirmPolicy::Never => false,
            BatchConfirmPolicy::Threshold(threshold) => count >= threshold,
        }
    }
}

#[derive(Debug, Clone, Copy, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum TuiDiffFocusMode {
    WholeBlock,
    ChangedWithContext,
}

#[derive(Debug, Clone, Copy, Deserialize, PartialEq, Eq, Hash)]
#[serde(rename_all = "snake_case")]
pub enum TuiDiffLineNumbers {
    Disabled,
    OldNew,
}

#[derive(Debug, Clone, Copy, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum TuiSpeedReadPunctuationDwell {
    Off,
    Light,
}

#[derive(Debug, Clone)]
pub struct TuiSpeedReadConfig {
    pub enabled: bool,
    pub default_wpm: u16,
    pub min_wpm: u16,
    pub max_wpm: u16,
    pub default_chunk_words: u8,
    pub min_chunk_words: u8,
    pub max_chunk_words: u8,
    pub loop_playback: bool,
    pub show_orp_highlight: bool,
    pub show_prose_optimization_hint: bool,
    pub punctuation_dwell: TuiSpeedReadPunctuationDwell,
    pub punctuation_dwell_multiplier: f64,
}

#[derive(Debug, Clone, Deserialize)]
struct TuiSpeedReadConfigRepr {
    #[serde(default = "default_tui_speed_read_enabled")]
    enabled: bool,
    #[serde(default = "default_speed_read_wpm")]
    default_wpm: u16,
    #[serde(default = "default_speed_read_min_wpm")]
    min_wpm: u16,
    #[serde(default = "default_speed_read_max_wpm")]
    max_wpm: u16,
    #[serde(default = "default_speed_read_chunk_words")]
    default_chunk_words: u8,
    #[serde(default = "default_speed_read_min_chunk_words")]
    min_chunk_words: u8,
    #[serde(default = "default_speed_read_max_chunk_words")]
    max_chunk_words: u8,
    #[serde(default = "default_speed_read_loop_playback")]
    loop_playback: bool,
    #[serde(default = "default_speed_read_show_orp_highlight")]
    show_orp_highlight: bool,
    #[serde(default = "default_speed_read_show_prose_hint")]
    show_prose_optimization_hint: bool,
    #[serde(default = "default_speed_read_punctuation_dwell")]
    punctuation_dwell: TuiSpeedReadPunctuationDwell,
    #[serde(default = "default_speed_read_punctuation_dwell_multiplier")]
    punctuation_dwell_multiplier: f64,
}

impl From<TuiSpeedReadConfigRepr> for TuiSpeedReadConfig {
    fn from(repr: TuiSpeedReadConfigRepr) -> Self {
        Self {
            enabled: repr.enabled,
            default_wpm: repr.default_wpm,
            min_wpm: repr.min_wpm,
            max_wpm: repr.max_wpm,
            default_chunk_words: repr.default_chunk_words,
            min_chunk_words: repr.min_chunk_words,
            max_chunk_words: repr.max_chunk_words,
            loop_playback: repr.loop_playback,
            show_orp_highlight: repr.show_orp_highlight,
            show_prose_optimization_hint: repr.show_prose_optimization_hint,
            punctuation_dwell: repr.punctuation_dwell,
            punctuation_dwell_multiplier: repr.punctuation_dwell_multiplier,
        }
    }
}

impl<'de> Deserialize<'de> for TuiSpeedReadConfig {
    fn deserialize<D>(deserializer: D) -> std::result::Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let config = Self::from(TuiSpeedReadConfigRepr::deserialize(deserializer)?);
        validate_speed_read_bounds(&config).map_err(serde::de::Error::custom)?;
        Ok(config)
    }
}

fn validate_speed_read_bounds(
    config: &TuiSpeedReadConfig,
) -> std::result::Result<(), &'static str> {
    if config.default_wpm == 0 || config.min_wpm == 0 || config.max_wpm == 0 {
        return Err("speed-read WPM values must be greater than 0");
    }
    if config.default_chunk_words == 0 || config.min_chunk_words == 0 || config.max_chunk_words == 0
    {
        return Err("speed-read chunk word values must be greater than 0");
    }
    if config.min_wpm > config.max_wpm {
        return Err("min_wpm must be <= max_wpm");
    }
    if config.default_wpm < config.min_wpm || config.default_wpm > config.max_wpm {
        return Err("default_wpm must be between min_wpm and max_wpm");
    }
    if config.min_chunk_words > config.max_chunk_words {
        return Err("min_chunk_words must be <= max_chunk_words");
    }
    if config.default_chunk_words < config.min_chunk_words
        || config.default_chunk_words > config.max_chunk_words
    {
        return Err("default_chunk_words must be between min_chunk_words and max_chunk_words");
    }
    if !config.punctuation_dwell_multiplier.is_finite()
        || config.punctuation_dwell_multiplier <= 0.0
    {
        return Err("punctuation_dwell_multiplier must be finite and greater than 0");
    }
    Ok(())
}

impl Default for TuiConfig {
    fn default() -> Self {
        Self {
            confirm_batch_sub_blocks: default_confirm_batch_sub_blocks(),
            diff_focus_mode: default_tui_diff_focus_mode(),
            diff_focus_context_lines: default_diff_focus_context_lines(),
            diff_line_numbers: default_tui_diff_line_numbers(),
            keybinds: TuiKeybindsConfig::default(),
            speed_read: TuiSpeedReadConfig::default(),
        }
    }
}

impl Default for TuiKeybindsConfig {
    fn default() -> Self {
        Self {
            scroll_up: default_tui_keybind_scroll_up(),
            scroll_down: default_tui_keybind_scroll_down(),
            prev: default_tui_keybind_prev(),
            next: default_tui_keybind_next(),
            parent: default_tui_keybind_parent(),
            child: default_tui_keybind_child(),
            approve: default_tui_keybind_approve(),
            note: default_tui_keybind_note(),
            toggle_view: default_tui_keybind_toggle_view(),
            speed_read: default_tui_keybind_speed_read(),
            root: default_tui_keybind_root(),
            recap_done: default_tui_keybind_recap_done(),
            quit: default_tui_keybind_quit(),
        }
    }
}

impl Default for TuiSpeedReadConfig {
    fn default() -> Self {
        Self {
            enabled: default_tui_speed_read_enabled(),
            default_wpm: default_speed_read_wpm(),
            min_wpm: default_speed_read_min_wpm(),
            max_wpm: default_speed_read_max_wpm(),
            default_chunk_words: default_speed_read_chunk_words(),
            min_chunk_words: default_speed_read_min_chunk_words(),
            max_chunk_words: default_speed_read_max_chunk_words(),
            loop_playback: default_speed_read_loop_playback(),
            show_orp_highlight: default_speed_read_show_orp_highlight(),
            show_prose_optimization_hint: default_speed_read_show_prose_hint(),
            punctuation_dwell: default_speed_read_punctuation_dwell(),
            punctuation_dwell_multiplier: default_speed_read_punctuation_dwell_multiplier(),
        }
    }
}

impl Default for AiConfig {
    fn default() -> Self {
        Self {
            enabled: default_ai_enabled(),
            provider: default_ai_provider(),
            model: default_ai_model(),
            max_context_lines: default_ai_max_context_lines(),
            cache: default_ai_cache(),
        }
    }
}

impl Default for ScanConfig {
    fn default() -> Self {
        Self {
            use_cache: default_scan_use_cache(),
            write_cache: default_scan_write_cache(),
            cache_dir: None,
            ignore_names: Vec::new(),
            ignore_globs: Vec::new(),
            ignore_path_prefixes: Vec::new(),
        }
    }
}

impl Default for FeedbackConfig {
    fn default() -> Self {
        Self {
            filters: BlockFilterConfig::default(),
            default_since: default_feedback_since(),
        }
    }
}

fn default_confirm_batch_sub_blocks() -> BatchConfirmPolicy {
    BatchConfirmPolicy::default()
}

fn default_tui_diff_focus_mode() -> TuiDiffFocusMode {
    TuiDiffFocusMode::WholeBlock
}

fn default_diff_focus_context_lines() -> usize {
    3
}

fn default_tui_diff_line_numbers() -> TuiDiffLineNumbers {
    TuiDiffLineNumbers::Disabled
}

fn default_tui_keybind_scroll_up() -> char {
    'k'
}

fn default_tui_keybind_scroll_down() -> char {
    'j'
}

fn default_tui_keybind_prev() -> char {
    'h'
}

fn default_tui_keybind_next() -> char {
    'l'
}

fn default_tui_keybind_parent() -> char {
    'P'
}

fn default_tui_keybind_child() -> char {
    'C'
}

fn default_tui_keybind_approve() -> char {
    'a'
}

fn default_tui_keybind_note() -> char {
    'c'
}

fn default_tui_keybind_toggle_view() -> char {
    'm'
}

fn default_tui_keybind_speed_read() -> char {
    'r'
}

fn default_tui_keybind_root() -> char {
    'g'
}

fn default_tui_keybind_recap_done() -> char {
    'd'
}

fn default_tui_keybind_quit() -> char {
    'q'
}

fn default_tui_speed_read_enabled() -> bool {
    true
}

fn default_speed_read_wpm() -> u16 {
    320
}

fn default_speed_read_min_wpm() -> u16 {
    120
}

fn default_speed_read_max_wpm() -> u16 {
    900
}

fn default_speed_read_chunk_words() -> u8 {
    2
}

fn default_speed_read_min_chunk_words() -> u8 {
    1
}

fn default_speed_read_max_chunk_words() -> u8 {
    5
}

fn default_speed_read_loop_playback() -> bool {
    false
}

fn default_speed_read_show_orp_highlight() -> bool {
    false
}

fn default_speed_read_show_prose_hint() -> bool {
    true
}

fn default_speed_read_punctuation_dwell() -> TuiSpeedReadPunctuationDwell {
    TuiSpeedReadPunctuationDwell::Light
}

fn default_speed_read_punctuation_dwell_multiplier() -> f64 {
    1.15
}

fn default_ai_enabled() -> bool {
    false
}

fn default_ai_provider() -> AiProviderConfig {
    AiProviderConfig::Auto
}

fn default_ai_model() -> String {
    "auto".to_string()
}

fn default_ai_max_context_lines() -> usize {
    80
}

fn default_ai_cache() -> bool {
    true
}

fn default_scan_use_cache() -> bool {
    true
}

fn default_scan_write_cache() -> bool {
    true
}

fn default_feedback_since() -> FeedbackSinceExpr {
    FeedbackSinceExpr::default()
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct BlockFilterConfig {
    #[serde(default, deserialize_with = "deserialize_block_kinds")]
    pub only: Vec<BlockKind>,
    #[serde(default, deserialize_with = "deserialize_block_kinds")]
    pub exclude: Vec<BlockKind>,
}

impl BlockFilterConfig {
    pub fn resolve_filters(
        &self,
        cli_only: &[BlockKind],
        cli_exclude: &[BlockKind],
    ) -> BlockFilters {
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

impl ScanConfig {
    pub fn resolve_options(&self) -> ScanOptions {
        let mut options = ScanOptions {
            cache_mode: ScanCacheMode::from_flags(self.use_cache, self.write_cache),
            cache_dir: self.cache_dir.clone(),
            ..ScanOptions::default()
        };
        options
            .ignore_names
            .extend(self.ignore_names.iter().cloned());
        options.ignore_names.sort();
        options.ignore_names.dedup();
        options
            .ignore_globs
            .extend(self.ignore_globs.iter().cloned());
        options.ignore_globs.sort();
        options.ignore_globs.dedup();
        options
            .ignore_path_prefixes
            .extend(self.ignore_path_prefixes.iter().cloned());
        options.ignore_path_prefixes.sort();
        options.ignore_path_prefixes.dedup();
        options
    }
}

const DEFAULT_EXCLUDED_BLOCK_KINDS: &[BlockKind] = &[BlockKind::Gap];

#[derive(Debug, Clone)]
pub struct BlockFilters {
    only: Option<HashSet<BlockKind>>,
    exclude: HashSet<BlockKind>,
    default_exclude: HashSet<BlockKind>,
}

impl Default for BlockFilters {
    fn default() -> Self {
        Self::from_lists(&[], &[])
    }
}

impl BlockFilters {
    pub fn from_lists(only: &[BlockKind], exclude: &[BlockKind]) -> Self {
        let only_set: HashSet<_> = only.iter().copied().collect();
        let exclude_set: HashSet<_> = exclude.iter().copied().collect();
        let default_exclude = DEFAULT_EXCLUDED_BLOCK_KINDS.iter().copied().collect();
        let only = if only_set.is_empty() {
            None
        } else {
            Some(only_set)
        };
        Self {
            only,
            exclude: exclude_set,
            default_exclude,
        }
    }

    pub fn allows_block(&self, kind: BlockKind) -> bool {
        if self.exclude.contains(&kind) {
            return false;
        }
        match &self.only {
            Some(only) => only.contains(&kind),
            None => !self.default_exclude.contains(&kind),
        }
    }

    pub fn allows_subblock(&self, kind: BlockKind) -> bool {
        if self.exclude.contains(&kind) {
            return false;
        }
        self.only_contains(kind) || !self.default_exclude.contains(&kind)
    }

    pub fn only_contains(&self, kind: BlockKind) -> bool {
        self.only.as_ref().is_some_and(|only| only.contains(&kind))
    }
}

pub fn load() -> Result<TrueflowConfig> {
    let current_dir = std::env::current_dir()?;
    let home_dir = dirs::home_dir();
    load_from_start_dir(&current_dir, home_dir.as_deref())
}

fn load_from_start_dir(start_dir: &Path, home_dir: Option<&Path>) -> Result<TrueflowConfig> {
    let paths = config_paths_for_start_dir(start_dir, home_dir);
    load_from_config_paths(&paths)
}

fn config_paths_for_start_dir(start_dir: &Path, home_dir: Option<&Path>) -> Vec<PathBuf> {
    let mut paths = Vec::new();
    if let Some(home_dir) = home_dir {
        let global_config = home_dir.join(GLOBAL_CONFIG_FILE_NAME);
        if global_config.is_file() {
            paths.push(global_config);
        }
    }

    let mut ancestor_paths = Vec::new();
    let mut current = Some(start_dir);
    while let Some(dir) = current {
        let candidate = dir.join(CONFIG_FILE_NAME);
        if candidate.is_file() {
            ancestor_paths.push(candidate);
        }
        current = dir.parent();
    }
    ancestor_paths.reverse();
    paths.extend(ancestor_paths);
    paths
}

fn load_from_config_paths(paths: &[PathBuf]) -> Result<TrueflowConfig> {
    if paths.is_empty() {
        return Ok(TrueflowConfig::default());
    }

    let mut merged = toml::Value::Table(toml::Table::new());
    for path in paths {
        let value = read_config_value(path)?;
        merge_toml_values(&mut merged, value);
    }

    merged.try_into().with_context(|| {
        format!(
            "Failed to parse merged config from {}",
            config_path_list(paths)
        )
    })
}

fn read_config_value(path: &Path) -> Result<toml::Value> {
    let content = std::fs::read_to_string(path)
        .with_context(|| format!("Failed to read config: {}", path.display()))?;
    toml::from_str(&content).with_context(|| format!("Failed to parse config: {}", path.display()))
}

fn merge_toml_values(base: &mut toml::Value, overlay: toml::Value) {
    match (base, overlay) {
        (toml::Value::Table(base_table), toml::Value::Table(overlay_table)) => {
            for (key, value) in overlay_table {
                match base_table.get_mut(&key) {
                    Some(existing) => merge_toml_values(existing, value),
                    None => {
                        base_table.insert(key, value);
                    }
                }
            }
        }
        (base, overlay) => *base = overlay,
    }
}

fn config_path_list(paths: &[PathBuf]) -> String {
    paths
        .iter()
        .map(|path| path.display().to_string())
        .collect::<Vec<_>>()
        .join(", ")
}

fn deserialize_block_kinds<'de, D>(deserializer: D) -> std::result::Result<Vec<BlockKind>, D::Error>
where
    D: Deserializer<'de>,
{
    let values = Vec::<String>::deserialize(deserializer)?;
    values
        .into_iter()
        .map(|value| value.parse().map_err(serde::de::Error::custom))
        .collect()
}

fn deserialize_repo_paths<'de, D>(deserializer: D) -> std::result::Result<Vec<RepoPath>, D::Error>
where
    D: Deserializer<'de>,
{
    let values = Vec::<String>::deserialize(deserializer)?;
    values
        .into_iter()
        .map(|value| RepoPath::new(value).map_err(serde::de::Error::custom))
        .collect()
}

fn deserialize_ignore_globs<'de, D>(deserializer: D) -> std::result::Result<Vec<String>, D::Error>
where
    D: Deserializer<'de>,
{
    let values = Vec::<String>::deserialize(deserializer)?;
    let mut builder = GitignoreBuilder::new(".");
    builder.allow_unclosed_class(false);
    for pattern in &values {
        builder.add_line(None, pattern).map_err(|err| {
            serde::de::Error::custom(format!("invalid scan ignore glob {pattern:?}: {err}"))
        })?;
    }
    Ok(values)
}

fn deserialize_single_char<'de, D>(deserializer: D) -> std::result::Result<char, D::Error>
where
    D: Deserializer<'de>,
{
    let value = String::deserialize(deserializer)?;
    let mut chars = value.chars();
    let Some(ch) = chars.next() else {
        return Err(serde::de::Error::custom("expected a single character"));
    };
    if chars.next().is_some() {
        return Err(serde::de::Error::custom("expected a single character"));
    }
    Ok(ch)
}

pub fn update_speed_read_defaults_in_file(
    path: &Path,
    default_wpm: u16,
    default_chunk_words: u8,
) -> Result<()> {
    let mut document = if path.is_file() {
        let source = std::fs::read_to_string(path)
            .with_context(|| format!("Failed to read config for update: {}", path.display()))?;
        source
            .parse::<DocumentMut>()
            .with_context(|| format!("Failed to parse config for update: {}", path.display()))?
    } else {
        DocumentMut::new()
    };

    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).with_context(|| {
            format!(
                "Failed to create config parent directory: {}",
                parent.display()
            )
        })?;
    }

    let root = document.as_table_mut();
    let tui_table = match root.entry("tui") {
        Entry::Vacant(entry) => entry
            .insert(Item::Table(Table::new()))
            .as_table_mut()
            .ok_or_else(|| anyhow!("Expected [tui] to be a table"))?,
        Entry::Occupied(entry) => {
            if !entry.get().is_table() {
                return Err(anyhow!("Expected [tui] to be a table"));
            }
            entry
                .into_mut()
                .as_table_mut()
                .ok_or_else(|| anyhow!("Expected [tui] to be a table"))?
        }
    };

    let speed_read_table = match tui_table.entry("speed_read") {
        Entry::Vacant(entry) => entry
            .insert(Item::Table(Table::new()))
            .as_table_mut()
            .ok_or_else(|| anyhow!("Expected [tui.speed_read] to be a table"))?,
        Entry::Occupied(entry) => {
            if !entry.get().is_table() {
                return Err(anyhow!("Expected [tui.speed_read] to be a table"));
            }
            entry
                .into_mut()
                .as_table_mut()
                .ok_or_else(|| anyhow!("Expected [tui.speed_read] to be a table"))?
        }
    };

    speed_read_table["default_wpm"] = value(i64::from(default_wpm));
    speed_read_table["default_chunk_words"] = value(i64::from(default_chunk_words));

    let updated = document.to_string();
    let _: TrueflowConfig = toml::from_str(&updated).with_context(|| {
        format!(
            "Updated speed-read defaults would make config invalid: {}",
            path.display()
        )
    })?;

    std::fs::write(path, updated)
        .with_context(|| format!("Failed to write config update: {}", path.display()))?;

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::feedback_since::FeedbackSinceExpr;

    #[test]
    fn ai_config_defaults_to_opt_in_auto_detection() {
        let cfg: TrueflowConfig = match toml::from_str("") {
            Ok(config) => config,
            Err(err) => panic!("parse config: {err}"),
        };
        assert!(!cfg.ai.enabled);
        assert_eq!(cfg.ai.provider, AiProviderConfig::Auto);
        assert_eq!(cfg.ai.model, "auto");
        assert_eq!(cfg.ai.max_context_lines, 80);
        assert!(cfg.ai.cache);
    }

    #[test]
    fn ai_config_parses_overrides() {
        let cfg: TrueflowConfig = match toml::from_str(
            r#"
[ai]
enabled = true
provider = "anthropic"
model = "claude-3-5-haiku-latest"
max_context_lines = 40
cache = false
"#,
        ) {
            Ok(config) => config,
            Err(err) => panic!("parse config: {err}"),
        };
        assert!(cfg.ai.enabled);
        assert_eq!(cfg.ai.provider, AiProviderConfig::Anthropic);
        assert_eq!(cfg.ai.model, "claude-3-5-haiku-latest");
        assert_eq!(cfg.ai.max_context_lines, 40);
        assert!(!cfg.ai.cache);
    }

    #[test]
    fn tui_config_defaults_to_whole_block_focus() {
        let cfg: TrueflowConfig = match toml::from_str("") {
            Ok(config) => config,
            Err(err) => panic!("parse config: {err}"),
        };
        assert_eq!(
            cfg.tui.confirm_batch_sub_blocks,
            BatchConfirmPolicy::Threshold(2)
        );
        assert_eq!(cfg.tui.diff_focus_mode, TuiDiffFocusMode::WholeBlock);
        assert_eq!(cfg.tui.diff_focus_context_lines, 3);
        assert_eq!(cfg.tui.diff_line_numbers, TuiDiffLineNumbers::Disabled);
        assert_eq!(cfg.tui.keybinds.scroll_up, 'k');
        assert_eq!(cfg.tui.keybinds.scroll_down, 'j');
        assert_eq!(cfg.tui.keybinds.prev, 'h');
        assert_eq!(cfg.tui.keybinds.next, 'l');
        assert_eq!(cfg.tui.keybinds.parent, 'P');
        assert_eq!(cfg.tui.keybinds.child, 'C');
        assert_eq!(cfg.tui.keybinds.approve, 'a');
        assert_eq!(cfg.tui.keybinds.note, 'c');
        assert_eq!(cfg.tui.keybinds.toggle_view, 'm');
        assert_eq!(cfg.tui.keybinds.speed_read, 'r');
        assert_eq!(cfg.tui.keybinds.root, 'g');
        assert_eq!(cfg.tui.keybinds.recap_done, 'd');
        assert_eq!(cfg.tui.keybinds.quit, 'q');
    }

    #[test]
    fn example_config_parses_and_tracks_current_defaults() {
        let repo_root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .unwrap_or_else(|| panic!("crate should live under the repo root"));
        let path = repo_root.join("trueflow.example.toml");
        let content = std::fs::read_to_string(&path)
            .unwrap_or_else(|err| panic!("read {}: {err}", path.display()));
        let cfg: TrueflowConfig = toml::from_str(&content)
            .unwrap_or_else(|err| panic!("parse {}: {err}", path.display()));
        let defaults = TrueflowConfig::default();

        assert_eq!(cfg.review.only, defaults.review.only);
        assert_eq!(cfg.review.exclude, defaults.review.exclude);
        assert_eq!(cfg.feedback.filters.only, defaults.feedback.filters.only);
        assert_eq!(
            cfg.feedback.filters.exclude,
            defaults.feedback.filters.exclude
        );
        assert_eq!(cfg.feedback.default_since, defaults.feedback.default_since);
        assert_eq!(cfg.scan.use_cache, defaults.scan.use_cache);
        assert_eq!(cfg.scan.write_cache, defaults.scan.write_cache);
        assert_eq!(cfg.scan.cache_dir, defaults.scan.cache_dir);
        assert_eq!(cfg.scan.ignore_names, defaults.scan.ignore_names);
        assert_eq!(
            cfg.scan.ignore_path_prefixes,
            defaults.scan.ignore_path_prefixes
        );
        assert_eq!(cfg.scan.ignore_globs, defaults.scan.ignore_globs);
        assert_eq!(
            cfg.tui.confirm_batch_sub_blocks,
            defaults.tui.confirm_batch_sub_blocks
        );
        assert_eq!(cfg.tui.diff_focus_mode, defaults.tui.diff_focus_mode);
        assert_eq!(
            cfg.tui.diff_focus_context_lines,
            defaults.tui.diff_focus_context_lines
        );
        assert_eq!(cfg.tui.diff_line_numbers, defaults.tui.diff_line_numbers);
        assert_eq!(cfg.tui.keybinds, defaults.tui.keybinds);
        assert_eq!(cfg.tui.speed_read.enabled, defaults.tui.speed_read.enabled);
        assert_eq!(
            cfg.tui.speed_read.default_wpm,
            defaults.tui.speed_read.default_wpm
        );
        assert_eq!(cfg.tui.speed_read.min_wpm, defaults.tui.speed_read.min_wpm);
        assert_eq!(cfg.tui.speed_read.max_wpm, defaults.tui.speed_read.max_wpm);
        assert_eq!(
            cfg.tui.speed_read.default_chunk_words,
            defaults.tui.speed_read.default_chunk_words
        );
        assert_eq!(
            cfg.tui.speed_read.min_chunk_words,
            defaults.tui.speed_read.min_chunk_words
        );
        assert_eq!(
            cfg.tui.speed_read.max_chunk_words,
            defaults.tui.speed_read.max_chunk_words
        );
        assert_eq!(
            cfg.tui.speed_read.loop_playback,
            defaults.tui.speed_read.loop_playback
        );
        assert_eq!(
            cfg.tui.speed_read.show_orp_highlight,
            defaults.tui.speed_read.show_orp_highlight
        );
        assert_eq!(
            cfg.tui.speed_read.show_prose_optimization_hint,
            defaults.tui.speed_read.show_prose_optimization_hint
        );
        assert_eq!(
            cfg.tui.speed_read.punctuation_dwell,
            defaults.tui.speed_read.punctuation_dwell
        );
        assert!(
            (cfg.tui.speed_read.punctuation_dwell_multiplier
                - defaults.tui.speed_read.punctuation_dwell_multiplier)
                .abs()
                <= f64::EPSILON
        );
        assert_eq!(cfg.ai.enabled, defaults.ai.enabled);
        assert_eq!(cfg.ai.provider, defaults.ai.provider);
        assert_eq!(cfg.ai.model, defaults.ai.model);
        assert_eq!(cfg.ai.max_context_lines, defaults.ai.max_context_lines);
        assert_eq!(cfg.ai.cache, defaults.ai.cache);
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
    fn tui_config_parses_confirm_batch_sub_blocks_threshold_and_never() {
        let threshold_cfg: TrueflowConfig = toml::from_str("[tui]\nconfirm_batch_sub_blocks = 1\n")
            .unwrap_or_else(|err| panic!("parse threshold config: {err}"));
        assert_eq!(
            threshold_cfg.tui.confirm_batch_sub_blocks,
            BatchConfirmPolicy::Threshold(1)
        );

        let never_cfg: TrueflowConfig =
            toml::from_str("[tui]\nconfirm_batch_sub_blocks = \"never\"\n")
                .unwrap_or_else(|err| panic!("parse never config: {err}"));
        assert_eq!(
            never_cfg.tui.confirm_batch_sub_blocks,
            BatchConfirmPolicy::Never
        );
    }

    #[test]
    fn tui_config_rejects_zero_confirm_batch_sub_blocks_threshold() {
        let err =
            toml::from_str::<TrueflowConfig>("[tui]\nconfirm_batch_sub_blocks = 0\n").unwrap_err();
        assert!(
            err.to_string()
                .contains("confirm_batch_sub_blocks threshold must be at least 1"),
            "unexpected parse error: {err}"
        );
    }

    #[test]
    fn tui_config_parses_old_new_diff_line_numbers() {
        let cfg: TrueflowConfig = match toml::from_str("[tui]\ndiff_line_numbers = \"old_new\"\n") {
            Ok(config) => config,
            Err(err) => panic!("parse config: {err}"),
        };
        assert_eq!(cfg.tui.diff_line_numbers, TuiDiffLineNumbers::OldNew);
    }

    #[test]
    fn tui_config_parses_keybind_overrides() {
        let cfg: TrueflowConfig = match toml::from_str(
            r#"
[tui.keybinds]
scroll_up = "i"
scroll_down = "m"
prev = "j"
next = "l"
parent = "u"
child = "o"
approve = "y"
note = "e"
toggle_view = "v"
speed_read = "s"
root = "z"
recap_done = "w"
quit = "x"
"#,
        ) {
            Ok(config) => config,
            Err(err) => panic!("parse config: {err}"),
        };
        assert_eq!(cfg.tui.keybinds.scroll_up, 'i');
        assert_eq!(cfg.tui.keybinds.scroll_down, 'm');
        assert_eq!(cfg.tui.keybinds.prev, 'j');
        assert_eq!(cfg.tui.keybinds.next, 'l');
        assert_eq!(cfg.tui.keybinds.parent, 'u');
        assert_eq!(cfg.tui.keybinds.child, 'o');
        assert_eq!(cfg.tui.keybinds.approve, 'y');
        assert_eq!(cfg.tui.keybinds.note, 'e');
        assert_eq!(cfg.tui.keybinds.toggle_view, 'v');
        assert_eq!(cfg.tui.keybinds.speed_read, 's');
        assert_eq!(cfg.tui.keybinds.root, 'z');
        assert_eq!(cfg.tui.keybinds.recap_done, 'w');
        assert_eq!(cfg.tui.keybinds.quit, 'x');
    }

    #[test]
    fn tui_config_rejects_multi_character_keybinds() {
        let err = toml::from_str::<TrueflowConfig>(
            r#"
[tui.keybinds]
note = "jk"
"#,
        )
        .unwrap_err();
        assert!(
            err.to_string().contains("expected a single character"),
            "unexpected parse error: {err}"
        );
    }

    #[test]
    fn tui_config_rejects_duplicate_keybinds() {
        let err = toml::from_str::<TrueflowConfig>(
            r#"
[tui.keybinds]
quit = "a"
"#,
        )
        .unwrap_err();
        assert!(
            err.to_string()
                .contains("duplicate TUI keybind 'a' for approve and quit"),
            "unexpected parse error: {err}"
        );
    }

    #[test]
    fn speed_read_config_defaults_are_populated() {
        let cfg: TrueflowConfig = match toml::from_str("") {
            Ok(config) => config,
            Err(err) => panic!("parse config: {err}"),
        };
        assert!(cfg.tui.speed_read.enabled);
        assert_eq!(cfg.tui.speed_read.default_wpm, 320);
        assert_eq!(cfg.tui.speed_read.min_wpm, 120);
        assert_eq!(cfg.tui.speed_read.max_wpm, 900);
        assert_eq!(cfg.tui.speed_read.default_chunk_words, 2);
        assert_eq!(cfg.tui.speed_read.min_chunk_words, 1);
        assert_eq!(cfg.tui.speed_read.max_chunk_words, 5);
        assert_eq!(
            cfg.tui.speed_read.punctuation_dwell,
            TuiSpeedReadPunctuationDwell::Light
        );
        assert!((cfg.tui.speed_read.punctuation_dwell_multiplier - 1.15).abs() < f64::EPSILON);
    }

    #[test]
    fn speed_read_config_parses_override_values() {
        let cfg: TrueflowConfig = match toml::from_str(
            r#"
[tui.speed_read]
enabled = false
default_wpm = 400
min_wpm = 100
max_wpm = 1000
default_chunk_words = 3
min_chunk_words = 1
max_chunk_words = 6
loop_playback = true
show_orp_highlight = true
show_prose_optimization_hint = false
punctuation_dwell = "off"
punctuation_dwell_multiplier = 1.2
"#,
        ) {
            Ok(config) => config,
            Err(err) => panic!("parse config: {err}"),
        };
        assert!(!cfg.tui.speed_read.enabled);
        assert_eq!(cfg.tui.speed_read.default_wpm, 400);
        assert_eq!(cfg.tui.speed_read.min_wpm, 100);
        assert_eq!(cfg.tui.speed_read.max_wpm, 1000);
        assert_eq!(cfg.tui.speed_read.default_chunk_words, 3);
        assert_eq!(cfg.tui.speed_read.min_chunk_words, 1);
        assert_eq!(cfg.tui.speed_read.max_chunk_words, 6);
        assert!(cfg.tui.speed_read.loop_playback);
        assert!(cfg.tui.speed_read.show_orp_highlight);
        assert!(!cfg.tui.speed_read.show_prose_optimization_hint);
        assert_eq!(
            cfg.tui.speed_read.punctuation_dwell,
            TuiSpeedReadPunctuationDwell::Off
        );
        assert!((cfg.tui.speed_read.punctuation_dwell_multiplier - 1.2).abs() < f64::EPSILON);
    }

    #[test]
    fn speed_read_config_rejects_inverted_wpm_bounds() {
        let err = toml::from_str::<TrueflowConfig>(
            r#"
[tui.speed_read]
enabled = false
min_wpm = 900
max_wpm = 120
"#,
        )
        .unwrap_err();

        assert!(
            err.to_string().contains("min_wpm must be <= max_wpm"),
            "unexpected parse error: {err}"
        );
    }

    #[test]
    fn speed_read_config_rejects_default_wpm_outside_bounds() {
        let err = toml::from_str::<TrueflowConfig>(
            r#"
[tui.speed_read]
default_wpm = 100
min_wpm = 120
max_wpm = 900
"#,
        )
        .unwrap_err();

        assert!(
            err.to_string()
                .contains("default_wpm must be between min_wpm and max_wpm"),
            "unexpected parse error: {err}"
        );
    }

    #[test]
    fn speed_read_config_rejects_inverted_chunk_word_bounds() {
        let err = toml::from_str::<TrueflowConfig>(
            r#"
[tui.speed_read]
min_chunk_words = 5
max_chunk_words = 1
"#,
        )
        .unwrap_err();

        assert!(
            err.to_string()
                .contains("min_chunk_words must be <= max_chunk_words"),
            "unexpected parse error: {err}"
        );
    }

    #[test]
    fn speed_read_config_rejects_default_chunk_words_outside_bounds() {
        let err = toml::from_str::<TrueflowConfig>(
            r#"
[tui.speed_read]
default_chunk_words = 8
min_chunk_words = 1
max_chunk_words = 5
"#,
        )
        .unwrap_err();

        assert!(
            err.to_string().contains(
                "default_chunk_words must be between min_chunk_words and max_chunk_words"
            ),
            "unexpected parse error: {err}"
        );
    }

    #[test]
    fn speed_read_config_rejects_zero_values_that_make_tui_state_invalid() {
        let err = toml::from_str::<TrueflowConfig>(
            r#"
[tui.speed_read]
default_wpm = 0
min_wpm = 0
max_wpm = 0
default_chunk_words = 0
min_chunk_words = 0
max_chunk_words = 0
"#,
        )
        .unwrap_err();

        assert!(
            err.to_string()
                .contains("speed-read WPM values must be greater than 0"),
            "unexpected parse error: {err}"
        );
    }

    #[test]
    fn speed_read_config_rejects_zero_punctuation_dwell_multiplier() {
        let err = toml::from_str::<TrueflowConfig>(
            r#"
[tui.speed_read]
punctuation_dwell_multiplier = 0.0
"#,
        )
        .unwrap_err();

        assert!(
            err.to_string()
                .contains("punctuation_dwell_multiplier must be finite and greater than 0"),
            "unexpected parse error: {err}"
        );
    }

    #[test]
    fn feedback_default_since_defaults_to_all() {
        let cfg: TrueflowConfig = match toml::from_str("") {
            Ok(config) => config,
            Err(err) => panic!("parse config: {err}"),
        };
        assert_eq!(cfg.feedback.default_since, FeedbackSinceExpr::default());
    }

    #[test]
    fn feedback_default_since_parses_override() {
        let cfg: TrueflowConfig = match toml::from_str("[feedback]\ndefault_since = \"last\"\n") {
            Ok(config) => config,
            Err(err) => panic!("parse config: {err}"),
        };
        assert_eq!(
            cfg.feedback.default_since,
            FeedbackSinceExpr::new("last").unwrap()
        );
    }

    #[test]
    fn feedback_default_since_rejects_invalid_value() {
        let err = toml::from_str::<TrueflowConfig>("[feedback]\ndefault_since = \"someday\"\n")
            .unwrap_err();
        assert!(
            err.to_string()
                .contains("Invalid feedback since value 'someday'"),
            "unexpected parse error: {err}"
        );
    }

    #[test]
    fn block_filter_config_parses_directly_to_block_kinds() {
        let cfg: TrueflowConfig = match toml::from_str(
            r#"
[review]
only = ["Function-Signature"]
exclude = ["gap"]

[feedback]
only = ["Struct"]
exclude = ["comment"]
"#,
        ) {
            Ok(config) => config,
            Err(err) => panic!("parse config: {err}"),
        };

        assert_eq!(cfg.review.only, vec![BlockKind::FunctionSignature]);
        assert_eq!(cfg.review.exclude, vec![BlockKind::Gap]);
        assert_eq!(cfg.feedback.filters.only, vec![BlockKind::Struct]);
        assert_eq!(cfg.feedback.filters.exclude, vec![BlockKind::Comment]);
    }

    #[test]
    fn block_filters_skip_gap_by_default() {
        let filters = BlockFilters::default();

        assert!(!filters.allows_block(BlockKind::Gap));
        assert!(!filters.allows_subblock(BlockKind::Gap));
        assert!(filters.allows_block(BlockKind::Function));
    }

    #[test]
    fn block_filters_keep_gap_skipped_when_other_excludes_are_configured() {
        let cfg: TrueflowConfig = toml::from_str(
            r#"
[review]
exclude = ["comment"]
"#,
        )
        .unwrap_or_else(|err| panic!("parse config: {err}"));
        let filters = cfg.review.resolve_filters(&[], &[]);

        assert!(!filters.allows_block(BlockKind::Gap));
        assert!(!filters.allows_block(BlockKind::Comment));
        assert!(filters.allows_block(BlockKind::Function));
    }

    #[test]
    fn block_filters_allow_explicit_only_gap() {
        let filters = BlockFilterConfig::default().resolve_filters(&[BlockKind::Gap], &[]);

        assert!(filters.allows_block(BlockKind::Gap));
        assert!(filters.allows_subblock(BlockKind::Gap));
        assert!(!filters.allows_block(BlockKind::Function));
    }

    #[test]
    fn block_filter_config_rejects_unknown_block_kinds() {
        let err = toml::from_str::<TrueflowConfig>("[review]\nonly = [\"not-a-real-kind\"]\n")
            .unwrap_err();
        assert!(
            err.to_string()
                .contains("Unknown block kind: not-a-real-kind"),
            "unexpected parse error: {err}"
        );
    }

    #[test]
    fn scan_config_resolves_defaults_and_overrides() {
        let cfg: TrueflowConfig = toml::from_str(
            r#"
[scan]
use_cache = false
write_cache = true
cache_dir = "custom-cache"
ignore_names = ["dist"]
ignore_globs = ["*.snap"]
ignore_path_prefixes = ["vendor", "generated"]
"#,
        )
        .unwrap_or_else(|err| panic!("parse config: {err}"));

        let options = cfg.scan.resolve_options();
        assert_eq!(options.cache_mode, ScanCacheMode::WriteOnly);
        assert_eq!(options.cache_dir, Some(PathBuf::from("custom-cache")));
        assert!(options.ignore_names.iter().any(|name| name == ".git"));
        assert!(options.ignore_names.iter().any(|name| name == "dist"));
        assert_eq!(options.ignore_globs, vec!["*.snap".to_string()]);
        assert_eq!(
            options.ignore_path_prefixes,
            vec![
                RepoPath::new("generated").unwrap(),
                RepoPath::new("vendor").unwrap(),
            ]
        );
    }

    #[test]
    fn scan_config_rejects_invalid_ignore_globs() {
        let err = toml::from_str::<TrueflowConfig>("[scan]\nignore_globs = [\"[\"]\n").unwrap_err();
        assert!(
            err.to_string().contains("invalid scan ignore glob"),
            "unexpected scan config error: {err}"
        );
    }

    #[test]
    fn load_uses_home_trueflow_toml_as_global_defaults() {
        let root = temp_config_dir("global_defaults");
        let home = root.join("home");
        let workdir = root.join("repo").join("nested");
        std::fs::create_dir_all(&workdir).unwrap_or_else(|err| panic!("create workdir: {err}"));
        write_config(
            &home.join(".trueflow.toml"),
            r#"
[ai]
enabled = true
provider = "claude_cli"
model = "claude-3-5-haiku-latest"

[tui]
diff_line_numbers = "old_new"
"#,
        );

        let cfg = load_from_start_dir(&workdir, Some(&home))
            .unwrap_or_else(|err| panic!("load config: {err}"));

        assert!(cfg.ai.enabled);
        assert_eq!(cfg.ai.provider, AiProviderConfig::ClaudeCli);
        assert_eq!(cfg.ai.model, "claude-3-5-haiku-latest");
        assert_eq!(cfg.tui.diff_line_numbers, TuiDiffLineNumbers::OldNew);
    }

    #[test]
    fn load_merges_global_and_ancestor_configs_with_closer_values_winning() {
        let root = temp_config_dir("global_and_closer_overrides");
        let home = root.join("home");
        let repo = root.join("repo");
        let nested = repo.join("nested");
        let workdir = nested.join("leaf");
        std::fs::create_dir_all(&workdir).unwrap_or_else(|err| panic!("create workdir: {err}"));
        write_config(
            &home.join(".trueflow.toml"),
            r#"
[ai]
enabled = true
provider = "claude_cli"
model = "global-model"
max_context_lines = 11

[tui]
diff_line_numbers = "old_new"
diff_focus_context_lines = 8
"#,
        );
        write_config(
            &repo.join("trueflow.toml"),
            r#"
[ai]
model = "repo-model"

[tui]
diff_focus_context_lines = 5
"#,
        );
        write_config(
            &nested.join("trueflow.toml"),
            r#"
[ai]
max_context_lines = 24
"#,
        );

        let cfg = load_from_start_dir(&workdir, Some(&home))
            .unwrap_or_else(|err| panic!("load config: {err}"));

        assert!(cfg.ai.enabled);
        assert_eq!(cfg.ai.provider, AiProviderConfig::ClaudeCli);
        assert_eq!(cfg.ai.model, "repo-model");
        assert_eq!(cfg.ai.max_context_lines, 24);
        assert_eq!(cfg.tui.diff_line_numbers, TuiDiffLineNumbers::OldNew);
        assert_eq!(cfg.tui.diff_focus_context_lines, 5);
    }

    fn temp_config_dir(name: &str) -> PathBuf {
        std::env::temp_dir()
            .join("trueflow_tests")
            .join(format!("config_{name}_{}", uuid::Uuid::new_v4()))
    }

    fn write_config(path: &Path, content: &str) {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).unwrap_or_else(|err| panic!("create parent: {err}"));
        }
        std::fs::write(path, content).unwrap_or_else(|err| panic!("write config: {err}"));
    }

    fn temp_speed_read_config_path(name: &str) -> PathBuf {
        temp_config_dir(name).join("trueflow.toml")
    }

    #[test]
    fn update_speed_read_defaults_creates_and_writes_config() {
        let path = temp_speed_read_config_path("speed_read_create");

        update_speed_read_defaults_in_file(&path, 360, 3)
            .unwrap_or_else(|err| panic!("update config: {err}"));

        let content =
            std::fs::read_to_string(&path).unwrap_or_else(|err| panic!("read config: {err}"));
        let parsed: TrueflowConfig =
            toml::from_str(&content).unwrap_or_else(|err| panic!("parse config: {err}"));
        assert_eq!(parsed.tui.speed_read.default_wpm, 360);
        assert_eq!(parsed.tui.speed_read.default_chunk_words, 3);
    }

    #[test]
    fn update_speed_read_defaults_preserves_existing_comments_and_keys() {
        let path = temp_speed_read_config_path("speed_read_preserve");
        let initial = r#"# user comment
[review]
exclude = ["gap"]

[tui]
# important note
confirm_batch_sub_blocks = 2
"#;
        write_config(&path, initial);

        update_speed_read_defaults_in_file(&path, 420, 4)
            .unwrap_or_else(|err| panic!("update config: {err}"));

        let content =
            std::fs::read_to_string(&path).unwrap_or_else(|err| panic!("read config: {err}"));
        assert!(
            content.contains("# user comment"),
            "expected comment to survive edit: {content}"
        );
        assert!(
            content.contains("confirm_batch_sub_blocks"),
            "expected existing key to survive edit: {content}"
        );

        let parsed: TrueflowConfig =
            toml::from_str(&content).unwrap_or_else(|err| panic!("parse config: {err}"));
        assert_eq!(parsed.tui.speed_read.default_wpm, 420);
        assert_eq!(parsed.tui.speed_read.default_chunk_words, 4);
    }

    #[test]
    fn update_speed_read_defaults_rejects_values_that_would_make_config_invalid() {
        let path = temp_speed_read_config_path("speed_read_invalid_update");
        let initial = r#"[tui.speed_read]
min_wpm = 120
max_wpm = 900
min_chunk_words = 1
max_chunk_words = 5
"#;
        write_config(&path, initial);

        let err = update_speed_read_defaults_in_file(&path, 100, 6).unwrap_err();

        assert!(
            err.to_string()
                .contains("Updated speed-read defaults would make config invalid"),
            "unexpected error: {err}"
        );
        let content =
            std::fs::read_to_string(&path).unwrap_or_else(|err| panic!("read config: {err}"));
        assert_eq!(content, initial);
    }

    #[test]
    fn update_speed_read_defaults_rejects_non_table_tui_config_without_overwriting() {
        let path = temp_speed_read_config_path("speed_read_non_table_tui");
        let initial = "tui = \"not a table\"\n";
        write_config(&path, initial);

        let err = update_speed_read_defaults_in_file(&path, 360, 3).unwrap_err();

        assert!(
            err.to_string().contains("Expected [tui] to be a table"),
            "unexpected error: {err}"
        );
        let content =
            std::fs::read_to_string(&path).unwrap_or_else(|err| panic!("read config: {err}"));
        assert_eq!(content, initial);
    }

    #[test]
    fn update_speed_read_defaults_rejects_non_table_speed_read_config_without_overwriting() {
        let path = temp_speed_read_config_path("speed_read_non_table_speed_read");
        let initial = "[tui]\nspeed_read = \"not a table\"\n";
        write_config(&path, initial);

        let err = update_speed_read_defaults_in_file(&path, 360, 3).unwrap_err();

        assert!(
            err.to_string()
                .contains("Expected [tui.speed_read] to be a table"),
            "unexpected error: {err}"
        );
        let content =
            std::fs::read_to_string(&path).unwrap_or_else(|err| panic!("read config: {err}"));
        assert_eq!(content, initial);
    }
}
