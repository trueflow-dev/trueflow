use crate::block::Block;
use crate::config::load as load_config;
use crate::context::TrueflowContext;
use crate::policy::should_skip_imports_by_default;
use crate::scanner;
use crate::store::{
    approved_hashes_from_verdicts, FileStore, Identity, Record, ReviewStore, Verdict,
};
use crate::tree;
use anyhow::Result;
use std::collections::HashMap;

pub fn run(
    _context: &TrueflowContext,
    format: &str,
    include_approved: bool,
    only: Vec<String>,
    exclude: Vec<String>,
) -> Result<()> {
    let config = load_config()?;
    let filters = config.feedback.resolve_filters(&only, &exclude);

    // 1. Scan Directory (Current State)
    let files = scanner::scan_directory(".")?;
    let tree = tree::build_tree_from_files(&files);

    // 2. Load DB
    let store = FileStore::new()?;
    let history = store.read_history()?;

    // 3. Group Reviews by Fingerprint
    // We want ALL reviews for a fingerprint, not just the latest.
    let mut reviews_by_fp: HashMap<String, Vec<Record>> = HashMap::new();
    let mut latest_verdict: HashMap<String, Verdict> = HashMap::new();

    for record in history {
        // Update latest verdict (Last Write Wins)
        latest_verdict.insert(record.fingerprint.clone(), record.verdict.clone());

        // Collect history
        reviews_by_fp
            .entry(record.fingerprint.clone())
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
                if !filters.allows_block(&block.kind) {
                    continue;
                }
                if should_skip_imports_by_default(&file.path, &block, &filters) {
                    continue;
                }

                let verdict = latest_verdict
                    .get(&block.hash)
                    .map(|value| value.as_str())
                    .unwrap_or("unreviewed");

                if !include_approved && verdict == "approved" {
                    continue;
                }

                if !include_approved
                    && tree
                        .node_by_path_and_hash(&file.path, &block.hash)
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
                if !filters.allows_block(&block.kind) {
                    continue;
                }
                if should_skip_imports_by_default(&file.path, &block, &filters) {
                    continue;
                }

                let verdict = latest_verdict
                    .get(&block.hash)
                    .map(|value| value.as_str())
                    .unwrap_or("unreviewed");

                if !include_approved && verdict == "approved" {
                    continue;
                }

                if !include_approved
                    && tree
                        .node_by_path_and_hash(&file.path, &block.hash)
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
    println!("{}", safe_content);
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
    s.replace("&", "&amp;")
        .replace("<", "&lt;")
        .replace(">", "&gt;")
        .replace("\"", "&quot;")
        .replace("'", "&apos;")
}
