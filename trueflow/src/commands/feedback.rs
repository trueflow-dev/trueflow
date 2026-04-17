use crate::block::BlockKind;
use crate::config::load as load_config;
use crate::context::TrueflowContext;
use crate::feedback_export::{
    FeedbackEntry, FeedbackQuery, RepoFeedbackContextResolver, build_feedback_cursor,
    collect_feedback_entries, feedback_cursor_path, resolve_allowed_revisions,
    resolve_since_filter, write_feedback_cursor,
};
use crate::feedback_since::{FeedbackSinceExpr, ResolvedFeedbackSince as ParsedFeedbackSince};
use crate::store::ReviewStore;
use crate::targets::{ReviewTarget, resolve_targets, workdir_prefix_from_git_root};
use anyhow::Result;
use clap::ValueEnum;

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum FeedbackFormat {
    Xml,
    Json,
}

pub fn run(
    _context: &TrueflowContext,
    format: FeedbackFormat,
    since: Option<&FeedbackSinceExpr>,
    targets: &[ReviewTarget],
    include_approved: bool,
    only: &[BlockKind],
    exclude: &[BlockKind],
) -> Result<()> {
    let config = load_config()?;
    let filters = config.feedback.filters.resolve_filters(only, exclude);
    let scan_options = config.scan.resolve_options();
    let effective_since = since.unwrap_or(&config.feedback.default_since);
    let resolved_targets = resolve_targets(targets)?;

    let store = crate::store::FileStore::new()?;
    let database = store.load_database()?;
    let since_mode = effective_since.resolve()?;
    let since_filter = resolve_since_filter(&store, since_mode)?;
    let explicit_selection = resolved_targets.explicit_selection();
    let changed_selection = targets
        .iter()
        .any(|target| matches!(target, ReviewTarget::DirtyWorktree))
        .then(|| resolved_targets.changed_selection())
        .flatten();
    let allowed_revisions = resolve_allowed_revisions(&resolved_targets.diff_selection)?;
    let query = FeedbackQuery {
        filters,
        explicit_selection,
        changed_selection,
        allowed_revisions,
        include_approved,
    };
    let workdir_prefix = workdir_prefix_from_git_root();
    let mut resolver = RepoFeedbackContextResolver::new(
        &resolved_targets.content_source,
        &scan_options,
        workdir_prefix.as_deref(),
    )?;
    let entries =
        collect_feedback_entries(database.records(), &since_filter, &query, &mut resolver)?;

    render_feedback(format, entries)?;

    if matches!(since_mode, ParsedFeedbackSince::Last)
        && let Some(cursor) = build_feedback_cursor(database.records())
    {
        write_feedback_cursor(feedback_cursor_path(&store).as_path(), &cursor)?;
    }

    Ok(())
}

fn render_feedback(format: FeedbackFormat, entries: Vec<FeedbackEntry>) -> Result<()> {
    match format {
        FeedbackFormat::Json => {
            let export_list = entries
                .into_iter()
                .map(|entry| {
                    serde_json::json!({
                        "file": entry.file_path,
                        "block": entry.block,
                        "reviews": entry.reviews,
                        "latest_verdict": entry.latest_verdict,
                    })
                })
                .collect::<Vec<_>>();
            println!("{}", serde_json::to_string_pretty(&export_list)?);
        }
        FeedbackFormat::Xml => {
            println!("<trueflow_feedback>");

            let mut current_file_path: Option<String> = None;
            for entry in entries {
                if current_file_path.as_deref() != Some(entry.file_path.as_str()) {
                    if current_file_path.is_some() {
                        println!("  </file>");
                    }
                    println!("  <file path=\"{}\">", escape_xml(&entry.file_path));
                    current_file_path = Some(entry.file_path.clone());
                }

                print_block_xml(&entry.block, &entry.reviews);
            }

            if current_file_path.is_some() {
                println!("  </file>");
            }

            println!("</trueflow_feedback>");
        }
    }

    Ok(())
}

fn print_block_xml(block: &crate::block::Block, reviews: &[crate::store::Record]) {
    println!(
        "    <block start_line=\"{}\" end_line=\"{}\" kind=\"{}\" hash=\"{}\">",
        block.start_line,
        block.end_line,
        escape_xml(block.kind.as_str()),
        block.hash
    );

    println!("      <context><![CDATA[");
    let safe_content = block.content.replace("]]>", "]]]]><![CDATA[>");
    println!("{safe_content}");
    println!("]]></context>");

    println!("      <reviews>");
    for review in reviews {
        let author = match &review.identity {
            crate::store::Identity::Email { email, .. } => email,
        };
        println!(
            "        <review verdict=\"{}\" author=\"{}\">",
            escape_xml(review.verdict.as_str()),
            escape_xml(author)
        );
        if let Some(note) = &review.note {
            println!("          <comment>{}</comment>", escape_xml(note));
        }
        println!("        </review>");
    }
    println!("      </reviews>");
    println!("    </block>");
}

fn escape_xml(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&apos;")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn feedback_format_exposes_xml_and_json_variants() {
        assert_eq!(
            FeedbackFormat::value_variants(),
            &[FeedbackFormat::Xml, FeedbackFormat::Json]
        );
    }
}
