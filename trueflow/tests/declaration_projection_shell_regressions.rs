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
