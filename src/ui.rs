use ratatui::{
    layout::{Alignment, Constraint, Direction, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Bar, BarChart, BarGroup, Block, Borders, Gauge, Paragraph, Wrap},
    Frame,
};
use ratatui_image::StatefulImage;
use std::time::Instant;

use crate::{
    streaming::{decay_for_elapsed, peak_decay_for_elapsed, NUM_BANDS},
    AppState,
};

pub fn render(f: &mut Frame, state: &mut AppState) {
    let area = f.area();
    let title = match &state.device_name {
        Some(name) => format!(" hifi · device: {name} "),
        None => " hifi ".to_string(),
    };
    let block = Block::default().title(title).borders(Borders::ALL);
    let inner = block.inner(area);
    f.render_widget(block, area);

    let status = status_line(state);
    let status_h = if status.is_some() { 1 } else { 0 };
    let viz_h = viz_height(state, inner.height);
    let top_h = top_height(inner.height);

    let layout = Layout::default()
        .direction(Direction::Vertical)
        .margin(1)
        .constraints([
            Constraint::Length(top_h),   // art + info (bounded)
            Constraint::Min(0),          // flex spacer
            Constraint::Length(1),       // progress
            Constraint::Length(viz_h),   // spectrum / hint / 0
            Constraint::Length(status_h),
            Constraint::Length(1),       // help
        ])
        .split(inner);

    render_top(f, layout[0], state);
    render_progress(f, layout[2], state);
    render_spectrum(f, layout[3], state);
    if let Some((text, color)) = status {
        let p = Paragraph::new(text)
            .wrap(Wrap { trim: true })
            .style(Style::default().fg(color));
        f.render_widget(p, layout[4]);
    }
    render_help(f, layout[5]);
}

fn top_height(inner_h: u16) -> u16 {
    if inner_h >= 18 {
        8
    } else if inner_h >= 12 {
        6
    } else {
        4
    }
}

fn viz_height(state: &AppState, inner_h: u16) -> u16 {
    let Some(b) = &state.bands else { return 0 };
    let active = b.lock().is_active;
    if active {
        if inner_h >= 18 {
            8
        } else if inner_h >= 12 {
            5
        } else {
            3
        }
    } else {
        // streaming up but audio not routed: show a 1-line hint
        1
    }
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

fn render_top(f: &mut Frame, area: Rect, state: &mut AppState) {
    let want_art = state.art.is_some() && area.width >= 50 && area.height >= 4;
    if !want_art {
        render_info(f, area, state);
        return;
    }
    let art_w = (area.height * 2).min(20).min(area.width / 3);
    let cols = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([
            Constraint::Length(art_w),
            Constraint::Length(2), // gap
            Constraint::Min(0),
        ])
        .split(area);
    if let Some(art) = state.art.as_mut() {
        f.render_stateful_widget(StatefulImage::default(), cols[0], art);
    }
    render_info(f, cols[2], state);
}

fn render_info(f: &mut Frame, area: Rect, state: &AppState) {
    let Some(pb) = &state.playback else {
        let p = Paragraph::new("Nothing playing.\n\nStart a track on any Spotify device,\nor pick this one in the Connect picker.")
            .alignment(Alignment::Left);
        f.render_widget(p, area);
        return;
    };

    let Some(track) = &pb.item else {
        f.render_widget(Paragraph::new("Track info unavailable."), area);
        return;
    };

    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(1), // title
            Constraint::Length(1), // artists
            Constraint::Length(1), // album
            Constraint::Min(0),
        ])
        .split(area);

    f.render_widget(
        Paragraph::new(track.name.as_str())
            .style(Style::default().add_modifier(Modifier::BOLD))
            .wrap(Wrap { trim: true }),
        chunks[0],
    );

    let artists = track
        .artists
        .iter()
        .map(|a| a.name.as_str())
        .collect::<Vec<_>>()
        .join(", ");
    f.render_widget(Paragraph::new(artists).wrap(Wrap { trim: true }), chunks[1]);

    f.render_widget(
        Paragraph::new(track.album.name.as_str())
            .style(Style::default().fg(Color::DarkGray))
            .wrap(Wrap { trim: true }),
        chunks[2],
    );
}

fn render_progress(f: &mut Frame, area: Rect, state: &AppState) {
    let Some(pb) = &state.playback else { return };
    let Some(track) = &pb.item else { return };
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
    f.render_widget(gauge, area);
}

fn render_spectrum(f: &mut Frame, area: Rect, state: &AppState) {
    if area.height == 0 {
        return;
    }
    let Some(bands_lock) = state.bands.as_ref() else {
        return;
    };
    let guard = bands_lock.lock();
    if !guard.is_active {
        drop(guard);
        let device = state.device_name.as_deref().unwrap_or("hifi");
        let p = Paragraph::new(Line::from(vec![
            Span::styled("◌ ", Style::default().fg(Color::DarkGray)),
            Span::styled(
                format!("Pick '{device}' in Spotify Connect to see the visualizer"),
                Style::default().fg(Color::DarkGray),
            ),
        ]))
        .alignment(Alignment::Center);
        f.render_widget(p, area);
        return;
    }
    let display_decay = decay_for_elapsed(guard.updated_at.elapsed());
    let peak_norm =
        (guard.peak_envelope * peak_decay_for_elapsed(guard.updated_at.elapsed())).max(1e-6);
    let values: [f32; NUM_BANDS] = guard.values;
    drop(guard);

    let num_bars = (area.width as usize).min(values.len()).max(1);
    let max_val = u64::from(area.height) * 8;
    let step = values.len() as f64 / num_bars as f64;
    let bars: Vec<Bar> = (0..num_bars)
        .map(|i| {
            let idx = ((i as f64 * step) as usize).min(values.len() - 1);
            let norm = ((values[idx] * display_decay) / peak_norm)
                .clamp(0.0, 1.0)
                .powf(0.5);
            let val = (norm * max_val as f32) as u64;
            Bar::default()
                .value(val)
                .text_value(String::new())
                .style(Style::default().fg(bar_color(norm)))
        })
        .collect();

    let chart = BarChart::default()
        .data(BarGroup::default().bars(&bars))
        .bar_width(1)
        .bar_gap(0)
        .max(max_val);
    f.render_widget(chart, area);
}

fn bar_color(t: f32) -> Color {
    let (r, g, b) = if t < 0.5 {
        let s = t * 2.0;
        (
            (30.0 + 20.0 * s) as u8,
            (100.0 + 155.0 * s) as u8,
            (255.0 * (1.0 - s * 0.5)) as u8,
        )
    } else {
        let s = (t - 0.5) * 2.0;
        (
            (50.0 + 205.0 * s) as u8,
            (255.0 * (1.0 - s)) as u8,
            (128.0 * (1.0 - s)) as u8,
        )
    };
    Color::Rgb(r, g, b)
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
