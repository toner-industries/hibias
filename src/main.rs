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
mod auth;
mod ui;

use api::{Playback, RateLimited, SpotifyClient};

#[derive(Default)]
pub struct AppState {
    pub playback: Option<Playback>,
    pub last_poll: Option<Instant>,
    pub error: Option<String>,
    pub rate_limited_until: Option<Instant>,
}

#[tokio::main]
async fn main() -> Result<()> {
    let auth = auth::Auth::init().await.context("authenticate")?;
    let client = Arc::new(SpotifyClient::new(auth)?);
    let state = Arc::new(Mutex::new(AppState::default()));

    if let Ok(p) = client.get_playback().await {
        let mut s = state.lock().await;
        s.playback = p;
        s.last_poll = Some(Instant::now());
    }

    let mut terminal = setup_terminal()?;
    install_panic_hook();
    let result = run(&mut terminal, client, state).await;
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
) -> Result<()> {
    let poll_state = state.clone();
    let poll_client = client.clone();
    let poll_handle = tokio::spawn(async move {
        let mut interval = tokio::time::interval(Duration::from_secs(5));
        interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        interval.tick().await; // skip immediate first tick — already fetched
        loop {
            interval.tick().await;
            match poll_client.get_playback().await {
                Ok(p) => {
                    let mut s = poll_state.lock().await;
                    s.playback = p;
                    s.last_poll = Some(Instant::now());
                    s.error = None;
                    s.rate_limited_until = None;
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
            let s = state.lock().await;
            terminal.draw(|f| ui::render(f, &s))?;
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
