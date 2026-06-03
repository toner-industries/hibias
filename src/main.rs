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
    apply_playback_force, dispatch_input, like_current_track, mode_name,
    play_browse_collection, play_browse_selection, play_selection, seek_relative, skip_track,
    spawn_post_play_poll, spawn_reconnect, toggle_playback, AppState, KeyAction,
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
    spawn_boot_seed(client.clone(), state.clone());

    let mut terminal = setup_terminal()?;
    install_panic_hook();
    let result = run(&mut terminal, client, state, art_loader).await;
    teardown_terminal(&mut terminal).ok();
    result
}

fn spawn_boot_seed(client: Arc<dyn SpotifyApi>, state: Arc<Mutex<AppState>>) {
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
                    apply_playback_force(&state, Some(synth)).await;
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
                    apply_playback_force(&state, Some(seed)).await;
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
    let poll_handle = spawn_playback_poll(client.clone(), state.clone());

    // The decoded album-art cache lives here in the head, not in AppState —
    // the core only signals what to show via `state.art_request`. Behind its
    // own mutex so the async fetch task and the (sync) render closure can both
    // reach it; lock order is always state-then-art_cache.
    let art_cache = Arc::new(Mutex::new(art::ArtCache::new()));

    // Read terminal events on a dedicated task and forward them over a
    // channel. The render loop selects on the *channel*, not on
    // `EventStream::next()` directly: a `select!` that loses the race cancels
    // (drops) the non-winning future, and crossterm's EventStream drops any
    // partially-read multi-byte escape sequence (arrow keys!) when its future
    // is cancelled. `mpsc::Receiver::recv()` is cancellation-safe and buffers,
    // so no keypress is ever lost to the redraw tick.
    let (event_tx, mut event_rx) = tokio::sync::mpsc::unbounded_channel();
    let event_reader = tokio::spawn(async move {
        let mut events = EventStream::new();
        while let Some(ev) = events.next().await {
            if event_tx.send(ev).is_err() {
                break; // render loop has exited
            }
        }
    });

    let mut redraw = tokio::time::interval(Duration::from_millis(100));

    loop {
        // Fetch art for the current track into the head-owned cache. The
        // `has_or_loading` guard means we spawn at most one fetch per track id
        // regardless of how many redraw ticks pass.
        let art_req = { state.lock().await.art_request.clone() };
        if let Some(req) = art_req {
            let mut cache = art_cache.lock().await;
            if !cache.has_or_loading(&req.track_id) {
                cache.begin_loading(req.track_id.clone());
                drop(cache);
                let loader = art_loader.clone();
                let cache = art_cache.clone();
                tokio::spawn(async move {
                    if let Ok(proto) = loader.load(&req.url).await {
                        cache.lock().await.store(req.track_id, proto);
                    }
                });
            }
        }

        // Mirror the client's rate-limit gate into UI state. The client is
        // the source of truth (`send_logged` writes it on any 429); we just
        // surface it here for the status line.
        let client_rl = client.rate_limited_until();
        {
            let mut s = state.lock().await;
            s.rate_limited_until = client_rl;
            let mut art = art_cache.lock().await;
            terminal.draw(|f| ui::render(f, &mut s, &mut art))?;
        }

        tokio::select! {
            _ = redraw.tick() => {}
            ev = event_rx.recv() => match ev {
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
                        KeyAction::LikeCurrent => like_current_track(&client, &state).await,
                        KeyAction::OpenLibrary => app::enter_library(&client, &state).await,
                        KeyAction::PlayLibrarySelection => {
                            app::play_library_selection(&client, &state).await
                        }
                        KeyAction::OpenDevices => app::open_devices(&client, &state).await,
                        KeyAction::TransferToDevice(id) => {
                            app::transfer_to_device(&client, &state, id).await
                        }
                        KeyAction::EnterSearch => app::enter_search(&client, &state).await,
                        KeyAction::OpenBrowse(coll) => app::enter_browse(&client, &state, coll).await,
                        KeyAction::PlayBrowseSelection => {
                            play_browse_selection(&client, &state).await;
                        }
                        KeyAction::PlayBrowseCollection => {
                            play_browse_collection(&client, &state).await;
                        }
                        KeyAction::SearchInputChanged => app::kick_search(&client, &state).await,
                        KeyAction::PlaySelection => {
                            play_selection(&client, &state).await;
                            // After a play, poll /me/player briefly so we
                            // can pick up Spotify's view *if* it actually
                            // updates (it may not — librespot frequently
                            // fails to report state). Stale polls are
                            // silently dropped by apply_playback's
                            // should_accept check.
                            spawn_post_play_poll(client.clone(), state.clone());
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
    event_reader.abort();
    Ok(())
}

/// Cadence for the /me/player poll. 10s while a track is playing — track
/// changes are picked up quickly enough for a TUI; ~5s median latency on
/// transitions is below most users' noticing threshold. 30s while paused
/// or idle — playback state is server-side static while we hold the
/// device, so frequent polling there is wasted traffic that contributes
/// to the sustained-load 429 pattern.
const PLAYBACK_POLL_PLAYING: Duration = Duration::from_secs(10);
const PLAYBACK_POLL_PAUSED: Duration = Duration::from_secs(30);

/// Poll /me/player on a play/pause-aware cadence and apply the result.
fn spawn_playback_poll(
    client: Arc<dyn SpotifyApi>,
    state: Arc<Mutex<AppState>>,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        // Track soft-cap engagement so we log only on edge transitions
        // (engaged → released, released → engaged) instead of every tick.
        let mut throttle_engaged = false;
        loop {
            // Sleep first so we don't fire immediately on startup —
            // `spawn_boot_seed` handles the first paint.
            let (delay, gated) = {
                let s = state.lock().await;
                let is_playing = s.playback.as_ref().map(|p| p.is_playing).unwrap_or(false);
                let delay = if is_playing {
                    PLAYBACK_POLL_PLAYING
                } else {
                    PLAYBACK_POLL_PAUSED
                };
                let gated = s
                    .rate_limited_until
                    .map(|t| t > Instant::now())
                    .unwrap_or(false);
                (delay, gated)
            };
            tokio::time::sleep(delay).await;
            if gated {
                continue;
            }
            if client.background_throttled() {
                if !throttle_engaged {
                    log::note(
                        "playback poll throttled",
                        Some("self-imposed soft cap reached"),
                    );
                    throttle_engaged = true;
                }
                continue;
            }
            if throttle_engaged {
                log::note("playback poll throttle released", None);
                throttle_engaged = false;
            }
            match client.get_playback().await {
                Ok(pb) => {
                    app::apply_playback(&state, pb).await;
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
