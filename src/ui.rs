use ratatui::{
    layout::{Alignment, Constraint, Direction, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Gauge, Paragraph, Wrap},
    Frame,
};
use std::time::Instant;

use crate::AppState;

pub fn render(f: &mut Frame, state: &AppState) {
    let area = f.area();
    let block = Block::default().title(" hifi ").borders(Borders::ALL);
    let inner = block.inner(area);
    f.render_widget(block, area);

    let status = status_line(state);
    let status_h = if status.is_some() { 1 } else { 0 };

    let layout = Layout::default()
        .direction(Direction::Vertical)
        .margin(1)
        .constraints([
            Constraint::Min(0),
            Constraint::Length(status_h),
            Constraint::Length(1),
        ])
        .split(inner);

    render_body(f, layout[0], state);
    if let Some((text, color)) = status {
        let p = Paragraph::new(text)
            .wrap(Wrap { trim: true })
            .style(Style::default().fg(color));
        f.render_widget(p, layout[1]);
    }
    render_help(f, layout[2]);
}

fn status_line(state: &AppState) -> Option<(String, Color)> {
    if let Some(until) = state.rate_limited_until {
        let remaining = until.saturating_duration_since(Instant::now()).as_secs();
        if remaining > 0 {
            return Some((
                format!("⚠ rate limited; retrying in {remaining}s"),
                Color::Yellow,
            ));
        }
    }
    state
        .error
        .as_ref()
        .map(|e| (format!("error: {e}"), Color::Red))
}

fn render_body(f: &mut Frame, area: Rect, state: &AppState) {
    let Some(pb) = &state.playback else {
        let p = Paragraph::new("Nothing playing.\n\nStart a track on any Spotify device.")
            .alignment(Alignment::Center);
        f.render_widget(p, area);
        return;
    };

    let Some(track) = &pb.item else {
        let p = Paragraph::new("Track info unavailable.").alignment(Alignment::Center);
        f.render_widget(p, area);
        return;
    };

    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(1), // title
            Constraint::Length(1), // artists
            Constraint::Length(1), // album
            Constraint::Length(1), // spacer
            Constraint::Length(1), // progress
            Constraint::Min(0),    // spacer
        ])
        .split(area);

    f.render_widget(
        Paragraph::new(track.name.as_str())
            .style(Style::default().add_modifier(Modifier::BOLD)),
        chunks[0],
    );

    let artists = track
        .artists
        .iter()
        .map(|a| a.name.as_str())
        .collect::<Vec<_>>()
        .join(", ");
    f.render_widget(Paragraph::new(artists), chunks[1]);

    f.render_widget(
        Paragraph::new(track.album.name.as_str())
            .style(Style::default().fg(Color::DarkGray)),
        chunks[2],
    );

    let progress_ms = displayed_progress(state).min(track.duration_ms);
    let ratio = (progress_ms as f64 / track.duration_ms.max(1) as f64).clamp(0.0, 1.0);
    let label = format!(
        "{}  {} / {}",
        if pb.is_playing { "▶" } else { "⏸" },
        fmt_dur(progress_ms),
        fmt_dur(track.duration_ms),
    );
    let gauge = Gauge::default()
        .ratio(ratio)
        .label(label)
        .gauge_style(Style::default().fg(Color::Green));
    f.render_widget(gauge, chunks[4]);
}

fn render_help(f: &mut Frame, area: Rect) {
    let help = Paragraph::new(Line::from(vec![
        Span::styled("[space]", Style::default().fg(Color::Cyan)),
        Span::raw(" play/pause   "),
        Span::styled("[q]", Style::default().fg(Color::Cyan)),
        Span::raw(" quit"),
    ]))
    .alignment(Alignment::Center);
    f.render_widget(help, area);
}

fn displayed_progress(s: &AppState) -> u64 {
    let Some(pb) = &s.playback else { return 0 };
    let base = pb.progress_ms.unwrap_or(0);
    if !pb.is_playing {
        return base;
    }
    let Some(polled) = s.last_poll else {
        return base;
    };
    base + polled.elapsed().as_millis() as u64
}

fn fmt_dur(ms: u64) -> String {
    let total_s = ms / 1000;
    let m = total_s / 60;
    let s = total_s % 60;
    format!("{m}:{s:02}")
}
