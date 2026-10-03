//! Interactive presentation of the same private snapshots used by --text/--json.
use super::{RequestRecord, Snapshot, get_snapshot};
use chrono::{DateTime, Utc};
use crossterm::event::{self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use ratatui::{
    Frame,
    layout::{Constraint, Layout, Rect},
    style::{Color, Style, Stylize},
    symbols::Marker,
    text::{Line, Span},
    widgets::{
        Axis, Block, BorderType, Chart, Dataset, GraphType, Paragraph, Row, Table, TableState, Tabs,
    },
};
use std::{
    collections::BTreeMap,
    path::Path,
    time::{Duration, Instant},
};

const PAGES: [&str; 6] = ["Overview", "Usage", "Provider", "Errors", "Events", "Help"];
const RANGES: [u64; 4] = [3600, 86400, 604800, 2592000];

struct App {
    page: usize,
    range: u64,
    theme: usize,
    light: bool,
    paused: bool,
    offset: usize,
    expanded_request: bool,
    detail_scroll: u16,
    refresh: bool,
    error: Option<String>,
}

impl App {
    fn new(range: u64) -> Self {
        Self {
            page: 0,
            range,
            theme: 0,
            light: false,
            paused: false,
            offset: 0,
            expanded_request: false,
            detail_scroll: 0,
            refresh: false,
            error: None,
        }
    }

    fn key(&mut self, key: KeyEvent, records: usize, detail_scroll_limit: u16) -> bool {
        if key.kind != KeyEventKind::Press {
            return false;
        }
        if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('c') {
            return true;
        }
        let previous_page = self.page;
        let previous_offset = self.offset;
        match key.code {
            KeyCode::Esc if self.expanded_request => self.expanded_request = false,
            KeyCode::Char('q') | KeyCode::Esc => return true,
            KeyCode::Enter
                if matches!(self.page, 0 | 3 | 4) && (records > 0 || self.expanded_request) =>
            {
                self.expanded_request = !self.expanded_request;
                self.detail_scroll = 0;
            }
            KeyCode::Char('t') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                self.light = !self.light
            }
            KeyCode::Char('t') => self.theme = (self.theme + 1) % 3,
            KeyCode::Tab | KeyCode::Right => {
                self.page = (self.page + 1) % PAGES.len();
                self.offset = 0;
            }
            KeyCode::BackTab | KeyCode::Left => {
                self.page = (self.page + PAGES.len() - 1) % PAGES.len();
                self.offset = 0;
            }
            KeyCode::Char(c @ '1'..='6') => {
                self.page = c as usize - '1' as usize;
                self.offset = 0;
            }
            KeyCode::Char('?') => {
                self.page = 5;
                self.offset = 0;
            }
            KeyCode::Char('r') => {
                let current = RANGES.iter().position(|&v| v == self.range);
                self.range = RANGES[current.map_or(0, |i| (i + 1) % RANGES.len())];
                self.offset = 0;
                self.detail_scroll = 0;
                self.refresh = true;
            }
            KeyCode::Char(' ') => {
                self.paused = !self.paused;
                self.refresh = !self.paused;
            }
            KeyCode::Down | KeyCode::Char('j') => {
                self.offset = (self.offset + 1).min(records.saturating_sub(1))
            }
            KeyCode::Up | KeyCode::Char('k') => self.offset = self.offset.saturating_sub(1),
            KeyCode::PageDown if self.expanded_request => {
                self.detail_scroll = self
                    .detail_scroll
                    .saturating_add(5)
                    .min(detail_scroll_limit);
            }
            KeyCode::PageUp if self.expanded_request => {
                self.detail_scroll = self.detail_scroll.saturating_sub(5)
            }
            KeyCode::Home if self.expanded_request => self.detail_scroll = 0,
            KeyCode::PageDown => self.offset = (self.offset + 10).min(records.saturating_sub(1)),
            KeyCode::PageUp => self.offset = self.offset.saturating_sub(10),
            KeyCode::Home => self.offset = 0,
            _ => {}
        }
        if self.page != previous_page {
            self.expanded_request = false;
        }
        if self.page != previous_page || self.offset != previous_offset {
            self.detail_scroll = 0;
        }
        false
    }
}

#[derive(Clone, Copy)]
struct Palette {
    border: Color,
    text: Color,
    muted: Color,
    good: Color,
    warn: Color,
    bad: Color,
    accent: Color,
    info: Color,
    chart: Color,
    rounded: bool,
}

impl Palette {
    fn new(app: &App) -> Self {
        let (good, warn, bad, info) = if app.light {
            (
                Color::Rgb(21, 128, 61),
                Color::Rgb(161, 98, 7),
                Color::Rgb(190, 18, 60),
                Color::Rgb(3, 105, 161),
            )
        } else {
            (
                Color::Rgb(74, 222, 128),
                Color::Rgb(253, 224, 71),
                Color::Rgb(251, 113, 133),
                Color::Rgb(103, 232, 249),
            )
        };
        let border = match (app.theme, app.light) {
            (0, false) => Color::Rgb(251, 191, 36),
            (0, true) => Color::Rgb(180, 83, 9),
            (1, false) => Color::Rgb(96, 165, 250),
            (1, true) => Color::Rgb(37, 99, 235),
            (_, false) => Color::Rgb(161, 161, 170),
            (_, true) => Color::Rgb(82, 82, 91),
        };
        Self {
            border,
            text: if app.light {
                Color::Rgb(24, 24, 27)
            } else {
                Color::Rgb(250, 250, 250)
            },
            muted: if app.light {
                Color::Rgb(82, 82, 91)
            } else {
                Color::Rgb(161, 161, 170)
            },
            good,
            warn,
            bad,
            info,
            chart: match app.theme {
                0 => good,
                1 => info,
                _ => border,
            },
            rounded: app.theme == 0,
            accent: if app.light {
                Color::Rgb(109, 40, 217)
            } else {
                Color::Rgb(196, 181, 253)
            },
        }
    }

    fn block(self, title: impl Into<Line<'static>>) -> Block<'static> {
        Block::bordered()
            .border_type(if self.rounded {
                BorderType::Rounded
            } else {
                BorderType::Plain
            })
            .border_style(Style::default().fg(self.border))
            .title(title.into().style(Style::default().fg(self.border).bold()))
    }
}

// Drop restores the operator's screen and input settings on every error path.
struct RestoreTerminal;
impl Drop for RestoreTerminal {
    fn drop(&mut self) {
        ratatui::restore();
    }
}

pub(super) async fn run(socket: &Path, range: u64, mut snapshot: Snapshot) -> Result<(), String> {
    let mut terminal = ratatui::try_init().map_err(|e| format!("cannot open terminal: {e}"))?;
    let _restore = RestoreTerminal;
    let mut app = App::new(range);
    let mut last_refresh = Instant::now();
    loop {
        if app.refresh || (!app.paused && last_refresh.elapsed() >= Duration::from_secs(2)) {
            match get_snapshot(socket, app.range).await {
                Ok(next) => {
                    snapshot = next;
                    app.error = None;
                }
                Err(error) => app.error = Some(error),
            }
            app.refresh = false;
            last_refresh = Instant::now();
        }
        terminal
            .draw(|f| draw(f, &app, &snapshot))
            .map_err(|e| format!("cannot draw monitor: {e}"))?;
        // Zero-time polling keeps the async runtime responsive without a blocked input thread.
        if event::poll(Duration::ZERO).map_err(|e| format!("cannot poll keyboard: {e}"))? {
            if let Event::Key(key) =
                event::read().map_err(|e| format!("cannot read keyboard: {e}"))?
            {
                let rows = match app.page {
                    1 => snapshot
                        .aggregate
                        .by_model
                        .len()
                        .max(snapshot.aggregate.by_client.len()),
                    2 => quota_lines(&snapshot, Palette::new(&app)).len(),
                    3 => snapshot
                        .records
                        .iter()
                        .filter(|r| r.error_code.is_some())
                        .count(),
                    _ => snapshot.records.len(),
                };
                let size = terminal
                    .size()
                    .map_err(|e| format!("cannot inspect terminal: {e}"))?;
                let detail_scroll_limit = selected_request(&snapshot, app.offset, app.page == 3)
                    .map(|r| {
                        request_detail_lines(r, size.width.saturating_sub(2), Palette::new(&app))
                            .len()
                            .saturating_sub(size.height.saturating_sub(7) as usize)
                            .min(u16::MAX as usize) as u16
                    })
                    .unwrap_or(0);
                if app.key(key, rows, detail_scroll_limit) {
                    return Ok(());
                }
            }
        }
        tokio::select! {
            _ = tokio::signal::ctrl_c() => return Ok(()),
            _ = tokio::time::sleep(Duration::from_millis(50)) => {},
        }
    }
}

fn draw(f: &mut Frame, app: &App, s: &Snapshot) {
    let p = Palette::new(app);
    let area = f.area();
    f.render_widget(Block::default().style(Style::default().fg(p.text)), area);
    if area.width < 60 || area.height < 18 {
        f.render_widget(
            Paragraph::new(vec![
                Line::from("AI ROUTER").style(Style::default().fg(p.border).bold()),
                Line::from("Enlarge the terminal to at least 60 columns × 18 rows."),
                Line::from(format!(
                    "Active {} · queued {} · requests {}",
                    s.active, s.queued, s.aggregate.requests
                )),
                Line::from("q / Ctrl+C quit · --text gives a compact snapshot"),
            ]),
            area,
        );
        return;
    }
    let [title, tabs, body, status, footer] = Layout::vertical([
        Constraint::Length(1),
        Constraint::Length(2),
        Constraint::Min(0),
        Constraint::Length(1),
        Constraint::Length(1),
    ])
    .areas(area);
    let state = if app.error.is_some() {
        "DISCONNECTED"
    } else if app.paused {
        "PAUSED"
    } else {
        "LIVE"
    };
    f.render_widget(
        Paragraph::new(Line::from(vec![
            Span::styled(" AI ROUTER ", Style::default().fg(p.border).bold()),
            Span::styled(
                format!(" ● {state} "),
                Style::default().fg(if app.error.is_some() {
                    p.bad
                } else if app.paused {
                    p.warn
                } else {
                    p.good
                }),
            ),
            Span::styled(
                format!(
                    "  {}  ·  updated {} UTC",
                    range_label(app.range),
                    clock(&s.generated_at)
                ),
                Style::default().fg(p.muted),
            ),
        ])),
        title,
    );
    let names = if area.width < 90 {
        ["Home", "Usage", "AGY", "Errors", "Events", "Help"]
    } else {
        PAGES
    };
    f.render_widget(
        Tabs::new(
            names
                .iter()
                .enumerate()
                .map(|(i, n)| format!("{} {n}", i + 1)),
        )
        .select(app.page)
        .style(Style::default().fg(p.muted))
        .highlight_style(Style::default().fg(p.border).bold())
        .divider("│"),
        tabs,
    );
    if app.expanded_request {
        if let Some(record) = selected_request(s, app.offset, app.page == 3) {
            request_detail(f, body, record, app.detail_scroll, true, p);
        } else {
            f.render_widget(
                Paragraph::new("No request in this range. Enter returns to the list.")
                    .style(Style::default().fg(p.muted))
                    .block(p.block(" Selected request ")),
                body,
            );
        }
    } else {
        match app.page {
            0 => overview(f, body, s, app, p),
            1 => usage(f, body, s, app, p),
            2 => provider(f, body, s, app, p),
            3 => errors(f, body, s, app, p),
            4 => requests(f, body, s, app.offset, false, p),
            _ => help(f, body, p),
        }
    }
    let message = if let Some(error) = &app.error {
        format!(" {error} · showing last snapshot; retrying every 2s")
    } else {
        format!(
            " AGY {} · {} · checked {} UTC · age {} · persistence errors {}",
            s.provider.version,
            if s.provider.authenticated {
                "authenticated"
            } else {
                "not authenticated"
            },
            clock(&s.provider.checked_at),
            s.provider_age_seconds
                .map(|n| format!("{n}s"))
                .unwrap_or_else(|| "unknown".into()),
            s.persistence_errors
        )
    };
    f.render_widget(
        Paragraph::new(message).style(Style::default().fg(
            if app.error.is_some() || s.persistence_errors > 0 {
                p.bad
            } else {
                p.muted
            },
        )),
        status,
    );
    let keys = if app.expanded_request {
        " ↑↓ request  PgUp/PgDn details  Enter/Esc collapse  q quit"
    } else if area.width < 95 {
        " 1–6 pages  r range  ↑↓ scroll  Space pause  ? help  q quit"
    } else {
        " Tab/1–6 pages  r range  ↑↓ scroll  Enter details  Space pause  t theme  ? help  q quit"
    };
    f.render_widget(
        Paragraph::new(keys).style(Style::default().fg(p.border)),
        footer,
    );
}

fn overview(f: &mut Frame, area: Rect, s: &Snapshot, app: &App, p: Palette) {
    let compact = area.height < 22;
    let [metrics, middle, recent] = Layout::vertical([
        Constraint::Length(4),
        Constraint::Length(if compact { 0 } else { 7 }),
        Constraint::Min(3),
    ])
    .areas(area);
    let block = p.block(" Live service ");
    let inner = block.inner(metrics);
    f.render_widget(block, metrics);
    let cells = Layout::horizontal([Constraint::Fill(1); 5]).split(inner);
    let a = &s.aggregate;
    let values = [
        ("REQUESTS", a.requests.to_string(), p.accent),
        (
            if area.width < 80 {
                "ACTIVE / Q"
            } else {
                "ACTIVE / QUEUED"
            },
            format!("{} / {}", s.active, s.queued),
            p.info,
        ),
        (
            "FAILURES",
            a.failures.to_string(),
            if a.failures > 0 { p.bad } else { p.good },
        ),
        ("P95 LATENCY", duration(a.duration_p95_ms), p.warn),
        (
            if area.width < 80 {
                "TOKENS"
            } else {
                "OBSERVED TOKENS"
            },
            count(a.total_tokens),
            p.accent,
        ),
    ];
    for (cell, (label, value, colour)) in cells.iter().zip(values) {
        f.render_widget(
            Paragraph::new(vec![
                Line::from(label).style(Style::default().fg(p.muted)),
                Line::from(value).style(Style::default().fg(colour).bold()),
            ]),
            *cell,
        );
    }
    if !compact {
        let [traffic, latency] =
            Layout::horizontal([Constraint::Percentage(60), Constraint::Percentage(40)])
                .areas(middle);
        traffic_chart(f, traffic, s, app.range, false, p);
        latency_panel(f, latency, s, p);
    }
    requests(f, recent, s, app.offset, false, p);
}

fn latency_panel(f: &mut Frame, area: Rect, s: &Snapshot, p: Palette) {
    let a = &s.aggregate;
    let rows = vec![
        Row::new([
            "Total".into(),
            duration(a.duration_p50_ms),
            duration(a.duration_p95_ms),
        ]),
        Row::new([
            if area.width < 32 {
                "Ready".into()
            } else {
                "AGY ready".into()
            },
            duration(a.startup_p50_ms),
            duration(a.startup_p95_ms),
        ]),
        Row::new([
            if area.width < 32 {
                "First".into()
            } else {
                "First output".into()
            },
            duration(a.first_output_p50_ms),
            duration(a.first_output_p95_ms),
        ]),
    ];
    f.render_widget(
        Table::new(
            rows,
            [
                Constraint::Fill(1),
                Constraint::Length(7),
                Constraint::Length(7),
            ],
        )
        .header(Row::new(["Latency", "p50", "p95"]).style(Style::default().fg(p.muted)))
        .block(p.block(" Response times ")),
        area,
    );
}

fn timeline(s: &Snapshot, range: u64, failures: bool, buckets: usize) -> Vec<(f64, f64)> {
    let now = DateTime::parse_from_rfc3339(&s.generated_at)
        .map(|d| d.timestamp())
        .unwrap_or_else(|_| Utc::now().timestamp());
    let mut counts = vec![0u64; buckets];
    for r in &s.records {
        if failures && r.error_code.is_none() {
            continue;
        }
        if let Ok(at) = DateTime::parse_from_rfc3339(&r.started_at) {
            let elapsed = at.timestamp() - (now - range as i64);
            if elapsed >= 0 && elapsed <= range as i64 {
                let i = ((elapsed as u128 * buckets as u128 / range.max(1) as u128) as usize)
                    .min(buckets - 1);
                counts[i] += 1;
            }
        }
    }
    counts
        .into_iter()
        .enumerate()
        .map(|(i, n)| (i as f64, n as f64))
        .collect()
}

fn traffic_chart(f: &mut Frame, area: Rect, s: &Snapshot, range: u64, failures: bool, p: Palette) {
    let points = timeline(s, range, failures, 48);
    let peak = points.iter().map(|v| v.1).fold(0.0_f64, f64::max);
    let max = peak.max(1.0);
    let colour = if failures { p.bad } else { p.chart };
    let title = format!(
        " {} · peak {} / bucket ",
        if failures {
            "Failures over time"
        } else {
            "Requests over time"
        },
        peak as u64
    );
    f.render_widget(
        Chart::new(vec![
            Dataset::default()
                .marker(Marker::Braille)
                .graph_type(GraphType::Line)
                .style(Style::default().fg(colour))
                .data(&points),
        ])
        .block(p.block(title))
        .x_axis(
            Axis::default()
                .style(Style::default().fg(p.muted))
                .bounds([0.0, 47.0])
                .labels([format!("−{}", range_label(range)), "now".into()]),
        )
        .y_axis(
            Axis::default()
                .style(Style::default().fg(p.muted))
                .bounds([0.0, max])
                .labels(["0".to_string(), format!("{}", max as u64)]),
        ),
        area,
    );
}

fn ranked(
    f: &mut Frame,
    area: Rect,
    title: &str,
    counts: &BTreeMap<String, usize>,
    offset: usize,
    p: Palette,
) {
    let mut items: Vec<_> = counts.iter().collect();
    items.sort_by(|a, b| b.1.cmp(a.1).then_with(|| a.0.cmp(b.0)));
    let max = items.first().map_or(1, |v| *v.1).max(1);
    let rows: Vec<_> = items
        .into_iter()
        .skip(offset)
        .map(|(name, n)| {
            let bar = "█".repeat((n.saturating_mul(12) / max).max(1));
            Row::new(vec![
                Span::styled(name.clone(), Style::default().fg(p.text)),
                Span::styled(n.to_string(), Style::default().fg(p.accent)),
                Span::styled(bar, Style::default().fg(p.info)),
            ])
        })
        .collect();
    if rows.is_empty() {
        f.render_widget(
            Paragraph::new("No events in this range.")
                .style(Style::default().fg(p.muted))
                .block(p.block(format!(" {title} "))),
            area,
        );
    } else {
        f.render_widget(
            Table::new(
                rows,
                [
                    Constraint::Fill(1),
                    Constraint::Length(7),
                    Constraint::Length(12),
                ],
            )
            .header(Row::new(["Name", "Calls", "Volume"]).style(Style::default().fg(p.muted)))
            .block(p.block(format!(" {title} · ↑↓ scroll "))),
            area,
        );
    }
}

fn usage(f: &mut Frame, area: Rect, s: &Snapshot, app: &App, p: Palette) {
    let [tokens, rankings] =
        Layout::vertical([Constraint::Length(6), Constraint::Min(0)]).areas(area);
    let a = &s.aggregate;
    f.render_widget(
        Paragraph::new(vec![
            Line::from(format!(
                "Input {}   Output {}   Total {}",
                count(a.input_tokens),
                count(a.output_tokens),
                count(a.total_tokens)
            ))
            .style(Style::default().fg(p.accent).bold()),
            Line::from(format!(
                "Thinking {}   Cache read {}",
                count(a.thinking_tokens),
                count(a.cache_read_tokens)
            )),
            Line::from(format!(
                "Usage records: {} complete · {} partial · {} unknown",
                a.complete_usage_records,
                a.partial_usage_records,
                a.requests
                    .saturating_sub(a.complete_usage_records + a.partial_usage_records)
            )),
            Line::from("Observed usage only; partial records may omit tokens.")
                .style(Style::default().fg(p.muted)),
        ])
        .block(p.block(" Token usage ")),
        tokens,
    );
    let direction = if area.width >= 100 {
        Layout::horizontal([Constraint::Fill(1); 2])
    } else {
        Layout::vertical([Constraint::Fill(1); 2])
    };
    let [models, clients] = direction.areas(rankings);
    ranked(
        f,
        models,
        "Models by request volume",
        &a.by_model,
        app.offset,
        p,
    );
    ranked(
        f,
        clients,
        "Clients by request volume",
        &a.by_client,
        app.offset,
        p,
    );
}

fn quota_lines(s: &Snapshot, p: Palette) -> Vec<Line<'static>> {
    let mut lines = Vec::new();
    if let Some(groups) = s
        .provider
        .quota
        .as_ref()
        .and_then(|q| q.get("groups"))
        .and_then(|g| g.as_array())
    {
        for group in groups {
            lines.push(
                Line::from(
                    group
                        .get("name")
                        .and_then(|n| n.as_str())
                        .unwrap_or("Quota group")
                        .to_string(),
                )
                .style(Style::default().fg(p.info).bold()),
            );
            if let Some(buckets) = group.get("buckets").and_then(|b| b.as_array()) {
                for b in buckets {
                    let name = b
                        .get("name")
                        .or_else(|| b.get("id"))
                        .and_then(|n| n.as_str())
                        .unwrap_or("bucket");
                    let fraction = b
                        .get("remaining_fraction")
                        .and_then(|v| v.as_f64())
                        .filter(|v| v.is_finite())
                        .map(|v| v.clamp(0.0, 1.0));
                    let remaining = fraction
                        .map(|v| {
                            format!(
                                "{}{} {:3.0}%",
                                "█".repeat((v * 20.0).round() as usize),
                                "░".repeat(20 - (v * 20.0).round() as usize),
                                v * 100.0
                            )
                        })
                        .unwrap_or_else(|| "unknown".into());
                    lines.push(Line::from(vec![
                        Span::raw(format!("  {name}  ")),
                        Span::styled(
                            remaining,
                            Style::default().fg(if fraction.is_some_and(|v| v < 0.2) {
                                p.warn
                            } else {
                                p.good
                            }),
                        ),
                    ]));
                    if let Some(reset) = b.get("reset_time").and_then(|r| r.as_str()) {
                        lines.push(
                            Line::from(format!("  Resets {reset}"))
                                .style(Style::default().fg(p.muted)),
                        );
                    }
                }
            }
        }
    }
    if lines.is_empty() {
        lines.push(
            Line::from("Quota has not been reported by AGY.").style(Style::default().fg(p.muted)),
        );
    }
    if !s.provider.models.is_empty() {
        lines.push(Line::from(""));
        lines.push(Line::from("Available models").style(Style::default().fg(p.info).bold()));
        for model in &s.provider.models {
            lines.push(Line::from(format!("  {} · {}", model.id, model.name)));
        }
    }
    lines
}

fn provider(f: &mut Frame, area: Rect, s: &Snapshot, app: &App, p: Palette) {
    let [status, quotas] =
        Layout::vertical([Constraint::Length(7), Constraint::Min(0)]).areas(area);
    let mut lines = vec![
        Line::from(format!(
            "● {}  ·  AGY {}",
            if s.provider.authenticated {
                "Authenticated"
            } else {
                "Not authenticated"
            },
            s.provider.version
        ))
        .style(
            Style::default()
                .fg(if s.provider.authenticated {
                    p.good
                } else {
                    p.bad
                })
                .bold(),
        ),
        Line::from(format!(
            "Checked {} · age {}",
            s.provider.checked_at,
            s.provider_age_seconds
                .map(|v| format!("{v}s"))
                .unwrap_or_else(|| "unknown".into())
        )),
        Line::from(format!(
            "{} available models · {} active · {} queued",
            s.provider.models.len(),
            s.active,
            s.queued
        )),
        Line::from(format!(
            "History: {} days / {} events · persistence errors {}",
            s.retention_days, s.max_events, s.persistence_errors
        ))
        .style(Style::default().fg(p.muted)),
    ];
    if let Some(error) = &s.provider.error {
        lines.push(Line::from(error.clone()).style(Style::default().fg(p.bad)));
    }
    f.render_widget(
        Paragraph::new(lines).block(p.block(" Provider status ")),
        status,
    );
    f.render_widget(
        Paragraph::new(quota_lines(s, p))
            .scroll((app.offset.min(u16::MAX as usize) as u16, 0))
            .block(p.block(" Quota remaining · cached at provider check · ↑↓ scroll ")),
        quotas,
    );
}

fn errors(f: &mut Frame, area: Rect, s: &Snapshot, app: &App, p: Palette) {
    let compact = area.height < 22;
    let [counters, middle, recent] = Layout::vertical([
        Constraint::Length(5),
        Constraint::Length(if compact { 0 } else { 7 }),
        Constraint::Min(3),
    ])
    .areas(area);
    let c = &s.counts_since_start;
    f.render_widget(
        Paragraph::new(vec![
            Line::from(format!(
                "{} rejected · auth {} · rate {} · scope {} · input {} · busy {} · other {}",
                c.total, c.auth, c.rate, c.scope, c.input, c.busy, c.other
            ))
            .style(
                Style::default()
                    .fg(if c.total > 0 { p.bad } else { p.good })
                    .bold(),
            ),
            Line::from(format!("Since {} · resets on restart", c.started_at))
                .style(Style::default().fg(p.muted)),
            Line::from("Counters include every rejection; recent rejection history is sampled.")
                .style(Style::default().fg(p.muted)),
        ])
        .block(p.block(" Rejections since process start ")),
        counters,
    );
    if !compact {
        let [chart, codes] =
            Layout::horizontal([Constraint::Percentage(55), Constraint::Percentage(45)])
                .areas(middle);
        traffic_chart(f, chart, s, app.range, true, p);
        ranked(
            f,
            codes,
            "Retained failure codes",
            &s.aggregate.by_error,
            0,
            p,
        );
    }
    requests(f, recent, s, app.offset, true, p);
}

fn requests(f: &mut Frame, area: Rect, s: &Snapshot, offset: usize, failures: bool, p: Palette) {
    let records: Vec<_> = s
        .records
        .iter()
        .rev()
        .filter(|r| !failures || r.error_code.is_some())
        .collect();
    let title = format!(
        " {} · {} retained · newest first · ↑↓ scroll ",
        if failures {
            "Failed requests"
        } else {
            "Recent requests"
        },
        records.len()
    );
    if records.is_empty() {
        f.render_widget(
            Paragraph::new(if failures {
                "No failures in this range."
            } else {
                "No requests in this range. Waiting for traffic…"
            })
            .style(Style::default().fg(p.muted))
            .block(p.block(title)),
            area,
        );
        return;
    }
    let offset = offset.min(records.len() - 1);
    let selected = records[offset];
    let [table_area, detail] = Layout::vertical([
        Constraint::Min(4),
        Constraint::Length(if area.height >= 8 {
            area.height.saturating_sub(4).min(12)
        } else {
            0
        }),
    ])
    .areas(area);
    if detail.height > 0 {
        request_detail(f, detail, selected, 0, false, p);
    }
    let wide = area.width >= 100;
    let mut headers = vec!["Time UTC", "Client", "Model", "Duration", "Status"];
    let mut widths = vec![
        Constraint::Length(8),
        Constraint::Percentage(16),
        Constraint::Fill(1),
        Constraint::Length(9),
        Constraint::Length(12),
    ];
    if wide {
        headers.push("Request ID");
        widths.push(Constraint::Length(16));
    }
    if failures {
        headers.push("Error");
        widths.push(Constraint::Percentage(22));
    }
    let rows = records.into_iter().skip(offset).map(|r| {
        let mut cells = vec![
            Span::styled(clock(&r.started_at), Style::default().fg(p.muted)),
            Span::styled(r.client_id.clone(), Style::default().fg(p.info)),
            Span::raw(r.model.clone()),
            Span::raw(duration(Some(r.duration_ms))),
            Span::styled(
                r.status.clone(),
                Style::default().fg(if r.error_code.is_some() {
                    p.bad
                } else {
                    p.good
                }),
            ),
        ];
        if wide {
            cells.push(Span::styled(
                r.request_id.clone(),
                Style::default().fg(p.muted),
            ));
        }
        if failures {
            cells.push(Span::styled(
                r.error_code.clone().unwrap_or_default(),
                Style::default().fg(p.bad),
            ));
        }
        Row::new(cells)
    });
    f.render_stateful_widget(
        Table::new(rows, widths)
            .header(Row::new(headers).style(Style::default().fg(p.muted).bold()))
            .row_highlight_style(Style::default().bold())
            .highlight_symbol("› ")
            .column_spacing(1)
            .block(p.block(title)),
        table_area,
        &mut TableState::default().with_selected(Some(0)),
    );
}

fn selected_request(s: &Snapshot, offset: usize, failures: bool) -> Option<&RequestRecord> {
    let mut records = s
        .records
        .iter()
        .rev()
        .filter(|r| !failures || r.error_code.is_some());
    records.nth(offset).or_else(|| {
        s.records
            .iter()
            .find(|r| !failures || r.error_code.is_some())
    })
}

fn request_detail_lines(record: &RequestRecord, width: u16, p: Palette) -> Vec<Line<'static>> {
    let started = DateTime::parse_from_rfc3339(&record.started_at)
        .map(|d| {
            d.with_timezone(&Utc)
                .format("%Y-%m-%d %H:%M:%S%.3f UTC")
                .to_string()
        })
        .unwrap_or_else(|_| "unknown".into());
    let ms = |value: Option<u64>| {
        value
            .map(|n| format!("{n}ms"))
            .unwrap_or_else(|| "unknown".into())
    };
    let mut fields = vec![
        (format!("Request ID: {}", record.request_id), p.info),
        (
            format!("Client: {} · Model: {}", record.client_id, record.model),
            p.text,
        ),
        (format!("Endpoint:   {}", record.endpoint), p.text),
        (format!("Started:    {started}"), p.muted),
        (
            format!(
                "Status:     {} · Error: {}",
                record.status,
                record.error_code.as_deref().unwrap_or("none")
            ),
            if record.error_code.is_some() {
                p.bad
            } else {
                p.good
            },
        ),
        (
            format!(
                "Latency: total {} · AGY ready {} · first output {}",
                ms(Some(record.duration_ms)),
                ms(record.startup_ms),
                ms(record.first_output_ms)
            ),
            p.text,
        ),
    ];
    if let Some(usage) = &record.usage {
        fields.extend([
            (
                format!(
                    "Usage:      {}",
                    if record.usage_partial {
                        "partial (totals may omit tokens)"
                    } else {
                        "complete"
                    }
                ),
                if record.usage_partial {
                    p.warn
                } else {
                    p.muted
                },
            ),
            (
                format!(
                    "Tokens: input {} · output {} · total {}",
                    usage.input_tokens, usage.output_tokens, usage.total_tokens
                ),
                p.accent,
            ),
            (
                format!(
                    "        thinking {} · cache read {}",
                    usage.thinking_tokens, usage.cache_read_tokens
                ),
                p.accent,
            ),
        ]);
    } else {
        fields.push((
            "Usage:      not reported · token counts unknown".into(),
            p.muted,
        ));
    }
    // Telemetry identifiers are ASCII. Wrap them explicitly so even long IDs can
    // be read in full and expanded-view scrolling has an exact line count.
    fields
        .into_iter()
        .flat_map(|(text, colour)| {
            text.chars()
                .collect::<Vec<_>>()
                .chunks(width.max(1) as usize)
                .map(|chunk| {
                    Line::from(chunk.iter().collect::<String>()).style(Style::default().fg(colour))
                })
                .collect::<Vec<_>>()
        })
        .collect()
}

fn request_detail(
    f: &mut Frame,
    area: Rect,
    record: &RequestRecord,
    scroll: u16,
    expanded: bool,
    p: Palette,
) {
    let title = if expanded {
        " Selected request · Enter/Esc collapse · PgUp/PgDn scroll "
    } else {
        " Selected request · Enter expand "
    };
    let block = p.block(title);
    let inner = block.inner(area);
    let lines = request_detail_lines(record, inner.width, p);
    let max_scroll = lines
        .len()
        .saturating_sub(inner.height as usize)
        .min(u16::MAX as usize) as u16;
    f.render_widget(
        Paragraph::new(lines)
            .scroll((scroll.min(max_scroll), 0))
            .block(block),
        area,
    );
}

fn help(f: &mut Frame, area: Rect, p: Palette) {
    f.render_widget(
        Paragraph::new(vec![
            Line::from("Navigate").style(Style::default().fg(p.border).bold()),
            Line::from("  Tab / ← → / 1–6   Switch pages"),
            Line::from("  ↑ ↓ / j k          Scroll events, failures, usage, or quota"),
            Line::from("  PgUp / PgDn / Home Jump through lists"),
            Line::from("  Enter               Expand / collapse selected request details"),
            Line::from("  r                   Cycle 1h → 24h → 7d → 30d"),
            Line::from("  Space               Pause / resume automatic refresh"),
            Line::from("  t / Ctrl+T          Warm, cool, mono theme / light or dark colours"),
            Line::from("  q / Esc / Ctrl+C    Exit and restore your terminal"),
            Line::from(""),
            Line::from("Reading the data").style(Style::default().fg(p.border).bold()),
            Line::from("  Charts, usage and request counts follow the selected time range."),
            Line::from("  Rejection counters cover the process lifetime and reset on restart."),
            Line::from("  Token totals contain observed usage; unknown usage stays unknown."),
            Line::from("  Provider/quota data is cached; its check time shows its age."),
            Line::from("  Disconnections keep the last snapshot visible while retrying."),
        ])
        .block(p.block(" Help ")),
        area,
    );
}

fn clock(value: &str) -> String {
    DateTime::parse_from_rfc3339(value)
        .map(|d| d.with_timezone(&Utc).format("%H:%M:%S").to_string())
        .unwrap_or_else(|_| "unknown".into())
}
fn range_label(seconds: u64) -> String {
    if seconds == 86400 {
        "24h".into()
    } else if seconds % 86400 == 0 {
        format!("{}d", seconds / 86400)
    } else if seconds % 3600 == 0 {
        format!("{}h", seconds / 3600)
    } else if seconds % 60 == 0 {
        format!("{}m", seconds / 60)
    } else {
        format!("{seconds}s")
    }
}
fn duration(ms: Option<u64>) -> String {
    match ms {
        None => "unknown".into(),
        Some(v) if v < 1000 => format!("{v}ms"),
        Some(v) if v < 60000 => format!("{:.1}s", v as f64 / 1000.0),
        Some(v) => format!("{:.1}m", v as f64 / 60000.0),
    }
}
fn count(n: u64) -> String {
    match n {
        0..=9999 => n.to_string(),
        10000..=999999 => format!("{:.1}k", n as f64 / 1000.0),
        _ => format!("{:.1}M", n as f64 / 1_000_000.0),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::monitor::{RequestRecord, Telemetry};
    use ratatui::{Terminal, backend::TestBackend};

    async fn fixture() -> Snapshot {
        let dir = tempfile::tempdir().unwrap();
        let telemetry = Telemetry::open(dir.path(), 30, 100).unwrap();
        let now = Utc::now();
        for i in 0..30 {
            telemetry
                .record(RequestRecord {
                    request_id: format!("req-{i:02}"),
                    client_id: "demo-client".into(),
                    model: "gemini-demo".into(),
                    endpoint: "/v1/responses".into(),
                    started_at: (now - chrono::Duration::minutes(i * 3)).to_rfc3339(),
                    duration_ms: 1234,
                    startup_ms: Some(80),
                    first_output_ms: Some(130),
                    status: if i % 5 == 0 { "failed" } else { "completed" }.into(),
                    error_code: (i % 5 == 0).then(|| "timeout".into()),
                    usage: None,
                    usage_partial: false,
                })
                .await;
        }
        let mut snapshot = telemetry.snapshot().await;
        snapshot.generated_at = now.to_rfc3339();
        snapshot
    }

    #[tokio::test]
    async fn every_page_renders_at_supported_terminal_sizes() {
        let s = fixture().await;
        for (width, height) in [(120, 36), (80, 24), (60, 18), (40, 10)] {
            for page in 0..PAGES.len() {
                let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
                let mut app = App::new(86400);
                app.page = page;
                terminal.draw(|f| draw(f, &app, &s)).unwrap();
                let screen = terminal
                    .backend()
                    .buffer()
                    .content
                    .iter()
                    .map(|c| c.symbol())
                    .collect::<String>();
                assert!(screen.contains("AI ROUTER"));
                if width >= 60 {
                    assert!(screen.contains("q quit"));
                }
            }
        }
    }

    #[test]
    fn keyboard_navigation_changes_range_pause_theme_and_bounds_scroll() {
        let mut app = App::new(86400);
        for code in [
            KeyCode::Right,
            KeyCode::Char('r'),
            KeyCode::Char(' '),
            KeyCode::Char('t'),
            KeyCode::PageDown,
        ] {
            assert!(!app.key(KeyEvent::new(code, KeyModifiers::NONE), 3, 0));
        }
        assert_eq!(
            (app.page, app.range, app.theme, app.offset),
            (1, 604800, 1, 2)
        );
        assert!(app.paused);
        assert!(!app.key(
            KeyEvent::new(KeyCode::Char('t'), KeyModifiers::CONTROL),
            3,
            0
        ));
        assert!(app.light);
        assert!(app.key(KeyEvent::new(KeyCode::Char('q'), KeyModifiers::NONE), 3, 0));
    }

    #[tokio::test]
    async fn selected_request_shows_full_metadata_and_honest_usage_with_expandable_details() {
        let mut snapshot = fixture().await;
        let record = snapshot.records.last_mut().unwrap();
        record.error_code = Some("provider_timeout".into());
        record.status = "failed".into();
        record.usage = Some(crate::protocol::Usage {
            input_tokens: 12000,
            output_tokens: 345,
            total_tokens: 12345,
            thinking_tokens: 67,
            cache_read_tokens: 890,
        });
        let mut app = App::new(86400);
        for partial in [false, true] {
            snapshot.records.last_mut().unwrap().usage_partial = partial;
            let mut terminal = Terminal::new(TestBackend::new(80, 24)).unwrap();
            terminal.draw(|f| draw(f, &app, &snapshot)).unwrap();
            let screen = terminal
                .backend()
                .buffer()
                .content
                .iter()
                .map(|c| c.symbol())
                .collect::<String>();
            for value in [
                "Request ID: req-29",
                "Client: demo-client",
                "Model: gemini-demo",
                "/v1/responses",
                "Started:",
                "failed",
                "provider_timeout",
                "total 1234ms",
                "AGY ready 80ms",
                "first output 130ms",
                "input 12000",
                "output 345",
                "total 12345",
                "thinking 67",
                "cache read 890",
            ] {
                assert!(screen.contains(value), "missing {value}");
            }
            assert!(screen.contains(if partial {
                "partial (totals may omit tokens)"
            } else {
                "complete"
            }));
        }
        let record = snapshot.records.last_mut().unwrap();
        record.request_id = "r".repeat(80);
        record.client_id = "c".repeat(64);
        record.model = "m".repeat(128);
        record.endpoint = format!("/{}", "e".repeat(79));
        record.error_code = Some("x".repeat(80));
        record.usage = None;
        record.startup_ms = None;
        record.first_output_ms = None;
        let lines = request_detail_lines(record, 58, Palette::new(&app));
        assert!(lines.iter().all(|line| line.width() <= 58));
        let text = lines
            .iter()
            .flat_map(|line| line.spans.iter())
            .map(|span| span.content.as_ref())
            .collect::<String>();
        assert!(text.contains(&record.request_id) && text.contains(&record.model));
        assert!(text.contains("token counts unknown") && text.contains("AGY ready unknown"));
        assert!(!text.contains("input 0"));
        let max_scroll = lines.len().saturating_sub(11) as u16;
        assert!(max_scroll > 0);
        assert!(!app.key(
            KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE),
            30,
            max_scroll
        ));
        assert!(app.expanded_request);
        for _ in 0..5 {
            app.key(
                KeyEvent::new(KeyCode::PageDown, KeyModifiers::NONE),
                30,
                max_scroll,
            );
        }
        assert_eq!(app.detail_scroll, max_scroll);
        let mut terminal = Terminal::new(TestBackend::new(60, 18)).unwrap();
        terminal.draw(|f| draw(f, &app, &snapshot)).unwrap();
        let screen = terminal
            .backend()
            .buffer()
            .content
            .iter()
            .map(|c| c.symbol())
            .collect::<String>();
        assert!(screen.contains("token counts unknown"));
        app.key(
            KeyEvent::new(KeyCode::Down, KeyModifiers::NONE),
            30,
            max_scroll,
        );
        assert_eq!(app.detail_scroll, 0);
        assert!(!app.key(
            KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE),
            30,
            max_scroll
        ));
        assert!(!app.expanded_request);
        app.expanded_request = true;
        assert!(!app.key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE), 0, 0));
        assert!(!app.expanded_request);
    }

    #[tokio::test]
    async fn timeline_uses_snapshot_time_range_and_failure_filter() {
        let s = fixture().await;
        let total: f64 = timeline(&s, 3600, false, 48).iter().map(|p| p.1).sum();
        let failures: f64 = timeline(&s, 3600, true, 48).iter().map(|p| p.1).sum();
        assert_eq!(total, 21.0);
        assert_eq!(failures, 5.0);
        assert!(
            timeline(&s, 86400, false, 48)
                .iter()
                .map(|p| p.1)
                .sum::<f64>()
                > total
        );
    }
}
