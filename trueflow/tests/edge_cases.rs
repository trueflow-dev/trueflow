use anyhow::{Context, Result};
use std::fs;
use trueflow::scanner::ScanResult;
use trueflow::sub_splitter;

mod common;
use common::*;

#[test]
fn test_binary_file() -> Result<()> {
    let repo = TestRepo::new("binary_file")?;
    let file_path = repo.path.join("binary.bin");
    // Write binary content (null byte)
    fs::write(&file_path, [0, 255, 0, 1])?;

    // Scan
    let output = repo.run(&["scan", "--json"])?;
    let arr = json_array(&output)?;

    let file_obj = arr
        .iter()
        .find(|obj| obj["path"].as_str().unwrap().contains("binary.bin"));
    assert!(file_obj.is_some(), "Binary file should be in output");
    let file_obj = file_obj.unwrap();
    assert_eq!(file_obj["bytes_hash"], file_obj["tree_hash"]);
    assert!(file_obj["bytes_hash"].as_str().is_some());
    assert!(file_obj["blocks"].as_array().unwrap().is_empty());

    Ok(())
}

#[test]
fn test_invalid_utf8() -> Result<()> {
    let repo = TestRepo::new("invalid_utf8")?;
    let file_path = repo.path.join("bad.txt");
    // Invalid UTF-8 sequence (0xFF)
    fs::write(&file_path, [0xFF, 0xFE, 0xFD])?;

    // Scan
    let output = repo.run(&["scan", "--json"])?;
    let arr = json_array(&output)?;
    let scan = json(&output)?;

    let file_obj = arr
        .iter()
        .find(|obj| obj["path"].as_str().unwrap().contains("bad.txt"));
    assert!(file_obj.is_none(), "Invalid UTF-8 file should be skipped");
    let diagnostics = scan["diagnostics"]
        .as_array()
        .context("diagnostics should be array")?;
    assert!(diagnostics.iter().any(|diagnostic| {
        diagnostic["path"].as_str() == Some("bad.txt")
            && diagnostic["reason"]
                .as_str()
                .unwrap_or_default()
                .contains("invalid UTF-8")
    }));
    assert!(scan["cache"]["read"].is_string());
    assert!(scan["cache"]["write"].is_string());

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

    let output = repo.run(&["scan", "--json"])?;
    let arr = json_array(&output)?;

    let file_obj = arr
        .iter()
        .find(|obj| obj["path"].as_str().unwrap().contains("empty.rs"));
    assert!(file_obj.is_some());
    let blocks = file_obj.unwrap()["blocks"].as_array().unwrap();
    assert!(blocks.is_empty());

    Ok(())
}

#[test]
fn test_scan_reports_structured_elisp_blocks_without_fallback_diagnostic() -> Result<()> {
    let repo = TestRepo::new("structured_elisp")?;
    repo.write(
        "main.el",
        "(require 'cl-lib)\n\n(defun greet ()\n  (message \"hi\"))\n",
    )?;

    let output = repo.run(&["scan", "--json"])?;
    let scan_result: ScanResult = serde_json::from_str(&output)?;
    let file_state = scan_result
        .files
        .iter()
        .find(|file| file.path.as_str() == "main.el")
        .context("missing scan output for main.el")?;

    assert_eq!(file_state.language, trueflow::analysis::Language::Elisp);
    assert!(!file_state.blocks.is_empty());
    assert!(!scan_result.diagnostics.iter().any(|diagnostic| {
        diagnostic.path.as_ref().map(|path| path.as_str()) == Some("main.el")
            && diagnostic.reason.contains("unsupported language")
    }));

    Ok(())
}

#[test]
fn test_unknown_code_extension_falls_back_to_text_and_review_still_works() -> Result<()> {
    let repo = TestRepo::new("unknown_code_extension")?;
    repo.write("main.bf", "++++[>++++<-]>+.\n\n[-]\n")?;

    let output = repo.run(&["scan", "--json"])?;
    let scan_result: ScanResult = serde_json::from_str(&output)?;
    let file_state = scan_result
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
        !scan_result.diagnostics.iter().any(|diagnostic| {
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

    let output = repo.run(&["scan", "--json"])?;
    let scan_result: ScanResult = serde_json::from_str(&output)?;
    let file_states = scan_result.files;

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
