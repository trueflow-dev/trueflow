use crate::block::{Block, BlockKind};
use crate::config::load as load_config;
use crate::context::TrueflowContext;
use crate::coverage::{CoverageBuildOptions, CoverageIndex};
use crate::policy::should_skip_imports_by_default;
use crate::scanner;
use crate::store::{FileStore, Identity, Record, ReviewStore};
use crate::targets::{
    ResolvedTargets, ReviewContentSource, ReviewTarget, resolve_targets,
    workdir_prefix_from_git_root,
};
use crate::tree;
use crate::vcs;
use anyhow::{Result, anyhow};
use chrono::{DateTime, Utc};
use std::fs;
use std::path::Path;

const FEEDBACK_CURSOR_FILE: &str = "feedback.cursor";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FeedbackFormat {
    Xml,
    Json,
}

impl FeedbackFormat {
    pub fn from_arg(raw: &str) -> Self {
        if raw == "json" { Self::Json } else { Self::Xml }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum FeedbackSince {
    All,
    Timestamp(i64),
    Last,
}

#[derive(Debug, Clone)]
struct FeedbackEntry {
    file_path: String,
    block: Block,
    reviews: Vec<Record>,
    latest_verdict: String,
}

struct FeedbackCollectionOptions<'a> {
    filters: &'a crate::config::BlockFilters,
    targets: &'a ResolvedTargets,
    include_approved: bool,
    workdir_prefix: Option<&'a str>,
}

pub fn run(
    _context: &TrueflowContext,
    format: FeedbackFormat,
    since: Option<&str>,
    targets: &[ReviewTarget],
    include_approved: bool,
    only: &[BlockKind],
    exclude: &[BlockKind],
) -> Result<()> {
    let config = load_config()?;
    let filters = config.feedback.resolve_filters(only, exclude);
    let scan_options = config.scan.resolve_options()?;
    let effective_since = since.or(Some(config.feedback.default_since.as_str()));
    let resolved_targets = resolve_targets(targets)?;

    // 1. Scan current or historical directory state.
    let workdir_prefix = workdir_prefix_from_git_root();
    let files = match &resolved_targets.content_source {
        ReviewContentSource::Workdir => scanner::scan_directory(".", &scan_options)?.files,
        ReviewContentSource::Revision(revision) => {
            let repo = vcs::repo_from_workdir()?;
            vcs::file_states_in_revision(&repo, revision.as_str(), workdir_prefix.as_deref())?
        }
    };
    let tree = tree::build_tree_from_files(&files);

    // 2. Load DB
    let store = FileStore::new()?;
    let database = store.load_database()?;
    let max_history_timestamp = database.max_timestamp();
    let since_mode = parse_feedback_since(effective_since)?;
    let since_threshold = resolve_since_threshold(&store, since_mode)?;

    let entries = collect_feedback_entries(
        &files,
        &tree,
        &database,
        since_threshold,
        &FeedbackCollectionOptions {
            filters: &filters,
            targets: &resolved_targets,
            include_approved,
            workdir_prefix: workdir_prefix.as_deref(),
        },
    )?;

    match format {
        FeedbackFormat::Json => {
            let export_list = entries
                .into_iter()
                .map(|entry| {
                    serde_json::json!({
                        "file": entry.file_path,
                        "block": entry.block,
                        "reviews": entry.reviews,
                        "latest_verdict": entry.latest_verdict,
                    })
                })
                .collect::<Vec<_>>();
            println!("{}", serde_json::to_string_pretty(&export_list)?);
        }
        FeedbackFormat::Xml => {
            println!("<trueflow_feedback>");

            let mut current_file_path: Option<String> = None;
            for entry in entries {
                if current_file_path.as_deref() != Some(entry.file_path.as_str()) {
                    if current_file_path.is_some() {
                        println!("  </file>");
                    }
                    println!("  <file path=\"{}\">", escape_xml(&entry.file_path));
                    current_file_path = Some(entry.file_path.clone());
                }

                print_block_xml(&entry.block, &entry.reviews);
            }

            if current_file_path.is_some() {
                println!("  </file>");
            }

            println!("</trueflow_feedback>");
        }
    }

    if matches!(since_mode, FeedbackSince::Last)
        && let Some(timestamp) = max_history_timestamp
    {
        write_feedback_cursor(feedback_cursor_path(&store).as_path(), timestamp)?;
    }

    Ok(())
}

fn print_block_xml(block: &Block, reviews: &[Record]) {
    println!(
        "    <block start_line=\"{}\" end_line=\"{}\" kind=\"{}\" hash=\"{}\">",
        block.start_line,
        block.end_line,
        escape_xml(block.kind.as_str()),
        block.hash
    );

    println!("      <context><![CDATA[");
    let safe_content = block.content.replace("]]>", "]]]]><![CDATA[>");
    println!("{safe_content}");
    println!("]]></context>");

    println!("      <reviews>");
    for r in reviews {
        let author = match &r.identity {
            Identity::Email { email, .. } => email,
        };
        println!(
            "        <review verdict=\"{}\" author=\"{}\">",
            escape_xml(r.verdict.as_str()),
            escape_xml(author)
        );
        if let Some(note) = &r.note {
            println!("          <comment>{}</comment>", escape_xml(note));
        }
        println!("        </review>");
    }
    println!("      </reviews>");
    println!("    </block>");
}

fn escape_xml(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&apos;")
}

fn collect_feedback_entries(
    files: &[crate::block::FileState],
    tree: &tree::Tree,
    database: &crate::store::ReviewDatabase,
    since_threshold: Option<i64>,
    options: &FeedbackCollectionOptions<'_>,
) -> Result<Vec<FeedbackEntry>> {
    let FeedbackCollectionOptions {
        filters,
        targets,
        include_approved,
        workdir_prefix,
    } = *options;
    let explicit_selection = targets.explicit_selection();
    let changed_selection = targets.changed_selection();
    let build_options = CoverageBuildOptions {
        workdir_prefix: workdir_prefix.map(str::to_string),
    };
    let coverage = CoverageIndex::build(tree, database, &build_options)?;
    let approved_targets = database.latest_index(None).approved_targets();
    let since_records = database
        .records()
        .iter()
        .filter(|record| since_threshold.is_none_or(|threshold| record.timestamp > threshold))
        .cloned()
        .collect::<Vec<_>>();
    let since_database = crate::store::ReviewDatabase::from_records(since_records);
    let since_coverage = CoverageIndex::build(tree, &since_database, &build_options)?;

    let mut entries = Vec::new();
    for file in files {
        if let Some(selection) = &explicit_selection
            && !selection.includes(&file.path, workdir_prefix)?
        {
            continue;
        }
        if let Some(selection) = &changed_selection
            && !selection.includes(&file.path, workdir_prefix)?
        {
            continue;
        }

        for block in &file.blocks {
            if !filters.allows_block(block.kind) {
                continue;
            }
            if should_skip_imports_by_default(file.path.as_str(), block, filters) {
                continue;
            }

            let Some(node_id) = tree.find_block_node(file.path.as_str(), block) else {
                continue;
            };
            let latest_verdict = coverage
                .node(node_id)
                .linked_records()
                .last()
                .map_or("unreviewed", |record| record.verdict.as_str());

            if !include_approved && latest_verdict == "approved" {
                continue;
            }
            if !include_approved && tree.is_node_covered(node_id, &approved_targets, workdir_prefix)
            {
                continue;
            }

            let reviews = since_coverage
                .node(node_id)
                .linked_records()
                .into_iter()
                .cloned()
                .collect::<Vec<_>>();
            if reviews.is_empty() {
                continue;
            }

            entries.push(FeedbackEntry {
                file_path: file.path.as_str().to_string(),
                block: block.clone(),
                reviews,
                latest_verdict: latest_verdict.to_string(),
            });
        }
    }

    Ok(entries)
}

fn parse_feedback_since(raw: Option<&str>) -> Result<FeedbackSince> {
    parse_feedback_since_with_now(raw, Utc::now())
}

fn parse_feedback_since_with_now(raw: Option<&str>, now: DateTime<Utc>) -> Result<FeedbackSince> {
    let Some(raw) = raw.map(str::trim).filter(|value| !value.is_empty()) else {
        return Ok(FeedbackSince::All);
    };

    if raw.eq_ignore_ascii_case("all") {
        return Ok(FeedbackSince::All);
    }
    if raw.eq_ignore_ascii_case("last") {
        return Ok(FeedbackSince::Last);
    }
    if let Ok(timestamp) = raw.parse::<i64>() {
        return Ok(FeedbackSince::Timestamp(timestamp));
    }
    if let Some(timestamp) = parse_relative_since_timestamp(raw, now)? {
        return Ok(FeedbackSince::Timestamp(timestamp));
    }

    let parsed = DateTime::parse_from_rfc3339(raw)
        .map(|value| value.with_timezone(&Utc).timestamp())
        .map_err(|error| {
            anyhow!(
                "Invalid --since value '{raw}'. Use 'all', 'last', relative durations like '1h', unix timestamp, or RFC3339 ({error})"
            )
        })?;
    Ok(FeedbackSince::Timestamp(parsed))
}

fn parse_relative_since_timestamp(raw: &str, now: DateTime<Utc>) -> Result<Option<i64>> {
    let trimmed = raw.trim();
    let trimmed = trimmed
        .strip_suffix("ago")
        .map(str::trim_end)
        .unwrap_or(trimmed);
    let compact = trimmed
        .chars()
        .filter(|ch| !ch.is_ascii_whitespace())
        .collect::<String>();
    if compact.is_empty() {
        return Ok(None);
    }

    let split_at = compact
        .find(|ch: char| !ch.is_ascii_digit())
        .unwrap_or(compact.len());
    if split_at == 0 || split_at == compact.len() {
        return Ok(None);
    }

    let amount = compact[..split_at]
        .parse::<i64>()
        .map_err(|error| anyhow!("invalid relative duration amount in '{raw}': {error}"))?;
    let unit = compact[split_at..].to_ascii_lowercase();
    let scale = match unit.as_str() {
        "s" | "sec" | "secs" | "second" | "seconds" => 1,
        "m" | "min" | "mins" | "minute" | "minutes" => 60,
        "h" | "hr" | "hrs" | "hour" | "hours" => 60 * 60,
        "d" | "day" | "days" => 60 * 60 * 24,
        "w" | "week" | "weeks" => 60 * 60 * 24 * 7,
        _ => return Ok(None),
    };
    let seconds = amount
        .checked_mul(scale)
        .ok_or_else(|| anyhow!("relative --since value '{raw}' overflowed"))?;
    let threshold = now
        .timestamp()
        .checked_sub(seconds)
        .ok_or_else(|| anyhow!("relative --since value '{raw}' underflowed"))?;
    Ok(Some(threshold))
}

fn resolve_since_threshold(store: &FileStore, since: FeedbackSince) -> Result<Option<i64>> {
    let threshold = match since {
        FeedbackSince::All => None,
        FeedbackSince::Timestamp(timestamp) => Some(timestamp),
        FeedbackSince::Last => read_feedback_cursor(feedback_cursor_path(store).as_path())?,
    };
    Ok(threshold)
}

fn feedback_cursor_path(store: &FileStore) -> std::path::PathBuf {
    store.trueflow_dir().join(FEEDBACK_CURSOR_FILE)
}

fn read_feedback_cursor(path: &Path) -> Result<Option<i64>> {
    let content = match fs::read_to_string(path) {
        Ok(content) => content,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error.into()),
    };

    let trimmed = content.trim();
    if trimmed.is_empty() {
        return Ok(None);
    }

    let value = trimmed.parse::<i64>().map_err(|error| {
        anyhow::anyhow!(
            "Invalid feedback cursor at {}: expected unix timestamp ({error})",
            path.display()
        )
    })?;
    Ok(Some(value))
}

fn write_feedback_cursor(path: &Path, timestamp: i64) -> Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    fs::write(path, format!("{timestamp}\n"))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::{TimeZone, Utc};

    #[test]
    fn feedback_format_uses_json_for_json_arg() {
        assert_eq!(FeedbackFormat::from_arg("json"), FeedbackFormat::Json);
    }

    #[test]
    fn feedback_format_preserves_xml_default_behavior_for_non_json_args() {
        assert_eq!(FeedbackFormat::from_arg("xml"), FeedbackFormat::Xml);
        assert_eq!(FeedbackFormat::from_arg("yaml"), FeedbackFormat::Xml);
        assert_eq!(FeedbackFormat::from_arg(""), FeedbackFormat::Xml);
    }

    #[test]
    fn parse_feedback_since_supports_relative_hours() {
        let now = Utc
            .timestamp_opt(10_000, 0)
            .single()
            .unwrap_or_else(|| panic!("valid timestamp"));
        let parsed = parse_feedback_since_with_now(Some("1h"), now)
            .unwrap_or_else(|error| panic!("relative duration should parse: {error}"));
        assert_eq!(parsed, FeedbackSince::Timestamp(6_400));
    }

    #[test]
    fn parse_feedback_since_supports_relative_days_with_ago_suffix() {
        let now = Utc
            .timestamp_opt(200_000, 0)
            .single()
            .unwrap_or_else(|| panic!("valid timestamp"));
        let parsed = parse_feedback_since_with_now(Some("2d ago"), now)
            .unwrap_or_else(|error| panic!("relative duration with ago should parse: {error}"));
        assert_eq!(parsed, FeedbackSince::Timestamp(27_200));
    }
}
