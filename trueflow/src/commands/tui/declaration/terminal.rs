use std::collections::VecDeque;
use std::path::Path;
use std::time::Duration;

use anyhow::{Context, Result, anyhow};
use crossterm::event::{self, Event, KeyCode, KeyEventKind, KeyModifiers};
use ratatui::widgets::{Block as UiBlock, Borders, Paragraph, Wrap};

use crate::commands::mark;
use crate::commands::review::{ReviewRequest, ReviewTarget, resolve_review_request};
use crate::config::{BlockFilters, TrueflowConfig};
use crate::github::{GhGitHubClient, prepare_pull_request_review};
use crate::store::{FileStore, ReviewStore};
use crate::vcs;

use super::super::{
    CliReviewRequest, TerminalSession, TuiRunRequest, build_pull_request_cli_requests,
    cli_review_request, execute_mark_for_tui, resolve_cli_review_request,
    resolve_pull_request_target_for_tui,
};
use super::relationship_bridge::ProductionRelationshipCoordinator;
use super::{
    DeclarationAppRuntime, DeclarationLayout, DeclarationPane, PreparedDeclarationLaunch,
    prepare_declaration_launch, render_declaration_review,
};
use crate::declaration::relationships::WorkspaceTrust;

const EVENT_TICK: Duration = Duration::from_millis(100);

enum RuntimeExit {
    Quit,
    AdvanceScope,
}

pub(in crate::commands::tui) fn run(
    config: &TrueflowConfig,
    request: &TuiRunRequest<'_>,
) -> Result<()> {
    let repo_root = vcs::git_root_from_workdir()?
        .ok_or_else(|| anyhow!("git repository required for declaration review"))?;
    let mut requests =
        declaration_requests(&repo_root, request.all, request.target, request.since)?;
    let first_request = requests
        .pop_front()
        .context("declaration review produced no launch request")?;
    let first_launch = prepare_request(&repo_root, config, first_request.request)?;

    let mut session = TerminalSession::enter()?;
    let run_result = (|| {
        let mut prepared = first_launch;
        loop {
            let area = session.terminal_mut().size()?;
            let identity = mark::structured_identity_from_workdir();
            let trust = if request.trust_lsp_workspace {
                WorkspaceTrust::TrustedForInvocation
            } else {
                WorkspaceTrust::Untrusted
            };
            let mut relationships =
                ProductionRelationshipCoordinator::new(&prepared, &repo_root, trust)?;
            let mut runtime = DeclarationAppRuntime::new_with_external_persistence(
                prepared,
                identity,
                area.width,
                area.height,
            )?;
            let exit = run_runtime(
                &mut session,
                &mut runtime,
                &mut relationships,
                !requests.is_empty(),
            )?;
            relationships.shutdown();
            match exit {
                RuntimeExit::Quit => return Ok(()),
                RuntimeExit::AdvanceScope => {
                    let request = requests
                        .pop_front()
                        .context("declaration scope queue ended unexpectedly")?;
                    prepared = prepare_request(&repo_root, config, request.request)?;
                }
            }
        }
    })();
    let restore_result = session.restore();
    match (run_result, restore_result) {
        (Ok(()), Ok(())) => Ok(()),
        (Err(primary), Ok(())) => Err(primary),
        (Ok(()), Err(restore)) => Err(restore),
        (Err(primary), Err(restore)) => Err(anyhow!(
            "{primary:#}\nterminal restore also failed: {restore:#}"
        )),
    }
}

fn declaration_requests(
    repo_root: &Path,
    all: bool,
    target: &[ReviewTarget],
    since: Option<&str>,
) -> Result<VecDeque<CliReviewRequest>> {
    if let Some(pull_request) = resolve_pull_request_target_for_tui(all, target, since)? {
        let prepared = prepare_pull_request_review(repo_root, &pull_request, &GhGitHubClient)?;
        return build_pull_request_cli_requests(&prepared.metadata);
    }

    let request = cli_review_request(all, target, since, &[], &[])?
        .unwrap_or(resolve_cli_review_request(false, &[], None)?);
    Ok(VecDeque::from([request]))
}

fn prepare_request(
    repo_root: &Path,
    config: &TrueflowConfig,
    request: ReviewRequest,
) -> Result<PreparedDeclarationLaunch> {
    let query = resolve_review_request(
        request,
        BlockFilters::default(),
        config.scan.resolve_options(),
    )?;
    let records = FileStore::new()?.read_history()?;
    prepare_declaration_launch(repo_root, &query, records)
}

fn run_runtime(
    session: &mut TerminalSession,
    runtime: &mut DeclarationAppRuntime<super::runtime::ExternalDeclarationPersistence>,
    relationships: &mut ProductionRelationshipCoordinator,
    has_later_scope: bool,
) -> Result<RuntimeExit> {
    if runtime.is_finished() && has_later_scope {
        return Ok(RuntimeExit::AdvanceScope);
    }

    loop {
        while let Some(update) = relationships.poll() {
            let _ = relationships.apply(runtime, update)?;
        }
        session.terminal_mut().draw(|frame| {
            let area = frame.area();
            if let Some(controller) = runtime.controller() {
                render_declaration_review(frame, area, controller);
            } else {
                let text = runtime.visible_text();
                frame.render_widget(
                    Paragraph::new(text)
                        .block(
                            UiBlock::default()
                                .borders(Borders::ALL)
                                .title("Declaration Review"),
                        )
                        .wrap(Wrap { trim: false }),
                    area,
                );
            }
        })?;

        if !event::poll(EVENT_TICK)? {
            continue;
        }
        let event = event::read()?;
        match event {
            Event::Resize(width, height) => {
                runtime.resize(width, height);
            }
            Event::Paste(text) => {
                if let Some(controller) = runtime.controller_mut()
                    && controller.is_editing()
                {
                    controller.insert_text(&text);
                }
            }
            Event::Key(key) if key.kind == KeyEventKind::Press => {
                let editing = runtime
                    .controller()
                    .is_some_and(|controller| controller.is_editing());
                if !editing
                    && key.code == KeyCode::Char('q')
                    && !key
                        .modifiers
                        .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT | KeyModifiers::SUPER)
                {
                    return Ok(RuntimeExit::Quit);
                }
                let request_relationships = !editing
                    && (key.code == KeyCode::Char('o')
                        || (key.code == KeyCode::Enter
                            && runtime.controller().is_some_and(|controller| {
                                matches!(
                                    controller.render_model().layout,
                                    DeclarationLayout::Single {
                                        pane: DeclarationPane::Outline,
                                        ..
                                    }
                                )
                            })));
                if !editing && !runtime.is_finished() && key.code == KeyCode::Char(' ') {
                    runtime.skip_current()?;
                } else if let Some(controller) = runtime.controller_mut() {
                    controller.handle_key(key.code)?;
                }

                if request_relationships
                    && let Some(declaration_id) = runtime
                        .current()
                        .map(|target| target.declaration.id.clone())
                {
                    let update = relationships.request(&declaration_id)?;
                    let _ = relationships.apply(runtime, update)?;
                }

                let actions = runtime
                    .controller_mut()
                    .map(|controller| controller.take_actions())
                    .unwrap_or_default();
                for action in actions {
                    runtime.submit_action_with_persistence(&action, |record| {
                        execute_mark_for_tui(
                            || mark::append_structured_record_with_noninteractive_signing(record),
                            || session.suspend(|| mark::append_structured_record(record)),
                        )
                    })?;
                }

                if runtime.is_finished() && has_later_scope {
                    return Ok(RuntimeExit::AdvanceScope);
                }
            }
            _ => {}
        }
    }
}
