use crate::context::TrueflowContext;
use crate::diff_logic::get_unreviewed_changes;
use anyhow::Result;
use log::warn;

pub fn run(_context: &TrueflowContext, json: bool) -> Result<()> {
    let unreviewed_changes = get_unreviewed_changes()?;

    if json {
        println!("{}", serde_json::to_string_pretty(&unreviewed_changes)?);
    } else {
        for change in unreviewed_changes {
            warn!("File: {}:{}", change.file, change.line);
            warn!("Fingerprint: {}", change.fingerprint);
            warn!("Status: {}", change.status);
            warn!("---");
        }
    }

    Ok(())
}
