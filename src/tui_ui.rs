use chrono::{DateTime, Duration, Utc};
use std::path::Path;
use std::time::{Duration as StdDuration, Instant};
use std::{io, io::Write};

use anyhow::{bail, Result};
use crossterm::event::{self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use crossterm::terminal::{disable_raw_mode, enable_raw_mode};
use crossterm::{
    execute,
    terminal::{EnterAlternateScreen, LeaveAlternateScreen},
};
use ratatui::{
    backend::CrosstermBackend,
    layout::{Alignment, Constraint, Direction, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Padding, Paragraph, Wrap},
    Frame,
};

use crate::cli::FilterOpts;
use crate::model::{
    AccountUsage, AccountUsageWindow, ActivitySignal, ContextUsage, KiroAccountUsage,
    LastTerminalEvent, Observed, TaskProgress, ThreadSnapshot, ThreadState, ThreadTreeNode,
    TokenUsage,
};
use crate::observer::Monitor;
use crate::runtime::RuntimeOverlay;

const REFRESH_INTERVAL_MS: u64 = 1000;
const HISTORY_LIMIT: usize = 10;
const TASK_DETAIL_LIMIT: usize = 5;
const SHORT_ID_LEN: usize = 8;
const AGENT_CARD_LINES: usize = 6;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum StateBucket {
    Running,
    Idle,
    Other,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum LocalStateFilter {
    All,
    Running,
}

impl LocalStateFilter {
    fn next(self) -> Self {
        match self {
            Self::All => Self::Running,
            Self::Running => Self::All,
        }
    }

    fn label(&self) -> &'static str {
        match self {
            Self::All => "All",
            Self::Running => "Running",
        }
    }

    fn matches(self, bucket: StateBucket) -> bool {
        match self {
            Self::All => true,
            Self::Running => matches!(bucket, StateBucket::Running),
        }
    }
}

#[derive(Debug)]
struct TuiState {
    selected: usize,
    selected_thread_id: Option<String>,
    search_query: String,
    search_mode: bool,
    state_filter: LocalStateFilter,
    show_activity: bool,
    show_technical: bool,
    show_help: bool,
}

impl TuiState {
    fn new() -> Self {
        Self {
            selected: 0,
            selected_thread_id: None,
            search_query: String::new(),
            search_mode: false,
            state_filter: LocalStateFilter::All,
            show_activity: false,
            show_technical: false,
            show_help: false,
        }
    }
}

#[derive(Clone, Debug)]
struct ListRow {
    thread_id: String,
    state: ThreadState,
    depth: usize,
    display_name: String,
    short_id: String,
    model: String,
    effort: String,
    token_total: String,
    context_usage: Option<String>,
    task_progress: Option<String>,
    origin_label: String,
    source_kind: String,
    provider_label: String,
    state_label: String,
    state_bucket: StateBucket,
    age_label: String,
    last_update: Option<DateTime<Utc>>,
    role: String,
    nickname: String,
    cwd: String,
}

pub fn run_tui(monitor: &mut Monitor, filters: &FilterOpts) -> Result<()> {
    if matches!(filters.runtime_events.as_deref(), Some("-")) {
        bail!("--runtime-events - is only supported for one-shot probe");
    }

    let mut state = TuiState::new();
    let mut _guard = TerminalGuard::enter()?;
    let mut force_refresh = true;
    let mut next_refresh_at = Instant::now();
    let mut last_refresh_at = Utc::now();
    let mut snapshot: Option<crate::model::ProbeOutput> = None;
    let mut all_rows: Vec<ListRow> = Vec::new();
    loop {
        let now = Instant::now();
        if force_refresh || now >= next_refresh_at {
            let runtime = RuntimeOverlay::from_source(filters.runtime_events.as_deref());
            let snap = monitor.probe_snapshot(filters, runtime, false)?;
            let now = Utc::now();
            last_refresh_at = now;
            let mut nodes = Vec::new();
            for node in &snap.tree {
                collect_tree_nodes(node, 0, &snap.threads, &mut nodes);
            }
            all_rows = nodes
                .into_iter()
                .filter_map(|(thread_id, depth)| {
                    snap.threads
                        .iter()
                        .find(|t| t.thread_id == thread_id)
                        .map(|thread| build_list_row(thread, depth, now))
                })
                .collect();
            snapshot = Some(snap);
            next_refresh_at = Instant::now() + StdDuration::from_millis(REFRESH_INTERVAL_MS);
            force_refresh = false;
        }

        let Some(active_snapshot) = snapshot.as_ref() else {
            continue;
        };

        let visible_rows =
            visible_rows_for_filter(&all_rows, &state.search_query, state.state_filter);

        if visible_rows.is_empty() {
            state.selected = 0;
            state.selected_thread_id = None;
        } else if let Some(selected_thread_id) = state.selected_thread_id.clone() {
            state.selected = visible_rows
                .iter()
                .position(|row| row.thread_id == selected_thread_id)
                .unwrap_or_else(|| state.selected.min(visible_rows.len().saturating_sub(1)));
        } else if state.selected >= visible_rows.len() {
            state.selected = visible_rows.len().saturating_sub(1);
        }

        if let Some(selected) = visible_rows.get(state.selected) {
            state.selected_thread_id = Some(selected.thread_id.clone());
        } else {
            state.selected_thread_id = None;
        }

        let selected_snapshot = state.selected_thread_id.as_deref().and_then(|thread_id| {
            active_snapshot
                .threads
                .iter()
                .find(|t| t.thread_id == thread_id)
        });

        let status_span = format_time_delta(Some(last_refresh_at), Utc::now());
        let counts = count_buckets(&all_rows);

        _guard.terminal.draw(|frame| {
            render_tui_view(
                frame,
                frame.area(),
                &TuiViewState {
                    visible_rows: &visible_rows,
                    selected_snapshot,
                    counts: &counts,
                    account_usage: &active_snapshot.account_usage,
                    kiro_account_usage: &active_snapshot.kiro_account_usage,
                    provider: active_snapshot.query.provider.as_deref().unwrap_or("codex"),
                    selected: state.selected,
                    state_filter: &state.state_filter,
                    last_refresh_label: status_span.as_str(),
                    show_activity: state.show_activity,
                    show_technical: state.show_technical,
                    show_help: state.show_help,
                    search_mode: state.search_mode,
                    search_query: &state.search_query,
                },
            );
        })?;

        let timeout = if state.search_mode {
            StdDuration::from_millis(100)
        } else {
            let remaining = next_refresh_at.saturating_duration_since(Instant::now());
            std::cmp::min(remaining, StdDuration::from_millis(100))
        };
        if event::poll(timeout)? {
            if let Some(KeyEvent {
                code, modifiers, ..
            }) = pressed_key_event(event::read()?)
            {
                if state.search_mode {
                    match code {
                        KeyCode::Esc => {
                            state.search_mode = false;
                            state.search_query.clear();
                        }
                        KeyCode::Enter => {
                            state.search_mode = false;
                        }
                        KeyCode::Backspace => {
                            state.search_query.pop();
                        }
                        KeyCode::Char(c) if !c.is_control() => {
                            state.search_query.push(c);
                        }
                        KeyCode::Char('?') => {
                            state.show_help = true;
                            state.search_mode = false;
                        }
                        _ => {}
                    }
                } else {
                    match code {
                        KeyCode::Char('q') | KeyCode::Esc => break,
                        KeyCode::Char('c') if modifiers.contains(KeyModifiers::CONTROL) => break,
                        KeyCode::Char('r') | KeyCode::F(5) => {
                            force_refresh = true;
                        }
                        KeyCode::Down | KeyCode::Char('j') => {
                            state.selected = state.selected.saturating_add(1);
                            if state.selected >= visible_rows.len() {
                                state.selected = visible_rows.len().saturating_sub(1);
                            }
                            if let Some(selected) = visible_rows.get(state.selected) {
                                state.selected_thread_id = Some(selected.thread_id.clone());
                            }
                        }
                        KeyCode::Up | KeyCode::Char('k') => {
                            state.selected = state.selected.saturating_sub(1);
                            if let Some(selected) = visible_rows.get(state.selected) {
                                state.selected_thread_id = Some(selected.thread_id.clone());
                            }
                        }
                        KeyCode::Char('/') => {
                            state.search_query.clear();
                            state.search_mode = true;
                            state.show_help = false;
                        }
                        KeyCode::Char('f') => {
                            state.state_filter = state.state_filter.next();
                        }
                        KeyCode::Char('i') => {
                            state.show_technical = !state.show_technical;
                        }
                        KeyCode::Char('?') => {
                            state.show_help = !state.show_help;
                        }
                        KeyCode::Enter => {
                            state.show_activity = !state.show_activity;
                        }
                        _ => {}
                    }
                }
            }
        } else if Instant::now() >= next_refresh_at {
            force_refresh = true;
        }

        if force_refresh {
            next_refresh_at = Instant::now();
        }
    }

    Ok(())
}

fn list_scroll_offset(
    selected_row: usize,
    visible_rows: usize,
    row_lines: usize,
    viewport_lines: usize,
) -> u16 {
    if visible_rows == 0 || row_lines == 0 || viewport_lines == 0 {
        return 0;
    }

    let total_lines = visible_rows.saturating_mul(row_lines);
    if total_lines <= viewport_lines {
        return 0;
    }

    let max_offset = total_lines.saturating_sub(viewport_lines);
    let row_top = selected_row.saturating_mul(row_lines);

    row_top.min(max_offset) as u16
}

struct TuiViewState<'a> {
    visible_rows: &'a [ListRow],
    selected_snapshot: Option<&'a ThreadSnapshot>,
    counts: &'a StateCounts,
    account_usage: &'a Observed<AccountUsage>,
    kiro_account_usage: &'a Observed<KiroAccountUsage>,
    provider: &'a str,
    selected: usize,
    state_filter: &'a LocalStateFilter,
    last_refresh_label: &'a str,
    show_activity: bool,
    show_technical: bool,
    show_help: bool,
    search_mode: bool,
    search_query: &'a str,
}

fn render_tui_view(frame: &mut Frame, size: Rect, render_state: &TuiViewState<'_>) {
    let is_wide = size.width >= 100;
    let header_height = if is_wide { 5 } else { 7 };

    let outer = Layout::default()
        .direction(Direction::Vertical)
        .margin(1)
        .constraints([
            Constraint::Length(header_height),
            Constraint::Min(1),
            Constraint::Length(3),
        ])
        .split(size);

    render_header(
        frame,
        outer[0],
        render_state.counts,
        render_state.account_usage,
        render_state.kiro_account_usage,
        render_state.provider,
        render_state.last_refresh_label,
        is_wide,
    );

    let body = if is_wide {
        Layout::default()
            .direction(Direction::Horizontal)
            .constraints([Constraint::Percentage(38), Constraint::Percentage(62)])
            .split(outer[1])
    } else {
        Layout::default()
            .direction(Direction::Vertical)
            .constraints([Constraint::Percentage(55), Constraint::Percentage(45)])
            .split(outer[1])
    };

    render_agents_pane(
        frame,
        body[0],
        render_state.visible_rows,
        render_state.selected,
    );

    if render_state.show_help {
        frame.render_widget(render_help(), body[1]);
    } else if let Some(selected) = render_state.selected_snapshot {
        render_details_pane(
            frame,
            body[1],
            selected,
            render_state.show_activity,
            render_state.show_technical,
            is_wide,
        );
    } else {
        frame.render_widget(
            Paragraph::new("No thread selected")
                .block(Block::default().borders(Borders::ALL).title("Details")),
            body[1],
        );
    }

    let footer = render_footer(
        render_state.search_mode,
        render_state.search_query,
        render_state.state_filter,
        outer[2].width.saturating_sub(2),
    );
    frame.render_widget(footer, outer[2]);
}

#[allow(clippy::too_many_arguments)]
fn render_header(
    frame: &mut Frame,
    area: Rect,
    counts: &StateCounts,
    account_usage: &Observed<AccountUsage>,
    kiro_account_usage: &Observed<KiroAccountUsage>,
    provider: &str,
    last_refresh: &str,
    is_wide: bool,
) {
    let border = Block::default().borders(Borders::ALL);
    let inner = border.inner(area);
    frame.render_widget(border, area);
    if inner.width == 0 || inner.height == 0 {
        return;
    }

    let title = Line::from(Span::styled(
        format!(
            "{} v{}",
            provider_title(provider),
            env!("CARGO_PKG_VERSION")
        ),
        Style::default()
            .fg(Color::Cyan)
            .add_modifier(Modifier::BOLD),
    ));

    let status_line = Line::from(vec![
        Span::styled("RUNNING ", Style::default().fg(Color::Green)),
        Span::styled(format!("{}  ", counts.running), Style::default()),
        Span::styled("IDLE ", Style::default().fg(Color::Rgb(255, 172, 51))),
        Span::styled(format!("{}  ", counts.idle), Style::default()),
        Span::styled("UNKNOWN ", Style::default().fg(Color::Gray)),
        Span::styled(format!("{}", counts.unknown), Style::default()),
    ]);

    if is_wide {
        let refresh = Line::from(Span::styled(
            format!("Auto-refresh • {}", last_refresh),
            Style::default().fg(Color::DarkGray),
        ));
        let header_rows = Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Length(1),
                Constraint::Length(1),
                Constraint::Length(1),
            ])
            .split(inner);
        let row = Layout::default()
            .direction(Direction::Horizontal)
            .constraints([
                Constraint::Percentage(34),
                Constraint::Percentage(46),
                Constraint::Percentage(20),
            ])
            .split(header_rows[0]);
        frame.render_widget(Paragraph::new(title), row[0]);
        frame.render_widget(Paragraph::new(status_line), row[1]);
        frame.render_widget(Paragraph::new(refresh).alignment(Alignment::Right), row[2]);
        let usage = usage_lines(provider, account_usage, kiro_account_usage, Utc::now());
        for (index, line) in usage.into_iter().take(2).enumerate() {
            frame.render_widget(Paragraph::new(line), header_rows[1 + index]);
        }
        return;
    }

    let header_rows = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(1),
            Constraint::Length(1),
            Constraint::Length(1),
            Constraint::Length(1),
            Constraint::Length(1),
        ])
        .split(inner);
    frame.render_widget(Paragraph::new(title), header_rows[0]);
    frame.render_widget(Paragraph::new(status_line), header_rows[1]);
    let usage = usage_lines(provider, account_usage, kiro_account_usage, Utc::now());
    for (index, line) in usage.into_iter().take(2).enumerate() {
        frame.render_widget(Paragraph::new(line), header_rows[2 + index]);
    }
    frame.render_widget(
        Paragraph::new(format!(
            "Updated: {}  •  Auto-refresh {}ms",
            last_refresh, REFRESH_INTERVAL_MS
        )),
        header_rows[4],
    );
}

fn provider_title(provider: &str) -> &'static str {
    match provider {
        "kiro" => "Kiro Agent Monitor",
        "all" => "Codex + Kiro Monitor",
        _ => "Codex Agent Monitor",
    }
}

fn usage_lines(
    provider: &str,
    account_usage: &Observed<AccountUsage>,
    kiro_account_usage: &Observed<KiroAccountUsage>,
    now: DateTime<Utc>,
) -> Vec<String> {
    let mut lines = Vec::new();
    if matches!(provider, "codex" | "all") {
        lines.push(account_usage_line("Codex usage", account_usage, now));
    }
    if matches!(provider, "kiro" | "all") {
        lines.push(kiro_account_usage_line(kiro_account_usage, now));
    }
    lines
}

fn account_usage_line(
    label: &str,
    account_usage: &Observed<AccountUsage>,
    now: DateTime<Utc>,
) -> String {
    let Some(usage) = account_usage.value.as_ref() else {
        return format!("{label}: unavailable");
    };

    let mut windows = Vec::new();
    if let Some(window) = usage.primary.as_ref() {
        windows.push(account_usage_window_label(window, now));
    }
    if let Some(window) = usage.secondary.as_ref() {
        windows.push(account_usage_window_label(window, now));
    }
    if windows.is_empty() {
        return format!("{label}: unavailable");
    }

    let observed = account_usage
        .observed_at
        .map(|timestamp| format_time_delta(Some(timestamp), now))
        .unwrap_or_else(|| "unknown age".to_string());
    format!("{label}: {}  •  observed {}", windows.join(" · "), observed)
}

fn kiro_account_usage_line(usage: &Observed<KiroAccountUsage>, now: DateTime<Utc>) -> String {
    let Some(value) = usage.value.as_ref() else {
        return format!(
            "Kiro credits: {}",
            usage.detail.as_deref().unwrap_or("unavailable")
        );
    };
    let Some(plan) = value.plan_credits.as_ref() else {
        return "Kiro credits: unavailable".to_string();
    };
    let stale = if usage.confidence == crate::model::Confidence::Low {
        "(stale) "
    } else {
        ""
    };
    let mut line = format!(
        "Kiro credits: {stale}{:.2} left / {:.2} plan credits",
        plan.remaining, plan.total
    );
    if let Some(reset) = value.billing_cycle_reset.as_deref() {
        line.push_str(&format!("  •  reset {reset}"));
    }
    for bonus in &value.bonus_credits {
        let Some(days) = bonus.days_until_expiry else {
            continue;
        };
        let expiry = if days == 0 {
            "expires today".to_string()
        } else {
            format!("expires in {days}d")
        };
        line.push_str(&format!(
            "  •  bonus {}: {:.2} left ({expiry})",
            bonus.name.as_deref().unwrap_or("credits"),
            bonus.remaining
        ));
    }
    for add_on in &value.add_on_credits {
        if add_on.is_active == Some(true) {
            line.push_str(&format!("  •  add-on: {:.2} left", add_on.remaining));
        }
    }
    let observed = usage
        .observed_at
        .map(|timestamp| format_time_delta(Some(timestamp), now))
        .unwrap_or_else(|| "unknown age".to_string());
    line.push_str(&format!("  •  observed {observed}"));
    line
}

fn account_usage_window_label(window: &AccountUsageWindow, now: DateTime<Utc>) -> String {
    let duration = window
        .window_minutes
        .map(account_usage_duration_label)
        .unwrap_or_else(|| "unknown window".to_string());
    let remaining = if window.resets_at.is_some_and(|reset| reset <= now) {
        "awaiting update".to_string()
    } else if let Some(used) = window.used_percent {
        format!("{:.0}%", (100.0 - used).clamp(0.0, 100.0))
    } else {
        "unavailable".to_string()
    };
    format!("{duration} {remaining}")
}

fn account_usage_duration_label(minutes: u64) -> String {
    match minutes {
        300 => "5h".to_string(),
        10_080 => "Weekly".to_string(),
        minutes if minutes % (24 * 60) == 0 => format!("{}d", minutes / (24 * 60)),
        minutes if minutes % 60 == 0 => format!("{}h", minutes / 60),
        minutes => format!("{}m", minutes),
    }
}

fn render_footer(
    search_mode: bool,
    query: &str,
    state_filter: &LocalStateFilter,
    width: u16,
) -> Paragraph<'static> {
    let (title, footer_text) =
        footer_text_for_width(search_mode, query, state_filter.label(), width as usize);

    let style = if search_mode {
        Style::default().fg(Color::Yellow)
    } else {
        Style::default().fg(Color::Cyan)
    };

    Paragraph::new(footer_text)
        .block(Block::default().borders(Borders::ALL).title(title))
        .alignment(Alignment::Left)
        .style(style)
}

fn footer_text_for_width(
    search_mode: bool,
    query: &str,
    filter_label: &str,
    width: usize,
) -> (String, String) {
    if search_mode {
        return footer_search_text_for_width(query, width);
    }

    let long = format!(
        "↑↓ Move  Enter More  / Search  f Filter:{filter_label}  r Refresh  i Details  ? Help  q Quit"
    );
    if long.chars().count() <= width {
        return (String::new(), long);
    }

    let medium = format!("↑↓ Move  Enter More  / Search  f Filter:{filter_label}  q Quit");
    if medium.chars().count() <= width {
        return ("".to_string(), medium);
    }

    let narrow = "↑↓ / Search  f q Quit".to_string();
    if narrow.chars().count() <= width {
        return ("".to_string(), narrow);
    }

    ("".to_string(), "q Quit".to_string())
}

fn footer_search_text_for_width(query: &str, width: usize) -> (String, String) {
    let title = "Search".to_string();
    let suffix = "  Enter apply  Esc cancel";
    if width == 0 {
        return (title, String::new());
    }

    let suffix_width = suffix.chars().count();
    if width <= suffix_width {
        return (title, truncate_by_chars(suffix, width));
    }

    let available_for_query = width.saturating_sub(suffix.chars().count() + 2);
    let query = if available_for_query == 0 {
        "".to_string()
    } else if query.chars().count() <= available_for_query {
        query.to_string()
    } else {
        format!(
            "{}…",
            truncate_by_chars(query, available_for_query.saturating_sub(1))
        )
    };
    let prefix = format!("/ {query}");
    (title, format!("{prefix}{suffix}"))
}

fn truncate_by_chars(value: &str, width: usize) -> String {
    if width == 0 {
        return String::new();
    }
    if value.chars().count() <= width {
        return value.to_string();
    }
    value.chars().take(width).collect()
}

fn render_help() -> Paragraph<'static> {
    let lines = vec![
        Line::from(Span::styled(
            "Keyboard controls",
            Style::default()
                .add_modifier(Modifier::BOLD)
                .add_modifier(Modifier::UNDERLINED),
        )),
        Line::from("q / Esc / Ctrl-C: quit"),
        Line::from("j, k, ↑, ↓: move selection"),
        Line::from("/: enter search mode"),
        Line::from("f: toggle local state filter (All ↔ Running; Running is newest first)"),
        Line::from("r or F5: refresh now"),
        Line::from("Enter: toggle recent activity"),
        Line::from("i: toggle technical details"),
        Line::from("?: hide help"),
    ];
    Paragraph::new(lines)
        .wrap(Wrap { trim: true })
        .block(Block::default().borders(Borders::ALL).title("Help"))
        .style(Style::default().fg(Color::Cyan))
}

fn render_agents_pane(frame: &mut Frame, area: Rect, rows: &[ListRow], selected: usize) {
    let list_block = Block::default().borders(Borders::ALL).title("Agents");
    let list_inner = list_block.inner(area);
    frame.render_widget(list_block, area);

    if rows.is_empty() {
        frame.render_widget(
            Paragraph::new("No threads").alignment(Alignment::Center),
            list_inner,
        );
        return;
    }

    let viewport_rows = list_inner.height as usize;
    let scroll_offset = list_scroll_offset(selected, rows.len(), AGENT_CARD_LINES, viewport_rows);
    let first_visible = (scroll_offset as usize) / AGENT_CARD_LINES;
    let start_y = list_inner.y;
    let mut y = start_y;
    for (idx, row) in rows.iter().enumerate().skip(first_visible) {
        let card_top = (idx - first_visible) as u16 * AGENT_CARD_LINES as u16;
        let remaining = list_inner.height.saturating_sub(card_top);
        if remaining == 0 {
            break;
        }
        let card_height = AGENT_CARD_LINES.min(remaining as usize) as u16;
        let card_area = Rect::new(list_inner.x, y, list_inner.width, card_height);
        render_agent_card(frame, card_area, row, idx == selected);
        y = y.saturating_add(card_height);
    }
}

fn render_agent_card(frame: &mut Frame, area: Rect, row: &ListRow, is_selected: bool) {
    let mut lines: Vec<Line> = Vec::new();
    let state_style = state_style(row.state.clone(), row.state_bucket);
    let state_text = human_state_label(&row.state_label);

    let prefix = tree_indent(row.depth);
    let title = Line::from(vec![
        Span::styled(prefix, Style::default().fg(Color::DarkGray)),
        Span::styled("● ", state_style),
        Span::styled(
            format!("[{}] ", row.provider_label),
            Style::default()
                .fg(if row.provider_label == "Kiro" {
                    Color::Magenta
                } else {
                    Color::Blue
                })
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled(
            row.display_name.clone(),
            Style::default()
                .fg(Color::White)
                .add_modifier(Modifier::BOLD),
        ),
    ]);
    lines.push(title);

    lines.push(Line::from(vec![
        Span::styled(state_text, state_style),
        Span::raw(format!(" • {}", row.age_label)),
    ]));

    let mut summary_metrics = Vec::new();
    if let Some(task_progress) = row.task_progress.as_deref() {
        summary_metrics.push(task_progress);
    }
    if let Some(context_usage) = row.context_usage.as_deref() {
        summary_metrics.push(context_usage);
    }
    if summary_metrics.is_empty() {
        summary_metrics.push(row.token_total.as_str());
    }
    let summary_metric = summary_metrics.join(" • ");
    lines.push(Line::from(vec![
        Span::styled(
            format!("{} • {} • ", row.model, row.effort),
            Style::default(),
        ),
        Span::styled(summary_metric, Style::default().fg(Color::Yellow)),
    ]));

    lines.push(Line::from(Span::styled(
        if row.source_kind.is_empty() {
            row.origin_label.clone()
        } else {
            format!("{} • {}", row.source_kind, row.origin_label)
        },
        Style::default(),
    )));

    while lines.len() < AGENT_CARD_LINES.saturating_sub(1) {
        lines.push(Line::from(" "));
    }

    let border_style = if is_selected {
        Style::default().fg(Color::Cyan)
    } else {
        Style::default().fg(Color::DarkGray)
    };
    let block = if is_selected {
        Block::default()
            .borders(Borders::ALL)
            .padding(Padding::horizontal(1))
            .border_style(border_style)
    } else {
        Block::default()
            .borders(Borders::BOTTOM)
            .padding(Padding::horizontal(1))
            .border_style(border_style)
    };
    if !is_selected && lines.len() < AGENT_CARD_LINES {
        lines.push(Line::from(" "));
    }
    frame.render_widget(
        Paragraph::new(lines)
            .alignment(Alignment::Left)
            .block(block),
        area,
    );
}

fn render_details_pane(
    frame: &mut Frame,
    area: Rect,
    selected: &ThreadSnapshot,
    show_activity: bool,
    show_technical: bool,
    is_wide: bool,
) {
    let panel = Block::default().borders(Borders::ALL).title("Details");
    let inner = panel.inner(area);
    frame.render_widget(panel, area);

    if inner.height < 2 || inner.width < 2 {
        return;
    }

    let (model, effort) = preferred_model_and_effort(selected);
    let (origin, cwd_path) = origin_label(selected.cwd.as_deref());
    let state_text = human_state_label(state_label(selected.state.clone()).as_str());
    let age = format_time_delta(last_update_for_thread(selected), Utc::now());

    let mut lines = Vec::new();
    lines.push(Line::from(vec![Span::styled(
        display_name(selected),
        Style::default().add_modifier(Modifier::BOLD),
    )]));
    if let Some(source_kind) = selected.source_kind.as_deref() {
        lines.push(Line::from(format!(
            "Provider: {}  •  Source: {}",
            provider_for_source_kind(Some(source_kind)),
            friendly_source_kind(source_kind),
        )));
    }
    lines.push(Line::from(vec![
        Span::styled(
            state_text,
            state_style(
                selected.state.clone(),
                bucket_for_state(selected.state.clone()),
            ),
        ),
        Span::styled(format!(" • {}", age), Style::default().fg(Color::DarkGray)),
    ]));
    lines.push(horizontal_divider(inner.width));
    lines.push(Line::from(Span::styled(
        "Current activity",
        Style::default()
            .fg(Color::Cyan)
            .add_modifier(Modifier::BOLD),
    )));
    lines.push(Line::from(current_activity_line(selected)));
    if let Some(progress) = selected.task_progress.value.as_ref() {
        lines.push(horizontal_divider(inner.width));
        lines.push(Line::from(Span::styled(
            "Task progress",
            Style::default()
                .fg(Color::Cyan)
                .add_modifier(Modifier::BOLD),
        )));
        lines.push(Line::from(task_progress_detail_label(progress)));
        if progress.tasks.is_empty() {
            lines.push(Line::from("No persisted tasks"));
        } else {
            for task in progress.tasks.iter().take(TASK_DETAIL_LIMIT) {
                lines.push(Line::from(format!(
                    "  #{} {}",
                    task.id,
                    task.status.as_str()
                )));
            }
            if progress.tasks.len() > TASK_DETAIL_LIMIT {
                lines.push(Line::from(format!(
                    "  +{} more",
                    progress.tasks.len() - TASK_DETAIL_LIMIT
                )));
            }
        }
    } else if let Some(detail) = selected
        .task_progress
        .detail
        .as_deref()
        .filter(|detail| *detail != "No local evidence")
    {
        lines.push(horizontal_divider(inner.width));
        lines.push(Line::from(Span::styled(
            "Task progress",
            Style::default()
                .fg(Color::Cyan)
                .add_modifier(Modifier::BOLD),
        )));
        lines.push(Line::from(format!("Unavailable: {detail}")));
    }
    lines.push(horizontal_divider(inner.width));
    if let Some(event) = selected.last_terminal_event.value.as_ref() {
        lines.push(Line::from(format!(
            "Last terminal result: {}",
            terminal_event_label(event)
        )));
    }

    if is_wide {
        let total_width = inner.width as usize;
        let model_col = total_width / 2;
        let effort_col = total_width.saturating_sub(model_col);
        let heading = format!(
            "{:<model_width$}{:<effort_width$}",
            "Model",
            "Effort",
            model_width = model_col,
            effort_width = effort_col.max(1),
        );
        lines.push(Line::from(Span::styled(
            heading,
            Style::default()
                .fg(Color::Cyan)
                .add_modifier(Modifier::BOLD),
        )));
        lines.push(Line::from(format!(
            "{:<width$}{:<width2$}",
            model,
            effort.to_string(),
            width = model_col,
            width2 = effort_col.max(1),
        )));
    } else {
        lines.push(horizontal_divider(inner.width));
        lines.push(Line::from(Span::styled(
            "Model",
            Style::default()
                .fg(Color::Cyan)
                .add_modifier(Modifier::BOLD),
        )));
        lines.push(Line::from(format!("  {}", model)));
        lines.push(Line::from(Span::styled(
            "Effort",
            Style::default()
                .fg(Color::Cyan)
                .add_modifier(Modifier::BOLD),
        )));
        lines.push(Line::from(format!("  {}", effort)));
    }

    lines.push(horizontal_divider(inner.width));
    lines.push(Line::from(Span::styled(
        "Token usage",
        Style::default()
            .fg(Color::Cyan)
            .add_modifier(Modifier::BOLD),
    )));
    lines.push(Line::from(format_token_usage_details(
        &selected.token_usage,
    )));
    if selected.context_usage.value.is_some() {
        lines.push(horizontal_divider(inner.width));
        lines.push(Line::from(Span::styled(
            "Context usage",
            Style::default()
                .fg(Color::Cyan)
                .add_modifier(Modifier::BOLD),
        )));
        lines.push(Line::from(format_context_usage_details(
            &selected.context_usage,
        )));
    }

    lines.push(horizontal_divider(inner.width));
    lines.push(Line::from(Span::styled(
        "Recent activity (UTC)",
        Style::default()
            .fg(Color::Cyan)
            .add_modifier(Modifier::BOLD),
    )));

    let limit = if show_activity { HISTORY_LIMIT } else { 3 };
    let activity_rows = latest_recent_activity(&selected.recent_activity, limit);
    if activity_rows.is_empty() {
        lines.push(Line::from("No recent activity"));
    } else {
        for entry in activity_rows {
            let ts = entry
                .timestamp
                .as_ref()
                .map(format_timestamp_short)
                .unwrap_or_else(|| "-".to_string());
            lines.push(Line::from(format!(
                "{:<7}  {:<12} {:<14} {}",
                ts,
                entry.kind.replace('_', " "),
                entry.tool_name.as_deref().unwrap_or("-").replace('_', " "),
                entry.status.as_deref().unwrap_or("-").replace('_', " "),
            )));
        }
    }
    lines.push(Line::from(format!(
        "Press Enter to {} recent activity",
        if show_activity {
            "show less"
        } else {
            "show more"
        }
    )));

    if show_technical {
        lines.push(Line::from(" "));
        lines.push(Line::from(Span::styled(
            "Technical details",
            Style::default()
                .fg(Color::Cyan)
                .add_modifier(Modifier::BOLD),
        )));
        if let Some(event) = selected.last_terminal_event.value.as_ref() {
            let source = selected
                .last_terminal_event
                .source
                .as_ref()
                .map(|value| value.kind.as_str())
                .unwrap_or("unknown");
            let observed_at = selected
                .last_terminal_event
                .observed_at
                .map(|value| value.to_rfc3339())
                .unwrap_or_else(|| "unknown".to_string());
            lines.push(Line::from(format!(
                "Terminal evidence: {} source={} observed_at={} confidence={:?}",
                terminal_event_label(event),
                source,
                observed_at,
                selected.last_terminal_event.confidence
            )));
        }
        lines.push(Line::from(format!("Full UUID: {}", selected.thread_id)));
        lines.push(Line::from(format!("Detected origin: {}", origin)));
        if !cwd_path.is_empty() {
            lines.push(Line::from(format!("Full path: {}", cwd_path)));
        }
        if let Some(rollout_path) = &selected.rollout_path {
            lines.push(Line::from(format!("Rollout path: {}", rollout_path)));
        }
        lines.push(Line::from("Press i to collapse"));
    } else {
        lines.push(Line::from(Span::styled(
            "Technical details",
            Style::default()
                .fg(Color::Cyan)
                .add_modifier(Modifier::BOLD),
        )));
        lines.push(Line::from("Press i to expand"));
    }

    frame.render_widget(Paragraph::new(lines).wrap(Wrap { trim: true }), inner);
}

fn horizontal_divider(width: u16) -> Line<'static> {
    let safe_width = width.saturating_sub(1) as usize;
    if safe_width == 0 {
        return Line::from("");
    }
    Line::from("─".repeat(safe_width))
}

fn human_state_label(state_label: &str) -> &'static str {
    match state_label {
        "RUNNING" => "Running",
        "IDLE" => "Idle",
        "UNKNOWN" => "Unknown",
        _ => "Unknown",
    }
}

fn current_activity_line(thread: &ThreadSnapshot) -> String {
    let signal = format!(
        " [activity: {}]",
        activity_signal_label(&thread.activity_signal)
    );
    if let Some(activity) = thread.recent_activity.last() {
        let kind = activity.kind.replace('_', " ");
        let tool = activity
            .tool_name
            .clone()
            .unwrap_or_else(|| "-".to_string())
            .replace('_', " ");
        let status = activity
            .status
            .clone()
            .unwrap_or_else(|| "-".to_string())
            .replace('_', " ");
        format!("{} {} {}{}", kind, tool, status, signal)
    } else {
        state_label(thread.state.clone()) + &signal
    }
}

fn activity_signal_label(signal: &Observed<ActivitySignal>) -> &'static str {
    match signal.value {
        Some(ActivitySignal::Recent) => "recent",
        Some(ActivitySignal::Stale) => "stale",
        Some(ActivitySignal::Unknown) | None => "unknown",
    }
}

fn terminal_event_label(event: &LastTerminalEvent) -> &'static str {
    match event {
        LastTerminalEvent::Completed => "completed",
        LastTerminalEvent::Failed => "failed",
        LastTerminalEvent::Interrupted => "interrupted",
    }
}

fn latest_recent_activity(
    activities: &[crate::model::ThreadActivity],
    limit: usize,
) -> Vec<&crate::model::ThreadActivity> {
    activities.iter().rev().take(limit).collect()
}

fn format_timestamp_short(ts: &DateTime<Utc>) -> String {
    ts.format("%H:%M").to_string()
}

fn last_update_for_thread(thread: &ThreadSnapshot) -> Option<DateTime<Utc>> {
    thread
        .recency_at
        .or(thread.updated_at)
        .or(thread.created_at)
}

fn state_style(state: ThreadState, bucket: StateBucket) -> Style {
    match (state, bucket) {
        (ThreadState::Running, _) => Style::default().fg(Color::Green),
        (ThreadState::Idle, _) => Style::default().fg(Color::Rgb(255, 172, 51)),
        _ => Style::default().fg(Color::DarkGray),
    }
}

fn bucket_for_state(state: ThreadState) -> StateBucket {
    match state {
        ThreadState::Running => StateBucket::Running,
        ThreadState::Idle => StateBucket::Idle,
        ThreadState::Unknown => StateBucket::Other,
    }
}

fn build_list_row(thread: &ThreadSnapshot, depth: usize, now: DateTime<Utc>) -> ListRow {
    let (model, effort) = preferred_model_and_effort(thread);
    let (origin_label, _) = origin_label(thread.cwd.as_deref());
    let short_id = short_id(&thread.thread_id);
    let age_label = format_time_delta(last_update_for_thread(thread), now);
    let bucket = bucket_for_state(thread.state.clone());
    let state_label = state_label(thread.state.clone());
    ListRow {
        thread_id: thread.thread_id.clone(),
        state: thread.state.clone(),
        depth,
        display_name: display_name(thread),
        short_id,
        model,
        effort,
        token_total: token_total_label(&thread.token_usage),
        context_usage: context_usage_card_label(&thread.context_usage),
        task_progress: task_progress_card_label(&thread.task_progress),
        origin_label,
        source_kind: thread.source_kind.clone().unwrap_or_default(),
        provider_label: provider_for_source_kind(thread.source_kind.as_deref()),
        state_label,
        state_bucket: bucket,
        age_label,
        last_update: last_update_for_thread(thread),
        role: thread.role.clone().unwrap_or_else(|| "-".to_string()),
        nickname: thread.nickname.clone().unwrap_or_else(|| "-".to_string()),
        cwd: thread.cwd.clone().unwrap_or_else(|| "-".to_string()),
    }
}

fn token_total_label(observed: &Observed<TokenUsage>) -> String {
    let total = observed
        .value
        .as_ref()
        .and_then(|usage| usage.total_tokens)
        .map(format_compact_count)
        .unwrap_or_else(|| "?".to_string());
    format!("Tokens {total}")
}

fn context_usage_card_label(observed: &Observed<ContextUsage>) -> Option<String> {
    observed
        .value
        .as_ref()
        .map(|usage| format!("Ctx ~{}", format_compact_count(usage.used_tokens_approx)))
}

fn task_progress_card_label(observed: &Observed<TaskProgress>) -> Option<String> {
    observed.value.as_ref().map(|progress| {
        format!(
            "Tasks {}/{}",
            progress.completed_count(),
            progress.tasks.len()
        )
    })
}

fn task_progress_detail_label(progress: &TaskProgress) -> String {
    let unknown = progress.tasks.len().saturating_sub(
        progress.completed_count() + progress.in_progress_count() + progress.pending_count(),
    );
    let mut label = format!(
        "{}/{} completed • {} active • {} pending",
        progress.completed_count(),
        progress.tasks.len(),
        progress.in_progress_count(),
        progress.pending_count(),
    );
    if unknown > 0 {
        label.push_str(&format!(" • {unknown} unknown"));
    }
    label
}

fn format_token_usage_details(observed: &Observed<TokenUsage>) -> String {
    let Some(usage) = observed.value.as_ref() else {
        return format!(
            "total=?  input=?  cached input=?  cache-write input=?  output=?  reasoning output=?  context window=?  (source={})",
            source_label(observed)
        );
    };
    format!(
        "total={}  input={}  cached input={}  cache-write input={}  output={}  reasoning output={}  context window={}  (source={})",
        format_optional_count(usage.total_tokens),
        format_optional_count(usage.input_tokens),
        format_optional_count(usage.cached_input_tokens),
        format_optional_count(usage.cache_write_input_tokens),
        format_optional_count(usage.output_tokens),
        format_optional_count(usage.reasoning_output_tokens),
        format_optional_count(usage.context_window),
        source_label(observed),
    )
}

fn format_context_usage_details(observed: &Observed<ContextUsage>) -> String {
    let Some(usage) = observed.value.as_ref() else {
        return format!("unavailable (source={})", source_label(observed));
    };
    format!(
        "{:.1}% used  •  approximately {} / {} tokens  •  current, not cumulative  (source={})",
        usage.used_percent,
        format_count(usage.used_tokens_approx),
        format_count(usage.context_window_tokens),
        source_label(observed),
    )
}

fn source_label<T>(observed: &Observed<T>) -> &str {
    observed
        .source
        .as_ref()
        .map(|source| source.kind.as_str())
        .unwrap_or("unknown")
}

fn format_optional_count(value: Option<u64>) -> String {
    value.map(format_count).unwrap_or_else(|| "?".to_string())
}

fn format_count(value: u64) -> String {
    let digits = value.to_string();
    let mut out = String::with_capacity(digits.len() + digits.len() / 3);
    for (index, digit) in digits.chars().enumerate() {
        if index > 0 && (digits.len() - index).is_multiple_of(3) {
            out.push(',');
        }
        out.push(digit);
    }
    out
}

fn format_compact_count(value: u64) -> String {
    if value >= 1_000_000 {
        format!("{:.1}M", value as f64 / 1_000_000.0)
    } else if value >= 1_000 {
        format!("{:.1}k", value as f64 / 1_000.0)
    } else {
        value.to_string()
    }
}

fn count_buckets(rows: &[ListRow]) -> StateCounts {
    let mut counts = StateCounts::default();
    for row in rows {
        match row.state_bucket {
            StateBucket::Running => counts.running += 1,
            StateBucket::Idle => counts.idle += 1,
            StateBucket::Other => counts.unknown += 1,
        }
    }
    counts
}

fn matches_filter_query(row: &ListRow, query: &str) -> bool {
    let q = query.trim().to_ascii_lowercase();
    if q.is_empty() {
        return true;
    }
    let haystack = format!(
        "{} {} {} {} {} {} {} {} {}",
        row.display_name,
        row.short_id,
        row.thread_id,
        row.role,
        row.nickname,
        row.cwd,
        row.model,
        row.effort,
        row.provider_label
    )
    .to_ascii_lowercase();
    haystack.contains(&q)
}

fn provider_for_source_kind(source_kind: Option<&str>) -> String {
    if source_kind.is_some_and(|kind| kind.starts_with("kiro_")) {
        "Kiro".to_string()
    } else {
        "Codex".to_string()
    }
}

fn friendly_source_kind(source_kind: &str) -> &str {
    match source_kind {
        "kiro_cli" => "Kiro CLI",
        "kiro_acp" => "Kiro ACP worker",
        _ => "Codex",
    }
}

fn visible_rows_for_filter(
    all_rows: &[ListRow],
    query: &str,
    state_filter: LocalStateFilter,
) -> Vec<ListRow> {
    let mut visible_rows: Vec<ListRow> = all_rows
        .iter()
        .filter(|row| matches_filter_query(row, query) && state_filter.matches(row.state_bucket))
        .cloned()
        .collect();
    if matches!(state_filter, LocalStateFilter::Running) {
        // Vec::sort_by is stable, so the existing tree order remains the
        // deterministic fallback for ties and unknown timestamps.
        visible_rows.sort_by_key(|row| std::cmp::Reverse(row.last_update));
    }
    visible_rows
}

fn tree_indent(depth: usize) -> String {
    if depth == 0 {
        "  ".to_string()
    } else {
        let mut prefix = "  ".repeat(depth);
        prefix.push_str("└─ ");
        prefix
    }
}

fn short_id(id: &str) -> String {
    let id = id.trim();
    if id.chars().count() <= SHORT_ID_LEN {
        id.to_string()
    } else {
        id.chars()
            .rev()
            .take(SHORT_ID_LEN)
            .collect::<Vec<_>>()
            .into_iter()
            .rev()
            .collect()
    }
}

fn format_time_delta(ts: Option<DateTime<Utc>>, now: DateTime<Utc>) -> String {
    match ts {
        None => "-".to_string(),
        Some(ts) => {
            let mut delta = now - ts;
            if delta < Duration::zero() {
                delta = Duration::zero();
            }
            if delta < Duration::seconds(1) {
                return "just now".to_string();
            }
            let secs = delta.num_seconds();
            if secs < 60 {
                format!("{secs}s")
            } else if secs < 3600 {
                format!("{}m", secs / 60)
            } else if secs < 86_400 {
                format!("{}h", secs / 3600)
            } else if secs < 604_800 {
                format!("{}d", secs / 86_400)
            } else {
                format!("{}w", secs / 604_800)
            }
        }
    }
}

fn state_label(state: ThreadState) -> String {
    match state {
        ThreadState::Running => "RUNNING",
        ThreadState::Idle => "IDLE",
        ThreadState::Unknown => "UNKNOWN",
    }
    .to_string()
}

fn preferred_model_and_effort(snapshot: &ThreadSnapshot) -> (String, String) {
    let effective = snapshot.model.effective.value.as_ref();
    let requested = snapshot.model.requested.value.as_ref();
    let configured = snapshot.model.configured.value.as_ref();
    effective
        .or(requested)
        .or(configured)
        .map(|spec| {
            (
                spec.model.clone().unwrap_or_else(|| "-".to_string()),
                spec.reasoning_effort
                    .clone()
                    .unwrap_or_else(|| "-".to_string()),
            )
        })
        .unwrap_or_else(|| ("-".to_string(), "-".to_string()))
}

fn display_name(snapshot: &ThreadSnapshot) -> String {
    if snapshot
        .nickname
        .as_deref()
        .is_some_and(|nickname| !nickname.trim().is_empty())
    {
        return snapshot.nickname.clone().unwrap_or_default();
    }
    if let Some(role) = snapshot
        .role
        .as_deref()
        .filter(|role| !role.trim().is_empty())
    {
        let generic_kiro_role = snapshot
            .source_kind
            .as_deref()
            .is_some_and(|kind| kind.starts_with("kiro_"))
            && matches!(role, "default" | "kiro_default");
        if !generic_kiro_role {
            return role.to_string();
        }
    }
    if snapshot.parent_thread_id.is_some() {
        format!("Worker {}", short_id(&snapshot.thread_id))
    } else {
        format!("Main task {}", short_id(&snapshot.thread_id))
    }
}

fn origin_label(cwd: Option<&str>) -> (String, String) {
    let Some(cwd) = cwd.and_then(normalize_path_for_display) else {
        return ("From: unknown".to_string(), "unknown".to_string());
    };
    let path = Path::new(&cwd);
    for ancestor in path
        .ancestors()
        .filter(|ancestor| !ancestor.as_os_str().is_empty())
    {
        let git_file = ancestor.join(".git");
        if git_file.is_dir() || git_file.is_file() {
            let repo_name = ancestor
                .file_name()
                .and_then(|name| name.to_str())
                .unwrap_or("-");
            return (format!("Project: {}", repo_name), cwd.to_string());
        }
    }
    (format!("From: {cwd}"), cwd.to_string())
}

fn normalize_path_for_display(cwd: &str) -> Option<String> {
    let trimmed = cwd.trim();
    if trimmed.is_empty() {
        return None;
    }
    let trimmed = trimmed
        .strip_prefix(r"\\?\")
        .or_else(|| trimmed.strip_prefix(r"//?/"))
        .unwrap_or(trimmed);
    Some(trimmed.to_string())
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

#[derive(Default)]
struct StateCounts {
    running: usize,
    idle: usize,
    unknown: usize,
}

fn pressed_key_event(event: Event) -> Option<KeyEvent> {
    match event {
        Event::Key(key_event) if key_event.kind == KeyEventKind::Press => Some(key_event),
        _ => None,
    }
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
        // Flush to avoid stale frame after entering alternate screen on some terminals.
        stdout.flush()?;
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

#[cfg(test)]
mod tests {
    use crate::model::{
        AccountUsage, AccountUsageWindow, Confidence, EvidenceSource, KiroAccountUsage,
        KiroCreditBalance, LastTerminalEvent, ModelSpec, TokenUsage,
    };
    use chrono::Duration;
    use chrono::Utc;
    use ratatui::backend::TestBackend;
    use ratatui::Terminal;
    use std::fs;
    use tempfile::TempDir;

    use super::*;

    #[allow(clippy::too_many_arguments)]
    pub(super) fn make_snapshot(
        thread_id: &str,
        nickname: Option<&str>,
        role: Option<&str>,
        parent_thread_id: Option<&str>,
        state: ThreadState,
        _state_detail: Option<&str>,
        cwd: Option<&str>,
        effective: Option<(&str, &str)>,
        requested: Option<(&str, &str)>,
        configured: Option<(&str, &str)>,
    ) -> ThreadSnapshot {
        ThreadSnapshot {
            thread_id: thread_id.to_string(),
            nickname: nickname.map(std::string::ToString::to_string),
            role: role.map(std::string::ToString::to_string),
            parent_thread_id: parent_thread_id.map(std::string::ToString::to_string),
            cwd: cwd.map(std::string::ToString::to_string),
            source_kind: None,
            children: Vec::new(),
            project: None,
            state: state.clone(),
            last_terminal_event: crate::model::Observed::unknown(),
            activity_signal: crate::model::Observed::unknown(),
            model: crate::model::ModelSummary {
                configured: crate::model::Observed {
                    value: configured.map(|(m, e)| ModelSpec {
                        model: Some(m.to_string()),
                        reasoning_effort: Some(e.to_string()),
                    }),
                    source: None,
                    observed_at: None,
                    confidence: crate::model::Confidence::Low,
                    detail: None,
                },
                requested: crate::model::Observed {
                    value: requested.map(|(m, e)| ModelSpec {
                        model: Some(m.to_string()),
                        reasoning_effort: Some(e.to_string()),
                    }),
                    source: None,
                    observed_at: None,
                    confidence: crate::model::Confidence::Low,
                    detail: None,
                },
                effective: crate::model::Observed {
                    value: effective.map(|(m, e)| ModelSpec {
                        model: Some(m.to_string()),
                        reasoning_effort: Some(e.to_string()),
                    }),
                    source: None,
                    observed_at: None,
                    confidence: crate::model::Confidence::Low,
                    detail: None,
                },
                rerouted_from: None,
                reroute_reason: None,
            },
            token_usage: crate::model::Observed::unknown(),
            context_usage: crate::model::Observed::unknown(),
            task_progress: crate::model::Observed::unknown(),
            created_at: None,
            updated_at: None,
            recency_at: None,
            rollout_path: None,
            warnings: Vec::new(),
            recent_activity: Vec::new(),
            evidence: crate::model::ThreadEvidence {
                nickname: crate::model::Observed {
                    value: nickname.map(std::string::ToString::to_string),
                    source: Some(crate::model::EvidenceSource {
                        kind: "evidence".to_string(),
                        detail: None,
                    }),
                    observed_at: None,
                    confidence: crate::model::Confidence::Low,
                    detail: None,
                },
                role: crate::model::Observed {
                    value: role.map(std::string::ToString::to_string),
                    source: Some(crate::model::EvidenceSource {
                        kind: "evidence".to_string(),
                        detail: None,
                    }),
                    observed_at: None,
                    confidence: crate::model::Confidence::Low,
                    detail: None,
                },
                parent_thread_id: crate::model::Observed {
                    value: parent_thread_id.map(std::string::ToString::to_string),
                    source: Some(crate::model::EvidenceSource {
                        kind: "evidence".to_string(),
                        detail: None,
                    }),
                    observed_at: None,
                    confidence: crate::model::Confidence::Low,
                    detail: None,
                },
                state: crate::model::Observed {
                    value: Some(state.clone()),
                    source: Some(crate::model::EvidenceSource {
                        kind: "evidence".to_string(),
                        detail: None,
                    }),
                    observed_at: None,
                    confidence: crate::model::Confidence::Low,
                    detail: None,
                },
                cwd: crate::model::Observed {
                    value: cwd.map(std::string::ToString::to_string),
                    source: Some(crate::model::EvidenceSource {
                        kind: "evidence".to_string(),
                        detail: None,
                    }),
                    observed_at: None,
                    confidence: crate::model::Confidence::Low,
                    detail: None,
                },
                source_kind: crate::model::Observed {
                    value: None,
                    source: None,
                    observed_at: None,
                    confidence: crate::model::Confidence::Low,
                    detail: None,
                },
                latest_activity: crate::model::Observed {
                    value: None,
                    source: None,
                    observed_at: None,
                    confidence: crate::model::Confidence::Low,
                    detail: None,
                },
            },
        }
    }

    #[test]
    fn pressed_key_event_filters_non_press_keyboard_events() {
        let key_event =
            |kind| KeyEvent::new_with_kind(KeyCode::Char('x'), KeyModifiers::empty(), kind);

        let pressed = pressed_key_event(Event::Key(key_event(KeyEventKind::Press)));
        assert!(matches!(pressed, Some(event) if event.code == KeyCode::Char('x')));

        for kind in [KeyEventKind::Repeat, KeyEventKind::Release] {
            assert!(pressed_key_event(Event::Key(key_event(kind))).is_none());
        }
        assert!(pressed_key_event(Event::Resize(80, 24)).is_none());
    }

    #[test]
    fn fallback_display_name_prefers_nickname_then_role_then_id_fallbacks() {
        let root = make_snapshot(
            "root-thread",
            Some("alpha"),
            Some("planner"),
            None,
            ThreadState::Running,
            Some("running"),
            None,
            None,
            None,
            None,
        );
        assert_eq!(display_name(&root), "alpha");

        let role = make_snapshot(
            "role-thread",
            None,
            Some("builder"),
            None,
            ThreadState::Running,
            Some("running"),
            None,
            None,
            None,
            None,
        );
        assert_eq!(display_name(&role), "builder");

        let child = make_snapshot(
            "child-thread",
            None,
            None,
            Some("root-thread"),
            ThreadState::Running,
            Some("running"),
            None,
            None,
            None,
            None,
        );
        let child_name = display_name(&child);
        assert!(child_name.starts_with("Worker "));
        assert!(child_name.ends_with(&short_id(&child.thread_id)));

        let root_fallback = make_snapshot(
            "main-thread",
            None,
            None,
            None,
            ThreadState::Running,
            Some("running"),
            None,
            None,
            None,
            None,
        );
        let root_name = display_name(&root_fallback);
        assert!(root_name.starts_with("Main task "));
        assert!(root_name.ends_with(&short_id(&root_fallback.thread_id)));
    }

    #[test]
    fn short_id_uses_last_8_characters() {
        assert_eq!(short_id("1234567890abcdef"), "90abcdef");
        assert_eq!(short_id("short"), "short");
    }

    #[test]
    fn age_prefers_recency_over_updated_over_created() {
        let created = Utc::now() - Duration::seconds(300);
        let updated = created + Duration::seconds(200);
        let recency = created + Duration::seconds(280);
        let mut thread = make_snapshot(
            "age",
            None,
            None,
            None,
            ThreadState::Running,
            Some("running"),
            None,
            None,
            None,
            None,
        );
        thread.created_at = Some(created);
        thread.updated_at = Some(updated);
        thread.recency_at = Some(recency);
        let ts = last_update_for_thread(&thread).expect("recency exists");
        assert_eq!(ts, recency);
    }

    #[test]
    fn latest_activity_is_newest_first() {
        let base_time = Utc::now();
        let thread = ThreadSnapshot {
            thread_id: "t".to_string(),
            nickname: None,
            role: None,
            parent_thread_id: None,
            cwd: None,
            source_kind: None,
            children: Vec::new(),
            project: None,
            state: ThreadState::Running,
            last_terminal_event: crate::model::Observed::unknown(),
            activity_signal: crate::model::Observed::unknown(),
            model: crate::model::ModelSummary {
                configured: crate::model::Observed {
                    value: None,
                    source: None,
                    observed_at: None,
                    confidence: crate::model::Confidence::Low,
                    detail: None,
                },
                requested: crate::model::Observed {
                    value: None,
                    source: None,
                    observed_at: None,
                    confidence: crate::model::Confidence::Low,
                    detail: None,
                },
                effective: crate::model::Observed {
                    value: None,
                    source: None,
                    observed_at: None,
                    confidence: crate::model::Confidence::Low,
                    detail: None,
                },
                rerouted_from: None,
                reroute_reason: None,
            },
            token_usage: crate::model::Observed::unknown(),
            context_usage: crate::model::Observed::unknown(),
            task_progress: crate::model::Observed::unknown(),
            created_at: None,
            updated_at: None,
            recency_at: None,
            rollout_path: None,
            warnings: Vec::new(),
            recent_activity: vec![
                crate::model::ThreadActivity {
                    kind: "old".to_string(),
                    tool_name: Some("tool".to_string()),
                    status: Some("start".to_string()),
                    timestamp: Some(base_time - Duration::minutes(2)),
                },
                crate::model::ThreadActivity {
                    kind: "new".to_string(),
                    tool_name: Some("tool".to_string()),
                    status: Some("stop".to_string()),
                    timestamp: Some(base_time),
                },
            ],
            evidence: crate::model::ThreadEvidence {
                nickname: crate::model::Observed {
                    value: None,
                    source: None,
                    observed_at: None,
                    confidence: crate::model::Confidence::Low,
                    detail: None,
                },
                role: crate::model::Observed {
                    value: None,
                    source: None,
                    observed_at: None,
                    confidence: crate::model::Confidence::Low,
                    detail: None,
                },
                parent_thread_id: crate::model::Observed {
                    value: None,
                    source: None,
                    observed_at: None,
                    confidence: crate::model::Confidence::Low,
                    detail: None,
                },
                state: crate::model::Observed {
                    value: Some(ThreadState::Running),
                    source: None,
                    observed_at: None,
                    confidence: crate::model::Confidence::Low,
                    detail: None,
                },
                cwd: crate::model::Observed {
                    value: None,
                    source: None,
                    observed_at: None,
                    confidence: crate::model::Confidence::Low,
                    detail: None,
                },
                source_kind: crate::model::Observed {
                    value: None,
                    source: None,
                    observed_at: None,
                    confidence: crate::model::Confidence::Low,
                    detail: None,
                },
                latest_activity: crate::model::Observed {
                    value: None,
                    source: None,
                    observed_at: None,
                    confidence: crate::model::Confidence::Low,
                    detail: None,
                },
            },
        };
        let latest = latest_recent_activity(&thread.recent_activity, 10);
        assert_eq!(latest.len(), 2);
        assert_eq!(latest[0].kind, "new");
        assert_eq!(latest[1].kind, "old");
    }

    #[test]
    fn render_tui_wide_layout_shows_compact_cards_and_no_duplicate_short_id() {
        let thread = make_snapshot(
            "thread-id-11111111112222333344445555",
            None,
            None,
            None,
            ThreadState::Running,
            Some("running"),
            Some("C:\\Users\\Work\\Projects\\Example"),
            Some(("m", "high")),
            None,
            None,
        );
        let mut thread_with_activity = thread.clone();
        thread_with_activity.token_usage = Observed {
            value: Some(TokenUsage {
                input_tokens: Some(1_234),
                cached_input_tokens: Some(20),
                cache_write_input_tokens: Some(3),
                output_tokens: Some(400),
                reasoning_output_tokens: Some(100),
                total_tokens: Some(1_757),
                context_window: Some(128_000),
            }),
            source: Some(EvidenceSource {
                kind: "rollout.token_count".to_string(),
                detail: None,
            }),
            observed_at: None,
            confidence: Confidence::High,
            detail: None,
        };
        let row = build_list_row(&thread_with_activity, 0, Utc::now());
        thread_with_activity.recent_activity = vec![
            crate::model::ThreadActivity {
                kind: "tool_call".to_string(),
                tool_name: Some("read".to_string()),
                status: Some("start".to_string()),
                timestamp: Some(Utc::now()),
            },
            crate::model::ThreadActivity {
                kind: "tool_call".to_string(),
                tool_name: Some("read".to_string()),
                status: Some("done".to_string()),
                timestamp: Some(Utc::now()),
            },
            crate::model::ThreadActivity {
                kind: "tool_call".to_string(),
                tool_name: Some("write".to_string()),
                status: Some("start".to_string()),
                timestamp: Some(Utc::now()),
            },
        ];
        thread_with_activity.last_terminal_event = Observed {
            value: Some(LastTerminalEvent::Completed),
            source: Some(EvidenceSource {
                kind: "rollout.lifecycle".to_string(),
                detail: Some("latest terminal lifecycle event".to_string()),
            }),
            observed_at: Some(Utc::now()),
            confidence: Confidence::Medium,
            detail: None,
        };
        let display = display_name(&thread_with_activity);
        let short_id = short_id(&thread_with_activity.thread_id);
        let backend = TestBackend::new(160, 48);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal
            .draw(|frame| {
                let rows = vec![row.clone()];
                render_tui_view(
                    frame,
                    frame.area(),
                    &TuiViewState {
                        visible_rows: &rows,
                        selected_snapshot: Some(&thread_with_activity),
                        counts: &StateCounts {
                            running: 1,
                            idle: 0,
                            unknown: 0,
                        },
                        account_usage: &Observed::unknown(),
                        kiro_account_usage: &Observed::unknown(),
                        provider: "codex",
                        selected: 0,
                        state_filter: &LocalStateFilter::All,
                        last_refresh_label: "just now",
                        show_activity: false,
                        show_technical: false,
                        show_help: false,
                        search_mode: false,
                        search_query: "",
                    },
                );
            })
            .unwrap();

        let terminal_size = terminal.size().unwrap();
        let terminal_area = Rect::new(0, 0, terminal_size.width, terminal_size.height);
        let buffer = terminal.backend().buffer().content();

        let outer = Layout::default()
            .direction(Direction::Vertical)
            .margin(1)
            .constraints([
                Constraint::Length(4),
                Constraint::Min(1),
                Constraint::Length(3),
            ])
            .split(terminal_area);

        let body = Layout::default()
            .direction(Direction::Horizontal)
            .constraints([Constraint::Percentage(38), Constraint::Percentage(62)])
            .split(outer[1]);
        let left_area = body[0];

        let mut left_rendered = String::new();
        let mut left_lines: Vec<String> = Vec::new();
        for y in left_area.y..(left_area.y + left_area.height) {
            let mut row_text = String::new();
            for x in left_area.x..(left_area.x + left_area.width) {
                let idx = (y as usize) * (terminal_size.width as usize) + (x as usize);
                if let Some(cell) = buffer.get(idx) {
                    let symbol = cell.symbol().to_string();
                    left_rendered.push_str(&symbol);
                    row_text.push_str(&symbol);
                }
            }
            left_lines.push(row_text);
        }

        let mut full_rendered = String::new();
        for cell in buffer.iter() {
            full_rendered.push_str(cell.symbol());
        }

        let mut right_rendered = String::new();
        for y in body[1].y..(body[1].y + body[1].height) {
            for x in body[1].x..(body[1].x + body[1].width) {
                let idx = (y as usize) * (terminal_size.width as usize) + (x as usize);
                if let Some(cell) = buffer.get(idx) {
                    right_rendered.push_str(cell.symbol());
                }
            }
            right_rendered.push('\n');
        }

        assert_eq!(display, format!("Main task {}", short_id));
        assert!(left_rendered.contains(&display));
        assert_eq!(left_rendered.matches(short_id.as_str()).count(), 1);
        assert!(!full_rendered.contains(thread.thread_id.as_str()));
        assert!(right_rendered.contains("Model"));
        assert!(right_rendered.contains("Effort"));
        assert!(right_rendered.contains("Current activity"));
        assert!(right_rendered.contains("Last terminal result: completed"));
        assert!(right_rendered.contains("Technical details"));
        assert!(right_rendered.contains("Press i to expand"));
        assert!(right_rendered.contains("Running"));
        assert!(right_rendered.contains("─"));
        assert!(left_rendered.contains("Project:") || left_rendered.contains("From:"));
        assert!(left_rendered.contains("Tokens 1.8k"));
        assert!(!right_rendered.contains("No recent activity"));
        assert!(right_rendered.contains("Recent activity (UTC)"));
        assert!(right_rendered.contains("Token usage"));
        assert!(right_rendered.contains("total=1,757"));
        assert!(right_rendered.contains("rollout.token_count"));
        assert!(right_rendered.contains("tool call"));
        assert!(full_rendered.contains("RUNNING"));
        assert!(full_rendered.contains("IDLE"));
        assert!(!full_rendered.contains("DONE"));
        assert!(!full_rendered.contains("FAILED"));

        let right_lines: Vec<&str> = right_rendered.lines().collect();
        let model = &row.model;
        let effort = &row.effort;
        let current_idx = right_lines
            .iter()
            .position(|line| line.contains("Current activity"))
            .expect("current activity heading present");
        let model_idx = right_lines
            .iter()
            .position(|line| line.contains("Model") && line.contains("Effort"))
            .expect("wide model/effort headings present");
        assert!(current_idx + 2 < right_lines.len());
        assert!(model_idx > current_idx + 1);
        assert!(right_lines[current_idx + 2].contains("─"));

        let model_line = right_lines[model_idx];
        let values_line = right_lines
            .get(model_idx + 1)
            .expect("model effort value line present");
        assert_eq!(model_line.find("Model"), values_line.find(model));
        assert_eq!(model_line.find("Effort"), values_line.find(effort));

        let card_title_line = left_lines
            .iter()
            .find(|line| line.contains(&display))
            .expect("selected card title line present");
        assert!(card_title_line.contains(&format!("   ● [Codex] {}", display)));

        let buffer = terminal.backend().buffer();
        let has_cyan_border = buffer.content().iter().enumerate().any(|(idx, cell)| {
            let y = idx / (terminal_size.width as usize);
            let x = idx % (terminal_size.width as usize);
            if x < left_area.x as usize
                || x >= (left_area.x + left_area.width) as usize
                || y < left_area.y as usize
                || y >= (left_area.y + left_area.height) as usize
            {
                return false;
            }
            matches!(
                cell.symbol(),
                "┌" | "╭"
                    | "╔"
                    | "┐"
                    | "╮"
                    | "╕"
                    | "┘"
                    | "╯"
                    | "╛"
                    | "└"
                    | "╰"
                    | "╚"
                    | "─"
            ) && matches!(cell.style().fg, Some(Color::Cyan))
        });
        assert!(has_cyan_border, "selected card should render cyan border");
        let has_cyan_bold_technical = buffer.content().iter().any(|cell| {
            cell.symbol() == "T"
                && matches!(cell.style().fg, Some(Color::Cyan))
                && cell.style().add_modifier.contains(Modifier::BOLD)
        });
        assert!(
            has_cyan_bold_technical,
            "collapsed technical heading should be cyan and bold"
        );
    }

    #[test]
    fn render_tui_narrow_layout_stays_readable_and_shows_quit_command() {
        let thread = make_snapshot(
            "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb",
            None,
            Some("Planner"),
            None,
            ThreadState::Running,
            Some("running"),
            Some("C:\\Users\\Work\\Projects\\VeryLongProjectName\\Nested\\Path"),
            Some(("model", "high")),
            None,
            None,
        );
        let row = build_list_row(&thread, 0, Utc::now());
        let backend = TestBackend::new(70, 24);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal
            .draw(|frame| {
                let rows = vec![row.clone()];
                render_tui_view(
                    frame,
                    frame.area(),
                    &TuiViewState {
                        visible_rows: &rows,
                        selected_snapshot: Some(&thread),
                        counts: &StateCounts {
                            running: 1,
                            idle: 0,
                            unknown: 0,
                        },
                        account_usage: &Observed::unknown(),
                        kiro_account_usage: &Observed::unknown(),
                        provider: "codex",
                        selected: 0,
                        state_filter: &LocalStateFilter::All,
                        last_refresh_label: "just now",
                        show_activity: false,
                        show_technical: false,
                        show_help: false,
                        search_mode: false,
                        search_query: "",
                    },
                );
            })
            .unwrap();

        let lines = terminal
            .backend()
            .buffer()
            .content
            .iter()
            .collect::<Vec<_>>();
        let rendered: String = lines.iter().map(|cell| cell.symbol()).collect();
        assert!(rendered.contains("q Quit"));
    }

    #[test]
    fn usage_header_keeps_usage_and_counts_visible_at_supported_widths() {
        let thread = make_snapshot(
            "usage-thread",
            Some("Usage"),
            None,
            None,
            ThreadState::Running,
            Some("running"),
            None,
            None,
            None,
            None,
        );
        let row = build_list_row(&thread, 0, Utc::now());
        let observed_at = Utc::now();
        let account_usage = Observed {
            value: Some(AccountUsage {
                primary: Some(AccountUsageWindow {
                    used_percent: Some(44.0),
                    window_minutes: Some(10_080),
                    resets_at: Some(observed_at + Duration::hours(1)),
                }),
                secondary: Some(AccountUsageWindow {
                    used_percent: Some(25.0),
                    window_minutes: Some(300),
                    resets_at: Some(observed_at + Duration::hours(1)),
                }),
            }),
            source: None,
            observed_at: Some(observed_at),
            confidence: Confidence::High,
            detail: None,
        };

        let expected_title = format!("Codex Agent Monitor v{}", env!("CARGO_PKG_VERSION"));
        for width in [120, 100, 80, 60] {
            let backend = TestBackend::new(width, 24);
            let mut terminal = Terminal::new(backend).unwrap();
            terminal
                .draw(|frame| {
                    let rows = vec![row.clone()];
                    render_tui_view(
                        frame,
                        frame.area(),
                        &TuiViewState {
                            visible_rows: &rows,
                            selected_snapshot: Some(&thread),
                            counts: &StateCounts {
                                running: 1,
                                idle: 2,
                                unknown: 7,
                            },
                            account_usage: &account_usage,
                            kiro_account_usage: &Observed::unknown(),
                            provider: "codex",
                            selected: 0,
                            state_filter: &LocalStateFilter::All,
                            last_refresh_label: "just now",
                            show_activity: false,
                            show_technical: false,
                            show_help: false,
                            search_mode: false,
                            search_query: "",
                        },
                    );
                })
                .unwrap();
            let rendered: String = terminal
                .backend()
                .buffer()
                .content()
                .iter()
                .map(|cell| cell.symbol())
                .collect();
            assert!(rendered.contains(&expected_title), "width {width}");
            assert!(rendered.contains("Codex usage:"), "width {width}");
            assert!(rendered.contains("Weekly 56%"), "width {width}");
            assert!(rendered.contains("5h 75%"), "width {width}");
            assert!(rendered.contains("observed just now"), "width {width}");
            assert!(rendered.contains("Auto-refresh"), "width {width}");
            assert!(rendered.contains("RUNNING"), "width {width}");
            assert!(rendered.contains("IDLE"), "width {width}");
            assert!(rendered.contains("UNKNOWN"), "width {width}");
        }
    }

    #[test]
    fn usage_header_shows_window_labels_unknown_and_expired_values() {
        let now = Utc::now();
        let usage = Observed {
            value: Some(AccountUsage {
                primary: Some(AccountUsageWindow {
                    used_percent: Some(120.0),
                    window_minutes: Some(10_080),
                    resets_at: Some(now + Duration::hours(1)),
                }),
                secondary: Some(AccountUsageWindow {
                    used_percent: None,
                    window_minutes: Some(300),
                    resets_at: None,
                }),
            }),
            source: None,
            observed_at: Some(now),
            confidence: Confidence::High,
            detail: None,
        };
        let line = account_usage_line("Codex usage", &usage, now);
        assert!(line.contains("Weekly 0%"));
        assert!(line.contains("5h unavailable"));

        let expired = Observed {
            value: Some(AccountUsage {
                primary: Some(AccountUsageWindow {
                    used_percent: Some(0.0),
                    window_minutes: Some(300),
                    resets_at: Some(now - Duration::seconds(1)),
                }),
                secondary: None,
            }),
            source: None,
            observed_at: Some(now),
            confidence: Confidence::High,
            detail: None,
        };
        assert!(account_usage_line("Codex usage", &expired, now).contains("5h awaiting update"));
        assert_eq!(
            account_usage_line("Codex usage", &Observed::unknown(), now),
            "Codex usage: unavailable"
        );
    }

    #[test]
    fn render_tui_very_narrow_layout_keeps_footer_command_visible() {
        let thread = make_snapshot(
            "cccccccccccccccccccccccccccccccc",
            None,
            None,
            None,
            ThreadState::Running,
            Some("running"),
            Some("C:\\Users\\Work\\Projects\\VeryLongProjectName\\Nested\\Path"),
            Some(("model", "high")),
            None,
            None,
        );
        let row = build_list_row(&thread, 0, Utc::now());
        let backend = TestBackend::new(16, 24);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal
            .draw(|frame| {
                let rows = vec![row.clone()];
                render_tui_view(
                    frame,
                    frame.area(),
                    &TuiViewState {
                        visible_rows: &rows,
                        selected_snapshot: Some(&thread),
                        counts: &StateCounts {
                            running: 1,
                            idle: 0,
                            unknown: 0,
                        },
                        account_usage: &Observed::unknown(),
                        kiro_account_usage: &Observed::unknown(),
                        provider: "codex",
                        selected: 0,
                        state_filter: &LocalStateFilter::All,
                        last_refresh_label: "just now",
                        show_activity: false,
                        show_technical: false,
                        show_help: false,
                        search_mode: false,
                        search_query: "",
                    },
                );
            })
            .unwrap();

        let footer_width = 16usize;
        let (title, footer_text) =
            footer_text_for_width(false, "", LocalStateFilter::All.label(), footer_width);
        assert!(footer_text.chars().count() <= footer_width);
        assert!(footer_text.contains("q Quit"), "q Quit should stay visible");
        assert!(title.is_empty());

        let lines = terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .collect::<Vec<_>>();
        let rendered: String = lines.iter().map(|cell| cell.symbol()).collect();
        assert!(rendered.contains("q Quit"));
    }

    #[test]
    fn list_scroll_offset_keeps_selected_card_visible_in_small_viewport() {
        assert_eq!(list_scroll_offset(0, 10, AGENT_CARD_LINES, 4), 0);
        assert_eq!(list_scroll_offset(1, 10, AGENT_CARD_LINES, 4), 6);
        assert_eq!(list_scroll_offset(2, 10, AGENT_CARD_LINES, 4), 12);
        assert_eq!(list_scroll_offset(9, 10, AGENT_CARD_LINES, 4), 54);
        assert_eq!(list_scroll_offset(3, 10, AGENT_CARD_LINES, 12), 18);
        assert_eq!(list_scroll_offset(0, 0, AGENT_CARD_LINES, 12), 0);
        assert_eq!(list_scroll_offset(1, 10, AGENT_CARD_LINES, 80), 0);
    }

    #[test]
    fn footer_text_keeps_quit_visible_on_standard_and_narrow_widths() {
        let filter = LocalStateFilter::All.label();
        let (title_80, footer_80) = footer_text_for_width(false, "", filter, 76);
        assert!(title_80.is_empty());
        assert!(footer_80.chars().count() <= 76);
        assert!(footer_80.contains("q Quit"));
        assert!(footer_80.contains("/ Search"));
        assert!(footer_80.contains("f Filter"));

        let (title_30, footer_30) = footer_text_for_width(false, "", filter, 30);
        assert!(title_30.is_empty());
        assert!(footer_30.chars().count() <= 30);
        assert!(footer_30.contains("q Quit"));

        let (title_16, footer_16) = footer_text_for_width(false, "", filter, 16);
        assert!(title_16.is_empty());
        assert!(footer_16.chars().count() <= 16);
        assert!(footer_16.contains("q Quit"));

        let (_title_10, footer_10) = footer_text_for_width(false, "", filter, 10);
        assert!(footer_10.chars().count() <= 10);
        assert!(footer_10.contains("q Quit"));
        assert_eq!(footer_10, "q Quit");
    }

    #[test]
    fn footer_search_text_truncates_query_to_width() {
        let (_, footer) =
            footer_text_for_width(true, "a query that is far too long for the footer", "", 32);
        assert!(footer.ends_with("Enter apply  Esc cancel"));
        assert!(footer.chars().count() <= 32);
    }

    #[test]
    fn unknown_state_keeps_factual_label() {
        assert_eq!(state_label(ThreadState::Unknown), "UNKNOWN".to_string());
    }

    #[test]
    fn model_preference_uses_effective_over_requested_over_configured() {
        let thread = make_snapshot(
            "t1",
            None,
            None,
            None,
            ThreadState::Running,
            Some("running"),
            None,
            Some(("effective", "high")),
            Some(("requested", "medium")),
            Some(("configured", "low")),
        );
        assert_eq!(
            preferred_model_and_effort(&thread),
            ("effective".to_string(), "high".to_string())
        );

        let thread_no_effective = make_snapshot(
            "t2",
            None,
            None,
            None,
            ThreadState::Running,
            Some("running"),
            None,
            None,
            Some(("requested", "medium")),
            Some(("configured", "low")),
        );
        assert_eq!(
            preferred_model_and_effort(&thread_no_effective),
            ("requested".to_string(), "medium".to_string())
        );

        let thread_only_configured = make_snapshot(
            "t3",
            None,
            None,
            None,
            ThreadState::Running,
            Some("running"),
            None,
            None,
            None,
            Some(("configured", "low")),
        );
        assert_eq!(
            preferred_model_and_effort(&thread_only_configured),
            ("configured".to_string(), "low".to_string())
        );

        let empty = make_snapshot(
            "t4",
            None,
            None,
            None,
            ThreadState::Running,
            Some("running"),
            None,
            None,
            None,
            None,
        );
        assert_eq!(
            preferred_model_and_effort(&empty),
            ("-".to_string(), "-".to_string())
        );
    }

    #[test]
    fn terminal_detail_does_not_create_a_state_bucket() {
        let done = make_snapshot(
            "done",
            None,
            None,
            None,
            ThreadState::Idle,
            Some("turn_completed"),
            None,
            None,
            None,
            None,
        );
        assert!(matches!(bucket_for_state(done.state), StateBucket::Idle));

        let not_done = make_snapshot(
            "notdone",
            None,
            None,
            None,
            ThreadState::Idle,
            Some("idle"),
            None,
            None,
            None,
            None,
        );
        assert!(matches!(
            bucket_for_state(not_done.state),
            StateBucket::Idle
        ));
    }

    #[test]
    fn origin_detects_project_root_and_falls_back_to_from_path() -> Result<(), std::io::Error> {
        let temp = TempDir::new()?;
        let repo_root = temp.path().join("repo");
        let child = repo_root.join("nested").join("dir");
        fs::create_dir_all(&child)?;
        fs::create_dir_all(child.parent().unwrap())?;
        fs::create_dir_all(repo_root.join(".git"))?;

        let (label, full) = origin_label(Some(&child.to_string_lossy()));
        assert_eq!(label, "Project: repo");
        assert_eq!(full, child.to_string_lossy().to_string());

        let cwd_only = temp.path().join("other");
        fs::create_dir_all(&cwd_only)?;
        let (label2, full2) = origin_label(Some(&cwd_only.to_string_lossy()));
        assert_eq!(label2, format!("From: {}", cwd_only.to_string_lossy()));
        assert_eq!(full2, cwd_only.to_string_lossy().to_string());
        Ok(())
    }

    #[test]
    fn origin_normalizes_windows_verbatim_paths() {
        let verbatim = r"\\?\C:\Users\Work\project";
        let (label, full) = origin_label(Some(verbatim));
        assert_eq!(label, "From: C:\\Users\\Work\\project");
        assert_eq!(full, "C:\\Users\\Work\\project");
    }

    #[test]
    fn filters_match_case_insensitive_query_across_fields() {
        let thread = make_snapshot(
            "abc12345",
            Some("Nicky"),
            Some("Planner"),
            None,
            ThreadState::Running,
            Some("running"),
            Some("C:\\Users\\Work"),
            Some(("gpt-4", "high")),
            None,
            None,
        );
        let row = build_list_row(&thread, 0, Utc::now());
        assert!(matches_filter_query(&row, "nicky"));
        assert!(matches_filter_query(&row, "ABC123"));
        assert!(matches_filter_query(&row, "planner"));
        assert!(matches_filter_query(&row, "gpt-4"));
        assert!(matches_filter_query(&row, "c:\\users"));
        assert!(matches_filter_query(&row, "codex"));
        assert!(!matches_filter_query(&row, "does-not-exist"));
    }

    #[test]
    fn tui_card_shows_kiro_source_kind_without_changing_card_height() {
        let mut snapshot = make_snapshot(
            "kiro:native-1",
            None,
            Some("agent"),
            None,
            ThreadState::Idle,
            None,
            Some("/work/kiro"),
            None,
            Some(("kiro-model", "high")),
            None,
        );
        snapshot.source_kind = Some("kiro_cli".to_string());
        let row = build_list_row(&snapshot, 0, Utc::now());
        let mut terminal = Terminal::new(TestBackend::new(100, AGENT_CARD_LINES as u16)).unwrap();
        terminal
            .draw(|frame| {
                render_agent_card(frame, frame.area(), &row, true);
            })
            .unwrap();
        let rendered: String = terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|cell| cell.symbol())
            .collect();
        assert!(rendered.contains("kiro_cli"));
        assert!(rendered.contains("[Kiro]"));
    }

    #[test]
    fn mixed_header_keeps_both_provider_balances_visible_at_supported_widths() {
        let thread = make_snapshot(
            "mixed",
            Some("Mixed task"),
            None,
            None,
            ThreadState::Idle,
            None,
            Some("/work/mixed"),
            None,
            None,
            None,
        );
        let row = build_list_row(&thread, 0, Utc::now());
        let now = Utc::now();
        let codex_usage = Observed {
            value: Some(AccountUsage {
                primary: Some(AccountUsageWindow {
                    used_percent: Some(19.0),
                    window_minutes: Some(300),
                    resets_at: None,
                }),
                secondary: None,
            }),
            source: None,
            observed_at: Some(now),
            confidence: Confidence::High,
            detail: None,
        };
        let kiro_usage = Observed {
            value: Some(KiroAccountUsage {
                plan_name: Some("KIRO PRO".to_string()),
                billing_cycle_reset: Some("2026-10-01".to_string()),
                plan_credits: Some(KiroCreditBalance {
                    used: 199.67,
                    total: 1000.0,
                    remaining: 800.33,
                }),
                bonus_credits: Vec::new(),
                add_on_credits: Vec::new(),
            }),
            source: None,
            observed_at: Some(now),
            confidence: Confidence::Medium,
            detail: None,
        };
        for width in [70, 80, 100, 120] {
            let mut terminal = Terminal::new(TestBackend::new(width, 24)).unwrap();
            terminal
                .draw(|frame| {
                    let rows = vec![row.clone()];
                    render_tui_view(
                        frame,
                        frame.area(),
                        &TuiViewState {
                            visible_rows: &rows,
                            selected_snapshot: Some(&thread),
                            counts: &StateCounts {
                                running: 0,
                                idle: 1,
                                unknown: 0,
                            },
                            account_usage: &codex_usage,
                            kiro_account_usage: &kiro_usage,
                            provider: "all",
                            selected: 0,
                            state_filter: &LocalStateFilter::All,
                            last_refresh_label: "just now",
                            show_activity: false,
                            show_technical: false,
                            show_help: false,
                            search_mode: false,
                            search_query: "",
                        },
                    );
                })
                .unwrap();
            let rendered: String = terminal
                .backend()
                .buffer()
                .content()
                .iter()
                .map(|cell| cell.symbol())
                .collect();
            assert!(rendered.contains("Codex usage:"), "width {width}");
            assert!(rendered.contains("Kiro credits:"), "width {width}");
            assert!(rendered.contains("800.33"), "width {width}");
            assert!(rendered.contains("1000.00"), "width {width}");
            assert!(rendered.contains("Codex + Kiro Monitor"), "width {width}");
        }
    }

    #[test]
    fn state_filter_toggles_between_all_and_running() {
        let running = make_snapshot(
            "r1",
            Some("r"),
            None,
            None,
            ThreadState::Running,
            Some("running"),
            None,
            None,
            None,
            None,
        );
        let row = build_list_row(&running, 0, Utc::now());
        assert!(matches_filter_query(&row, ""));

        assert_eq!(StateBucket::Running, row.state_bucket);
        assert!(LocalStateFilter::Running.matches(row.state_bucket));
        assert_eq!(LocalStateFilter::Running.next(), LocalStateFilter::All);
        assert_eq!(LocalStateFilter::All.next(), LocalStateFilter::Running);
    }

    #[test]
    fn running_filter_orders_newest_known_activity_first() {
        let now = Utc::now();
        let mut oldest = make_snapshot(
            "oldest",
            None,
            None,
            None,
            ThreadState::Running,
            Some("running"),
            None,
            None,
            None,
            None,
        );
        oldest.created_at = Some(now - Duration::minutes(3));

        let mut updated = make_snapshot(
            "updated",
            None,
            None,
            None,
            ThreadState::Running,
            Some("running"),
            None,
            None,
            None,
            None,
        );
        updated.created_at = Some(now - Duration::minutes(5));
        updated.updated_at = Some(now - Duration::minutes(2));

        let mut newest = make_snapshot(
            "newest",
            None,
            None,
            None,
            ThreadState::Running,
            Some("running"),
            None,
            None,
            None,
            None,
        );
        newest.created_at = Some(now - Duration::minutes(10));
        newest.updated_at = Some(now - Duration::minutes(4));
        newest.recency_at = Some(now - Duration::minutes(1));

        let unknown = make_snapshot(
            "unknown",
            None,
            None,
            None,
            ThreadState::Running,
            Some("running"),
            None,
            None,
            None,
            None,
        );

        let rows = vec![
            build_list_row(&oldest, 0, now),
            build_list_row(&updated, 0, now),
            build_list_row(&newest, 0, now),
            build_list_row(&unknown, 0, now),
        ];
        let all = visible_rows_for_filter(&rows, "", LocalStateFilter::All);
        assert_eq!(
            all.iter()
                .map(|row| row.thread_id.as_str())
                .collect::<Vec<_>>(),
            vec!["oldest", "updated", "newest", "unknown"]
        );

        let running = visible_rows_for_filter(&rows, "", LocalStateFilter::Running);
        assert_eq!(
            running
                .iter()
                .map(|row| row.thread_id.as_str())
                .collect::<Vec<_>>(),
            vec!["newest", "updated", "oldest", "unknown"]
        );
    }
}

#[cfg(test)]
mod kiro_task_progress_tests {
    use super::*;
    use crate::model::{
        Confidence, ContextUsage, EvidenceSource, TaskProgress, TaskStatus, ThreadTask,
    };
    use ratatui::{backend::TestBackend, Terminal};

    fn context_observation() -> Observed<ContextUsage> {
        Observed {
            value: Some(ContextUsage {
                used_percent: 94.69118,
                used_tokens_approx: 257_560,
                context_window_tokens: 272_000,
            }),
            source: Some(EvidenceSource {
                kind: "kiro.session.context_usage".to_string(),
                detail: None,
            }),
            observed_at: None,
            confidence: Confidence::Medium,
            detail: Some("current context; not cumulative".to_string()),
        }
    }

    #[test]
    fn kiro_task_progress_is_visible_on_card_and_details() {
        let mut snapshot = tests::make_snapshot(
            "kiro:task-session-12345678",
            None,
            Some("default"),
            None,
            ThreadState::Unknown,
            None,
            Some("/work/tasks"),
            None,
            Some(("kiro-model", "high")),
            None,
        );
        snapshot.source_kind = Some("kiro_cli".to_string());
        snapshot.context_usage = context_observation();
        snapshot.task_progress = Observed {
            value: Some(TaskProgress {
                tasks: vec![
                    ThreadTask {
                        id: 1,
                        status: TaskStatus::InProgress,
                    },
                    ThreadTask {
                        id: 2,
                        status: TaskStatus::Completed,
                    },
                    ThreadTask {
                        id: 3,
                        status: TaskStatus::Pending,
                    },
                ],
            }),
            source: Some(EvidenceSource {
                kind: "kiro.tasks".to_string(),
                detail: None,
            }),
            observed_at: None,
            confidence: Confidence::Medium,
            detail: None,
        };

        assert_eq!(display_name(&snapshot), "Main task 12345678");
        let row = build_list_row(&snapshot, 0, Utc::now());
        let mut card = Terminal::new(TestBackend::new(100, AGENT_CARD_LINES as u16)).unwrap();
        card.draw(|frame| render_agent_card(frame, frame.area(), &row, true))
            .unwrap();
        let card_text = card
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|cell| cell.symbol())
            .collect::<String>();
        assert!(card_text.contains("Tasks 1/3"));
        assert!(card_text.contains("Ctx ~257.6k"));
        assert!(!card_text.contains("Tokens ?"));

        let mut details = Terminal::new(TestBackend::new(100, 42)).unwrap();
        details
            .draw(|frame| {
                render_details_pane(frame, frame.area(), &snapshot, false, false, true);
            })
            .unwrap();
        let detail_text = details
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|cell| cell.symbol())
            .collect::<String>();
        assert!(detail_text.contains("Task progress"));
        assert!(detail_text.contains("1/3 completed"));
        assert!(detail_text.contains("1 active"));
        assert!(detail_text.contains("1 pending"));
        assert!(detail_text.contains("#1 in progress"));
        assert!(detail_text.contains("#2 completed"));
        assert!(detail_text.contains("#3 pending"));
        assert!(detail_text.contains("Context usage"));
        assert!(detail_text.contains("94.7% used"));
        assert!(detail_text.contains("approximately 257,560 / 272,000 tokens"));
        assert!(detail_text.contains("current, not cumulative"));
        assert!(detail_text.contains("kiro.session.context_usage"));
    }

    #[test]
    fn kiro_missing_task_plan_is_explicit_while_context_remains_visible() {
        let mut snapshot = tests::make_snapshot(
            "kiro:no-plan-12345678",
            None,
            Some("default"),
            None,
            ThreadState::Idle,
            None,
            Some("/work/no-plan"),
            None,
            Some(("kiro-model", "high")),
            None,
        );
        snapshot.source_kind = Some("kiro_cli".to_string());
        snapshot.context_usage = context_observation();
        snapshot.task_progress = Observed {
            value: None,
            source: Some(EvidenceSource {
                kind: "kiro.tasks".to_string(),
                detail: None,
            }),
            observed_at: None,
            confidence: Confidence::Low,
            detail: Some("Kiro did not persist a task plan".to_string()),
        };

        let row = build_list_row(&snapshot, 0, Utc::now());
        let mut card = Terminal::new(TestBackend::new(100, AGENT_CARD_LINES as u16)).unwrap();
        card.draw(|frame| render_agent_card(frame, frame.area(), &row, true))
            .unwrap();
        let card_text = card
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|cell| cell.symbol())
            .collect::<String>();
        assert!(card_text.contains("Ctx ~257.6k"));
        assert!(!card_text.contains("Tokens ?"));

        let mut details = Terminal::new(TestBackend::new(100, 36)).unwrap();
        details
            .draw(|frame| {
                render_details_pane(frame, frame.area(), &snapshot, false, false, true);
            })
            .unwrap();
        let detail_text = details
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|cell| cell.symbol())
            .collect::<String>();
        assert!(detail_text.contains("Task progress"));
        assert!(detail_text.contains("Unavailable: Kiro did not persist a task plan"));
        assert!(detail_text.contains("Context usage"));
        assert!(detail_text.contains("approximately 257,560 / 272,000 tokens"));
    }
}
