//! Rolling session statistics rendered from cumulative control-plane metrics.

use ratatui::{
    Frame,
    layout::{Constraint, Direction, Layout, Rect},
    style::{Modifier, Style},
    text::{Line, Span},
    widgets::{Gauge, Paragraph, Sparkline},
};
use serde_json::Value;

use crate::console::state::ConsoleState;

use super::{ACCENT, BAD, GOOD, MUTED, WARN, panel};

pub(super) fn render(frame: &mut Frame<'_>, area: Rect, state: &ConsoleState) {
    let rows = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(6),
            Constraint::Percentage(44),
            Constraint::Percentage(44),
        ])
        .split(area);
    render_summary(frame, rows[0], state);

    let upper = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Percentage(50), Constraint::Percentage(50)])
        .split(rows[1]);
    render_sparkline(
        frame,
        upper[0],
        "Completed throughput / minute",
        state.statistics.throughput(),
        GOOD,
        None,
        None,
    );
    render_sparkline(
        frame,
        upper[1],
        "Failures + expiry / minute",
        state.statistics.failures(),
        BAD,
        None,
        None,
    );

    let lower = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([
            Constraint::Percentage(34),
            Constraint::Percentage(33),
            Constraint::Percentage(33),
        ])
        .split(rows[2]);
    render_sparkline(
        frame,
        lower[0],
        "Queued Jobs",
        state.statistics.queue_depth(),
        WARN,
        None,
        None,
    );
    render_sparkline(
        frame,
        lower[1],
        "Active Attempts",
        state.statistics.active_attempts(),
        ACCENT,
        None,
        None,
    );
    render_sparkline(
        frame,
        lower[2],
        "Free memory %",
        state.statistics.free_memory(),
        GOOD,
        Some(100),
        Some(
            state
                .statistics
                .latest()
                .free_memory_percent
                .map(|value| value.to_string())
                .unwrap_or_else(|| "unknown".into()),
        ),
    );
}

fn render_summary(frame: &mut Frame<'_>, area: Rect, state: &ConsoleState) {
    let metrics = state.snapshot.metrics.value.as_ref();
    let succeeded = metric(metrics, "succeeded");
    let failed = metric(metrics, "failed");
    let cancelled = metric(metrics, "cancelled");
    let expired = metric(metrics, "expired");
    let completed = succeeded
        .saturating_add(failed)
        .saturating_add(cancelled)
        .saturating_add(expired);
    let success_ratio = if completed == 0 {
        0.0
    } else {
        succeeded as f64 / completed as f64
    };
    let latest = state.statistics.latest();
    let (cost, tokens, estimates) = ledger_totals(&state.snapshot.budget.value);
    if area.width < 110 {
        let compact = vec![
            Line::from(format!(
                "success {:.1}%  terminal={}  succeeded={}  failed/expired={}",
                success_ratio * 100.0,
                completed,
                succeeded,
                failed.saturating_add(expired)
            )),
            Line::from(format!(
                "throughput={}/min  failures={}/min  queue-wait={}ms",
                latest.throughput_per_minute,
                latest.failures_per_minute,
                latest.average_queue_wait_ms
            )),
            Line::from(format!(
                "queued={}  active={}  free-memory={}%  samples={}",
                latest.queue_depth,
                latest.active_attempts,
                latest
                    .free_memory_percent
                    .map(|value| value.to_string())
                    .unwrap_or_else(|| "—".into()),
                state.statistics.len()
            )),
            Line::from(format!(
                "ledger cost=${cost:.6}  tokens={tokens}  estimated-entries={estimates}"
            )),
        ];
        frame.render_widget(
            Paragraph::new(compact).block(panel("Runtime statistics summary")),
            area,
        );
        return;
    }

    let chunks = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([
            Constraint::Percentage(35),
            Constraint::Percentage(35),
            Constraint::Percentage(30),
        ])
        .split(area);
    let success = Gauge::default()
        .block(panel("Daemon success ratio"))
        .gauge_style(Style::default().fg(if completed == 0 {
            MUTED
        } else if success_ratio >= 0.95 {
            GOOD
        } else if success_ratio >= 0.8 {
            WARN
        } else {
            BAD
        }))
        .ratio(success_ratio.clamp(0.0, 1.0))
        .label(format!(
            "{:.1}%  {} succeeded / {} terminal",
            success_ratio * 100.0,
            succeeded,
            completed
        ));
    frame.render_widget(success, chunks[0]);

    let current = vec![
        Line::from(vec![
            Span::styled("throughput  ", Style::default().fg(MUTED)),
            Span::styled(
                format!("{} / min", latest.throughput_per_minute),
                Style::default().fg(GOOD).add_modifier(Modifier::BOLD),
            ),
            Span::styled("   failures  ", Style::default().fg(MUTED)),
            Span::styled(
                format!("{} / min", latest.failures_per_minute),
                Style::default().fg(BAD),
            ),
        ]),
        Line::from(vec![
            Span::styled("queue wait  ", Style::default().fg(MUTED)),
            Span::raw(format!("{} ms latest avg", latest.average_queue_wait_ms)),
            Span::styled("   samples  ", Style::default().fg(MUTED)),
            Span::raw(state.statistics.len().to_string()),
        ]),
    ];
    frame.render_widget(
        Paragraph::new(current).block(panel("Current rate — rolling Console window")),
        chunks[1],
    );

    let accounting = vec![
        Line::from(vec![
            Span::styled("cost      ", Style::default().fg(MUTED)),
            Span::raw(format!("${cost:.6}")),
        ]),
        Line::from(vec![
            Span::styled("tokens    ", Style::default().fg(MUTED)),
            Span::raw(tokens.to_string()),
            Span::styled("   estimated entries  ", Style::default().fg(MUTED)),
            Span::raw(estimates.to_string()),
        ]),
    ];
    frame.render_widget(
        Paragraph::new(accounting).block(panel("Stored usage ledger totals")),
        chunks[2],
    );
}

fn render_sparkline(
    frame: &mut Frame<'_>,
    area: Rect,
    title: &str,
    data: Vec<u64>,
    color: ratatui::style::Color,
    max: Option<u64>,
    latest_label: Option<String>,
) {
    let latest = data.last().copied().unwrap_or_default();
    let title = format!(
        "{title} — now {}",
        latest_label.unwrap_or_else(|| latest.to_string())
    );
    let mut sparkline = Sparkline::default()
        .block(panel(&title))
        .data(&data)
        .style(Style::default().fg(color));
    if let Some(max) = max {
        sparkline = sparkline.max(max);
    }
    frame.render_widget(sparkline, area);
}

fn metric(metrics: Option<&Value>, key: &str) -> u64 {
    metrics
        .and_then(|metrics| metrics.get(key))
        .and_then(Value::as_u64)
        .unwrap_or_default()
}

fn ledger_totals(budget: &Option<Value>) -> (f64, u64, usize) {
    budget
        .as_ref()
        .and_then(|value| value.get("usage_ledger"))
        .and_then(Value::as_array)
        .map(|entries| {
            entries
                .iter()
                .fold((0.0, 0_u64, 0_usize), |(cost, tokens, estimates), entry| {
                    (
                        cost + entry
                            .get("amount_usd")
                            .and_then(Value::as_f64)
                            .unwrap_or_default(),
                        tokens.saturating_add(
                            entry
                                .get("total_tokens")
                                .and_then(Value::as_u64)
                                .unwrap_or_default(),
                        ),
                        estimates
                            + usize::from(
                                entry
                                    .get("estimated")
                                    .and_then(Value::as_bool)
                                    .unwrap_or(false),
                            ),
                    )
                })
        })
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::ledger_totals;

    #[test]
    fn usage_totals_preserve_cost_tokens_and_estimate_evidence() {
        let budget = Some(json!({"usage_ledger": [
            {"amount_usd": 0.25, "total_tokens": 100, "estimated": false},
            {"amount_usd": 0.5, "total_tokens": 50, "estimated": true}
        ]}));
        assert_eq!(ledger_totals(&budget), (0.75, 150, 1));
    }
}
