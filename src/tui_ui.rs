use std::io;
use std::time::{Duration, Instant};

use anyhow::{bail, Result};
use crossterm::event::{self, Event, KeyCode, KeyEvent, KeyModifiers};
use crossterm::terminal::{disable_raw_mode, enable_raw_mode};
use crossterm::{
    execute,
    terminal::{EnterAlternateScreen, LeaveAlternateScreen},
};
use ratatui::{
    backend::CrosstermBackend,
    layout::{Alignment, Constraint, Direction, Layout},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Paragraph, Wrap},
};

use crate::cli::FilterOpts;
use crate::model::ThreadTreeNode;
use crate::observer::Monitor;
use crate::runtime::RuntimeOverlay;

pub fn run_tui(monitor: &mut Monitor, filters: &FilterOpts) -> Result<()> {
    if matches!(filters.runtime_events.as_deref(), Some("-")) {
        bail!("--runtime-events - is only supported for one-shot probe");
    }

    let mut state = TuiState::new();
    state.refresh_interval_ms = 1000;
    let mut _guard = TerminalGuard::enter()?;
    loop {
        let now = Instant::now();
        let runtime = RuntimeOverlay::from_source(filters.runtime_events.as_deref());
        let snapshot = monitor.probe_snapshot(filters, runtime, false)?;

        let mut nodes = Vec::new();
        for node in &snapshot.tree {
            collect_tree_nodes(node, 0, &snapshot.threads, &mut nodes);
        }
        if nodes.is_empty() {
            state.selected = 0;
        } else if state.selected >= nodes.len() {
            state.selected = nodes.len().saturating_sub(1);
        }

        _guard.terminal.draw(|frame| {
            let size = frame.area();
            let root = Layout::default()
                .direction(Direction::Vertical)
                .margin(1)
                .constraints([Constraint::Length(3), Constraint::Min(0)])
                .split(size);
            let header = Paragraph::new(
                "q quit, j/k or arrows move, Enter toggle details, r refresh manually",
            )
            .alignment(Alignment::Left)
            .style(Style::default().fg(Color::Cyan))
            .block(Block::default().borders(Borders::ALL).title("Help"));
            frame.render_widget(header, root[0]);

            let body = Layout::default()
                .direction(Direction::Horizontal)
                .constraints([Constraint::Percentage(55), Constraint::Percentage(45)])
                .split(root[1]);

            let list_lines: Vec<Line> = nodes
                .iter()
                .enumerate()
                .filter_map(|(idx, (thread_id, depth))| {
                    let thread = snapshot
                        .threads
                        .iter()
                        .find(|t| t.thread_id == *thread_id)?;
                    let prefix = if idx == state.selected { ">" } else { " " };
                    let indent = "  ".repeat(*depth);
                    let marker = match thread.state {
                        crate::model::ThreadState::Running => "R",
                        crate::model::ThreadState::Idle => "I",
                        crate::model::ThreadState::Interrupted => "X",
                        crate::model::ThreadState::Failed => "F",
                        crate::model::ThreadState::Done => "D",
                        crate::model::ThreadState::Unknown => "?",
                    };
                    Some(Line::from(format!(
                        "{} [{}] {}{}",
                        prefix, marker, indent, thread.thread_id
                    )))
                })
                .collect();
            frame.render_widget(
                Paragraph::new(list_lines)
                    .wrap(Wrap { trim: true })
                    .block(Block::default().borders(Borders::ALL).title("Threads")),
                body[0],
            );

            let details = if let Some((thread_id, _)) = nodes.get(state.selected) {
                render_details(
                    snapshot.threads.iter().find(|t| &t.thread_id == thread_id),
                    state.show_details,
                )
            } else {
                Paragraph::new("No thread selected")
                    .block(Block::default().borders(Borders::ALL).title("Details"))
            };
            frame.render_widget(details, body[1]);
        })?;

        let timeout = Duration::from_millis(100);
        if event::poll(timeout)? {
            if let Event::Key(KeyEvent {
                code, modifiers, ..
            }) = event::read()?
            {
                match code {
                    KeyCode::Char('q') | KeyCode::Esc => break,
                    KeyCode::Char('c') if modifiers.contains(KeyModifiers::CONTROL) => break,
                    KeyCode::Char('r') | KeyCode::F(5) => {}
                    KeyCode::Down | KeyCode::Char('j') => {
                        state.selected = state.selected.saturating_add(1)
                    }
                    KeyCode::Up | KeyCode::Char('k') => {
                        state.selected = state.selected.saturating_sub(1)
                    }
                    KeyCode::Enter => state.show_details = !state.show_details,
                    _ => {}
                }
                let max = nodes.len().saturating_sub(1);
                if state.selected > max {
                    state.selected = max;
                }
            }
        }
        let elapsed = now.elapsed();
        if elapsed < Duration::from_millis(state.refresh_interval_ms) {
            std::thread::sleep(Duration::from_millis(state.refresh_interval_ms) - elapsed);
        }
    }

    Ok(())
}

fn collect_tree_nodes(
    node: &ThreadTreeNode,
    depth: usize,
    threads: &[crate::model::ThreadSnapshot],
    out: &mut Vec<(String, usize)>,
) {
    if threads
        .iter()
        .any(|thread| thread.thread_id == node.thread_id)
    {
        out.push((node.thread_id.clone(), depth));
    }
    for child in &node.children {
        collect_tree_nodes(child, depth + 1, threads, out);
    }
}

fn model_label(model: &Option<crate::model::ModelSpec>) -> String {
    match model {
        Some(model) => format!(
            "{} / {}",
            model.model.clone().unwrap_or_else(|| "-".to_string()),
            model
                .reasoning_effort
                .clone()
                .unwrap_or_else(|| "-".to_string())
        ),
        None => "-".to_string(),
    }
}

fn source_label(source: &Option<crate::model::EvidenceSource>) -> String {
    source
        .as_ref()
        .map(|s| s.kind.clone())
        .unwrap_or_else(|| "unknown".to_string())
}

fn render_details(
    selected: Option<&crate::model::ThreadSnapshot>,
    show_all: bool,
) -> Paragraph<'static> {
    if let Some(selected) = selected {
        let configured = model_label(&selected.model.configured.value);
        let requested = model_label(&selected.model.requested.value);
        let effective = model_label(&selected.model.effective.value);
        let req = selected
            .model
            .requested
            .value
            .as_ref()
            .and_then(|v| v.reasoning_effort.clone())
            .unwrap_or_else(|| "unknown".to_string());
        let _ = show_all;

        let mut rows = vec![
            Line::from(Span::styled(
                format!("Thread: {}", selected.thread_id),
                Style::default().add_modifier(Modifier::BOLD),
            )),
            Line::from(format!("State: {:?}", selected.state)),
            Line::from(
                selected
                    .state_detail
                    .clone()
                    .unwrap_or_else(|| "-".to_string()),
            ),
            Line::from(format!(
                "Nickname: {} (source: {})",
                selected.nickname.clone().unwrap_or_else(|| "-".to_string()),
                selected
                    .evidence
                    .nickname
                    .source
                    .as_ref()
                    .map(|s| s.kind.clone())
                    .unwrap_or_else(|| "unknown".to_string())
            )),
            Line::from(format!(
                "Role: {} (source: {})",
                selected.role.clone().unwrap_or_else(|| "-".to_string()),
                selected
                    .evidence
                    .role
                    .source
                    .as_ref()
                    .map(|s| s.kind.clone())
                    .unwrap_or_else(|| "unknown".to_string())
            )),
            Line::from(format!(
                "Parent: {} (source: {})",
                selected
                    .parent_thread_id
                    .clone()
                    .unwrap_or_else(|| "-".to_string()),
                selected
                    .evidence
                    .parent_thread_id
                    .source
                    .as_ref()
                    .map(|s| s.kind.clone())
                    .unwrap_or_else(|| "unknown".to_string())
            )),
            Line::from(format!(
                "CWD: {} (source: {})",
                selected.cwd.clone().unwrap_or_else(|| "-".to_string()),
                selected
                    .evidence
                    .cwd
                    .source
                    .as_ref()
                    .map(|s| s.kind.clone())
                    .unwrap_or_else(|| "unknown".to_string())
            )),
            Line::from(format!(
                "Source kind: {} (source: {})",
                selected
                    .source_kind
                    .clone()
                    .unwrap_or_else(|| "-".to_string()),
                selected
                    .evidence
                    .source_kind
                    .source
                    .as_ref()
                    .map(|s| s.kind.clone())
                    .unwrap_or_else(|| "unknown".to_string())
            )),
            Line::from(format!(
                "Configured model: {} (source: {})",
                configured,
                source_label(&selected.model.configured.source)
            )),
            Line::from(format!(
                "Requested model: {} (source: {})",
                requested,
                source_label(&selected.model.requested.source)
            )),
            Line::from(format!(
                "Effective model: {} (source: {})",
                effective,
                source_label(&selected.model.effective.source)
            )),
            Line::from(format!(
                "State detail: {} (source: {})",
                selected
                    .state_detail
                    .clone()
                    .unwrap_or_else(|| "-".to_string()),
                selected
                    .evidence
                    .state
                    .source
                    .as_ref()
                    .map(|s| s.kind.clone())
                    .unwrap_or_else(|| "unknown".to_string())
            )),
            Line::from(format!("Requested model effort: {}", req)),
        ];
        if show_all {
            rows.push(Line::from(Span::styled(
                "Recent activity",
                Style::default().add_modifier(Modifier::UNDERLINED),
            )));
            for a in selected.recent_activity.iter().take(10) {
                rows.push(Line::from(format!(
                    "{} {} {}",
                    a.kind,
                    a.tool_name.clone().unwrap_or_else(|| "-".to_string()),
                    a.status.clone().unwrap_or_default()
                )));
            }
            if let Some(path) = &selected.rollout_path {
                rows.push(Line::from(format!("Rollout: {path}")));
            }
        }
        return Paragraph::new(rows).block(Block::default().borders(Borders::ALL).title("Details"));
    }
    Paragraph::new("No thread selected")
        .block(Block::default().borders(Borders::ALL).title("Details"))
}

struct TerminalGuard {
    terminal: ratatui::Terminal<CrosstermBackend<io::Stdout>>,
}

impl TerminalGuard {
    fn enter() -> Result<Self> {
        enable_raw_mode()?;
        let mut stdout = io::stdout();
        execute!(
            stdout,
            EnterAlternateScreen,
            crossterm::terminal::Clear(crossterm::terminal::ClearType::All)
        )?;
        let backend = CrosstermBackend::new(stdout);
        let mut terminal = ratatui::Terminal::new(backend)?;
        terminal.hide_cursor()?;
        Ok(Self { terminal })
    }
}

impl Drop for TerminalGuard {
    fn drop(&mut self) {
        let _ = disable_raw_mode();
        let _ = execute!(
            self.terminal.backend_mut(),
            LeaveAlternateScreen,
            crossterm::terminal::Clear(crossterm::terminal::ClearType::All)
        );
        let _ = self.terminal.show_cursor();
    }
}

#[derive(Debug)]
struct TuiState {
    selected: usize,
    refresh_interval_ms: u64,
    show_details: bool,
}

impl TuiState {
    fn new() -> Self {
        Self {
            selected: 0,
            refresh_interval_ms: 1_000,
            show_details: true,
        }
    }
}
