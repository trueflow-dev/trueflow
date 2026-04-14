use anyhow::{Context, Result};
use std::path::PathBuf;

mod common;
use common::*;

use trueflow::analysis::Language;
use trueflow::block::BlockKind;
use trueflow::block_splitter::{self, BlockSplitStrategy};
use trueflow::sub_splitter::{self, SubSplitSemantics};

#[test]
fn test_nix_fixture_scan_detects_stable_structural_blocks() -> Result<()> {
    let repo = TestRepo::fixture("nix_support")?;

    let output = repo.run(&["scan", "--json"])?;
    let files = json_array(&output)?;

    let default_nix = files
        .iter()
        .find(|file| file["path"].as_str() == Some("default.nix"))
        .context("missing scan output for default.nix")?;
    assert_eq!(default_nix["language"].as_str(), Some("Nix"));

    let blocks = default_nix["blocks"]
        .as_array()
        .context("default.nix blocks should be array")?;
    let kinds = block_kinds_without_gaps(blocks);

    assert!(
        kinds.contains(&"FunctionSignature"),
        "expected file-level function signature block: {kinds:?}"
    );
    assert!(
        kinds.iter().filter(|kind| **kind == "variable").count() >= 3,
        "expected multiple reviewable variable bindings: {kinds:?}"
    );
    assert!(
        !kinds.contains(&"Paragraph"),
        "did not expect textual fallback blocks: {kinds:?}"
    );

    Ok(())
}

#[test]
fn test_nix_sub_block_review_supports_nested_attrsets_lists_functions_and_if_branches() -> Result<()>
{
    let repo_root = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let file_path = repo_root.join("example_repos/nix_support/default.nix");
    let content = std::fs::read_to_string(&file_path)?;

    let split = block_splitter::split(&content, Language::Nix);
    assert_eq!(split.strategy, BlockSplitStrategy::Structured);
    let blocks = split.blocks;

    let defaults_block = blocks
        .iter()
        .find(|block| block.kind == BlockKind::Variable && block.content.contains("defaults = {"))
        .cloned()
        .context("expected defaults binding block")?;
    let defaults_result = sub_splitter::split_result(&defaults_block, Language::Nix)?;
    assert_eq!(
        defaults_result.semantics,
        SubSplitSemantics::StructuralChildren
    );
    let defaults_children = defaults_result
        .blocks
        .iter()
        .filter(|block| block.kind != BlockKind::Gap)
        .collect::<Vec<_>>();
    assert!(
        defaults_children
            .first()
            .is_some_and(|block| block.kind == BlockKind::Preamble),
        "expected defaults binding preamble: {defaults_children:#?}"
    );
    assert!(
        defaults_children.iter().any(
            |block| block.kind == BlockKind::Variable && block.content.contains("retries = 3;")
        ),
        "expected scalar binding child in defaults split: {defaults_children:#?}"
    );
    assert!(
        defaults_children
            .iter()
            .any(|block| block.kind == BlockKind::Variable && block.content.contains("labels = {")),
        "expected nested attrset binding child in defaults split: {defaults_children:#?}"
    );
    assert!(
        defaults_children
            .iter()
            .any(|block| block.kind == BlockKind::Comment
                && block.content.contains("keep monitored packages")),
        "expected comment-preserving child in defaults split: {defaults_children:#?}"
    );

    let labels_binding = defaults_children
        .iter()
        .find(|block| block.kind == BlockKind::Variable && block.content.contains("labels = {"))
        .map(|block| (*block).clone())
        .context("expected labels binding child")?;
    let labels_result = sub_splitter::split_result(&labels_binding, Language::Nix)?;
    assert_eq!(
        labels_result.semantics,
        SubSplitSemantics::StructuralChildren
    );
    let label_children = labels_result
        .blocks
        .iter()
        .filter(|block| block.kind != BlockKind::Gap)
        .collect::<Vec<_>>();
    assert!(
        label_children
            .iter()
            .any(|block| block.kind == BlockKind::Variable
                && block.content.contains("tier = \"backend\";")),
        "expected tier child binding: {label_children:#?}"
    );
    assert!(
        label_children
            .iter()
            .any(|block| block.kind == BlockKind::Variable
                && block.content.contains("oncall = \"platform\";")),
        "expected oncall child binding: {label_children:#?}"
    );

    let packages_binding = defaults_children
        .iter()
        .find(|block| block.kind == BlockKind::Variable && block.content.contains("packages = ["))
        .map(|block| (*block).clone())
        .context("expected packages binding child")?;
    let packages_result = sub_splitter::split_result(&packages_binding, Language::Nix)?;
    assert_eq!(
        packages_result.semantics,
        SubSplitSemantics::StructuralChildren
    );
    let package_children = packages_result
        .blocks
        .iter()
        .filter(|block| block.kind != BlockKind::Gap)
        .collect::<Vec<_>>();
    assert!(
        package_children
            .iter()
            .any(|block| block.kind == BlockKind::Content && block.content.contains("pkgs.git")),
        "expected scalar list element child: {package_children:#?}"
    );
    assert!(
        package_children
            .iter()
            .any(|block| block.kind == BlockKind::Section
                && block.content.contains("name = \"helper\"")),
        "expected attrset list element child: {package_children:#?}"
    );

    let package_attrset = package_children
        .iter()
        .find(|block| {
            block.kind == BlockKind::Section && block.content.contains("name = \"helper\"")
        })
        .map(|block| (*block).clone())
        .context("expected package attrset child")?;
    let package_attrset_result = sub_splitter::split_result(&package_attrset, Language::Nix)?;
    assert_eq!(
        package_attrset_result.semantics,
        SubSplitSemantics::StructuralChildren
    );
    let package_attrset_children = package_attrset_result
        .blocks
        .iter()
        .filter(|block| block.kind != BlockKind::Gap)
        .collect::<Vec<_>>();
    assert!(
        package_attrset_children
            .iter()
            .any(|block| block.kind == BlockKind::Variable
                && block.content.contains("name = \"helper\";")),
        "expected name binding child in package attrset: {package_attrset_children:#?}"
    );
    assert!(
        package_attrset_children
            .iter()
            .any(|block| block.kind == BlockKind::Variable
                && block.content.contains("enabled = true;")),
        "expected enabled binding child in package attrset: {package_attrset_children:#?}"
    );

    let mk_worker_block = blocks
        .iter()
        .find(|block| {
            block.kind == BlockKind::Variable && block.content.contains("mkWorker = name:")
        })
        .cloned()
        .context("expected mkWorker binding block")?;
    let mk_worker_result = sub_splitter::split_result(&mk_worker_block, Language::Nix)?;
    assert_eq!(
        mk_worker_result.semantics,
        SubSplitSemantics::StructuralChildren
    );
    let mk_worker_children = mk_worker_result
        .blocks
        .iter()
        .filter(|block| block.kind != BlockKind::Gap)
        .collect::<Vec<_>>();
    assert!(
        mk_worker_children
            .iter()
            .filter(|block| block.kind == BlockKind::FunctionSignature)
            .count()
            == 1
            && mk_worker_children.iter().any(|block| {
                block.kind == BlockKind::FunctionSignature
                    && block.content.contains("mkWorker = name:")
            }),
        "expected one combined function signature child for mkWorker: {mk_worker_children:#?}"
    );
    assert!(
        mk_worker_children
            .iter()
            .any(|block| block.kind == BlockKind::Variable
                && block.content.contains("packageSet = with pkgs; [")),
        "expected let binding child for mkWorker: {mk_worker_children:#?}"
    );
    assert!(
        mk_worker_children
            .iter()
            .any(|block| block.kind == BlockKind::Section
                && block.content.contains("inherit name packageSet;")),
        "expected attrset body child for mkWorker: {mk_worker_children:#?}"
    );
    assert!(
        mk_worker_children
            .iter()
            .any(|block| block.kind == BlockKind::Comment
                && block.content.contains("build package set")),
        "expected comment-preserving child for mkWorker: {mk_worker_children:#?}"
    );

    let package_set_binding = mk_worker_children
        .iter()
        .find(|block| {
            block.kind == BlockKind::Variable && block.content.contains("packageSet = with pkgs; [")
        })
        .map(|block| (*block).clone())
        .context("expected packageSet binding child")?;
    let package_set_result = sub_splitter::split_result(&package_set_binding, Language::Nix)?;
    assert_eq!(
        package_set_result.semantics,
        SubSplitSemantics::StructuralChildren
    );
    let package_set_children = package_set_result
        .blocks
        .iter()
        .filter(|block| block.kind != BlockKind::Gap)
        .collect::<Vec<_>>();
    assert!(
        package_set_children
            .iter()
            .any(|block| block.kind == BlockKind::Preamble && block.content.contains("with pkgs;")),
        "expected with-expression preamble child: {package_set_children:#?}"
    );
    assert!(
        package_set_children
            .iter()
            .any(|block| block.kind == BlockKind::List && block.content.contains("git")),
        "expected packageSet list child: {package_set_children:#?}"
    );

    let mk_worker_body = mk_worker_children
        .iter()
        .find(|block| {
            block.kind == BlockKind::Section && block.content.contains("inherit name packageSet;")
        })
        .map(|block| (*block).clone())
        .context("expected mkWorker body attrset child")?;
    let mk_worker_body_result = sub_splitter::split_result(&mk_worker_body, Language::Nix)?;
    assert_eq!(
        mk_worker_body_result.semantics,
        SubSplitSemantics::StructuralChildren
    );
    let mk_worker_body_children = mk_worker_body_result
        .blocks
        .iter()
        .filter(|block| block.kind != BlockKind::Gap)
        .collect::<Vec<_>>();
    assert!(
        mk_worker_body_children
            .iter()
            .any(|block| block.kind == BlockKind::Import
                && block.content.contains("inherit name packageSet;")),
        "expected inherit child in mkWorker body: {mk_worker_body_children:#?}"
    );
    assert!(
        mk_worker_body_children
            .iter()
            .any(|block| block.kind == BlockKind::Variable
                && block.content.contains("meta = assert name != \"\"; {")),
        "expected meta binding child in mkWorker body: {mk_worker_body_children:#?}"
    );

    let meta_binding = mk_worker_body_children
        .iter()
        .find(|block| {
            block.kind == BlockKind::Variable
                && block.content.contains("meta = assert name != \"\"; {")
        })
        .map(|block| (*block).clone())
        .context("expected meta binding child")?;
    let meta_result = sub_splitter::split_result(&meta_binding, Language::Nix)?;
    assert_eq!(meta_result.semantics, SubSplitSemantics::StructuralChildren);
    let meta_children = meta_result
        .blocks
        .iter()
        .filter(|block| block.kind != BlockKind::Gap)
        .collect::<Vec<_>>();
    assert!(
        meta_children
            .iter()
            .any(|block| block.kind == BlockKind::Preamble
                && block.content.contains("assert name != \"\";")),
        "expected assert preamble child in meta binding: {meta_children:#?}"
    );
    assert!(
        meta_children
            .iter()
            .any(|block| block.kind == BlockKind::Section
                && block.content.contains("role = \"worker\";")),
        "expected attrset child in meta binding: {meta_children:#?}"
    );

    let selected_block = blocks
        .iter()
        .find(|block| {
            block.kind == BlockKind::Variable
                && block.content.contains("selected = if pkgs.stdenv.isLinux")
        })
        .cloned()
        .context("expected selected binding block")?;
    let selected_result = sub_splitter::split_result(&selected_block, Language::Nix)?;
    assert_eq!(
        selected_result.semantics,
        SubSplitSemantics::StructuralChildren
    );
    let selected_children = selected_result
        .blocks
        .iter()
        .filter(|block| block.kind != BlockKind::Gap)
        .collect::<Vec<_>>();
    assert!(
        selected_children
            .iter()
            .any(|block| block.kind == BlockKind::Preamble && block.content.contains("selected = "))
            && selected_children.iter().any(|block| {
                block.kind == BlockKind::Preamble
                    && block.content.contains("if pkgs.stdenv.isLinux then")
            }),
        "expected binding + if preamble children: {selected_children:#?}"
    );
    assert!(
        selected_children
            .iter()
            .filter(|block| block.kind == BlockKind::Section)
            .count()
            >= 2,
        "expected structural then/else branch children: {selected_children:#?}"
    );

    Ok(())
}
