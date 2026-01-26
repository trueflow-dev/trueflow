use crate::analysis::Language;
use crate::block::{Block, BlockKind};
use crate::config::{BlockFilters, load as load_config};
use crate::context::TrueflowContext;
use crate::scanner;
use crate::store::{FileStore, ReviewStore, Verdict};
use crate::sub_splitter;
use crate::tree;
use crate::vcs;
use anyhow::{Result, anyhow};
use log::info;
use serde::Serialize;
use std::collections::{HashMap, HashSet};
use std::path::Path;

#[derive(Serialize)]
pub struct UnreviewedFile {
    pub path: String,
    pub language: Language,
    pub blocks: Vec<Block>,
}

pub struct ReviewOptions {
    pub all: bool,
    pub targets: Vec<ReviewTarget>,
    pub only: Vec<String>,
    pub exclude: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReviewTarget {
    DirtyWorktree,
    All,
    File(String),
    Revision(String),
    RevisionRange { start: String, end: String },
}

pub struct ReviewSummary {
    pub files: Vec<UnreviewedFile>,
    pub total_blocks: usize,
    #[allow(dead_code)]
    pub review_state: HashMap<String, Verdict>,
    pub tree: tree::Tree,
    pub unreviewed_block_nodes: HashSet<tree::TreeNodeId>,
}

pub fn collect_review_summary(
    _context: &TrueflowContext,
    options: &ReviewOptions,
    filters: &BlockFilters,
) -> Result<ReviewSummary> {
    info!(
        "review collect (all={}, only={:?}, exclude={:?})",
        options.all, options.only, options.exclude
    );
    let target_paths = resolve_review_targets(options)?;

    // 1. Load Approved Hashes
    let store = FileStore::new()?;
    let history = store.read_history()?;
    info!("loaded {} review records", history.len());

    // Compute current status for each fingerprint (Last Write Wins by timestamp)
    let mut sorted_history = history;
    sorted_history.sort_by_key(|record| record.timestamp);

    let mut fingerprint_status = HashMap::<String, Verdict>::new();
    for record in sorted_history {
        if record.check == "review" {
            fingerprint_status.insert(record.fingerprint, record.verdict);
        }
    }
    let approved_hashes = approved_hashes_from_status(&fingerprint_status);

    // 2. Scan Directory (Merkle Tree)
    let files = scanner::scan_directory(".")?;
    info!("scanned {} files", files.len());
    let tree = tree::build_tree_from_files(&files);

    // 3. Subtraction (Tree Traversal)
    let mut unreviewed_files = Vec::new();
    let mut total_blocks = 0;
    let mut unreviewed_block_nodes = HashSet::new();

    for file in files {
        if let Some(targets) = &target_paths {
            let path_normalized = Path::new(&file.path)
                .strip_prefix("./")
                .unwrap_or(Path::new(&file.path));
            let path_str = path_normalized.to_string_lossy();
            if !targets.contains(path_str.as_ref()) && !targets.contains(&file.path) {
                continue;
            }
        }

        let language = file.language.clone();
        let mut reviewable_blocks = Vec::new();
        for block in file.blocks {
            if !filters.allows_block(&block.kind) {
                continue;
            }
            reviewable_blocks.push(block);
        }
        total_blocks += reviewable_blocks.len();

        // Optimization: If the FILE hash is approved, everything inside is approved.
        if fingerprint_status.get(&file.file_hash) == Some(&Verdict::Approved) {
            continue;
        }

        let mut unreviewed_blocks = Vec::new();
        for block in reviewable_blocks {
            let node_id = tree.node_by_path_and_hash(tree::normalize_path(&file.path), &block.hash);
            if let Some(node_id) = node_id
                && tree.is_node_covered(node_id, &approved_hashes)
            {
                continue;
            }

            // Check status
            if fingerprint_status.get(&block.hash) == Some(&Verdict::Approved) {
                continue;
            }

            if !fingerprint_status.contains_key(&block.hash) {
                // Not explicitly approved. Check implicit approval via sub-blocks.
                if let Ok(sub_blocks) = sub_splitter::split(&block, language.clone())
                    && !sub_blocks.is_empty()
                {
                    let all_approved = sub_blocks.iter().all(|sb| {
                        if !filters.allows_subblock(&sb.kind) {
                            return true;
                        }
                        fingerprint_status.get(&sb.hash) == Some(&Verdict::Approved)
                    });

                    if all_approved {
                        continue;
                    }
                }
            }

            if let Some(node_id) = node_id {
                unreviewed_block_nodes.insert(node_id);
            }
            unreviewed_blocks.push(block);
        }

        if !unreviewed_blocks.is_empty() {
            unreviewed_files.push(UnreviewedFile {
                path: file.path,
                language,
                blocks: unreviewed_blocks,
            });
        }
    }

    // 1. Sort blocks within files
    for file in &mut unreviewed_files {
        file.blocks
            .sort_by_key(|block| (kind_rank(block), block.start_line));
    }

    // 2. Sort files (Files with higher priority blocks come first)
    unreviewed_files.sort_by(|a, b| {
        let rank_fn = |file: &UnreviewedFile| file.blocks.first().map(kind_rank).unwrap_or(100);
        (rank_fn(a), &a.path).cmp(&(rank_fn(b), &b.path))
    });

    Ok(ReviewSummary {
        files: unreviewed_files,
        total_blocks,
        review_state: fingerprint_status,
        tree,
        unreviewed_block_nodes,
    })
}

pub fn collect_unreviewed(
    context: &TrueflowContext,
    options: &ReviewOptions,
    filters: &BlockFilters,
) -> Result<Vec<UnreviewedFile>> {
    Ok(collect_review_summary(context, options, filters)?.files)
}

fn resolve_review_targets(options: &ReviewOptions) -> Result<Option<HashSet<String>>> {
    let targets = normalize_targets(options);
    if targets
        .iter()
        .any(|target| matches!(target, ReviewTarget::All))
    {
        return Ok(None);
    }

    let mut paths = HashSet::new();
    for target in targets {
        match target {
            ReviewTarget::DirtyWorktree => {
                if let Ok(dirty) = get_dirty_files() {
                    paths.extend(dirty);
                }
            }
            ReviewTarget::File(path) => {
                paths.insert(path);
            }
            ReviewTarget::Revision(revision) => {
                paths.extend(vcs::files_changed_in_revision(&revision)?);
            }
            ReviewTarget::RevisionRange { start, end } => {
                paths.extend(vcs::files_changed_in_range(&start, &end)?);
            }
            ReviewTarget::All => {}
        }
    }

    if paths.is_empty() {
        Ok(Some(HashSet::new()))
    } else {
        Ok(Some(paths))
    }
}

fn normalize_targets(options: &ReviewOptions) -> Vec<ReviewTarget> {
    if options.all {
        return vec![ReviewTarget::All];
    }
    if options.targets.is_empty() {
        return vec![ReviewTarget::DirtyWorktree];
    }
    options.targets.clone()
}

fn parse_review_targets(values: &[String]) -> Result<Vec<ReviewTarget>> {
    let mut targets = Vec::new();
    for raw in values {
        if let Some(rest) = raw.strip_prefix("file:") {
            targets.push(ReviewTarget::File(rest.to_string()));
            continue;
        }
        if let Some(rest) = raw.strip_prefix("rev:") {
            if let Some((start, end)) = rest.split_once("..") {
                targets.push(ReviewTarget::RevisionRange {
                    start: start.to_string(),
                    end: end.to_string(),
                });
            } else {
                targets.push(ReviewTarget::Revision(rest.to_string()));
            }
            continue;
        }
        return Err(anyhow!("Unknown review target: {}", raw));
    }
    Ok(targets)
}

pub fn run(
    context: &TrueflowContext,
    json: bool,
    all: bool,
    target: Vec<String>,
    only: Vec<String>,
    exclude: Vec<String>,
) -> Result<()> {
    info!(
        "review start (json={}, all={}, target={:?}, only={:?}, exclude={:?})",
        json, all, target, only, exclude
    );
    let config = load_config()?;
    let filters = config.review.resolve_filters(&only, &exclude);
    let options = ReviewOptions {
        all,
        targets: parse_review_targets(&target)?,
        only,
        exclude,
    };
    let unreviewed_files = collect_unreviewed(context, &options, &filters)?;

    let total_blocks: usize = unreviewed_files.iter().map(|file| file.blocks.len()).sum();
    info!(
        "unreviewed summary (files={}, blocks={})",
        unreviewed_files.len(),
        total_blocks
    );
    if json {
        println!("{}", serde_json::to_string_pretty(&unreviewed_files)?);
    } else if unreviewed_files.is_empty() {
        println!("All clear! No unreviewed blocks found.");
    } else {
        for file in unreviewed_files {
            println!("File: {}", file.path);
            for block in file.blocks {
                println!(
                    "  [Unreviewed] L{}-L{} (Hash: {}) Kind: {}",
                    block.start_line, block.end_line, block.hash, block.kind
                );
                if let Some(first_line) = block.content.lines().next() {
                    println!("    > {}", first_line.trim());
                }
            }
        }
    }

    Ok(())
}

fn get_dirty_files() -> Result<HashSet<String>> {
    vcs::dirty_files_from_workdir()
}

fn approved_hashes_from_status(status: &HashMap<String, Verdict>) -> HashSet<String> {
    status
        .iter()
        .filter_map(|(hash, verdict)| {
            if verdict == &Verdict::Approved {
                Some(hash.clone())
            } else {
                None
            }
        })
        .collect()
}

fn kind_rank(block: &Block) -> u8 {
    if block.tags.iter().any(|tag| tag == "test") {
        return 10;
    }

    match block.kind {
        BlockKind::Struct
        | BlockKind::Enum
        | BlockKind::Type
        | BlockKind::Interface
        | BlockKind::Class => 0,
        BlockKind::FunctionSignature => 20,
        BlockKind::Import | BlockKind::Module | BlockKind::Imports => 25,
        BlockKind::Const | BlockKind::Static => 30,
        BlockKind::Impl => 40,
        BlockKind::Function | BlockKind::Method => 50,
        BlockKind::Gap | BlockKind::Comment => 95,
        _ => 60,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_block(kind: BlockKind, tags: &[&str]) -> Block {
        Block {
            hash: "hash".to_string(),
            content: "content".to_string(),
            kind,
            tags: tags.iter().map(|tag| (*tag).to_string()).collect(),
            complexity: 0,
            start_line: 0,
            end_line: 1,
        }
    }

    #[test]
    fn test_review_priority_ordering() {
        let test_block = make_block(BlockKind::Function, &["test"]);
        let signature_block = make_block(BlockKind::FunctionSignature, &[]);
        let function_block = make_block(BlockKind::Function, &[]);

        let ordered = [
            make_block(BlockKind::Struct, &[]),
            test_block,
            signature_block,
            function_block,
        ];

        for window in ordered.windows(2) {
            let first = kind_rank(&window[0]);
            let second = kind_rank(&window[1]);
            assert!(
                first < second,
                "expected {:?} (rank {}) before {:?} (rank {})",
                window[0].kind,
                first,
                window[1].kind,
                second
            );
        }

        let data_rank = kind_rank(&make_block(BlockKind::Struct, &[]));
        assert_eq!(data_rank, kind_rank(&make_block(BlockKind::Enum, &[])));
        assert_eq!(data_rank, kind_rank(&make_block(BlockKind::Type, &[])));
        assert_eq!(data_rank, kind_rank(&make_block(BlockKind::Interface, &[])));
        assert_eq!(data_rank, kind_rank(&make_block(BlockKind::Class, &[])));
    }
}
