use crate::context::TrueflowContext;
use anyhow::Result;
use crossterm::{
    event::{self, Event, KeyCode, KeyEventKind},
    execute,
    terminal::{EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode},
};
use ratatui::{
    Frame, Terminal,
    layout::{Alignment, Constraint, Direction, Layout},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Paragraph, Wrap},
};
use std::io::{self, Stdout};

struct AppState {
    show_subblocks: bool,
    subblock_index: usize,
}

struct UiPalette {
    bg: Color,
    fg: Color,
    code_fg: Color,
    dim: Color,
    add: Color,
    del: Color,
    code_bg: Color,
}

pub fn run(_context: &TrueflowContext) -> Result<()> {
    let mut terminal = setup_terminal()?;
    let mut state = AppState {
        show_subblocks: false,
        subblock_index: 0,
    };
    let run_result = run_app(&mut terminal, &mut state);
    restore_terminal(&mut terminal)?;
    run_result
}

fn setup_terminal() -> Result<Terminal<ratatui::backend::CrosstermBackend<Stdout>>> {
    let mut stdout = io::stdout();
    enable_raw_mode()?;
    execute!(stdout, EnterAlternateScreen)?;
    let backend = ratatui::backend::CrosstermBackend::new(stdout);
    Ok(Terminal::new(backend)?)
}

fn restore_terminal(
    terminal: &mut Terminal<ratatui::backend::CrosstermBackend<Stdout>>,
) -> Result<()> {
    disable_raw_mode()?;
    execute!(terminal.backend_mut(), LeaveAlternateScreen,)?;
    terminal.show_cursor()?;
    Ok(())
}

fn run_app(
    terminal: &mut Terminal<ratatui::backend::CrosstermBackend<Stdout>>,
    state: &mut AppState,
) -> Result<()> {
    loop {
        terminal.draw(|f| ui(f, state))?;

        if event::poll(std::time::Duration::from_millis(16))?
            && let Event::Key(key) = event::read()?
            && key.kind == KeyEventKind::Press
        {
            match key.code {
                KeyCode::Char('q') => return Ok(()),
                KeyCode::Char('s') => {
                    if state.show_subblocks {
                        state.show_subblocks = false;
                    } else {
                        state.show_subblocks = true;
                        state.subblock_index = 0;
                    }
                }
                _ => {}
            }
        }
    }
}

fn ui(frame: &mut Frame, state: &AppState) {
    // Light Theme
    let palette = UiPalette {
        bg: Color::Rgb(248, 248, 245),
        fg: Color::Rgb(60, 56, 54),
        code_fg: Color::Rgb(40, 40, 40),
        dim: Color::Rgb(146, 131, 116),
        add: Color::Rgb(184, 187, 38),
        del: Color::Rgb(251, 73, 52),
        code_bg: Color::Rgb(240, 240, 238),
    };

    let area = frame.size();

    // Fill background
    let bg_block = Block::default().style(Style::default().bg(palette.bg));
    frame.render_widget(bg_block, area);

    // Layout
    let vertical_layout = Layout::default()
        .direction(Direction::Vertical)
        .constraints(vec![
            Constraint::Length(1), // Header
            Constraint::Min(0),    // Content
            Constraint::Length(1), // Footer
        ])
        .split(area);

    // Header
    let header_text = if state.show_subblocks {
        "Trueflow TUI (Sub-blocks)"
    } else {
        "Trueflow TUI"
    };
    let header = Paragraph::new(header_text)
        .style(Style::default().fg(palette.fg).bg(palette.bg))
        .alignment(Alignment::Center);
    frame.render_widget(header, vertical_layout[0]);

    // Center Content
    let horizontal_layout = Layout::default()
        .direction(Direction::Horizontal)
        .constraints(vec![
            Constraint::Fill(1),
            Constraint::Length(80), // Centered 80ch
            Constraint::Fill(1),
        ])
        .split(vertical_layout[1]);

    let center_area = horizontal_layout[1];

    let block_kind = "function";
    let block_name = Some("main");
    let block_path = "src/main.rs";
    let block_hash = "a1b2c3d4e5f6";
    let subblock_views = sample_subblocks();
    let subblock_labels: Vec<&str> = subblock_views.iter().map(|sb| sb.label).collect();

    let mut header_lines = render_header_lines(
        block_kind,
        block_name,
        block_path,
        block_hash,
        &subblock_labels,
        &palette,
    );
    header_lines.push(Line::from(""));
    header_lines.push(Line::from(""));

    let code_lines = if state.show_subblocks {
        let active_index = state
            .subblock_index
            .min(subblock_views.len().saturating_sub(1));
        let fallback = SubblockView {
            label: "(none)",
            lines: Vec::new(),
        };
        let active_subblock = subblock_views.get(active_index).unwrap_or(&fallback);
        render_active_subblock_lines(
            active_subblock,
            active_index,
            subblock_views.len(),
            &palette,
        )
    } else {
        render_block_lines(&palette)
    };

    let content_layout = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Min(0),
            Constraint::Length(1),
            Constraint::Percentage(10),
        ])
        .split(center_area);

    let header_height = header_lines.len().max(1) as u16;
    let body_layout = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Length(header_height), Constraint::Min(0)])
        .split(content_layout[0]);

    let header = Paragraph::new(header_lines)
        .style(Style::default().fg(palette.fg).bg(palette.bg))
        .wrap(Wrap { trim: false });
    frame.render_widget(header, body_layout[0]);

    let code = Paragraph::new(code_lines)
        .style(Style::default().fg(palette.code_fg).bg(palette.code_bg))
        .wrap(Wrap { trim: false })
        .block(Block::default().style(Style::default().bg(palette.code_bg)));
    frame.render_widget(code, body_layout[1]);

    let actions_text = if state.show_subblocks {
        "Actions: [a]pprove  [x]reject  [c]omment  [s]parent  [q]uit"
    } else {
        "Actions: [a]pprove  [x]reject  [c]omment  [s]ubdivide  [q]uit"
    };
    let actions = Paragraph::new(Line::from(Span::styled(
        actions_text,
        Style::default()
            .fg(palette.dim)
            .bg(palette.bg)
            .add_modifier(Modifier::BOLD),
    )))
    .alignment(Alignment::Center);
    frame.render_widget(actions, content_layout[1]);

    // Footer
    let footer = Paragraph::new("Status: Ready (Press 's' to subdivide, 'q' to quit)")
        .style(Style::default().fg(palette.dim).bg(palette.bg))
        .alignment(Alignment::Left);
    frame.render_widget(footer, vertical_layout[2]);
}

fn render_header_lines(
    block_kind: &str,
    block_name: Option<&str>,
    path: &str,
    hash: &str,
    subblocks: &[&str],
    palette: &UiPalette,
) -> Vec<Line<'static>> {
    let mut lines = Vec::new();
    let name_suffix = block_name
        .map(|name| format!(" {}", name))
        .unwrap_or_default();
    let short_hash = &hash[..hash.len().min(8)];
    let header_text = format!(
        "{}{} in {} (hash={}), subblocks:",
        block_kind, name_suffix, path, short_hash
    );
    lines.push(Line::from(Span::styled(
        header_text,
        Style::default()
            .fg(palette.fg)
            .bg(palette.bg)
            .add_modifier(Modifier::BOLD),
    )));

    let tree_lines = format_subblock_tree(subblocks);
    for line in tree_lines {
        lines.push(Line::from(Span::styled(
            line,
            Style::default().fg(palette.dim).bg(palette.bg),
        )));
    }

    lines
}

fn format_subblock_tree(subblocks: &[&str]) -> Vec<String> {
    if subblocks.is_empty() {
        return vec!["└─ (none)".to_string()];
    }

    let display: Vec<&str> = if subblocks.len() > 4 {
        let mut items = Vec::new();
        items.extend_from_slice(&subblocks[..2]);
        items.push("...");
        items.extend_from_slice(&subblocks[subblocks.len() - 2..]);
        items
    } else {
        subblocks.to_vec()
    };

    let last_index = display.len().saturating_sub(1);
    display
        .into_iter()
        .enumerate()
        .map(|(idx, label)| {
            let prefix = if idx == last_index {
                "└─"
            } else {
                "├─"
            };
            format!("{} {}", prefix, label)
        })
        .collect()
}

struct SubblockView {
    label: &'static str,
    lines: Vec<&'static str>,
}

fn sample_subblocks() -> Vec<SubblockView> {
    vec![
        SubblockView {
            label: "Signature",
            lines: vec!["fn main() {"],
        },
        SubblockView {
            label: "CodeParagraph",
            lines: vec!["-    println!(\"Hello\");"],
        },
        SubblockView {
            label: "CodeParagraph",
            lines: vec!["+    println!(\"Hello, Trueflow!\");"],
        },
        SubblockView {
            label: "CodeParagraph",
            lines: vec!["    // This is centered."],
        },
        SubblockView {
            label: "CodeParagraph",
            lines: vec!["}"],
        },
    ]
}

fn render_block_lines(palette: &UiPalette) -> Vec<Line<'static>> {
    let raw_lines = vec![
        "fn main() {",
        "-    println!(\"Hello\");",
        "+    println!(\"Hello, Trueflow!\");",
        "    // This is centered.",
        "}",
    ];

    raw_lines
        .into_iter()
        .map(|line| format_code_line(line, palette))
        .collect()
}

fn render_active_subblock_lines(
    subblock: &SubblockView,
    index: usize,
    total: usize,
    palette: &UiPalette,
) -> Vec<Line<'static>> {
    let mut lines = Vec::new();
    let label = format!(
        "Sub-block {}/{} ({})",
        index + 1,
        total.max(1),
        subblock.label
    );
    lines.push(Line::from(Span::styled(
        label,
        Style::default()
            .fg(palette.dim)
            .bg(palette.bg)
            .add_modifier(Modifier::BOLD),
    )));
    lines.push(Line::from(""));
    lines.extend(
        subblock
            .lines
            .iter()
            .map(|line| format_code_line(line, palette)),
    );
    lines
}

fn format_code_line(line: &str, palette: &UiPalette) -> Line<'static> {
    let gutter_left = 4;
    let gutter_right = 2;
    let gutter_spacing = " ".repeat(gutter_left + gutter_right + 1);

    if let Some(rest) = line.strip_prefix('+') {
        let marker = format!("{}+{}", " ".repeat(gutter_left), " ".repeat(gutter_right));
        let style = Style::default()
            .fg(palette.add)
            .bg(palette.code_bg)
            .add_modifier(Modifier::BOLD);
        return Line::from(vec![
            Span::styled(marker, style),
            Span::styled(rest.to_string(), style),
        ]);
    }

    if let Some(rest) = line.strip_prefix('-') {
        let marker = format!("{}-{}", " ".repeat(gutter_left), " ".repeat(gutter_right));
        let style = Style::default()
            .fg(palette.del)
            .bg(palette.code_bg)
            .add_modifier(Modifier::BOLD);
        return Line::from(vec![
            Span::styled(marker, style),
            Span::styled(rest.to_string(), style),
        ]);
    }

    let trimmed = line.trim_start();
    let comment_style = if trimmed.starts_with("//")
        || trimmed.starts_with('#')
        || trimmed.starts_with("/*")
        || trimmed.starts_with('*')
    {
        Style::default().fg(palette.dim).bg(palette.code_bg)
    } else {
        Style::default().fg(palette.code_fg).bg(palette.code_bg)
    };
    let gutter_style = Style::default().fg(palette.dim).bg(palette.code_bg);
    Line::from(vec![
        Span::styled(gutter_spacing, gutter_style),
        Span::styled(line.to_string(), comment_style),
    ])
}
