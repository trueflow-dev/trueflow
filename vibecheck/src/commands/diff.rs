use anyhow::Result;
use crate::diff_logic::get_unreviewed_changes;

pub fn run(json: bool) -> Result<()> {
    let unreviewed_changes = get_unreviewed_changes()?;
    
    if json {
        println!("{}", serde_json::to_string_pretty(&unreviewed_changes)?);
    } else {
        // Text output
        for change in unreviewed_changes {
            println!("File: {}:{}", change.file, change.line);
            println!("Fingerprint: {}", change.fingerprint);
            println!("Status: {}", change.status);
            println!("---");
        }
    }

    Ok(())
}
