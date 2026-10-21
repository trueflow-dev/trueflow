use std::path::Path;

use anyhow::Result;
use trueflow::analysis::Language;
use trueflow::declaration::{Capability, DeclarationKind, Visibility, project_source};

#[test]
fn shell_projection_covers_function_syntaxes_without_executable_bodies() -> Result<()> {
    const SOURCE: &str = r#"#!/usr/bin/env bash

readonly RELEASE_CHANNEL=stable

# Deploys the selected artifact.
deploy() {
    local artifact=$1
    nested_implementation_detail() {
        printf '%s\n' "$artifact"
    }
    nested_implementation_detail
}

# Reports service status.
function healthcheck {
    printf '%s\n' healthy
}

printf '%s\n' "$RELEASE_CHANNEL"
"#;

    let facts = project_source(Path::new("scripts/release.sh"), Language::Shell, SOURCE)?;

    assert_eq!(facts.capabilities.inventory, Capability::Complete);
    assert_eq!(facts.capabilities.callable_projection, Capability::Complete);
    assert!(matches!(
        facts.capabilities.aggregate_projection,
        Capability::NotApplicable { .. }
    ));
    assert!(matches!(
        facts.capabilities.type_use_sites,
        Capability::NotApplicable { .. }
    ));
    assert_eq!(
        facts
            .declarations()
            .iter()
            .map(|declaration| (
                declaration.kind,
                declaration.name.as_str(),
                declaration.visibility.clone(),
                declaration.projection_text.as_str(),
            ))
            .collect::<Vec<_>>(),
        [
            (
                DeclarationKind::Function,
                "deploy",
                Visibility::Implicit,
                "# Deploys the selected artifact.\ndeploy()",
            ),
            (
                DeclarationKind::Function,
                "healthcheck",
                Visibility::Implicit,
                "# Reports service status.\nfunction healthcheck",
            ),
        ]
    );
    assert!(
        facts
            .declarations()
            .iter()
            .all(|declaration| !declaration.projection_text.contains("printf"))
    );
    Ok(())
}

#[test]
fn doc_comment_directly_after_code_does_not_absorb_the_previous_newline() -> Result<()> {
    let project = |source: &str| -> Result<Vec<_>> {
        let facts = project_source(Path::new("scripts/deploy.sh"), Language::Shell, source)?;
        Ok(facts
            .declarations()
            .iter()
            .map(|declaration| {
                (
                    declaration.projection_text.clone(),
                    declaration.projection_hash.clone(),
                )
            })
            .collect())
    };

    let adjacent =
        project("set -eu\n# Deploys.\ndeploy() { :; }\n# Cleans up.\ncleanup() { :; }\n")?;
    let spaced =
        project("set -eu\n\n# Deploys.\ndeploy() { :; }\n\n# Cleans up.\ncleanup() { :; }\n")?;

    assert_eq!(
        adjacent
            .iter()
            .map(|(text, _)| text.as_str())
            .collect::<Vec<_>>(),
        ["# Deploys.\ndeploy()", "# Cleans up.\ncleanup()"]
    );
    assert_eq!(
        adjacent, spaced,
        "a blank line above a doc comment must not change the projection"
    );
    Ok(())
}
