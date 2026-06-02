use anyhow::{Context, Result};
use crossterm::{
    event::{Event, EventStream, KeyCode, KeyEventKind, KeyModifiers},
    execute,
    terminal::{disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen},
};
use futures_util::StreamExt;
use ratatui::{backend::CrosstermBackend, Terminal};
use std::{
    io::{self, Stdout},
    sync::Arc,
    time::{Duration, Instant},
};
use tokio::sync::Mutex;

mod api;
mod app;
mod art;
mod auth;
mod input;
mod keys;
mod log;
mod recent;
mod streaming;
#[cfg(test)]
mod test_support;
mod ui;

use api::{Playback, RateLimited, SpotifyApi, SpotifyClient};
use app::{
    apply_playback_force, dispatch_input, mode_name, play_browse_collection, play_browse_selection,
    play_selection, seek_relative, skip_track, spawn_post_play_poll, spawn_reconnect,
    toggle_playback, AppState, KeyAction,
};
use input::{Input, Key, Mods};

#[tokio::main]
async fn main() -> Result<()> {
    let log_path = std::env::current_dir()
        .unwrap_or_else(|_| std::path::PathBuf::from("."))
        .join("hifi.log.sqlite");
    if let Err(e) = log::init(&log_path) {
        eprintln!("warning: log init failed: {e:#}");
    } else {
        eprintln!("Logging to {}", log_path.display());
    }
    log::note("app start", None);

    eprintln!("Authenticating...");
    let auth = match auth::Auth::init().await.context("authenticate") {
        Ok(a) => a,
        Err(e) => {
            log::error("auth::init", &format!("{e:#}"));
            return Err(e);
        }
    };
    let client: Arc<dyn SpotifyApi> = Arc::new(SpotifyClient::new(auth)?);

    eprintln!("Probing terminal for image support...");
    let art_loader = Arc::new(art::ArtLoader::new(reqwest::Client::new()));
    if !art_loader.enabled() {
        eprintln!("(no image protocol detected — album art will be skipped)");
    }

    let recent_queries = recent::load_queries();
    let state = Arc::new(Mutex::new(AppState {
        recent_queries,
        ..Default::default()
    }));

    // Start the Connect device in the background so we can render the TUI
    // immediately — librespot's Spirc handshake usually takes a couple of
    // seconds and we don't want to block the screen on it.
    spawn_reconnect(&client, &state, "boot");

    // Kick off a recently-played fetch in the background. Doubles as:
    //   (a) the "Recently played" section in the search overlay, and
    //   (b) the initial now-playing display, so users don't see a stale
    //       track from whatever device /me/player happened to return at boot.
    spawn_boot_seed(client.clone(), state.clone(), art_loader.clone());

    let mut terminal = setup_terminal()?;
    install_panic_hook();
    let result = run(&mut terminal, client, state, art_loader).await;
    teardown_terminal(&mut terminal).ok();
    result
}

fn spawn_boot_seed(
    client: Arc<dyn SpotifyApi>,
    state: Arc<Mutex<AppState>>,
    art_loader: Arc<art::ArtLoader>,
) {
    tokio::spawn(async move {
        match client.get_recently_played(20).await {
            Ok(tracks) => {
                log::note(
                    "recently_played loaded",
                    Some(&format!("count={}", tracks.len())),
                );
                let first = tracks.first().cloned();
                {
                    let mut g = state.lock().await;
                    g.recent_tracks = tracks;
                }
                if let Some(t) = first {
                    let synth = Playback {
                        is_playing: false,
                        progress_ms: Some(0),
                        item: Some(t),
                        context: None,
                        timestamp: None,
                    };
                    apply_playback_force(&state, &art_loader, Some(synth)).await;
                }
            }
            Err(e) => {
                log::note(
                    "recently_played unavailable",
                    Some(&format!("{e:#} (likely missing scope — run `just reauth`)")),
                );
                if let Ok(Some(pb)) = client.get_playback().await {
                    log::note(
                        "boot seed via /me/player fallback",
                        pb.item.as_ref().map(|t| t.name.as_str()),
                    );
                    let mut seed = pb;
                    seed.is_playing = false;
                    seed.timestamp = None;
                    apply_playback_force(&state, &art_loader, Some(seed)).await;
                }
            }
        }
    });
}

fn setup_terminal() -> Result<Terminal<CrosstermBackend<Stdout>>> {
    enable_raw_mode()?;
    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen)?;
    Ok(Terminal::new(CrosstermBackend::new(stdout))?)
}

fn install_panic_hook() {
    let original = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let _ = disable_raw_mode();
        let _ = execute!(io::stdout(), LeaveAlternateScreen);
        original(info);
    }));
}

fn teardown_terminal(t: &mut Terminal<CrosstermBackend<Stdout>>) -> Result<()> {
    disable_raw_mode()?;
    execute!(t.backend_mut(), LeaveAlternateScreen)?;
    t.show_cursor()?;
    Ok(())
}

/// Translate a crossterm key event into our frontend-neutral `Input`. The
/// only place crossterm types touch the app layer.
fn input_from_crossterm(code: KeyCode, mods: KeyModifiers) -> Input {
    let key = match code {
        KeyCode::Char(c) => Key::Char(c),
        KeyCode::Esc => Key::Esc,
        KeyCode::Enter => Key::Enter,
        KeyCode::Backspace => Key::Backspace,
        KeyCode::Left => Key::Left,
        KeyCode::Right => Key::Right,
        KeyCode::Up => Key::Up,
        KeyCode::Down => Key::Down,
        KeyCode::Tab => Key::Tab,
        _ => Key::Other,
    };
    let mods = Mods {
        shift: mods.contains(KeyModifiers::SHIFT),
        ctrl: mods.contains(KeyModifiers::CONTROL),
        alt: mods.contains(KeyModifiers::ALT),
    };
    Input { key, mods }
}

async fn run(
    terminal: &mut Terminal<CrosstermBackend<Stdout>>,
    client: Arc<dyn SpotifyApi>,
    state: Arc<Mutex<AppState>>,
    art_loader: Arc<art::ArtLoader>,
) -> Result<()> {
    let dev_handle = spawn_devices_probe(client.clone(), state.clone());
    let poll_handle = spawn_playback_poll(client.clone(), state.clone(), art_loader.clone());

    let mut events = EventStream::new();
    let mut redraw = tokio::time::interval(Duration::from_millis(100));

    loop {
        // Mirror the client's rate-limit gate into UI state. The client is
        // the source of truth (`send_logged` writes it on any 429); we just
        // surface it here for the status line.
        let client_rl = client.rate_limited_until();
        {
            let mut s = state.lock().await;
            s.rate_limited_until = client_rl;
            terminal.draw(|f| ui::render(f, &mut s))?;
        }

        tokio::select! {
            _ = redraw.tick() => {}
            ev = events.next() => match ev {
                Some(Ok(Event::Key(k))) if k.kind == KeyEventKind::Press => {
                    let inp = input_from_crossterm(k.code, k.modifiers);
                    if inp.is_ctrl_c() {
                        log::key("ctrl-c", mode_name(&*state.lock().await));
                        log::note("quit", Some("ctrl-c"));
                        break;
                    }
                    let mode_before = mode_name(&*state.lock().await).to_string();
                    log::key(&input::label(inp), &mode_before);
                    let action = dispatch_input(inp, &state).await;
                    let mode_after = mode_name(&*state.lock().await).to_string();
                    if mode_before != mode_after {
                        log::mode_change(&mode_before, &mode_after);
                    }
                    match action {
                        KeyAction::Quit => {
                            log::note("quit", Some("user"));
                            break;
                        }
                        KeyAction::Stay => {}
                        KeyAction::TogglePlayback => toggle_playback(&client, &state).await,
                        KeyAction::Seek(delta_ms) => seek_relative(&client, &state, delta_ms).await,
                        KeyAction::NextTrack => skip_track(&client, &state, true).await,
                        KeyAction::PrevTrack => skip_track(&client, &state, false).await,
                        KeyAction::Reconnect => {
                            spawn_reconnect(&client, &state, "user: :reconnect");
                        }
                        KeyAction::EnterSearch => app::enter_search(&client, &state).await,
                        KeyAction::OpenBrowse(coll) => app::enter_browse(&client, &state, coll).await,
                        KeyAction::PlayBrowseSelection => {
                            play_browse_selection(&client, &state, &art_loader).await;
                        }
                        KeyAction::PlayBrowseCollection => {
                            play_browse_collection(&client, &state, &art_loader).await;
                        }
                        KeyAction::SearchInputChanged => app::kick_search(&client, &state).await,
                        KeyAction::PlaySelection => {
                            play_selection(&client, &state, &art_loader).await;
                            // After a play, poll /me/player briefly so we
                            // can pick up Spotify's view *if* it actually
                            // updates (it may not — librespot frequently
                            // fails to report state). Stale polls are
                            // silently dropped by apply_playback's
                            // should_accept check.
                            spawn_post_play_poll(client.clone(), state.clone(), art_loader.clone());
                        }
                    }
                }
                Some(Err(e)) => {
                    log::error("event stream", &format!("{e:#}"));
                    break;
                }
                None => break,
                _ => {}
            }
        }
    }

    poll_handle.abort();
    dev_handle.abort();
    Ok(())
}

/// Periodic device probe — logs Spotify's view of our device every 5s.
/// Surfaces "device drops out of active state" issues.
fn spawn_devices_probe(
    client: Arc<dyn SpotifyApi>,
    state: Arc<Mutex<AppState>>,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let mut interval = tokio::time::interval(Duration::from_secs(5));
        interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        interval.tick().await;
        loop {
            interval.tick().await;
            {
                let s = state.lock().await;
                if s.rate_limited_until
                    .map(|t| t > Instant::now())
                    .unwrap_or(false)
                {
                    continue;
                }
            }
            match client.get_devices().await {
                Ok(devs) => {
                    let our_id = client.device_id_for_log();
                    let summary = devs
                        .iter()
                        .map(|d| {
                            let mark = match (&our_id, d.id.as_deref()) {
                                (Some(o), Some(i)) if o == i => "*",
                                _ => " ",
                            };
                            format!(
                                "{mark}{}(active={},id={})",
                                d.name,
                                d.is_active,
                                d.id.as_deref().unwrap_or("?")
                            )
                        })
                        .collect::<Vec<_>>()
                        .join(" | ");
                    log::note("devices probe", Some(&summary));
                    // Update our "is the librespot device still registered?"
                    // flag. Only meaningful once we've actually started a
                    // device (our_id is set). We do NOT auto-reconnect here
                    // — the probe runs every 5s and repeated reconnect
                    // attempts hammer Spotify. Instead, the next user
                    // play/pause/seek/skip will trigger the reconnect, or
                    // they can run `:reconnect` manually.
                    if let Some(id) = our_id {
                        let now_present = devs.iter().any(|d| d.id.as_deref() == Some(id.as_str()));
                        let mut s = state.lock().await;
                        let was_present = s.device_present;
                        s.device_present = Some(now_present);
                        if was_present == Some(true) && !now_present {
                            log::error(
                                "device dropped",
                                "librespot Spirc lost its Connect cloud registration — next user action will reconnect",
                            );
                        }
                    }
                }
                Err(e) => {
                    if let Some(secs) = e.downcast_ref::<RateLimited>().map(|r| r.0) {
                        let mut s = state.lock().await;
                        s.rate_limited_until =
                            Some(Instant::now() + Duration::from_secs(secs));
                    }
                    log::note("devices probe failed", Some(&format!("{e:#}")));
                }
            }
        }
    })
}

/// Poll /me/player every 5s and apply the result.
fn spawn_playback_poll(
    client: Arc<dyn SpotifyApi>,
    state: Arc<Mutex<AppState>>,
    art_loader: Arc<art::ArtLoader>,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let mut interval = tokio::time::interval(Duration::from_secs(5));
        interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        interval.tick().await;
        loop {
            interval.tick().await;
            {
                let s = state.lock().await;
                if s.rate_limited_until
                    .map(|t| t > Instant::now())
                    .unwrap_or(false)
                {
                    continue;
                }
            }
            match client.get_playback().await {
                Ok(pb) => {
                    app::apply_playback(&state, &art_loader, pb).await;
                }
                Err(e) => {
                    let retry = e.downcast_ref::<RateLimited>().map(|r| r.0);
                    {
                        let mut s = state.lock().await;
                        s.error = Some(format!("{e:#}"));
                        s.rate_limited_until =
                            retry.map(|secs| Instant::now() + Duration::from_secs(secs));
                    }
                    if let Some(secs) = retry {
                        tokio::time::sleep(Duration::from_secs(secs.max(5))).await;
                    }
                }
            }
        }
    })
}
