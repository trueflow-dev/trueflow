use anyhow::{Context, Result};
use schemars::generate::SchemaSettings;
use serde_json::Value;
use std::fs;
use std::path::PathBuf;

#[test]
fn review_record_schema_matches_snapshot() -> Result<()> {
    let schema_value = generated_record_schema()?;

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

#[test]
fn declaration_shape_implications_require_v5_and_a_complete_declaration_shape() -> Result<()> {
    let schema = generated_record_schema()?;

    for signal in ["target", "check", "declaration_locator", "comment_anchor"] {
        let implication = declaration_implication(&schema, signal)?;
        let consequence = implication
            .get("then")
            .context("declaration implication must have a then schema")?;

        assert_eq!(
            consequence.pointer("/properties/version/const"),
            Some(&Value::from(5)),
            "{signal}=declaration must constrain the record to V5"
        );

        if signal != "target" {
            assert_eq!(
                consequence.pointer("/properties/target/properties/kind/const"),
                Some(&Value::from("declaration")),
                "{signal}=declaration must require a declaration target"
            );
        }
        if signal != "check" {
            assert_eq!(
                consequence.pointer("/properties/check/const"),
                Some(&Value::from("declaration")),
                "{signal}=declaration must require the declaration check"
            );
        }
        if signal != "declaration_locator" {
            assert_eq!(
                consequence.pointer("/properties/declaration_locator/$ref"),
                Some(&Value::from("#/$defs/DeclarationRecordLocator")),
                "{signal}=declaration must require a non-null declaration locator"
            );
            assert!(
                required_contains(consequence, "declaration_locator"),
                "{signal}=declaration must make the declaration locator required"
            );
        }
    }
    Ok(())
}

#[test]
fn generated_schema_keeps_ordinary_v2_v3_v4_records_compatible() -> Result<()> {
    let schema = generated_record_schema()?;
    let version = schema
        .pointer("/properties/version")
        .context("record schema must define version")?;
    let minimum = version["minimum"]
        .as_u64()
        .context("record version must have an integer minimum")?;
    let maximum = version["maximum"]
        .as_u64()
        .context("record version must have an integer maximum")?;

    for legacy_version in [2, 3, 4] {
        assert!(
            (minimum..=maximum).contains(&legacy_version),
            "ordinary V{legacy_version} records must remain schema-valid"
        );
    }

    for field in ["declaration_locator", "comment_anchor"] {
        assert!(
            !required_contains(&schema, field),
            "ordinary records may omit {field}"
        );
        let alternatives = schema
            .pointer(&format!("/properties/{field}/anyOf"))
            .and_then(Value::as_array)
            .with_context(|| format!("{field} must have schema alternatives"))?;
        assert!(
            alternatives
                .iter()
                .any(|alternative| alternative.get("type") == Some(&Value::from("null"))),
            "ordinary records may encode {field} as null"
        );
    }

    assert_eq!(
        schema.pointer("/properties/check/type"),
        Some(&Value::from("string")),
        "ordinary review checks must remain allowed"
    );
    let targets = schema
        .pointer("/$defs/ReviewTargetRef/oneOf")
        .and_then(Value::as_array)
        .context("record target variants must be present")?;
    assert!(
        targets.iter().any(|target| {
            target.pointer("/properties/kind/const") == Some(&Value::from("block"))
        }),
        "ordinary block targets must remain allowed"
    );
    Ok(())
}

fn generated_record_schema() -> Result<Value> {
    let schema = SchemaSettings::draft2020_12()
        .for_serialize()
        .into_generator()
        .into_root_schema_for::<trueflow::store::Record>();
    Ok(serde_json::to_value(schema)?)
}

fn declaration_implication<'a>(schema: &'a Value, signal: &str) -> Result<&'a Value> {
    let implications = schema
        .get("allOf")
        .and_then(Value::as_array)
        .context("record schema must define declaration implications")?;

    implications
        .iter()
        .find(|implication| {
            required_contains(&implication["if"], signal)
                && match signal {
                    "target" => {
                        implication.pointer("/if/properties/target/properties/kind/const")
                            == Some(&Value::from("declaration"))
                    }
                    "check" => {
                        implication.pointer("/if/properties/check/const")
                            == Some(&Value::from("declaration"))
                    }
                    "declaration_locator" => {
                        implication.pointer("/if/properties/declaration_locator/$ref")
                            == Some(&Value::from("#/$defs/DeclarationRecordLocator"))
                    }
                    "comment_anchor" => {
                        implication.pointer("/if/properties/comment_anchor/type")
                            == Some(&Value::from("object"))
                            && implication
                                .pointer("/if/properties/comment_anchor/properties/type/const")
                                == Some(&Value::from("declaration"))
                    }
                    _ => false,
                }
        })
        .with_context(|| format!("record schema must constrain declaration signal {signal}"))
}

fn required_contains(schema: &Value, field: &str) -> bool {
    schema
        .get("required")
        .and_then(Value::as_array)
        .is_some_and(|required| required.iter().any(|value| value == field))
}

fn schema_snapshot_path() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("review_record.schema.json")
}
