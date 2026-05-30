use ratatui::{
    layout::{Alignment, Constraint, Direction, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Clear, Gauge, Paragraph, Wrap},
    Frame,
};
use ratatui_image::StatefulImage;
use std::time::Instant;

use crate::keys::{self, ModeMask};
use crate::{AppState, Cmd, CommandState, Mode, SearchState};

pub fn render(f: &mut Frame, state: &mut AppState) {
    let area = f.area();
    let title = match (state.reconnecting, &state.device_name, &state.streaming_failed) {
        (true, _, _) => " hifi · reconnecting... ".to_string(),
        (false, Some(name), _) => format!(" hifi · device: {name} "),
        (false, None, None) => " hifi · starting device... ".to_string(),
        (false, None, Some(_)) => " hifi · streaming unavailable ".to_string(),
    };
    let block = Block::default().title(title).borders(Borders::ALL);
    let inner = block.inner(area);
    f.render_widget(block, area);

    let status = status_line(state);
    let status_h = if status.is_some() { 1 } else { 0 };
    let top_h = top_height(inner.height);

    let layout = Layout::default()
        .direction(Direction::Vertical)
        .margin(1)
        .constraints([
            Constraint::Length(top_h),
            Constraint::Min(0),
            Constraint::Length(1),
            Constraint::Length(1),
            Constraint::Length(status_h),
            Constraint::Length(1),
        ])
        .split(inner);

    render_top(f, layout[0], state);
    render_progress(f, layout[2], layout[3], state);
    if let Some((text, color)) = status {
        let p = Paragraph::new(text)
            .wrap(Wrap { trim: true })
            .style(Style::default().fg(color));
        f.render_widget(p, layout[4]);
    }
    render_footer(f, layout[5], state.mode.mask());

    match &state.mode {
        Mode::NowPlaying => {}
        Mode::Search(search) => render_search_overlay(f, area, search),
        Mode::Help => render_help_overlay(f, area),
        Mode::Command(cmd) => render_command_overlay(f, area, cmd),
    }
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
    // Surface a persistent warning when the librespot device has dropped off
    // Spotify Connect — without it the user just sees mysterious 404s.
    if state.device_present == Some(false) && state.error.is_none() {
        return Some((
            "⚠ Connect device 'hifi' is offline — restart hifi to reconnect".to_string(),
            Color::Yellow,
        ));
    }
    if let Some(msg) = &state.streaming_failed {
        if state.error.is_none() {
            return Some((format!("⚠ streaming disabled: {msg}"), Color::Yellow));
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
            Constraint::Length(2),
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
        let msg = if state.device_name.is_none() && state.streaming_failed.is_none() {
            "Connecting to Spotify...\n\nStarting the 'hifi' Connect device — this usually takes a couple of seconds."
        } else {
            "Nothing playing.\n\nStart a track on any Spotify device,\nor pick this one in the Connect picker."
        };
        let p = Paragraph::new(msg).alignment(Alignment::Left);
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
            Constraint::Length(1),
            Constraint::Length(1),
            Constraint::Length(1),
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

fn render_progress(f: &mut Frame, label_area: Rect, bar_area: Rect, state: &AppState) {
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
    f.render_widget(Paragraph::new(label), label_area);
    let gauge = Gauge::default()
        .ratio(ratio)
        .label("")
        .gauge_style(Style::default().fg(Color::Green));
    f.render_widget(gauge, bar_area);
}

fn render_footer(f: &mut Frame, area: Rect, mode: ModeMask) {
    let mut spans = Vec::new();
    let mut first = true;
    for h in keys::for_mode(mode) {
        if !first {
            spans.push(Span::raw("   "));
        }
        first = false;
        spans.push(Span::styled(
            format!("[{}]", h.key),
            Style::default().fg(Color::Cyan),
        ));
        spans.push(Span::raw(format!(" {}", h.action)));
    }
    let p = Paragraph::new(Line::from(spans)).alignment(Alignment::Center);
    f.render_widget(p, area);
}

fn render_search_overlay(f: &mut Frame, area: Rect, s: &SearchState) {
    let rect = centered(area, 70, 80);
    f.render_widget(Clear, rect);
    let block = Block::default().title(" search ").borders(Borders::ALL);
    let inner = block.inner(rect);
    f.render_widget(block, rect);

    let layout = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(1), // input
            Constraint::Length(1), // hint / count
            Constraint::Min(0),    // results
        ])
        .split(inner);

    render_search_input(f, layout[0], s);
    let total = visible_total(s);
    let hint = if s.input.is_empty() {
        if total == 0 {
            "type to search · esc to close".to_string()
        } else {
            "type to search · ↑/↓ to pick · enter to play / re-run · esc to close".to_string()
        }
    } else if total == 0 {
        format!("no results for \"{}\"", s.input)
    } else {
        format!("{total} results · ↑/↓ to move · enter to play · esc to close")
    };
    f.render_widget(
        Paragraph::new(hint).style(Style::default().fg(Color::DarkGray)),
        layout[1],
    );

    render_search_results(f, layout[2], s);
}

fn render_search_input(f: &mut Frame, area: Rect, s: &SearchState) {
    let prompt = Span::styled("/ ", Style::default().fg(Color::Green));
    let chars: Vec<char> = s.input.chars().collect();
    let mut spans = vec![prompt];
    for (i, c) in chars.iter().enumerate() {
        if i == s.cursor {
            spans.push(Span::styled(
                c.to_string(),
                Style::default().add_modifier(Modifier::REVERSED),
            ));
        } else {
            spans.push(Span::raw(c.to_string()));
        }
    }
    if s.cursor >= chars.len() {
        spans.push(Span::styled(
            " ",
            Style::default().add_modifier(Modifier::REVERSED),
        ));
    }
    f.render_widget(Paragraph::new(Line::from(spans)), area);
}

fn render_search_results(f: &mut Frame, area: Rect, s: &SearchState) {
    let mut lines: Vec<Line> = Vec::new();
    let mut row = 0usize;

    // Empty input → show only the two "recents" sections; the live search
    // sections below would all be empty anyway.
    if s.input.is_empty() {
        if s.recent_queries.is_empty() && s.recent_tracks.is_empty() {
            lines.push(Line::from(Span::styled(
                "  start typing to search Spotify",
                Style::default().fg(Color::DarkGray),
            )));
        } else {
            push_section(
                &mut lines,
                "Recent searches",
                s.recent_queries.iter().map(|q| format!("  {q}")),
                &mut row,
                s.selected,
            );
            push_section(
                &mut lines,
                "Recently played",
                s.recent_tracks.iter().map(|t| {
                    format!(
                        "  {} — {}",
                        t.name,
                        t.artists.iter().map(|a| a.name.as_str()).collect::<Vec<_>>().join(", ")
                    )
                }),
                &mut row,
                s.selected,
            );
        }
        f.render_widget(Paragraph::new(lines).wrap(Wrap { trim: false }), area);
        return;
    }

    if let Some(ctx) = &s.in_context {
        if !ctx.filtered.is_empty() {
            push_header(&mut lines, "In current playlist");
            for &i in &ctx.filtered {
                let t = &ctx.tracks[i];
                let label = format!(
                    "  {} — {}",
                    t.name,
                    t.artists.iter().map(|a| a.name.as_str()).collect::<Vec<_>>().join(", ")
                );
                lines.push(styled_row(label, row == s.selected));
                row += 1;
            }
        } else if !s.input.is_empty() && !ctx.tracks.is_empty() {
            // playlist loaded, no matches — keep the section header so user knows it was searched
            push_header(&mut lines, "In current playlist (no matches)");
        }
    }

    push_section(&mut lines, "Tracks", s.results.tracks.iter().map(|t| {
        format!(
            "  {} — {}",
            t.name,
            t.artists.iter().map(|a| a.name.as_str()).collect::<Vec<_>>().join(", ")
        )
    }), &mut row, s.selected);

    push_section(&mut lines, "Albums", s.results.albums.iter().map(|a| {
        let artists = a.artists.iter().map(|x| x.name.as_str()).collect::<Vec<_>>().join(", ");
        if artists.is_empty() {
            format!("  {}", a.name)
        } else {
            format!("  {} — {}", a.name, artists)
        }
    }), &mut row, s.selected);

    push_section(
        &mut lines,
        "Artists",
        s.results.artists.iter().map(|a| format!("  {}", a.name)),
        &mut row,
        s.selected,
    );

    push_section(
        &mut lines,
        "Playlists",
        s.results.playlists.iter().map(|p| {
            let owner = p.owner.as_ref().and_then(|o| o.display_name.as_deref()).unwrap_or("");
            if owner.is_empty() {
                format!("  {}", p.name)
            } else {
                format!("  {} — {}", p.name, owner)
            }
        }),
        &mut row,
        s.selected,
    );

    f.render_widget(Paragraph::new(lines).wrap(Wrap { trim: false }), area);
}

fn push_header(lines: &mut Vec<Line>, text: &str) {
    if !lines.is_empty() {
        lines.push(Line::from(""));
    }
    lines.push(Line::from(Span::styled(
        text.to_string(),
        Style::default().fg(Color::DarkGray).add_modifier(Modifier::BOLD),
    )));
}

fn push_section<I: Iterator<Item = String>>(
    lines: &mut Vec<Line>,
    title: &str,
    items: I,
    row: &mut usize,
    selected: usize,
) {
    let collected: Vec<String> = items.collect();
    if collected.is_empty() {
        return;
    }
    push_header(lines, title);
    for label in collected {
        lines.push(styled_row(label, *row == selected));
        *row += 1;
    }
}

fn styled_row(label: String, selected: bool) -> Line<'static> {
    if selected {
        Line::from(Span::styled(
            label,
            Style::default()
                .bg(Color::DarkGray)
                .add_modifier(Modifier::BOLD),
        ))
    } else {
        Line::from(label)
    }
}

fn visible_total(s: &SearchState) -> usize {
    if s.input.is_empty() {
        return s.recent_queries.len() + s.recent_tracks.len();
    }
    let in_ctx = s.in_context.as_ref().map(|c| c.filtered.len()).unwrap_or(0);
    in_ctx
        + s.results.tracks.len()
        + s.results.albums.len()
        + s.results.artists.len()
        + s.results.playlists.len()
}

fn render_command_overlay(f: &mut Frame, area: Rect, cmd: &CommandState) {
    let filtered = cmd.filtered();
    // Height: title + input + hint + a row per command, capped by viewport.
    let desired = 4 + filtered.len() as u16;
    let height = desired.min(area.height.saturating_sub(2));
    let width = 64u16.min(area.width.saturating_sub(2));
    let rect = centered_exact(area, width, height);
    f.render_widget(Clear, rect);
    let block = Block::default()
        .title(" command (`:`) ")
        .borders(Borders::ALL);
    let inner = block.inner(rect);
    f.render_widget(block, rect);

    let layout = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(1), // input
            Constraint::Length(1), // hint
            Constraint::Min(0),    // list
        ])
        .split(inner);

    // Input line — vim-style ":" prompt followed by the typed query.
    let mut spans = vec![Span::styled(": ", Style::default().fg(Color::Green))];
    let chars: Vec<char> = cmd.input.chars().collect();
    for (i, c) in chars.iter().enumerate() {
        if i == cmd.cursor {
            spans.push(Span::styled(
                c.to_string(),
                Style::default().add_modifier(Modifier::REVERSED),
            ));
        } else {
            spans.push(Span::raw(c.to_string()));
        }
    }
    if cmd.cursor >= chars.len() {
        spans.push(Span::styled(
            " ",
            Style::default().add_modifier(Modifier::REVERSED),
        ));
    }
    f.render_widget(Paragraph::new(Line::from(spans)), layout[0]);

    let hint = if filtered.is_empty() {
        format!("no commands match \"{}\"", cmd.input)
    } else {
        format!(
            "{}/{} · ↑/↓ to move · enter to run · esc to close",
            filtered.len(),
            Cmd::ALL.len()
        )
    };
    f.render_widget(
        Paragraph::new(hint).style(Style::default().fg(Color::DarkGray)),
        layout[1],
    );

    let lines: Vec<Line> = filtered
        .iter()
        .enumerate()
        .map(|(i, c)| {
            let label = format!("  {:<12}  {}", c.name(), c.description());
            if i == cmd.selected {
                Line::from(Span::styled(
                    label,
                    Style::default()
                        .bg(Color::DarkGray)
                        .add_modifier(Modifier::BOLD),
                ))
            } else {
                Line::from(label)
            }
        })
        .collect();
    f.render_widget(Paragraph::new(lines).wrap(Wrap { trim: false }), layout[2]);
}

fn render_help_overlay(f: &mut Frame, area: Rect) {
    let rows: Vec<&keys::Hotkey> = keys::for_mode(ModeMask::NOW_PLAYING).collect();
    let height = (rows.len() as u16 + 4).min(area.height);
    let width = 44u16.min(area.width.saturating_sub(2));
    let rect = centered_exact(area, width, height);
    f.render_widget(Clear, rect);
    let block = Block::default().title(" help ").borders(Borders::ALL);
    let inner = block.inner(rect);
    f.render_widget(block, rect);

    let mut lines = vec![Line::from(Span::styled(
        "Hotkeys",
        Style::default().fg(Color::DarkGray),
    )), Line::from("")];
    for h in rows {
        lines.push(Line::from(vec![
            Span::styled(format!("  {:<8}", h.key), Style::default().fg(Color::Cyan)),
            Span::raw("  "),
            Span::raw(h.action),
        ]));
    }
    lines.push(Line::from(""));
    lines.push(Line::from(Span::styled(
        "  esc or ? to close",
        Style::default().fg(Color::DarkGray),
    )));

    f.render_widget(Paragraph::new(lines), inner);
}

fn centered(area: Rect, pct_w: u16, pct_h: u16) -> Rect {
    let w = (area.width as u32 * pct_w as u32 / 100) as u16;
    let h = (area.height as u32 * pct_h as u32 / 100) as u16;
    centered_exact(area, w, h)
}

fn centered_exact(area: Rect, w: u16, h: u16) -> Rect {
    let w = w.min(area.width);
    let h = h.min(area.height);
    let x = area.x + (area.width.saturating_sub(w)) / 2;
    let y = area.y + (area.height.saturating_sub(h)) / 2;
    Rect { x, y, width: w, height: h }
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
