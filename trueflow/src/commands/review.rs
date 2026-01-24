use crate::block::{Block, BlockKind};
use crate::context::TrueflowContext;
use crate::scanner;
use crate::store::{FileStore, ReviewStore, Verdict};
use crate::sub_splitter;
use anyhow::Result;
use git2::Repository;
use log::info;
use serde::Serialize;
use std::collections::HashSet;
use std::path::Path;

#[derive(Serialize)]
pub struct UnreviewedFile {
    pub path: String,
    pub blocks: Vec<Block>,
}

pub fn run(_context: &TrueflowContext, json: bool, all: bool, exclude: Vec<String>) -> Result<()> {
    info!(
        "review start (json={}, all={}, exclude={:?})",
        json, all, exclude
    );
    // 1. Load Approved Hashes
    let store = FileStore::new()?;
    let history = store.read_history()?;
    info!("loaded {} review records", history.len());

    // Compute current status for each fingerprint (Last Write Wins by timestamp)
    let mut sorted_history = history;
    sorted_history.sort_by_key(|record| record.timestamp);

    let mut fingerprint_status = std::collections::HashMap::<String, Verdict>::new();
    for record in sorted_history {
        if record.check == "review" {
            fingerprint_status.insert(record.fingerprint, record.verdict);
        }
    }

    // 2. Scan Directory (Merkle Tree)
    // Filter by git status if !all
    let dirty_files = if !all {
        match get_dirty_files() {
            Ok(s) => Some(s),
            Err(_) => {
                if !json {
                    eprintln!("Warning: Not a git repo, scanning all files.");
                }
                None
            }
        }
    } else {
        None
    };

    let files = scanner::scan_directory(".")?;
    info!("scanned {} files", files.len());

    // 3. Subtraction (Tree Traversal)
    let mut unreviewed_files = Vec::new();
    let exclude_set: HashSet<BlockKind> = exclude
        .iter()
        .filter_map(|value| value.parse::<BlockKind>().ok())
        .collect();

    for file in files {
        // Git Filter
        if let Some(dirty) = &dirty_files {
            let path_normalized = Path::new(&file.path)
                .strip_prefix("./")
                .unwrap_or(Path::new(&file.path));

            let path_str = path_normalized.to_string_lossy();

            if !dirty.contains(path_str.as_ref()) && !dirty.contains(&file.path) {
                continue;
            }
        }

        // Optimization: If the FILE hash is approved, everything inside is approved.
        if fingerprint_status.get(&file.file_hash) == Some(&Verdict::Approved) {
            continue;
        }

        let mut unreviewed_blocks = Vec::new();
        for block in file.blocks {
            if exclude_set.contains(&block.kind) {
                continue;
            }

            // Check status
            if fingerprint_status.get(&block.hash) == Some(&Verdict::Approved) {
                continue;
            }

            if !fingerprint_status.contains_key(&block.hash) {
                // Not explicitly approved. Check implicit approval via sub-blocks.
                let lang = file.language.clone();

                if let Ok(sub_blocks) = sub_splitter::split(&block, lang)
                    && !sub_blocks.is_empty()
                {
                    let all_approved = sub_blocks.iter().all(|sb| {
                        if exclude_set.contains(&sb.kind) {
                            return true;
                        }
                        fingerprint_status.get(&sb.hash) == Some(&Verdict::Approved)
                    });

                    if all_approved {
                        continue;
                    }
                }
            }

            unreviewed_blocks.push(block);
        }

        if !unreviewed_blocks.is_empty() {
            unreviewed_files.push(UnreviewedFile {
                path: file.path,
                blocks: unreviewed_blocks,
            });
        }
    }

    // 1. Sort blocks within files
    for file in &mut unreviewed_files {
        file.blocks.sort_by(|a, b| {
            let rank_a = kind_rank(&a.kind);
            let rank_b = kind_rank(&b.kind);
            match rank_a.cmp(&rank_b) {
                std::cmp::Ordering::Equal => a.start_line.cmp(&b.start_line),
                other => other,
            }
        });
    }

    // 2. Sort files (Files with higher priority blocks come first)
    unreviewed_files.sort_by(|a, b| {
        let min_rank_a = a.blocks.first().map(|b| kind_rank(&b.kind)).unwrap_or(100);
        let min_rank_b = b.blocks.first().map(|b| kind_rank(&b.kind)).unwrap_or(100);
        match min_rank_a.cmp(&min_rank_b) {
            std::cmp::Ordering::Equal => a.path.cmp(&b.path),
            other => other,
        }
    });

    // 4. Output
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
    let repo = Repository::discover(".")?;
    let mut dirty = HashSet::new();

    // StatusOptions defaults include WT_NEW, WT_MODIFIED, INDEX_NEW, INDEX_MODIFIED
    let statuses = repo.statuses(None)?;

    for entry in statuses.iter() {
        if let Some(path) = entry.path() {
            dirty.insert(path.to_string());
        }
    }
    Ok(dirty)
}

fn kind_rank(kind: &BlockKind) -> u8 {
    match kind {
        BlockKind::Import | BlockKind::Module | BlockKind::Imports | BlockKind::Signature => 0,
        BlockKind::Const | BlockKind::Static => 10,
        BlockKind::Struct | BlockKind::Enum | BlockKind::Type | BlockKind::Interface => 20,
        BlockKind::Impl => 30,
        BlockKind::Function | BlockKind::Method => 40,
        BlockKind::Test => 90,
        BlockKind::Gap | BlockKind::Comment => 95,
        _ => 50,
    }
}
