use anyhow::{Context, Result};
use std::fs;
use trueflow::sub_splitter;

use trueflow_test_support::*;

const DEEP_NESTING_DEPTH: usize = 16_000;

#[test]
fn test_binary_file() -> Result<()> {
    let repo = TestRepo::new("binary_file")?;
    let file_path = repo.path.join("binary.bin");
    // Write binary content (null byte)
    fs::write(&file_path, [0, 255, 0, 1])?;

    let scan = repo.scan_without_cache()?;

    let file_obj = scan
        .files
        .iter()
        .find(|file| file.path.as_str().contains("binary.bin"));
    assert!(file_obj.is_some(), "Binary file should be in output");
    let file_obj = file_obj.unwrap();
    assert_eq!(
        file_obj.bytes_hash.to_string(),
        file_obj.tree_hash.to_string()
    );
    assert!(file_obj.blocks.is_empty());

    Ok(())
}

#[test]
fn test_invalid_utf8() -> Result<()> {
    let repo = TestRepo::new("invalid_utf8")?;
    let file_path = repo.path.join("bad.txt");
    // Invalid UTF-8 sequence (0xFF)
    fs::write(&file_path, [0xFF, 0xFE, 0xFD])?;

    let scan = repo.scan_without_cache()?;

    let file_obj = scan
        .files
        .iter()
        .find(|file| file.path.as_str().contains("bad.txt"));
    assert!(file_obj.is_none(), "Invalid UTF-8 file should be skipped");
    assert!(scan.diagnostics.iter().any(|diagnostic| {
        diagnostic.path.as_ref().map(|path| path.as_str()) == Some("bad.txt")
            && diagnostic.reason.contains("invalid UTF-8")
    }));
    assert_eq!(
        scan.cache.read,
        trueflow::scanner::ScanCacheReadStatus::Disabled
    );
    assert_eq!(
        scan.cache.write,
        trueflow::scanner::ScanCacheWriteStatus::Disabled
    );

    Ok(())
}

#[test]
fn test_review_warns_on_skipped_invalid_utf8() -> Result<()> {
    let repo = TestRepo::new("review_warns_invalid_utf8")?;
    repo.write("src/main.rs", "fn main() {}\n")?;
    fs::write(repo.path.join("bad.txt"), [0xFF, 0xFE, 0xFD])?;

    let output = repo.run_raw(&["review", "--all", "--json"])?;
    assert!(output.status.success(), "review failed unexpectedly");
    let stderr = String::from_utf8(output.stderr)?;
    assert!(
        stderr.contains("warning:"),
        "expected warning in stderr: {stderr}"
    );
    assert!(
        stderr.contains("bad.txt"),
        "expected bad file path in stderr: {stderr}"
    );
    assert!(
        stderr.contains("invalid UTF-8"),
        "expected invalid UTF-8 reason in stderr: {stderr}"
    );

    Ok(())
}

#[test]
fn test_empty_file() -> Result<()> {
    let repo = TestRepo::new("empty_file")?;
    let file_path = repo.path.join("empty.rs");
    fs::write(&file_path, "")?;

    let scan = repo.scan_without_cache()?;

    let file_obj = scan
        .files
        .iter()
        .find(|file| file.path.as_str().contains("empty.rs"));
    assert!(file_obj.is_some());
    assert!(file_obj.unwrap().blocks.is_empty());

    Ok(())
}

fn scan_deeply_nested_file_without_stack_overflow(
    repo_name: &str,
    path: &str,
    content: &str,
) -> Result<()> {
    let repo = TestRepo::new(repo_name)?;
    repo.write(path, content)?;

    let scan = repo.scan_without_cache()?;

    assert_eq!(scan.files.len(), 1);
    assert_eq!(scan.files[0].path.as_str(), path);
    Ok(())
}

#[test]
fn test_scan_handles_deeply_nested_dart_without_stack_overflow() -> Result<()> {
    let expression = nested_parenthesized_expression(DEEP_NESTING_DEPTH, "1");
    scan_deeply_nested_file_without_stack_overflow(
        "deeply_nested_dart",
        "deep.dart",
        &format!("class A {{\n  var x = {expression};\n}}\n"),
    )
}

#[test]
fn test_scan_handles_deeply_nested_clojure_without_stack_overflow() -> Result<()> {
    let expression = nested_parenthesized_expression(DEEP_NESTING_DEPTH, "1");
    scan_deeply_nested_file_without_stack_overflow(
        "deeply_nested_clojure",
        "deep.clj",
        &format!("(defn deep [] {expression})\n"),
    )
}

#[test]
fn test_scan_handles_deeply_nested_go_without_stack_overflow() -> Result<()> {
    let expression = nested_parenthesized_expression(DEEP_NESTING_DEPTH, "1");
    scan_deeply_nested_file_without_stack_overflow(
        "deeply_nested_go",
        "deep.go",
        &format!("package main\nfunc main() {{ var x = {expression}; _ = x }}\n"),
    )
}

#[test]
fn test_scan_handles_deeply_nested_cpp_without_stack_overflow() -> Result<()> {
    let expression = nested_parenthesized_expression(DEEP_NESTING_DEPTH, "1");
    scan_deeply_nested_file_without_stack_overflow(
        "deeply_nested_cpp",
        "deep.cpp",
        &format!("int main() {{ auto x = {expression}; return x; }}\n"),
    )
}

#[test]
fn test_scan_handles_deeply_nested_nix_without_stack_overflow() -> Result<()> {
    scan_deeply_nested_file_without_stack_overflow(
        "deeply_nested_nix",
        "deep.nix",
        &nested_nix_functions(DEEP_NESTING_DEPTH),
    )
}

fn nested_parenthesized_expression(depth: usize, atom: &str) -> String {
    let mut expression = String::with_capacity(atom.len().saturating_add(depth.saturating_mul(2)));
    expression.push_str(&"(".repeat(depth));
    expression.push_str(atom);
    expression.push_str(&")".repeat(depth));
    expression
}

fn nested_nix_functions(depth: usize) -> String {
    let mut expression = String::new();
    for index in 0..depth {
        expression.push_str(&format!("arg{index}: "));
    }
    expression.push_str("1\n");
    expression
}

fn nested_nix_attrset_function(depth: usize) -> String {
    let mut expression = String::from("{ value = ");
    for index in 0..depth {
        expression.push_str(&format!("arg{index}: "));
    }
    expression.push_str("1; }\n");
    expression
}

#[test]
fn test_review_handles_deeply_nested_nix_subblocks_without_stack_overflow() -> Result<()> {
    let repo = TestRepo::new("deeply_nested_nix_review")?;
    repo.write("default.nix", &nested_nix_attrset_function(16_000))?;

    let output = repo.run_raw(&["review", "--all", "--json"])?;
    assert!(
        output.status.success(),
        "review should not abort on deep Nix subblocks; stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    Ok(())
}

#[test]
fn test_scan_reports_structured_elisp_blocks_without_fallback_diagnostic() -> Result<()> {
    let repo = TestRepo::new("structured_elisp")?;
    repo.write(
        "main.el",
        "(require 'cl-lib)\n\n(defun greet ()\n  (message \"hi\"))\n",
    )?;

    let scan = repo.scan_without_cache()?;
    let file_state = scan
        .files
        .iter()
        .find(|file| file.path.as_str() == "main.el")
        .context("missing scan output for main.el")?;

    assert_eq!(file_state.language, trueflow::analysis::Language::Elisp);
    assert!(!file_state.blocks.is_empty());
    assert!(!scan.diagnostics.iter().any(|diagnostic| {
        diagnostic.path.as_ref().map(|path| path.as_str()) == Some("main.el")
            && diagnostic.reason.contains("unsupported language")
    }));

    Ok(())
}

#[test]
fn test_unknown_code_extension_falls_back_to_text_and_review_still_works() -> Result<()> {
    let repo = TestRepo::new("unknown_code_extension")?;
    repo.write("main.bf", "++++[>++++<-]>+.\n\n[-]\n")?;

    let scan = repo.scan_without_cache()?;
    let file_state = scan
        .files
        .iter()
        .find(|file| file.path.as_str() == "main.bf")
        .context("missing scan output for main.bf")?;

    assert_eq!(file_state.language, trueflow::analysis::Language::Text);
    assert!(!file_state.blocks.is_empty());
    assert!(
        file_state
            .blocks
            .iter()
            .any(|block| block.kind == trueflow::block::BlockKind::Paragraph)
    );
    assert!(
        !scan.diagnostics.iter().any(|diagnostic| {
            diagnostic.path.as_ref().map(|path| path.as_str()) == Some("main.bf")
        }),
        "did not expect fallback diagnostics for unknown text-classified files"
    );

    let review_output = repo.run(&["review", "--all", "--json"])?;
    let review_files = json_array(&review_output)?;
    let review_file = review_files
        .iter()
        .find(|file| file["path"].as_str() == Some("main.bf"))
        .context("missing review output for main.bf")?;
    let review_blocks = review_file["blocks"].as_array().context("blocks")?;

    assert!(!review_blocks.is_empty());
    assert!(
        review_blocks
            .iter()
            .any(|block| block["kind"].as_str() == Some("Paragraph"))
    );

    Ok(())
}

#[test]
fn test_sub_splitter_avoids_empty_blocks() -> Result<()> {
    let repo = TestRepo::new("sub_splitter_empty")?;
    let test_cases = [
        (
            "leading_newlines.rs",
            "\n\n\nfn main() {\n    println!(\"hi\");\n}\n",
        ),
        (
            "comment_gaps.rs",
            "// leading comment\n\n\nfn handler() {\n    // inner\n\n    action();\n}\n",
        ),
        (
            "attribute_gap.rs",
            "\n\n#[test]\nfn it_works() {\n    assert!(true);\n}\n",
        ),
    ];

    for &(name, content) in &test_cases {
        let file_path = repo.path.join(name);
        fs::write(&file_path, content)?;
    }

    let scan = repo.scan_without_cache()?;
    let file_states = scan.files;

    for &(name, _) in &test_cases {
        let file_state = file_states
            .iter()
            .find(|file| file.path.as_str().ends_with(name))
            .unwrap_or_else(|| panic!("missing scan output for {name}"));

        assert!(
            !file_state.blocks.is_empty(),
            "expected blocks for {}",
            file_state.path
        );

        for block in &file_state.blocks {
            assert!(
                !block.content.is_empty(),
                "empty block in {} for {}",
                file_state.path,
                block.kind
            );
            let sub_blocks = sub_splitter::split(block, file_state.language)?;
            assert!(
                !sub_blocks.is_empty(),
                "expected sub-blocks for {} block {}",
                file_state.path,
                block.kind
            );
            for sub_block in &sub_blocks {
                assert!(
                    !sub_block.content.is_empty(),
                    "empty sub-block in {} for {}",
                    file_state.path,
                    sub_block.kind
                );
            }
        }
    }

    Ok(())
}
