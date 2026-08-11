//! Pure terminal rendering for the operator console projection.

mod logs;
mod statistics;

use ratatui::{
    Frame,
    layout::{Alignment, Constraint, Direction, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span, Text},
    widgets::{Block, Borders, Paragraph, Row, Table, TableState, Tabs, Wrap},
};
use serde_json::Value;

use super::{ConsoleState, Tab};
use crate::{daemon_supervisor::DaemonSupervisor, operator_client::EndpointSnapshot};

pub(super) const ACCENT: Color = Color::Cyan;
pub(super) const GOOD: Color = Color::Green;
pub(super) const WARN: Color = Color::Yellow;
pub(super) const BAD: Color = Color::Red;
pub(super) const MUTED: Color = Color::DarkGray;

pub(super) fn render(frame: &mut Frame<'_>, state: &ConsoleState, supervisor: &DaemonSupervisor) {
    let areas = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(3),
            Constraint::Min(10),
            Constraint::Length(3),
        ])
        .split(frame.area());
    render_header(frame, areas[0], state, supervisor);
    match state.tab {
        Tab::Overview => render_overview(frame, areas[1], state, supervisor),
        Tab::Statistics => statistics::render(frame, areas[1], state),
        Tab::Jobs => render_jobs(frame, areas[1], state),
        Tab::Resources => render_resources(frame, areas[1], state),
        Tab::Logs => logs::render(frame, areas[1], state),
        Tab::Config => render_config(frame, areas[1], state, supervisor),
    }
    render_footer(frame, areas[2], state);
}

fn render_header(
    frame: &mut Frame<'_>,
    area: Rect,
    state: &ConsoleState,
    supervisor: &DaemonSupervisor,
) {
    let chunks = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Length(20), Constraint::Min(40)])
        .split(area);
    let connected = state.snapshot.health.value.is_some();
    let process = if supervisor.owns_running_process() {
        format!("owned pid {}", supervisor.pid().unwrap_or_default())
    } else if connected {
        "attached daemon".into()
    } else {
        "daemon offline".into()
    };
    let title = Paragraph::new(vec![
        Line::from(Span::styled(
            " infer console ",
            Style::default()
                .fg(Color::Black)
                .bg(ACCENT)
                .add_modifier(Modifier::BOLD),
        )),
        Line::from(Span::styled(process, connection_style(connected))),
    ])
    .block(Block::default().borders(Borders::ALL));
    frame.render_widget(title, chunks[0]);

    let titles = [
        "1 Home", "2 Stats", "3 Jobs", "4 Models", "5 Logs", "6 Config",
    ]
    .into_iter()
    .map(Line::from);
    let tabs = Tabs::new(titles)
        .select(state.tab.index())
        .highlight_style(Style::default().fg(ACCENT).add_modifier(Modifier::BOLD))
        .divider("│")
        .block(Block::default().borders(Borders::ALL));
    frame.render_widget(tabs, chunks[1]);
}

fn render_overview(
    frame: &mut Frame<'_>,
    area: Rect,
    state: &ConsoleState,
    supervisor: &DaemonSupervisor,
) {
    let columns = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Percentage(48), Constraint::Percentage(52)])
        .split(area);
    let left = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Length(8), Constraint::Min(8)])
        .split(columns[0]);
    let right = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Percentage(45), Constraint::Percentage(55)])
        .split(columns[1]);

    let connected = state.snapshot.health.value.is_some();
    let contract = string_at(&state.snapshot.contract, "/contract_version").unwrap_or("—");
    let daemon_uptime = supervisor
        .uptime_seconds()
        .map(format_duration)
        .unwrap_or_else(|| "external/unknown".into());
    let overview = vec![
        kv_line("daemon", if connected { "reachable" } else { "offline" }),
        kv_line("contract", contract),
        kv_line("owned uptime", &daemon_uptime),
        kv_line(
            "refresh",
            &format!(
                "generation {} @ {}",
                state.snapshot.generation, state.snapshot.refreshed_at_unix_ms
            ),
        ),
        endpoint_error_line("health", &state.snapshot.health),
    ];
    frame.render_widget(Paragraph::new(overview).block(panel("Runtime")), left[0]);

    let metrics = state.snapshot.metrics.value.as_ref();
    let metric_lines = [
        "submitted",
        "dispatched",
        "succeeded",
        "failed",
        "cancelled",
        "expired",
        "queue_rejected",
    ]
    .into_iter()
    .map(|key| {
        let value = metrics
            .and_then(|value| value.get(key))
            .and_then(Value::as_u64)
            .map(|value| value.to_string())
            .unwrap_or_else(|| "—".into());
        kv_line(key, &value)
    })
    .chain(std::iter::once(endpoint_error_line(
        "metrics",
        &state.snapshot.metrics,
    )))
    .collect::<Vec<_>>();
    frame.render_widget(
        Paragraph::new(metric_lines).block(panel("Job totals")),
        left[1],
    );

    let queue_lines = state
        .snapshot
        .metrics
        .value
        .as_ref()
        .and_then(|value| value.get("provider_queues"))
        .and_then(Value::as_object)
        .map(|queues| {
            queues
                .iter()
                .map(|(provider, queue)| {
                    Line::from(vec![
                        Span::styled(format!("{provider:<22}"), Style::default().fg(ACCENT)),
                        Span::raw(format!(
                            " active={} i/n/b={}/{}/{}",
                            u64_at(queue, "/active"),
                            u64_at(queue, "/pending_interactive"),
                            u64_at(queue, "/pending_normal"),
                            u64_at(queue, "/pending_background")
                        )),
                    ])
                })
                .collect::<Vec<_>>()
        })
        .unwrap_or_else(|| vec![endpoint_error_line("queues", &state.snapshot.metrics)]);
    frame.render_widget(
        Paragraph::new(queue_lines).block(panel("Provider queues")),
        right[0],
    );

    let reservation_count = array_len(&state.snapshot.budget, "/active_reservations");
    let ledger_count = array_len(&state.snapshot.budget, "/usage_ledger");
    let providers = array_len(&state.snapshot.providers, "/providers");
    let resources = array_len(&state.snapshot.resources, "/providers");
    let summary = vec![
        kv_line("providers", &providers.to_string()),
        kv_line("native inventories", &resources.to_string()),
        kv_line("active reservations", &reservation_count.to_string()),
        kv_line("usage entries", &ledger_count.to_string()),
        endpoint_error_line("providers", &state.snapshot.providers),
        endpoint_error_line("budget", &state.snapshot.budget),
        endpoint_error_line("resources", &state.snapshot.resources),
    ];
    frame.render_widget(
        Paragraph::new(summary)
            .block(panel("Control-plane summary"))
            .wrap(Wrap { trim: true }),
        right[1],
    );
}

fn render_jobs(frame: &mut Frame<'_>, area: Rect, state: &ConsoleState) {
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Percentage(62), Constraint::Percentage(38)])
        .split(area);
    let header = Row::new([
        "ID",
        "Intent",
        "State",
        "Priority",
        "Provider",
        "Deployment",
    ])
    .style(Style::default().fg(ACCENT).add_modifier(Modifier::BOLD));
    let rows = state
        .snapshot
        .jobs
        .value
        .as_ref()
        .and_then(|value| value.get("jobs"))
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .map(|job| {
            Row::new([
                short(value_str(job, "id"), 18),
                short(value_str(job, "intent"), 24),
                value_str(job, "state").to_owned(),
                value_str(job, "priority").to_owned(),
                short(value_str(job, "provider"), 20),
                short(value_str(job, "deployment"), 24),
            ])
            .style(job_style(value_str(job, "state")))
        })
        .collect::<Vec<_>>();
    let title = match &state.snapshot.jobs.error {
        Some(error) => format!("Jobs — {error}"),
        None => format!("Jobs — latest {}", rows.len()),
    };
    let table = Table::new(
        rows,
        [
            Constraint::Length(19),
            Constraint::Length(25),
            Constraint::Length(12),
            Constraint::Length(12),
            Constraint::Length(20),
            Constraint::Min(16),
        ],
    )
    .header(header)
    .column_spacing(1)
    .row_highlight_style(
        Style::default()
            .bg(Color::Rgb(25, 55, 70))
            .add_modifier(Modifier::BOLD),
    )
    .highlight_symbol("▶ ")
    .block(panel(&title));
    let selected = state.selected_job.as_ref().and_then(|selected| {
        state
            .job_targets()
            .iter()
            .position(|job| &job.id == selected)
    });
    let mut table_state = TableState::default().with_selected(selected);
    frame.render_stateful_widget(table, chunks[0], &mut table_state);

    let detail = state.job_detail.as_ref();
    let selected_id = state.selected_job.as_deref().unwrap_or("—");
    let lines = if let Some(detail) = detail {
        let attempts = detail
            .get("attempts")
            .and_then(Value::as_array)
            .map(Vec::len)
            .unwrap_or_default();
        vec![
            Line::from(vec![
                Span::styled("ID          ", Style::default().fg(MUTED)),
                Span::raw(value_str(detail, "response_id")),
                Span::styled("   state  ", Style::default().fg(MUTED)),
                Span::styled(
                    value_str(detail, "state"),
                    job_style(value_str(detail, "state")),
                ),
            ]),
            Line::from(vec![
                Span::styled("route       ", Style::default().fg(MUTED)),
                Span::raw(format!(
                    "{} / {}  attempts={attempts}",
                    value_str(detail, "selected_provider"),
                    value_str(detail, "selected_deployment")
                )),
            ]),
            Line::from(vec![
                Span::styled("policy      ", Style::default().fg(MUTED)),
                Span::raw(format!(
                    "{}  placement={}  capability={}  resource={}",
                    value_str(detail, "policy"),
                    value_str(detail, "placement"),
                    value_str(detail, "capability_level"),
                    value_str(detail, "resource_class")
                )),
            ]),
            Line::from(vec![
                Span::styled("error       ", Style::default().fg(MUTED)),
                Span::styled(
                    detail.get("error").and_then(Value::as_str).unwrap_or("—"),
                    Style::default().fg(if detail.get("error").is_some_and(Value::is_string) {
                        BAD
                    } else {
                        MUTED
                    }),
                ),
            ]),
        ]
    } else {
        vec![
            Line::from(format!("Selected {selected_id}")),
            Line::from(Span::styled(
                "Press Enter to load the payload-free routing, Attempt and error explanation.",
                Style::default().fg(MUTED),
            )),
        ]
    };
    frame.render_widget(
        Paragraph::new(lines)
            .block(panel("Selected Job detail — Enter inspect  C cancel"))
            .wrap(Wrap { trim: true }),
        chunks[1],
    );
}

fn render_resources(frame: &mut Frame<'_>, area: Rect, state: &ConsoleState) {
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Percentage(32),
            Constraint::Percentage(43),
            Constraint::Percentage(25),
        ])
        .split(area);
    let provider_rows = state
        .snapshot
        .providers
        .value
        .as_ref()
        .and_then(|value| value.get("providers"))
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .map(|provider| {
            Row::new([
                value_str(provider, "id").to_owned(),
                value_str(provider, "kind").to_owned(),
                value_str(provider, "placement").to_owned(),
                bool_label(provider.get("configured")),
                bool_label(provider.get("circuit_open")),
                provider
                    .get("max_concurrency")
                    .and_then(Value::as_u64)
                    .unwrap_or_default()
                    .to_string(),
                provider
                    .get("max_queue")
                    .and_then(Value::as_u64)
                    .unwrap_or_default()
                    .to_string(),
            ])
        })
        .collect::<Vec<_>>();
    let providers = Table::new(
        provider_rows,
        [
            Constraint::Length(22),
            Constraint::Length(14),
            Constraint::Length(13),
            Constraint::Length(11),
            Constraint::Length(9),
            Constraint::Length(7),
            Constraint::Length(7),
        ],
    )
    .header(
        Row::new([
            "Provider",
            "Kind",
            "Placement",
            "Configured",
            "Circuit",
            "Capacity",
            "Queue cap",
        ])
        .style(Style::default().fg(ACCENT).add_modifier(Modifier::BOLD)),
    )
    .block(panel("Providers — MLX appears here as audio_worker"));
    frame.render_widget(providers, chunks[0]);

    let lifecycle_rows = state
        .resource_targets()
        .into_iter()
        .map(|resource| {
            Row::new([
                resource.provider,
                resource.deployment,
                resource.model_id,
                resource.state.clone(),
                resource.active_reservations.to_string(),
                resource
                    .resident_memory_bytes
                    .map(format_bytes)
                    .unwrap_or_else(|| "—".into()),
            ])
            .style(resource_style(&resource.state))
        })
        .collect::<Vec<_>>();
    let lifecycle = Table::new(
        lifecycle_rows,
        [
            Constraint::Length(18),
            Constraint::Length(25),
            Constraint::Min(20),
            Constraint::Length(13),
            Constraint::Length(8),
            Constraint::Length(10),
        ],
    )
    .header(
        Row::new([
            "Provider",
            "Deployment",
            "Model",
            "State",
            "Active",
            "Memory",
        ])
        .style(Style::default().fg(ACCENT).add_modifier(Modifier::BOLD)),
    )
    .row_highlight_style(
        Style::default()
            .bg(Color::Rgb(25, 55, 70))
            .add_modifier(Modifier::BOLD),
    )
    .highlight_symbol("▶ ")
    .block(panel(
        "Managed deployments — ↑/↓ select  L load  U unload  P probe  R refresh",
    ));
    let selected = state.selected_resource.as_ref().and_then(|selected| {
        state.resource_targets().iter().position(|resource| {
            resource.provider == selected.0 && resource.deployment == selected.1
        })
    });
    let mut table_state = TableState::default().with_selected(selected);
    frame.render_stateful_widget(lifecycle, chunks[1], &mut table_state);

    let pressure = state
        .snapshot
        .resources
        .value
        .as_ref()
        .and_then(|value| value.get("system_pressure"));
    let mut lines = vec![Line::from(vec![
        Span::styled("System pressure  ", Style::default().fg(ACCENT)),
        Span::styled(
            pressure
                .and_then(|value| value.get("level"))
                .and_then(Value::as_str)
                .unwrap_or("unknown"),
            pressure_style(
                pressure
                    .and_then(|value| value.get("level"))
                    .and_then(Value::as_str)
                    .unwrap_or("unknown"),
            ),
        ),
        Span::raw(format!(
            "  free={}%  source={}",
            pressure
                .and_then(|value| value.get("free_memory_percent"))
                .and_then(Value::as_u64)
                .map(|value| value.to_string())
                .unwrap_or_else(|| "—".into()),
            pressure
                .and_then(|value| value.get("source"))
                .and_then(Value::as_str)
                .unwrap_or("—")
        )),
    ])];
    if let Some(inventories) = state
        .snapshot
        .resources
        .value
        .as_ref()
        .and_then(|value| value.get("providers"))
        .and_then(Value::as_array)
    {
        for inventory in inventories {
            lines.push(Line::from(vec![
                Span::styled(
                    format!("{:<22}", value_str(inventory, "provider")),
                    Style::default().fg(ACCENT),
                ),
                Span::raw(format!(
                    " state={} installed={} available={} unavailable={}",
                    value_str(inventory, "state"),
                    value_array_len(inventory, "discovered_models"),
                    value_array_len(inventory, "available_deployments"),
                    value_array_len(inventory, "unavailable_deployments")
                )),
            ]));
        }
    }
    lines.push(endpoint_error_line("resources", &state.snapshot.resources));
    let recommendation = state
        .snapshot
        .resources
        .value
        .as_ref()
        .and_then(|value| value.pointer("/eviction_recommendation/status"))
        .and_then(Value::as_str)
        .unwrap_or("unknown");
    lines.push(kv_line("eviction plan", recommendation));
    lines.push(Line::from(Span::styled(
        "Lifecycle actions remain reservation-safe; an active deployment cannot be unloaded.",
        Style::default().fg(WARN),
    )));
    frame.render_widget(
        Paragraph::new(lines)
            .block(panel("Native resources"))
            .wrap(Wrap { trim: true }),
        chunks[2],
    );
}

fn render_config(
    frame: &mut Frame<'_>,
    area: Rect,
    state: &ConsoleState,
    supervisor: &DaemonSupervisor,
) {
    let status_style = if state.config_valid {
        Style::default().fg(GOOD)
    } else {
        Style::default().fg(BAD)
    };
    let lines = vec![
        Line::from(vec![
            Span::styled("Path       ", Style::default().fg(ACCENT)),
            Span::raw(state.config_path.display().to_string()),
        ]),
        Line::from(vec![
            Span::styled("Status     ", Style::default().fg(ACCENT)),
            Span::styled(&state.config_status, status_style),
        ]),
        Line::from(vec![
            Span::styled("Daemon     ", Style::default().fg(ACCENT)),
            Span::raw(supervisor.daemon_bin().display().to_string()),
        ]),
        Line::from(""),
        Line::from(Span::styled(
            "v  validate the strict TOML contract",
            Style::default().fg(Color::White),
        )),
        Line::from(Span::styled(
            "e  open the configured file in $VISUAL/$EDITOR, then validate it",
            Style::default().fg(Color::White),
        )),
        Line::from(Span::styled(
            "r  restart a console-owned daemon after a valid edit",
            Style::default().fg(Color::White),
        )),
        Line::from(""),
        Line::from(Span::styled(
            "The first console release does not hot-reload RuntimeConfig. Auth source changes require reopening the console; secrets never enter TOML.",
            Style::default().fg(WARN),
        )),
        Line::from(Span::styled(
            "Stopping/restarting is limited to the child process started by this console; an attached external daemon is never killed.",
            Style::default().fg(WARN),
        )),
    ];
    frame.render_widget(
        Paragraph::new(Text::from(lines))
            .block(panel("Configuration"))
            .wrap(Wrap { trim: true }),
        area,
    );
}

fn render_footer(frame: &mut Frame<'_>, area: Rect, state: &ConsoleState) {
    let style = if state.status_is_error {
        Style::default().fg(BAD)
    } else {
        Style::default().fg(GOOD)
    };
    let contextual = match state.tab {
        Tab::Overview => "s start · x stop · r restart",
        Tab::Jobs => "↑/↓ select · Enter inspect · C cancel",
        Tab::Resources => "↑/↓ select · L/U load/unload · P probe · R refresh",
        Tab::Statistics => "z clear session history",
        Tab::Logs => "↑/↓ scroll · p pause · f filter · / search · c clear",
        Tab::Config => "v validate · e edit · r restart",
    };
    let controls = Line::from(vec![
        Span::styled(
            " ",
            Style::default().fg(ACCENT).add_modifier(Modifier::BOLD),
        ),
        Span::styled(
            contextual,
            Style::default().fg(ACCENT).add_modifier(Modifier::BOLD),
        ),
        Span::styled(
            "   Tab/←→ pages · g refresh · q quit",
            Style::default().fg(MUTED),
        ),
    ]);
    let status = Line::from(vec![
        Span::styled(" status  ", Style::default().fg(MUTED)),
        Span::styled(&state.status, style),
    ]);
    frame.render_widget(
        Paragraph::new(vec![controls, status]).alignment(Alignment::Left),
        area,
    );
}

pub(super) fn panel(title: &str) -> Block<'_> {
    Block::default().borders(Borders::ALL).title(Span::styled(
        format!(" {title} "),
        Style::default().fg(ACCENT),
    ))
}

fn kv_line(key: &str, value: &str) -> Line<'static> {
    Line::from(vec![
        Span::styled(format!("{key:<20}"), Style::default().fg(MUTED)),
        Span::raw(value.to_owned()),
    ])
}

fn endpoint_error_line(label: &str, endpoint: &EndpointSnapshot) -> Line<'static> {
    match &endpoint.error {
        Some(error) => Line::from(vec![
            Span::styled(format!("{label:<20}"), Style::default().fg(MUTED)),
            Span::styled(error.to_owned(), Style::default().fg(BAD)),
        ]),
        None => kv_line(label, "ok"),
    }
}

fn string_at<'a>(endpoint: &'a EndpointSnapshot, pointer: &str) -> Option<&'a str> {
    endpoint.value.as_ref()?.pointer(pointer)?.as_str()
}

fn array_len(endpoint: &EndpointSnapshot, pointer: &str) -> usize {
    endpoint
        .value
        .as_ref()
        .and_then(|value| value.pointer(pointer))
        .and_then(Value::as_array)
        .map(Vec::len)
        .unwrap_or_default()
}

fn u64_at(value: &Value, pointer: &str) -> u64 {
    value
        .pointer(pointer)
        .and_then(Value::as_u64)
        .unwrap_or_default()
}

fn value_str<'a>(value: &'a Value, key: &str) -> &'a str {
    value.get(key).and_then(Value::as_str).unwrap_or("—")
}

fn value_array_len(value: &Value, key: &str) -> usize {
    value
        .get(key)
        .and_then(Value::as_array)
        .map(Vec::len)
        .unwrap_or_default()
}

fn bool_label(value: Option<&Value>) -> String {
    match value.and_then(Value::as_bool) {
        Some(true) => "yes".into(),
        Some(false) => "no".into(),
        None => "—".into(),
    }
}

fn short(value: &str, limit: usize) -> String {
    let mut chars = value.chars();
    let prefix = chars.by_ref().take(limit).collect::<String>();
    if chars.next().is_some() {
        format!(
            "{}…",
            prefix
                .chars()
                .take(limit.saturating_sub(1))
                .collect::<String>()
        )
    } else {
        prefix
    }
}

fn format_duration(seconds: u64) -> String {
    let hours = seconds / 3_600;
    let minutes = (seconds % 3_600) / 60;
    let seconds = seconds % 60;
    format!("{hours:02}:{minutes:02}:{seconds:02}")
}

fn connection_style(connected: bool) -> Style {
    Style::default().fg(if connected { GOOD } else { BAD })
}

fn pressure_style(level: &str) -> Style {
    Style::default().fg(match level {
        "normal" => GOOD,
        "elevated" => WARN,
        "critical" => BAD,
        _ => MUTED,
    })
}

fn job_style(state: &str) -> Style {
    Style::default().fg(match state {
        "succeeded" => GOOD,
        "failed" | "expired" => BAD,
        "cancelled" => WARN,
        "running" => ACCENT,
        _ => Color::White,
    })
}

fn resource_style(state: &str) -> Style {
    Style::default().fg(match state {
        "ready" => GOOD,
        "loading" | "benchmarking" | "draining" | "unloading" => ACCENT,
        "failed" => BAD,
        "absent" => MUTED,
        _ => Color::White,
    })
}

fn format_bytes(bytes: u64) -> String {
    const GIB: f64 = 1024.0 * 1024.0 * 1024.0;
    const MIB: f64 = 1024.0 * 1024.0;
    if bytes as f64 >= GIB {
        format!("{:.1} GiB", bytes as f64 / GIB)
    } else {
        format!("{:.0} MiB", bytes as f64 / MIB)
    }
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use ratatui::{Terminal, backend::TestBackend};

    use super::{format_bytes, format_duration, render, short};
    use crate::{
        console::state::{ConsoleState, Tab},
        daemon_supervisor::DaemonSupervisor,
    };

    #[test]
    fn display_projection_truncates_unicode_and_formats_uptime() {
        assert_eq!(short("中文模型名称", 5), "中文模型…");
        assert_eq!(short("short", 8), "short");
        assert_eq!(format_duration(3_661), "01:01:01");
        assert_eq!(format_bytes(3 * 1024 * 1024 * 1024), "3.0 GiB");
    }

    #[test]
    fn every_console_tab_renders_at_the_supported_terminal_sizes() {
        let config =
            PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../config/infer.example.toml");
        let mut state = ConsoleState::new(config.clone(), 50);
        let (supervisor, _logs) = DaemonSupervisor::new(None, config);
        for (width, height) in [(80, 24), (140, 36)] {
            let backend = TestBackend::new(width, height);
            let mut terminal = Terminal::new(backend).unwrap();
            for tab in Tab::ALL {
                state.tab = tab;
                terminal
                    .draw(|frame| render(frame, &state, &supervisor))
                    .unwrap();
            }
        }
    }
}
