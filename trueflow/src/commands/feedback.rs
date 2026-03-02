use crate::block::Block;
use crate::config::load as load_config;
use crate::context::TrueflowContext;
use crate::policy::should_skip_imports_by_default;
use crate::scanner;
use crate::store::{
    FileStore, Identity, Record, ReviewStore, Verdict, approved_hashes_from_verdicts,
    latest_verdicts,
};
use crate::tree;
use anyhow::Result;
use chrono::{DateTime, Utc};
use std::collections::HashMap;
use std::fs;
use std::path::Path;

const FEEDBACK_CURSOR_FILE: &str = "feedback.cursor";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum FeedbackSince {
    All,
    Timestamp(i64),
    Last,
}

pub fn run(
    _context: &TrueflowContext,
    format: &str,
    since: Option<&str>,
    include_approved: bool,
    only: &[String],
    exclude: &[String],
) -> Result<()> {
    let config = load_config()?;
    let filters = config.feedback.resolve_filters(only, exclude);
    let effective_since = since.or(Some(config.feedback.default_since.as_str()));

    // 1. Scan Directory (Current State)
    let files = scanner::scan_directory(".")?;
    let tree = tree::build_tree_from_files(&files);

    // 2. Load DB
    let store = FileStore::new()?;
    let history = store.read_history()?;
    let max_history_timestamp = history.iter().map(|record| record.timestamp).max();
    let since_mode = parse_feedback_since(effective_since)?;
    let since_threshold = resolve_since_threshold(&store, since_mode)?;

    // 3. Group Reviews by target key
    // We want ALL reviews for a fingerprint, not just the latest.
    let latest_verdict: HashMap<String, Verdict> = latest_verdicts(&history, None);
    let mut reviews_by_fp: HashMap<String, Vec<Record>> = HashMap::new();

    for record in history {
        if !matches_since(record.timestamp, since_threshold) {
            continue;
        }
        reviews_by_fp
            .entry(record.lookup_key().to_string())
            .or_default()
            .push(record);
    }

    let approved_hashes = approved_hashes_from_verdicts(&latest_verdict);

    if format == "json" {
        // Output JSON
        // Structure: List of objects with { path, block, reviews }
        let mut export_list = Vec::new();

        for file in files {
            for block in file.blocks {
                if !filters.allows_block(block.kind) {
                    continue;
                }
                if should_skip_imports_by_default(&file.path, &block, &filters) {
                    continue;
                }

                let verdict = latest_verdict
                    .get(&block.hash)
                    .map_or("unreviewed", crate::store::Verdict::as_str);

                if !include_approved && verdict == "approved" {
                    continue;
                }

                if !include_approved
                    && tree
                        .find_block_node(&file.path, &block)
                        .is_some_and(|node_id| tree.is_node_covered(node_id, &approved_hashes))
                {
                    continue;
                }

                // Only include if there is actual history (or if it's unreviewed? No, "feedback" usually means critiques)
                // If it's unreviewed, the agent might not care unless we want to ask for review?
                // The prompt was "review content that we just did".
                // So we only export things THAT HAVE REVIEWS.
                // If verdict is "unreviewed", skip.

                if let Some(reviews) = reviews_by_fp.get(&block.hash) {
                    export_list.push(serde_json::json!({
                        "file": file.path,
                        "block": block,
                        "reviews": reviews,
                        "latest_verdict": verdict
                    }));
                }
            }
        }
        println!("{}", serde_json::to_string_pretty(&export_list)?);
    } else {
        // Output XML
        println!("<trueflow_feedback>");

        for file in files {
            // Buffer block output so we only print <file> tag if needed?
            // Actually, XML structure <file path="..."> is better if it wraps blocks.
            // But we can just print blocks flat inside root if easier?
            // User requested hierarchical.

            // Let's iterate blocks first to see if we have anything to print
            let mut blocks_to_print = Vec::new();

            for block in file.blocks {
                if !filters.allows_block(block.kind) {
                    continue;
                }
                if should_skip_imports_by_default(&file.path, &block, &filters) {
                    continue;
                }

                let verdict = latest_verdict
                    .get(&block.hash)
                    .map_or("unreviewed", crate::store::Verdict::as_str);

                if !include_approved && verdict == "approved" {
                    continue;
                }

                if !include_approved
                    && tree
                        .find_block_node(&file.path, &block)
                        .is_some_and(|node_id| tree.is_node_covered(node_id, &approved_hashes))
                {
                    continue;
                }

                if let Some(reviews) = reviews_by_fp.get(&block.hash) {
                    blocks_to_print.push((block, reviews));
                }
            }

            if !blocks_to_print.is_empty() {
                println!("  <file path=\"{}\">", escape_xml(&file.path));
                for (block, reviews) in blocks_to_print {
                    print_block_xml(&block, reviews);
                }
                println!("  </file>");
            }
        }

        println!("</trueflow_feedback>");
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

fn parse_feedback_since(raw: Option<&str>) -> Result<FeedbackSince> {
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

    let parsed = DateTime::parse_from_rfc3339(raw)
        .map(|value| value.with_timezone(&Utc).timestamp())
        .map_err(|error| {
            anyhow::anyhow!(
                "Invalid --since value '{raw}'. Use 'all', 'last', unix timestamp, or RFC3339 ({error})"
            )
        })?;
    Ok(FeedbackSince::Timestamp(parsed))
}

fn resolve_since_threshold(store: &FileStore, since: FeedbackSince) -> Result<Option<i64>> {
    let threshold = match since {
        FeedbackSince::All => None,
        FeedbackSince::Timestamp(timestamp) => Some(timestamp),
        FeedbackSince::Last => read_feedback_cursor(feedback_cursor_path(store).as_path())?,
    };
    Ok(threshold)
}

fn matches_since(timestamp: i64, threshold: Option<i64>) -> bool {
    threshold.is_none_or(|threshold| timestamp > threshold)
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
