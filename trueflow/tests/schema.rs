use anyhow::Result;
use schemars::generate::SchemaSettings;
use serde_json::Value;
use std::fs;
use std::path::PathBuf;

#[test]
fn review_record_schema_matches_snapshot() -> Result<()> {
    let schema = SchemaSettings::draft2020_12()
        .for_serialize()
        .into_generator()
        .into_root_schema_for::<trueflow::store::Record>();
    let schema_value = serde_json::to_value(schema)?;

    if std::env::var("TRUEFLOW_PRINT_SCHEMA").is_ok() {
        println!("{}", serde_json::to_string_pretty(&schema_value)?);
    }

    let expected_path = schema_snapshot_path();
    let expected = fs::read_to_string(&expected_path)?;
    let expected_json: Value = serde_json::from_str(&expected)?;

    assert_eq!(
        schema_value, expected_json,
        "schema mismatch at {expected_path:?}"
    );
    Ok(())
}

fn schema_snapshot_path() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("review_record.schema.json")
}
