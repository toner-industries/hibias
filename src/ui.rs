use ratatui::{
    layout::{Alignment, Constraint, Direction, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Clear, Paragraph, Wrap},
    Frame,
};
use ratatui_image::StatefulImage;
use std::time::Instant;

use crate::app::{
    AppState, BrowseState, Cmd, CommandState, DevicesState, Focus, LibraryState, LibraryTab,
    Overlay, SearchState, Tab,
};
use crate::art::ArtCache;
use crate::keys::{self, ModeMask};

/// Fixed canvas size. Layouts compute against this regardless of the actual
/// terminal dimensions, so the UI doesn't reflow as the user resizes.
/// Terminals larger than this leave the surrounding cells empty; smaller
/// terminals clip the bottom-right.
pub const FIXED_W: u16 = 96;
pub const FIXED_H: u16 = 40;

pub fn render(f: &mut Frame, state: &mut AppState, art: &mut ArtCache) {
    // Pin to a fixed-size top-left rect — ratatui silently clips writes
    // that fall outside the terminal buffer, so a smaller terminal just
    // crops the canvas instead of rearranging the layout.
    let area = Rect {
        x: 0,
        y: 0,
        width: FIXED_W,
        height: FIXED_H,
    };
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

    // Shared chrome: a tab strip on top, the body in the middle (per active
    // tab), then the optional status line and the footer.
    let rows = Layout::default()
        .direction(Direction::Vertical)
        .margin(1)
        .constraints([
            Constraint::Length(1),         // tab strip
            Constraint::Min(0),            // body
            Constraint::Length(status_h),  // status line
            Constraint::Length(1),         // footer
        ])
        .split(inner);

    let tabs_focused = state.overlay.is_none() && state.focus == Focus::Tabs;
    render_tab_strip(f, rows[0], state.tab, tabs_focused);
    match state.tab {
        Tab::NowPlaying => render_now_playing_body(f, rows[1], state, art),
        Tab::Search => render_search_tab(f, rows[1], &state.search),
        Tab::Library => render_library_tab(f, rows[1], &state.library),
    }
    if let Some((text, color)) = status {
        let p = Paragraph::new(text)
            .wrap(Wrap { trim: true })
            .style(Style::default().fg(color));
        f.render_widget(p, rows[2]);
    }
    if tabs_focused {
        // Replace the per-tab footer with tab-strip navigation hints.
        render_tab_footer(f, rows[3]);
    } else {
        render_footer(f, rows[3], crate::app::active_mask(state));
    }

    // Transient overlays draw on top of the whole canvas.
    match &state.overlay {
        None => {}
        Some(Overlay::Help) => render_help_overlay(f, area),
        Some(Overlay::Command(cmd)) => render_command_overlay(f, area, cmd),
        Some(Overlay::Devices(dev)) => render_devices_overlay(f, area, dev),
        Some(Overlay::Browse(browse)) => render_browse_overlay(f, area, browse),
    }
}

/// The top "Now Playing | Search | Library" strip. The active tab is cyan and
/// bold; the others are dim. When the strip itself is focused (the user
/// arrowed up onto it), the active tab is shown as a filled pill so it's clear
/// left/right will switch tabs. Mirrors the tab row in design/mockups.html.
fn render_tab_strip(f: &mut Frame, area: Rect, active: Tab, focused: bool) {
    let mut spans = Vec::new();
    for (i, tab) in Tab::ALL.iter().enumerate() {
        if i > 0 {
            spans.push(Span::styled("  ·  ", Style::default().fg(Color::DarkGray)));
        }
        let style = if *tab == active {
            if focused {
                Style::default()
                    .fg(Color::Black)
                    .bg(Color::Cyan)
                    .add_modifier(Modifier::BOLD)
            } else {
                Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD)
            }
        } else {
            Style::default().fg(Color::DarkGray)
        };
        let label = if *tab == active && focused {
            format!(" {} ", tab.label())
        } else {
            tab.label().to_string()
        };
        spans.push(Span::styled(label, style));
    }
    f.render_widget(Paragraph::new(Line::from(spans)), area);
}

/// Footer shown while the tab strip is focused.
fn render_tab_footer(f: &mut Frame, area: Rect) {
    let spans = vec![
        Span::styled("[←/→]", Style::default().fg(Color::Cyan)),
        Span::raw(" switch tab   "),
        Span::styled("[↓]", Style::default().fg(Color::Cyan)),
        Span::raw(" enter   "),
        Span::styled("[esc]", Style::default().fg(Color::Cyan)),
        Span::raw(" back"),
    ];
    let p = Paragraph::new(Line::from(spans)).alignment(Alignment::Center);
    f.render_widget(p, area);
}

/// Now Playing body: album art + track info up top, progress at the bottom.
fn render_now_playing_body(f: &mut Frame, area: Rect, state: &mut AppState, art: &mut ArtCache) {
    let top_h = top_height(area.height);
    let rows = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(top_h),
            Constraint::Min(0),
            Constraint::Length(1),
            Constraint::Length(1),
        ])
        .split(area);
    render_top(f, rows[0], state, art);
    render_up_next(f, rows[1], state);
    render_progress(f, rows[2], rows[3], state);
}

/// "Up Next": a compact peek at the upcoming queue, shown in the gap between
/// the now-playing header and the progress bar. The data is fetched on demand
/// (see [`crate::app::refresh_queue`]) so it can be briefly stale; empty when
/// nothing is queued or nothing is playing.
fn render_up_next(f: &mut Frame, area: Rect, state: &AppState) {
    // Only meaningful alongside a current track. Without one the queue is
    // empty and Now Playing shows its placeholder instead.
    let current_id = state
        .playback
        .as_ref()
        .and_then(|p| p.item.as_ref())
        .and_then(|t| t.id.as_deref());
    if current_id.is_none() || area.height < 2 {
        return;
    }
    // Skip a stale leading entry that's actually the track now playing — the
    // fetched queue can lag a track behind until it's re-fetched.
    let upcoming: Vec<_> = state
        .queue
        .iter()
        .filter(|t| t.id.as_deref() != current_id)
        .take(5)
        .collect();
    if upcoming.is_empty() {
        return;
    }

    let mut lines = vec![
        Line::from(""),
        Line::from(Span::styled(
            "Up Next",
            Style::default()
                .fg(Color::DarkGray)
                .add_modifier(Modifier::BOLD),
        )),
    ];
    for (i, t) in upcoming.iter().enumerate() {
        let artists = t
            .artists
            .iter()
            .map(|a| a.name.as_str())
            .collect::<Vec<_>>()
            .join(", ");
        let label = if artists.is_empty() {
            t.name.clone()
        } else {
            format!("{} — {artists}", t.name)
        };
        lines.push(Line::from(vec![
            Span::styled(format!("{}. ", i + 1), Style::default().fg(Color::DarkGray)),
            Span::raw(label),
        ]));
    }
    // No wrap: long rows truncate at the canvas width rather than reflowing,
    // and any lines past the available height clip cleanly.
    f.render_widget(Paragraph::new(lines), area);
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
    // Errors take precedence so a failure isn't masked by a stale notice.
    if let Some(e) = &state.error {
        return Some((format!("error: {e}"), Color::Red));
    }
    // Transient command notices ("♥ liked: …") sit above rate-limit /
    // device warnings so the user sees the immediate effect of what they
    // just did. Lazily expire so we don't need a separate prune tick.
    if let Some((msg, until)) = &state.notice {
        if *until > Instant::now() {
            return Some((msg.clone(), Color::Green));
        }
    }
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
    if state.device_present == Some(false) {
        return Some((
            "⚠ Connect device 'hifi' is offline — restart hifi to reconnect".to_string(),
            Color::Yellow,
        ));
    }
    if let Some(msg) = &state.streaming_failed {
        return Some((format!("⚠ streaming disabled: {msg}"), Color::Yellow));
    }
    None
}

fn render_top(f: &mut Frame, area: Rect, state: &AppState, art: &mut ArtCache) {
    // Only show art alongside a real track. The cache only returns an image
    // whose track id matches what's playing, so a stale cover is never paired
    // with a changed track — but we also gate on `has_track` here to keep the
    // rendering invariant local to this function.
    let has_track = state
        .playback
        .as_ref()
        .and_then(|p| p.item.as_ref())
        .is_some();
    let cover = if has_track && area.width >= 50 && area.height >= 4 {
        art.get_for(state.current_track_id.as_deref())
    } else {
        None
    };
    let Some(cover) = cover else {
        render_info(f, area, state);
        return;
    };
    let art_w = (area.height * 2).min(20).min(area.width / 3);
    let cols = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([
            Constraint::Length(art_w),
            Constraint::Length(2),
            Constraint::Min(0),
        ])
        .split(area);
    f.render_stateful_widget(StatefulImage::default(), cols[0], cover);
    render_info(f, cols[2], state);
}

fn render_info(f: &mut Frame, area: Rect, state: &AppState) {
    // `apply_playback_inner` filters out item-less Playbacks before they
    // reach state, but treating `playback: Some(pb { item: None })` as
    // equivalent to `playback: None` here too means a regression in the
    // filter can't surface as the old "Track info unavailable + stale art"
    // hybrid.
    let track = state.playback.as_ref().and_then(|p| p.item.as_ref());
    let Some(track) = track else {
        let msg = if state.device_name.is_none() && state.streaming_failed.is_none() {
            "Connecting to Spotify...\n\nStarting the 'hifi' Connect device — this usually takes a couple of seconds."
        } else {
            "Nothing playing.\n\nStart a track on any Spotify device,\nor pick this one in the Connect picker."
        };
        let p = Paragraph::new(msg).alignment(Alignment::Left);
        f.render_widget(p, area);
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

/// Progress-bar / play-symbol palette, kept as RGB (not named ANSI colors) so
/// the leading bar cell and the play glyph can be *blended* between the two —
/// named colors can't be interpolated.
const BAR_FILLED: (u8, u8, u8) = (46, 204, 64); // green
const BAR_EMPTY: (u8, u8, u8) = (74, 74, 74); // dark gray

fn rgb((r, g, b): (u8, u8, u8)) -> Color {
    Color::Rgb(r, g, b)
}

/// Linear blend from `a` (t=0) to `b` (t=1), clamped.
fn lerp_rgb(a: (u8, u8, u8), b: (u8, u8, u8), t: f64) -> Color {
    let t = t.clamp(0.0, 1.0);
    let mix = |x: u8, y: u8| (x as f64 + (y as f64 - x as f64) * t).round() as u8;
    Color::Rgb(mix(a.0, b.0), mix(a.1, b.1), mix(a.2, b.2))
}

fn render_progress(f: &mut Frame, label_area: Rect, bar_area: Rect, state: &AppState) {
    let Some(pb) = &state.playback else { return };
    let Some(track) = &pb.item else { return };
    let raw_ms = displayed_progress(state);
    let progress_ms = raw_ms.min(track.duration_ms);
    let ratio = (progress_ms as f64 / track.duration_ms.max(1) as f64).clamp(0.0, 1.0);

    // The play glyph stays put and instead "breathes": on a 2s cycle it eases
    // up from gray to filled green and back, gradually fading between the two
    // like the progress bar's leading cell rather than snapping between them.
    // The cycle *starts* at gray so pressing play reads as gray pause → gray
    // triangle → animation, the triangle picking up exactly where the pause
    // glyph's color left off. Driven by the real-time playback position so it
    // animates between polls. The pulse is biased toward green (squared) so it
    // rests bright and only dips to gray briefly. Under an automated harness it
    // never flashes (rests green) so screenshots are stable; when paused it's a
    // steady gray ⏸.
    let (symbol, symbol_color) = if pb.is_playing {
        let dip = if crate::testmode::under_test() {
            0.0
        } else {
            let phase = (raw_ms % 2000) as f64 / 2000.0;
            // Raised cosine: 1 (gray) at phase 0, 0 (green) at phase 0.5, back
            // to 1. Squared to dwell near green and make the gray dip brief.
            let pulse = (1.0 + (2.0 * std::f64::consts::PI * phase).cos()) / 2.0;
            pulse * pulse
        };
        ("▶", lerp_rgb(BAR_FILLED, BAR_EMPTY, dip))
    } else {
        ("⏸", rgb(BAR_EMPTY))
    };
    let label = Line::from(vec![
        Span::styled(symbol, Style::default().fg(symbol_color)),
        Span::raw(format!(
            "  {} / {}",
            fmt_dur(progress_ms),
            fmt_dur(track.duration_ms)
        )),
    ]);
    f.render_widget(Paragraph::new(label), label_area);

    // Progress bar: completed cells solid green, empty cells dark gray, and a
    // single *leading* cell that fades gray→green by the sub-cell progress.
    // Spaces with a bg colour fill the whole cell (no glyph "hump"). Because
    // the leading cell tracks the fractional position, a long track — where one
    // cell is many seconds — still shows the bar visibly creeping forward on
    // that one active character, instead of sitting frozen between whole-cell
    // ticks. The cell locks solid green as the song crosses into the next one.
    let total = bar_area.width as usize;
    let exact = ratio * total as f64;
    let filled = (exact.floor() as usize).min(total);
    let frac = (exact - filled as f64).clamp(0.0, 1.0);

    let mut spans = Vec::new();
    if filled > 0 {
        spans.push(Span::styled(
            " ".repeat(filled),
            Style::default().bg(rgb(BAR_FILLED)),
        ));
    }
    if filled < total {
        spans.push(Span::styled(
            " ".to_string(),
            Style::default().bg(lerp_rgb(BAR_EMPTY, BAR_FILLED, frac)),
        ));
        let trailing = total - filled - 1;
        if trailing > 0 {
            spans.push(Span::styled(
                " ".repeat(trailing),
                Style::default().bg(rgb(BAR_EMPTY)),
            ));
        }
    }
    f.render_widget(Paragraph::new(Line::from(spans)), bar_area);
}

fn render_footer(f: &mut Frame, area: Rect, mode: ModeMask) {
    let mut spans = Vec::new();
    let mut first = true;
    for h in keys::for_mode(mode) {
        // ctrl-c lives in the help overlay; surfacing it in every mode's
        // footer just eats horizontal room at the fixed canvas width.
        if h.key == "ctrl-c" {
            continue;
        }
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

fn render_search_tab(f: &mut Frame, area: Rect, s: &SearchState) {
    let layout = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(1), // input
            Constraint::Length(1), // hint / count
            Constraint::Min(0),    // results
        ])
        .split(area);

    render_search_input(f, layout[0], s);
    // Single source of truth shared with the dispatcher's selection clamp —
    // this count MUST match what `render_search_results` draws, or selection
    // and scroll desync.
    let total = crate::app::visible_row_count(s);
    let loading = s.is_loading();
    let hint = if s.input.is_empty() {
        if total == 0 {
            "type to search · esc to close".to_string()
        } else {
            "↑/↓ pick · enter play/re-run · esc close".to_string()
        }
    } else if loading {
        if s.last_query.is_empty() {
            "loading…".to_string()
        } else {
            let q = truncate_for_hint(&s.last_query, 40);
            format!("loading… (showing \"{q}\")")
        }
    } else if total == 0 {
        let q = truncate_for_hint(&s.input, 45);
        format!("no results for \"{q}\"")
    } else {
        format!("{total} results · ↑/↓ move · enter play · esc close")
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
    // Visual line index (within `lines`, which also holds headers and blank
    // separators) of the selected row — so the list can scroll to keep it on
    // screen instead of letting the selection walk off the bottom unseen.
    let mut selected_line: Option<usize> = None;

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
                &mut selected_line,
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
                &mut selected_line,
            );
        }
        // No wrap — long row labels clip at the right edge instead of
        // taking up two rows each and pushing later rows off-screen.
        render_scrolled(f, area, lines, selected_line);
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
                if row == s.selected {
                    selected_line = Some(lines.len());
                }
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
    }), &mut row, s.selected, &mut selected_line);

    push_section(&mut lines, "Albums", s.results.albums.iter().map(|a| {
        let artists = a.artists.iter().map(|x| x.name.as_str()).collect::<Vec<_>>().join(", ");
        if artists.is_empty() {
            format!("  {}", a.name)
        } else {
            format!("  {} — {}", a.name, artists)
        }
    }), &mut row, s.selected, &mut selected_line);

    push_section(
        &mut lines,
        "Artists",
        s.results.artists.iter().map(|a| format!("  {}", a.name)),
        &mut row,
        s.selected,
        &mut selected_line,
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
        &mut selected_line,
    );

    render_scrolled(f, area, lines, selected_line);
}

/// Render `lines` into `area`, scrolled vertically so the row at `selected_line`
/// stays visible. Search results (section headers + items across four sections)
/// routinely overflow the results viewport; without this the selection could
/// move past the last visible row with nothing scrolling into view.
fn render_scrolled(f: &mut Frame, area: Rect, mut lines: Vec<Line>, selected_line: Option<usize>) {
    let height = area.height as usize;
    if height == 0 || lines.is_empty() {
        return;
    }
    let offset = compute_scroll(selected_line.unwrap_or(0), lines.len(), height);
    lines.drain(..offset);
    lines.truncate(height);
    f.render_widget(Paragraph::new(lines), area);
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
    selected_line: &mut Option<usize>,
) {
    let collected: Vec<String> = items.collect();
    if collected.is_empty() {
        return;
    }
    push_header(lines, title);
    for label in collected {
        if *row == selected {
            *selected_line = Some(lines.len());
        }
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
    f.render_widget(Paragraph::new(lines), layout[2]);
}

fn render_browse_overlay(f: &mut Frame, area: Rect, browse: &BrowseState) {
    let rect = centered(area, 70, 80);
    f.render_widget(Clear, rect);
    let title = format!(" browse · {} ", browse.collection.kind.label());
    let block = Block::default().title(title).borders(Borders::ALL);
    let inner = block.inner(rect);
    f.render_widget(block, rect);

    let layout = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(1), // name
            Constraint::Length(1), // subtitle
            Constraint::Length(1), // hint
            Constraint::Min(0),    // list
        ])
        .split(inner);

    f.render_widget(
        Paragraph::new(browse.collection.name.clone())
            .style(Style::default().add_modifier(Modifier::BOLD)),
        layout[0],
    );
    f.render_widget(
        Paragraph::new(browse.collection.subtitle.clone())
            .style(Style::default().fg(Color::DarkGray)),
        layout[1],
    );

    let (hint, hint_color) = if browse.loading {
        ("loading...".to_string(), Color::DarkGray)
    } else if let Some(e) = &browse.error {
        // [p] play / [esc] back are already in the mode's footer, no need
        // to repeat them in the hint and crowd out the actual message.
        if is_browse_forbidden(e) {
            (
                format!("⚠ Spotify locked this {} (API) — [p] plays anyway", browse.collection.kind.label()),
                Color::Yellow,
            )
        } else {
            let short = truncate_for_hint(e, 50);
            (format!("error: {short}"), Color::Red)
        }
    } else {
        (
            format!("{} tracks", browse.tracks.len()),
            Color::DarkGray,
        )
    };
    f.render_widget(
        Paragraph::new(hint)
            .style(Style::default().fg(hint_color))
            .wrap(Wrap { trim: true }),
        layout[2],
    );

    // Compute the visible window. We keep the selected index inside it by
    // centering when possible; long collections scroll smoothly.
    let list_h = layout[3].height as usize;
    if list_h == 0 || browse.tracks.is_empty() {
        return;
    }
    let scroll = compute_scroll(browse.selected, browse.tracks.len(), list_h);
    let end = (scroll + list_h).min(browse.tracks.len());

    let lines: Vec<Line> = browse.tracks[scroll..end]
        .iter()
        .enumerate()
        .map(|(offset, t)| {
            let idx = scroll + offset;
            let artists = t
                .artists
                .iter()
                .map(|a| a.name.as_str())
                .collect::<Vec<_>>()
                .join(", ");
            let label = if artists.is_empty() {
                format!("  {:>3}. {}", idx + 1, t.name)
            } else {
                format!("  {:>3}. {} — {}", idx + 1, t.name, artists)
            };
            if idx == browse.selected {
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
    f.render_widget(Paragraph::new(lines), layout[3]);
}

/// Heuristic: was this Browse fetch error a Spotify access restriction?
/// In late 2024 Spotify locked down /playlists/{id}/tracks (and several
/// other browse endpoints) to apps created before the change; everyone
/// else gets a 403. We surface this with a different, less alarming hint.
fn is_browse_forbidden(e: &str) -> bool {
    e.contains("403 Forbidden")
        || e.contains("\"status\": 403")
        || e.contains("\"status\" : 403")
        || e.contains("Insufficient client scope")
}

/// Given a selected row, the total number of rows, and the height of the
/// list area, return the index of the first row to render so that the
/// selection stays visible (and centered when possible).
fn compute_scroll(selected: usize, total: usize, list_h: usize) -> usize {
    if total <= list_h {
        return 0;
    }
    let half = list_h / 2;
    let max_scroll = total - list_h;
    selected.saturating_sub(half).min(max_scroll)
}

fn render_help_overlay(f: &mut Frame, area: Rect) {
    let rows = keys::HELP_ROWS;
    let height = (rows.len() as u16 + 4).min(area.height);
    let width = 44u16.min(area.width.saturating_sub(2));
    let rect = centered_exact(area, width, height);
    f.render_widget(Clear, rect);
    let block = Block::default().title(" help ").borders(Borders::ALL);
    let inner = block.inner(rect);
    f.render_widget(block, rect);

    let mut lines = vec![
        Line::from(Span::styled("Hotkeys", Style::default().fg(Color::DarkGray))),
        Line::from(""),
    ];
    for (key, action) in rows {
        lines.push(Line::from(vec![
            Span::styled(format!("  {key:<10}"), Style::default().fg(Color::Cyan)),
            Span::raw("  "),
            Span::raw(*action),
        ]));
    }
    lines.push(Line::from(""));
    lines.push(Line::from(Span::styled(
        "  esc or ? to close",
        Style::default().fg(Color::DarkGray),
    )));

    f.render_widget(Paragraph::new(lines), inner);
}

/// The Library tab: a sub-tab strip (Liked / Playlists / Albums / Artists)
/// over the active section's list. Mirrors design/mockups.html.
fn render_library_tab(f: &mut Frame, area: Rect, lib: &LibraryState) {
    let rows = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(1), // sub-tab strip
            Constraint::Length(1), // header / status
            Constraint::Min(0),    // list
        ])
        .split(area);

    // Sub-tab strip.
    let mut spans = Vec::new();
    for (i, t) in LibraryTab::ALL.iter().enumerate() {
        if i > 0 {
            spans.push(Span::styled(" · ", Style::default().fg(Color::DarkGray)));
        }
        let style = if *t == lib.tab {
            Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(Color::DarkGray)
        };
        spans.push(Span::styled(t.label(), style));
    }
    f.render_widget(Paragraph::new(Line::from(spans)), rows[0]);

    // Collect the active section's display rows + status, generically.
    let (status, labels, loading, error) = library_section_view(lib);
    let header = if loading {
        ("loading…".to_string(), Color::DarkGray)
    } else if let Some(e) = error {
        (truncate_for_hint(e, 60), Color::Red)
    } else {
        (status, Color::DarkGray)
    };
    f.render_widget(
        Paragraph::new(header.0).style(Style::default().fg(header.1)),
        rows[1],
    );

    let list_h = rows[2].height as usize;
    if list_h == 0 || labels.is_empty() {
        return;
    }
    let scroll = compute_scroll(lib.selected, labels.len(), list_h);
    let end = (scroll + list_h).min(labels.len());
    let lines: Vec<Line> = labels[scroll..end]
        .iter()
        .enumerate()
        .map(|(offset, label)| styled_row(format!("  {label}"), scroll + offset == lib.selected))
        .collect();
    f.render_widget(Paragraph::new(lines), rows[2]);
}

/// Build the (status, row labels, loading, error) view for the active Library
/// sub-tab, so the renderer stays generic over the four section types.
fn library_section_view(lib: &LibraryState) -> (String, Vec<String>, bool, Option<&str>) {
    match lib.tab {
        LibraryTab::Liked => {
            let labels = lib
                .liked
                .items
                .iter()
                .map(|t| {
                    let artists =
                        t.artists.iter().map(|a| a.name.as_str()).collect::<Vec<_>>().join(", ");
                    if artists.is_empty() {
                        t.name.clone()
                    } else {
                        format!("{} — {}", t.name, artists)
                    }
                })
                .collect::<Vec<_>>();
            (
                format!("Liked songs · {}", lib.liked.items.len()),
                labels,
                lib.liked.loading,
                lib.liked.error.as_deref(),
            )
        }
        LibraryTab::Playlists => {
            let labels = lib
                .playlists
                .items
                .iter()
                .map(|p| {
                    let owner = p.owner.as_ref().and_then(|o| o.display_name.as_deref()).unwrap_or("");
                    if owner.is_empty() {
                        p.name.clone()
                    } else {
                        format!("{} — {}", p.name, owner)
                    }
                })
                .collect::<Vec<_>>();
            (
                format!("Your playlists · {}", lib.playlists.items.len()),
                labels,
                lib.playlists.loading,
                lib.playlists.error.as_deref(),
            )
        }
        LibraryTab::Albums => {
            let labels = lib
                .albums
                .items
                .iter()
                .map(|a| {
                    let artists =
                        a.artists.iter().map(|x| x.name.as_str()).collect::<Vec<_>>().join(", ");
                    if artists.is_empty() {
                        a.name.clone()
                    } else {
                        format!("{} — {}", a.name, artists)
                    }
                })
                .collect::<Vec<_>>();
            (
                format!("Saved albums · {}", lib.albums.items.len()),
                labels,
                lib.albums.loading,
                lib.albums.error.as_deref(),
            )
        }
        LibraryTab::Artists => {
            let labels = lib.artists.items.iter().map(|a| a.name.clone()).collect::<Vec<_>>();
            (
                format!("Following · {}", lib.artists.items.len()),
                labels,
                lib.artists.loading,
                lib.artists.error.as_deref(),
            )
        }
    }
}

/// The device-picker overlay — a centered box listing Connect devices, the
/// active one marked. Mirrors design/mockups.html.
fn render_devices_overlay(f: &mut Frame, area: Rect, dev: &DevicesState) {
    let desired = 4 + dev.devices.len().max(1) as u16;
    let height = desired.min(area.height.saturating_sub(2));
    let width = 49u16.min(area.width.saturating_sub(2));
    let rect = centered_exact(area, width, height);
    f.render_widget(Clear, rect);
    let block = Block::default().title(" devices ").borders(Borders::ALL);
    let inner = block.inner(rect);
    f.render_widget(block, rect);

    let layout = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Length(1), Constraint::Min(0)])
        .split(inner);

    let (status, color) = if dev.loading {
        ("loading devices…".to_string(), Color::DarkGray)
    } else if let Some(e) = &dev.error {
        (truncate_for_hint(e, 45), Color::Red)
    } else if dev.devices.is_empty() {
        ("no devices found".to_string(), Color::DarkGray)
    } else {
        (
            format!("{} devices · ↑/↓ move · enter transfer", dev.devices.len()),
            Color::DarkGray,
        )
    };
    f.render_widget(
        Paragraph::new(status).style(Style::default().fg(color)),
        layout[0],
    );

    let lines: Vec<Line> = dev
        .devices
        .iter()
        .enumerate()
        .map(|(i, d)| {
            let marker = if d.is_active { "✓" } else { " " };
            let label = format!("  {marker} {}", d.name);
            styled_row(label, i == dev.selected)
        })
        .collect();
    f.render_widget(Paragraph::new(lines), layout[1]);
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

/// Cap a user-provided string to N chars, with an ellipsis. Used in hints
/// so long queries don't push the surrounding hint text off-canvas.
fn truncate_for_hint(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_string()
    } else {
        let head: String = s.chars().take(max.saturating_sub(1)).collect();
        format!("{head}…")
    }
}

fn displayed_progress(s: &AppState) -> u64 {
    let Some(pb) = &s.playback else { return 0 };
    let base = pb.progress_ms.unwrap_or(0);
    if !pb.is_playing {
        return base;
    }
    // Under an automated harness, don't fold in wall-clock elapsed time — that
    // keeps the progress bar and play glyph byte-stable across screenshots.
    if crate::testmode::under_test() {
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn forbidden_classifier_matches_403_shapes() {
        // The real 403 body Spotify sent us when /playlists/{id}/tracks
        // got locked down.
        assert!(is_browse_forbidden(
            "GET https://api.spotify.com/v1/playlists/xyz/tracks: 403 Forbidden: {\"error\": {\"status\": 403, \"message\": \"Forbidden\"}}"
        ));
        assert!(is_browse_forbidden("\"status\" : 403"));
        assert!(is_browse_forbidden("Insufficient client scope"));
        assert!(!is_browse_forbidden("rate limited; retry after 30s"));
        assert!(!is_browse_forbidden("connection refused"));
    }

    #[test]
    fn progress_bar_fill_is_uniform_across_the_midpoint() {
        use crate::api::{Album, Playback, Track};
        use ratatui::backend::TestBackend;
        use ratatui::layout::Rect;
        use ratatui::Terminal;

        // ratio = 180/300 = 0.6, i.e. just past the midpoint — the region where
        // the old Gauge widget drew a "hump" by special-casing its center cell.
        let track = Track {
            id: Some("x".into()),
            uri: None,
            name: "Song".into(),
            duration_ms: 300_000,
            artists: vec![],
            album: Album::default(),
        };
        let state = AppState {
            // Paused so displayed_progress returns the stored value verbatim,
            // keeping the rendered ratio deterministic (no elapsed-time drift).
            playback: Some(Playback {
                is_playing: false,
                progress_ms: Some(180_000),
                item: Some(track),
                context: None,
                timestamp: None,
            }),
            last_poll: None,
            ..Default::default()
        };

        let width: u16 = 30;
        let mut terminal = Terminal::new(TestBackend::new(width, 2)).unwrap();
        terminal
            .draw(|f| {
                let label_area = Rect::new(0, 0, width, 1);
                let bar_area = Rect::new(0, 1, width, 1);
                render_progress(f, label_area, bar_area, &state);
            })
            .unwrap();

        let buf = terminal.backend().buffer();
        // 0.6 * 30 = 18.0 exactly, so there's no partial leading cell here.
        let filled = (0.6 * f64::from(width)).floor() as u16; // 18

        // Every filled cell is identical — a space on the solid green bg. In
        // particular the center column matches its neighbors, which is exactly
        // what the Gauge widget broke (it painted the center cell differently,
        // producing the visible hump once the fill crossed it).
        for x in 0..filled {
            let cell = &buf[(x, 1)];
            assert_eq!(cell.symbol(), " ", "filled cell {x} symbol");
            assert_eq!(cell.bg, rgb(BAR_FILLED), "filled cell {x} bg");
        }
        // Unfilled cells are the dim track, also uniform.
        for x in filled..width {
            assert_eq!(buf[(x, 1)].bg, rgb(BAR_EMPTY), "unfilled cell {x} bg");
        }
    }

    #[test]
    fn progress_bar_leading_cell_fades_by_subcell_progress() {
        use crate::api::{Album, Playback, Track};
        use ratatui::backend::TestBackend;
        use ratatui::layout::Rect;
        use ratatui::Terminal;

        // ratio = 5.5/10 = 0.55 → 5 solid cells, leading cell (index 5) half
        // way through, then 4 empty cells.
        let track = Track {
            id: Some("x".into()),
            uri: None,
            name: "Song".into(),
            duration_ms: 100_000,
            artists: vec![],
            album: Album::default(),
        };
        let state = AppState {
            playback: Some(Playback {
                is_playing: false,
                progress_ms: Some(55_000),
                item: Some(track),
                context: None,
                timestamp: None,
            }),
            last_poll: None,
            ..Default::default()
        };

        let width: u16 = 10;
        let mut terminal = Terminal::new(TestBackend::new(width, 2)).unwrap();
        terminal
            .draw(|f| {
                render_progress(f, Rect::new(0, 0, width, 1), Rect::new(0, 1, width, 1), &state);
            })
            .unwrap();
        let buf = terminal.backend().buffer();

        for x in 0..5 {
            assert_eq!(buf[(x, 1)].bg, rgb(BAR_FILLED), "solid cell {x}");
        }
        // The leading cell is a blend — neither fully green nor fully gray.
        let lead = buf[(5, 1)].bg;
        assert_eq!(lead, lerp_rgb(BAR_EMPTY, BAR_FILLED, 0.5), "leading blend");
        assert_ne!(lead, rgb(BAR_FILLED));
        assert_ne!(lead, rgb(BAR_EMPTY));
        for x in 6..width {
            assert_eq!(buf[(x, 1)].bg, rgb(BAR_EMPTY), "trailing cell {x}");
        }
    }

    #[test]
    fn compute_scroll_keeps_selection_visible() {
        // 100 items, list of 10 rows. Selected near the start -> scroll 0.
        assert_eq!(compute_scroll(0, 100, 10), 0);
        assert_eq!(compute_scroll(4, 100, 10), 0);
        // Centering kicks in once the selection passes the midpoint of the
        // viewport — for list_h=10 the midpoint is 5.
        assert_eq!(compute_scroll(5, 100, 10), 0);
        assert_eq!(compute_scroll(10, 100, 10), 5);
        // Near the end: clamped so we don't scroll past max.
        assert_eq!(compute_scroll(99, 100, 10), 90);
        // List fits entirely: no scroll.
        assert_eq!(compute_scroll(7, 8, 10), 0);
    }

    #[test]
    fn search_results_scroll_keeps_last_selection_visible() {
        use crate::api::{Artist, SearchResults, Track};
        use ratatui::backend::TestBackend;
        use ratatui::layout::Rect;
        use ratatui::Terminal;

        // 12 tracks + a section header = 13 lines, far more than the 6-row area.
        let tracks: Vec<Track> = (0..12)
            .map(|i| Track {
                id: Some(format!("t{i}")),
                uri: None,
                name: format!("Track {i:02}"),
                duration_ms: 1000,
                artists: vec![Artist {
                    uri: None,
                    name: "Artist".into(),
                }],
                album: Default::default(),
            })
            .collect();
        let mut s = SearchState::new(None, vec![], vec![]);
        s.input = "q".into();
        s.results = SearchResults {
            tracks,
            ..Default::default()
        };
        s.selected = 11; // the last row

        let (width, height) = (40u16, 6u16);
        let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
        terminal
            .draw(|f| render_search_results(f, Rect::new(0, 0, width, height), &s))
            .unwrap();
        let buf = terminal.backend().buffer();
        let text: String = (0..height)
            .map(|y| (0..width).map(|x| buf[(x, y)].symbol()).collect::<String>())
            .collect::<Vec<_>>()
            .join("\n");

        // The selected last row scrolled into view; the first scrolled off.
        assert!(text.contains("Track 11"), "last row should be visible:\n{text}");
        assert!(
            !text.contains("Track 00"),
            "first row should have scrolled off:\n{text}"
        );
    }
}
