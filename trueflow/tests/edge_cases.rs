use anyhow::Result;
use std::fs;
use std::path::PathBuf;
use std::process::Command;
use trueflow::block::FileState;
use trueflow::sub_splitter;
use uuid::Uuid;

struct TestEnv {
    root: PathBuf,
}

impl TestEnv {
    fn new() -> Result<Self> {
        let temp_dir = std::env::temp_dir()
            .join("trueflow_test_edge")
            .join(Uuid::new_v4().to_string());
        if temp_dir.exists() {
            fs::remove_dir_all(&temp_dir)?;
        }
        fs::create_dir_all(&temp_dir)?;
        Command::new("git")
            .arg("init")
            .current_dir(&temp_dir)
            .output()?;
        Ok(Self { root: temp_dir })
    }

    fn run_trueflow(&self, args: &[&str]) -> Result<String> {
        let bin = env!("CARGO_BIN_EXE_trueflow");
        let output = Command::new(bin)
            .args(args)
            .current_dir(&self.root)
            .output()?;

        if !output.status.success() {
            return Err(anyhow::anyhow!(
                "trueflow failed: {}",
                String::from_utf8_lossy(&output.stderr)
            ));
        }
        Ok(String::from_utf8(output.stdout)?)
    }
}

#[test]
fn test_binary_file() -> Result<()> {
    let env = TestEnv::new()?;
    let file_path = env.root.join("binary.bin");
    // Write binary content (null byte)
    fs::write(&file_path, [0, 255, 0, 1])?;

    // Scan
    let output = env.run_trueflow(&["scan", "--json"])?;
    let json: serde_json::Value = serde_json::from_str(&output)?;
    let arr = json.as_array().expect("Array");

    let file_obj = arr
        .iter()
        .find(|obj| obj["path"].as_str().unwrap().contains("binary.bin"));
    assert!(file_obj.is_some(), "Binary file should be in output");
    let file_obj = file_obj.unwrap();
    assert_eq!(file_obj["file_hash"], "binary_skipped");
    assert!(file_obj["blocks"].as_array().unwrap().is_empty());

    Ok(())
}

#[test]
fn test_invalid_utf8() -> Result<()> {
    let env = TestEnv::new()?;
    let file_path = env.root.join("bad.txt");
    // Invalid UTF-8 sequence (0xFF)
    fs::write(&file_path, [0xFF, 0xFE, 0xFD])?;

    // Scan
    let output = env.run_trueflow(&["scan", "--json"])?;
    let json: serde_json::Value = serde_json::from_str(&output)?;
    let arr = json.as_array().expect("Array");

    let file_obj = arr
        .iter()
        .find(|obj| obj["path"].as_str().unwrap().contains("bad.txt"));
    assert!(file_obj.is_none(), "Invalid UTF-8 file should be skipped");

    Ok(())
}

#[test]
fn test_empty_file() -> Result<()> {
    let env = TestEnv::new()?;
    let file_path = env.root.join("empty.rs");
    fs::write(&file_path, "")?;

    let output = env.run_trueflow(&["scan", "--json"])?;
    let json: serde_json::Value = serde_json::from_str(&output)?;
    let arr = json.as_array().unwrap();

    let file_obj = arr
        .iter()
        .find(|obj| obj["path"].as_str().unwrap().contains("empty.rs"));
    assert!(file_obj.is_some());
    let blocks = file_obj.unwrap()["blocks"].as_array().unwrap();
    assert!(blocks.is_empty());

    Ok(())
}

#[test]
fn test_sub_splitter_avoids_empty_blocks() -> Result<()> {
    let env = TestEnv::new()?;
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
        let file_path = env.root.join(name);
        fs::write(&file_path, content)?;
    }

    let output = env.run_trueflow(&["scan", "--json"])?;
    let file_states: Vec<FileState> = serde_json::from_str(&output)?;

    for &(name, _) in &test_cases {
        let file_state = file_states
            .iter()
            .find(|file| file.path.ends_with(name))
            .unwrap_or_else(|| panic!("missing scan output for {}", name));

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
            let sub_blocks = sub_splitter::split(block, file_state.language.clone())?;
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
