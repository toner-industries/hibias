use anyhow::{Context, Result};
use crossterm::{
    event::{Event, EventStream, KeyCode, KeyEventKind, KeyModifiers},
    execute,
    terminal::{disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen},
};
use futures_util::StreamExt;
use parking_lot::Mutex as PMutex;
use ratatui::{backend::CrosstermBackend, Terminal};
use ratatui_image::protocol::StatefulProtocol;
use std::{
    io::{self, Stdout},
    sync::Arc,
    time::{Duration, Instant},
};
use tokio::sync::Mutex;

mod api;
mod art;
mod auth;
mod streaming;
mod ui;

use api::{Playback, RateLimited, SpotifyClient};
use streaming::VisBands;

#[derive(Default)]
pub struct AppState {
    pub playback: Option<Playback>,
    pub last_poll: Option<Instant>,
    pub error: Option<String>,
    pub rate_limited_until: Option<Instant>,
    pub bands: Option<Arc<PMutex<VisBands>>>,
    pub art: Option<StatefulProtocol>,
    pub current_track_id: Option<String>,
    pub device_name: Option<String>,
}

#[tokio::main]
async fn main() -> Result<()> {
    eprintln!("Authenticating...");
    let auth = auth::Auth::init().await.context("authenticate")?;
    let client = Arc::new(SpotifyClient::new(auth)?);

    eprintln!("Probing terminal for image support...");
    let art_loader = Arc::new(art::ArtLoader::new(reqwest::Client::new()));
    if !art_loader.enabled() {
        eprintln!("(no image protocol detected — album art will be skipped)");
    }

    eprintln!("Starting Connect device 'hifi'...");
    let (bands, device_name) = match streaming::start("hifi").await {
        Ok(s) => (Some(s.bands), Some(s.device_name)),
        Err(e) => {
            eprintln!("warning: streaming disabled: {e:#}");
            (None, None)
        }
    };

    let state = Arc::new(Mutex::new(AppState {
        bands,
        device_name,
        ..Default::default()
    }));

    // Initial fetch — also sets current_track_id and kicks off art load
    if let Ok(pb) = client.get_playback().await {
        apply_playback(&state, &art_loader, pb).await;
    }

    let mut terminal = setup_terminal()?;
    install_panic_hook();
    let result = run(&mut terminal, client, state, art_loader).await;
    teardown_terminal(&mut terminal).ok();
    result
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

async fn run(
    terminal: &mut Terminal<CrosstermBackend<Stdout>>,
    client: Arc<SpotifyClient>,
    state: Arc<Mutex<AppState>>,
    art_loader: Arc<art::ArtLoader>,
) -> Result<()> {
    let poll_state = state.clone();
    let poll_client = client.clone();
    let poll_loader = art_loader.clone();
    let poll_handle = tokio::spawn(async move {
        let mut interval = tokio::time::interval(Duration::from_secs(5));
        interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        interval.tick().await;
        loop {
            interval.tick().await;
            match poll_client.get_playback().await {
                Ok(pb) => {
                    apply_playback(&poll_state, &poll_loader, pb).await;
                }
                Err(e) => {
                    let retry = e.downcast_ref::<RateLimited>().map(|r| r.0);
                    {
                        let mut s = poll_state.lock().await;
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
    });

    let mut events = EventStream::new();
    let mut redraw = tokio::time::interval(Duration::from_millis(100));

    loop {
        {
            let mut s = state.lock().await;
            terminal.draw(|f| ui::render(f, &mut s))?;
        }

        tokio::select! {
            _ = redraw.tick() => {}
            ev = events.next() => match ev {
                Some(Ok(Event::Key(k))) if k.kind == KeyEventKind::Press => match k.code {
                    KeyCode::Char('q') | KeyCode::Esc => break,
                    KeyCode::Char('c') if k.modifiers.contains(KeyModifiers::CONTROL) => break,
                    KeyCode::Char(' ') => toggle_playback(&client, &state).await,
                    _ => {}
                },
                Some(Err(_)) | None => break,
                _ => {}
            }
        }
    }

    poll_handle.abort();
    Ok(())
}

async fn apply_playback(
    state: &Arc<Mutex<AppState>>,
    art_loader: &Arc<art::ArtLoader>,
    pb: Option<Playback>,
) {
    let new_track_id = pb
        .as_ref()
        .and_then(|p| p.item.as_ref())
        .and_then(|t| t.id.clone());
    let cover_url = pb
        .as_ref()
        .and_then(|p| p.item.as_ref())
        .and_then(|t| t.album.cover_url())
        .map(|s| s.to_string());

    let needs_art_fetch = {
        let mut s = state.lock().await;
        let prev = s.current_track_id.clone();
        s.playback = pb;
        s.last_poll = Some(Instant::now());
        s.error = None;
        s.rate_limited_until = None;
        s.current_track_id = new_track_id.clone();
        new_track_id.is_some() && prev != new_track_id
    };

    if needs_art_fetch {
        if let Some(url) = cover_url {
            let s = state.clone();
            let loader = art_loader.clone();
            let id_at_fetch = new_track_id.clone();
            tokio::spawn(async move {
                match loader.load(&url).await {
                    Ok(proto) => {
                        let mut g = s.lock().await;
                        // Drop the result if user moved on while we were downloading.
                        if g.current_track_id == id_at_fetch {
                            g.art = Some(proto);
                        }
                    }
                    Err(_) => {
                        // Soft fail — keep prior art (or nothing).
                    }
                }
            });
        }
    }
}

async fn toggle_playback(client: &SpotifyClient, state: &Mutex<AppState>) {
    let was_playing = {
        let s = state.lock().await;
        if s.rate_limited_until
            .map(|t| t > Instant::now())
            .unwrap_or(false)
        {
            return;
        }
        s.playback.as_ref().map(|p| p.is_playing).unwrap_or(false)
    };
    let result = if was_playing {
        client.pause().await
    } else {
        client.play().await
    };
    let mut s = state.lock().await;
    match result {
        Ok(()) => {
            if let Some(p) = s.playback.as_mut() {
                p.is_playing = !was_playing;
            }
            s.error = None;
        }
        Err(e) => s.error = Some(format!("{e:#}")),
    }
}
