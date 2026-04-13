use crate::config::load as load_config;
use crate::context::TrueflowContext;
use crate::coverage::{
    BindingRelation, CoverageBuildOptions, CoverageDiagnostic, CoverageIndex, CoveragePolicy,
    CoverageScope,
};
use crate::path_utils;
use crate::repo_path::RepoPath;
use crate::scanner;
use crate::store::{FileStore, Record, ReviewCheck, ReviewStore, Verdict};
use crate::sub_splitter;
use crate::tree;
use crate::vcs;
use anyhow::{Context, Result, bail};
use serde::Serialize;
use std::collections::{BTreeMap, BTreeSet, HashSet};

#[derive(Serialize)]
struct InspectCoverageRecord {
    record: Record,
    binding_relation: Option<BindingRelation>,
}

#[derive(Serialize)]
struct InspectCheckCoverage {
    direct_latest_verdict: Option<Verdict>,
    effective_latest_verdict: Option<Verdict>,
    direct_identity_count: usize,
    effective_identity_count: usize,
}

#[derive(Serialize)]
struct InspectPolicyCoverage {
    single_review_direct: bool,
    single_review_effective: bool,
    two_person_review_direct: bool,
    two_person_review_effective: bool,
}

#[derive(Serialize)]
struct InspectSubtreeCoverage {
    descendant_block_count: usize,
    all_descendants_single_review_direct: bool,
    all_descendants_single_review_effective: bool,
    all_descendants_two_person_review_direct: bool,
    all_descendants_two_person_review_effective: bool,
}

#[derive(Serialize)]
struct InspectCoverageSummary {
    resolved_as_tree_node: bool,
    direct_reviews: Vec<InspectCoverageRecord>,
    linked_reviews: Vec<InspectCoverageRecord>,
    effective_reviews: Vec<InspectCoverageRecord>,
    checks: BTreeMap<String, InspectCheckCoverage>,
    policies: InspectPolicyCoverage,
    subtree: Option<InspectSubtreeCoverage>,
    diagnostics: Vec<CoverageDiagnostic>,
}

#[derive(Serialize)]
struct InspectCoverageOutput {
    block: crate::block::Block,
    coverage: InspectCoverageSummary,
}

#[derive(Clone)]
struct MatchedBlock {
    block: crate::block::Block,
    language: crate::analysis::Language,
    path: RepoPath,
}

pub fn run(
    _context: &TrueflowContext,
    fingerprint: &str,
    split: bool,
    coverage: bool,
) -> Result<()> {
    let config = load_config()?;
    let scan_options = config.scan.resolve_options()?;
    let scan_result = scanner::scan_directory(".", &scan_options)?;
    let files = scan_result.files;
    let matched = find_matching_block(&files, fingerprint)?;
    if split {
        let sub_blocks = sub_splitter::split(&matched.block, matched.language)?;
        if coverage {
            let (tree, database, workdir_prefix) = load_coverage_inputs(&files)?;
            let coverage_index =
                CoverageIndex::build(&tree, &database, &CoverageBuildOptions { workdir_prefix })?;
            let outputs = sub_blocks
                .into_iter()
                .map(|block| InspectCoverageOutput {
                    coverage: build_coverage_summary(&coverage_index, &matched.path, &block),
                    block,
                })
                .collect::<Vec<_>>();
            println!("{}", serde_json::to_string_pretty(&outputs)?);
        } else {
            println!("{}", serde_json::to_string_pretty(&sub_blocks)?);
        }
    } else if coverage {
        let (tree, database, workdir_prefix) = load_coverage_inputs(&files)?;
        let coverage_index =
            CoverageIndex::build(&tree, &database, &CoverageBuildOptions { workdir_prefix })?;
        let output = InspectCoverageOutput {
            coverage: build_coverage_summary(&coverage_index, &matched.path, &matched.block),
            block: matched.block,
        };
        println!("{}", serde_json::to_string_pretty(&output)?);
    } else {
        println!("{}", serde_json::to_string_pretty(&matched.block)?);
    }

    Ok(())
}

fn find_matching_block(
    files: &[crate::block::FileState],
    fingerprint: &str,
) -> Result<MatchedBlock> {
    let mut matches = Vec::new();

    for file in files {
        for block in &file.blocks {
            if block.hash.as_str().starts_with(fingerprint) {
                matches.push(MatchedBlock {
                    block: block.clone(),
                    language: file.language,
                    path: file.path.clone(),
                });
            }
        }
    }

    if matches.is_empty() {
        for file in files {
            for block in &file.blocks {
                if let Ok(sub_blocks) = sub_splitter::split(block, file.language) {
                    for sub_block in sub_blocks {
                        if sub_block.hash.as_str().starts_with(fingerprint) {
                            matches.push(MatchedBlock {
                                block: sub_block,
                                language: file.language,
                                path: file.path.clone(),
                            });
                        }
                    }
                }
            }
        }
    }

    if matches.is_empty() {
        bail!("Block not found");
    }
    if matches.len() > 1 {
        bail!(
            "Multiple blocks matched fingerprint ({} matches). Use a longer prefix.",
            matches.len()
        );
    }

    matches.pop().context("Block not found")
}

fn load_coverage_inputs(
    files: &[crate::block::FileState],
) -> Result<(tree::Tree, crate::store::ReviewDatabase, Option<String>)> {
    let tree = tree::build_tree_from_files(files);
    let store = FileStore::new()?;
    let database = store.load_database()?;
    let workdir_prefix = workdir_prefix_from_git_root();
    Ok((tree, database, workdir_prefix))
}

fn build_coverage_summary(
    coverage: &CoverageIndex<'_>,
    path: &RepoPath,
    block: &crate::block::Block,
) -> InspectCoverageSummary {
    let block_coverage = coverage.block(path, block);
    let direct_reviews = block_coverage.direct_records();
    let linked_reviews = block_coverage.linked_records();
    let effective_reviews = block_coverage.effective_records();

    let mut checks = BTreeSet::new();
    for record in linked_reviews.iter().chain(effective_reviews.iter()) {
        checks.insert(record.check.as_str().to_string());
    }
    if checks.is_empty() {
        checks.insert(ReviewCheck::review().as_str().to_string());
    }

    let mut check_summaries = BTreeMap::new();
    for check_name in checks {
        let check = ReviewCheck::new(&check_name)
            .unwrap_or_else(|error| panic!("inspect check should be valid: {error}"));
        check_summaries.insert(
            check_name,
            InspectCheckCoverage {
                direct_latest_verdict: block_coverage.direct_latest_verdict_for(&check).cloned(),
                effective_latest_verdict: block_coverage
                    .effective_latest_verdict_for(&check)
                    .cloned(),
                direct_identity_count: block_coverage.direct_distinct_identity_count(&check),
                effective_identity_count: block_coverage.effective_distinct_identity_count(&check),
            },
        );
    }

    let single_direct = CoveragePolicy::single_review();
    let single_effective = CoveragePolicy::single_review().with_scope(CoverageScope::Effective);
    let two_direct = CoveragePolicy::two_person_review();
    let two_effective = CoveragePolicy::two_person_review().with_scope(CoverageScope::Effective);

    let related_record_ids = linked_reviews
        .iter()
        .map(|record| record.id.clone())
        .collect::<HashSet<_>>();
    let diagnostics = coverage
        .diagnostics()
        .iter()
        .filter(|diagnostic| {
            diagnostic_record_id(diagnostic).is_some_and(|id| related_record_ids.contains(id))
        })
        .cloned()
        .collect::<Vec<_>>();

    let subtree = block_coverage.resolved_node_id().and_then(|node_id| {
        if !coverage.is_container_block_node(node_id) {
            return None;
        }

        let subtree = coverage.subtree(node_id);
        Some(InspectSubtreeCoverage {
            descendant_block_count: subtree.descendant_block_nodes().len(),
            all_descendants_single_review_direct: subtree
                .all_descendant_blocks_well_reviewed(&single_direct),
            all_descendants_single_review_effective: subtree
                .all_descendant_blocks_well_reviewed(&single_effective),
            all_descendants_two_person_review_direct: subtree
                .all_descendant_blocks_well_reviewed(&two_direct),
            all_descendants_two_person_review_effective: subtree
                .all_descendant_blocks_well_reviewed(&two_effective),
        })
    });

    InspectCoverageSummary {
        resolved_as_tree_node: block_coverage.resolved_node_id().is_some(),
        direct_reviews: direct_reviews
            .into_iter()
            .map(|record| InspectCoverageRecord {
                binding_relation: coverage.binding_relation_for_record(&record.id),
                record: record.clone(),
            })
            .collect(),
        linked_reviews: linked_reviews
            .into_iter()
            .map(|record| InspectCoverageRecord {
                binding_relation: coverage.binding_relation_for_record(&record.id),
                record: record.clone(),
            })
            .collect(),
        effective_reviews: effective_reviews
            .into_iter()
            .map(|record| InspectCoverageRecord {
                binding_relation: coverage.binding_relation_for_record(&record.id),
                record: record.clone(),
            })
            .collect(),
        checks: check_summaries,
        policies: InspectPolicyCoverage {
            single_review_direct: block_coverage.is_well_reviewed(&single_direct),
            single_review_effective: block_coverage.is_well_reviewed(&single_effective),
            two_person_review_direct: block_coverage.is_well_reviewed(&two_direct),
            two_person_review_effective: block_coverage.is_well_reviewed(&two_effective),
        },
        subtree,
        diagnostics,
    }
}

fn diagnostic_record_id(diagnostic: &CoverageDiagnostic) -> Option<&String> {
    match diagnostic {
        CoverageDiagnostic::AmbiguousRecord { record_id, .. }
        | CoverageDiagnostic::UnresolvedRecord { record_id } => Some(record_id),
    }
}

fn workdir_prefix_from_git_root() -> Option<String> {
    let repo_root = vcs::git_root_from_workdir().ok().flatten()?;
    path_utils::current_workdir_prefix_for_repo_root(&repo_root)
}
