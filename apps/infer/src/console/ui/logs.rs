//! Filtered, scrollable rendering for the console-owned daemon log stream.

use ratatui::{
    Frame,
    layout::{Constraint, Direction, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Paragraph, Wrap},
};

use crate::{console::state::ConsoleState, daemon_supervisor::LogLevel};

use super::{ACCENT, BAD, GOOD, MUTED, WARN, panel};

pub(super) fn render(frame: &mut Frame<'_>, area: Rect, state: &ConsoleState) {
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Length(4), Constraint::Min(4)])
        .split(area);
    let logs = &state.logs;
    let query = if logs.editing_query {
        format!("/{}█", logs.query)
    } else if logs.query.is_empty() {
        "—".into()
    } else {
        format!("/{}", logs.query)
    };
    let summary = vec![
        Line::from(vec![
            Span::styled("filter ", Style::default().fg(MUTED)),
            Span::styled(logs.filter.label(), Style::default().fg(ACCENT)),
            Span::raw("   "),
            Span::styled("search ", Style::default().fg(MUTED)),
            Span::raw(query),
            Span::raw("   "),
            Span::styled("mode ", Style::default().fg(MUTED)),
            Span::styled(
                if logs.follow { "FOLLOW" } else { "PAUSED" },
                Style::default()
                    .fg(if logs.follow { GOOD } else { WARN })
                    .add_modifier(Modifier::BOLD),
            ),
        ]),
        Line::from(vec![
            Span::styled("buffered ", Style::default().fg(MUTED)),
            Span::raw(logs.len().to_string()),
            Span::raw("   "),
            Span::styled("matching ", Style::default().fg(MUTED)),
            Span::raw(logs.matching_count().to_string()),
            Span::raw("   "),
            Span::styled("warn ", Style::default().fg(WARN)),
            Span::raw(logs.level_count(LogLevel::Warn).to_string()),
            Span::raw("   "),
            Span::styled("error ", Style::default().fg(BAD)),
            Span::raw(logs.level_count(LogLevel::Error).to_string()),
            Span::raw(format!("   offset {}", logs.offset_from_bottom())),
        ]),
    ];
    frame.render_widget(
        Paragraph::new(summary).block(panel(
            "Log controls — f source/severity  / search  p pause/follow  c clear",
        )),
        chunks[0],
    );

    let visible_height = chunks[1].height.saturating_sub(2) as usize;
    let first_timestamp = logs
        .visible(usize::MAX)
        .first()
        .map(|line| line.recorded_at_unix_ms);
    let lines = logs
        .visible(visible_height)
        .into_iter()
        .map(|line| {
            let elapsed = first_timestamp
                .map(|start| line.recorded_at_unix_ms.saturating_sub(start))
                .unwrap_or_default();
            let style = level_style(line.level);
            Line::from(vec![
                Span::styled(
                    format!("{} ", format_elapsed(elapsed)),
                    Style::default().fg(MUTED),
                ),
                Span::styled(format!("{:<7} ", line.level.label()), style),
                Span::styled(
                    format!("{:<7} ", line.source.label()),
                    source_style(line.source),
                ),
                Span::raw(&line.text),
            ])
        })
        .collect::<Vec<_>>();
    let title = if logs.len() == 0 {
        "Logs — start inferd from this console to capture stdout/stderr".into()
    } else {
        format!(
            "Logs — showing {}/{} matching lines",
            lines.len(),
            logs.matching_count()
        )
    };
    frame.render_widget(
        Paragraph::new(lines)
            .block(panel(&title))
            .wrap(Wrap { trim: false }),
        chunks[1],
    );
}

fn level_style(level: LogLevel) -> Style {
    Style::default().fg(match level {
        LogLevel::Info => Color::White,
        LogLevel::Warn => WARN,
        LogLevel::Error => BAD,
    })
}

fn source_style(source: crate::daemon_supervisor::LogSource) -> Style {
    use crate::daemon_supervisor::LogSource;
    Style::default().fg(match source {
        LogSource::System => ACCENT,
        LogSource::Stdout => Color::White,
        LogSource::Stderr => WARN,
    })
}

fn format_elapsed(elapsed_ms: u64) -> String {
    let seconds = elapsed_ms / 1_000;
    format!(
        "+{:02}:{:02}:{:02}",
        seconds / 3_600,
        (seconds % 3_600) / 60,
        seconds % 60
    )
}

#[cfg(test)]
mod tests {
    use super::format_elapsed;

    #[test]
    fn elapsed_log_time_is_compact() {
        assert_eq!(format_elapsed(3_661_999), "+01:01:01");
    }
}
