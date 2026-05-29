use anyhow::{Context, Result};
use crossterm::{
    event::{Event, EventStream, KeyCode, KeyEventKind, KeyModifiers},
    execute,
    terminal::{disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen},
};
use futures_util::StreamExt;
use ratatui::{backend::CrosstermBackend, Terminal};
use ratatui_image::protocol::StatefulProtocol;
use std::{
    io::{self, Stdout},
    sync::Arc,
    time::{Duration, Instant, SystemTime},
};
use tokio::sync::Mutex;
use tokio::task::AbortHandle;

mod api;
mod art;
mod auth;
mod keys;
mod log;
mod streaming;
mod ui;

use api::{Context as PlaybackContext, Playback, RateLimited, SearchResults, SpotifyClient, Track};
use keys::ModeMask;

#[derive(Default)]
pub struct AppState {
    pub playback: Option<Playback>,
    pub last_poll: Option<Instant>,
    pub error: Option<String>,
    pub rate_limited_until: Option<Instant>,
    pub art: Option<StatefulProtocol>,
    pub current_track_id: Option<String>,
    pub device_name: Option<String>,
    pub mode: Mode,
    /// Unix ms of the last local action (play/pause). When `/me/player`
    /// returns data with an older timestamp than this, we treat it as stale
    /// — librespot frequently fails to report state to Spotify Connect, so
    /// `/me/player` keeps serving whichever device reported last.
    pub last_local_action_ms: u64,
}

#[derive(Default)]
pub enum Mode {
    #[default]
    NowPlaying,
    Search(SearchState),
    Help,
}

impl Mode {
    pub fn mask(&self) -> ModeMask {
        match self {
            Mode::NowPlaying => ModeMask::NOW_PLAYING,
            Mode::Search(_) => ModeMask::SEARCH,
            Mode::Help => ModeMask::HELP,
        }
    }
}

pub struct SearchState {
    pub input: String,
    pub cursor: usize, // char index
    pub last_query: String,
    pub request_id: u64,
    pub applied_id: u64,
    pub debounce: Option<AbortHandle>,
    pub results: SearchResults,
    pub selected: usize,
    pub in_context: Option<InContext>,
}

impl SearchState {
    fn new(in_context: Option<InContext>) -> Self {
        Self {
            input: String::new(),
            cursor: 0,
            last_query: String::new(),
            request_id: 0,
            applied_id: 0,
            debounce: None,
            results: SearchResults::default(),
            selected: 0,
            in_context,
        }
    }
}

pub struct InContext {
    pub playlist_uri: String,
    pub tracks: Vec<Track>,
    pub filtered: Vec<usize>,
}

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
    let client = Arc::new(SpotifyClient::new(auth)?);

    eprintln!("Probing terminal for image support...");
    let art_loader = Arc::new(art::ArtLoader::new(reqwest::Client::new()));
    if !art_loader.enabled() {
        eprintln!("(no image protocol detected — album art will be skipped)");
    }

    eprintln!("Starting Connect device 'hifi'...");
    let device_name = match streaming::start("hifi").await {
        Ok(s) => {
            log::note(
                "connect device started",
                Some(&format!("name={} id={}", s.device_name, s.device_id)),
            );
            client.set_device_id(s.device_id.clone());
            // Spirc::new returns before Spotify's Connect cloud has registered us.
            // Wait until our device appears in /me/player/devices before
            // transferring; otherwise transfer_playback 404s "Device not found".
            let client_bg = client.clone();
            let device_id_bg = s.device_id.clone();
            tokio::spawn(async move {
                wait_then_transfer(&client_bg, &device_id_bg).await;
            });
            Some(s.device_name)
        }
        Err(e) => {
            eprintln!("warning: streaming disabled: {e:#}");
            log::error("streaming::start", &format!("{e:#}"));
            None
        }
    };

    let state = Arc::new(Mutex::new(AppState {
        device_name,
        ..Default::default()
    }));

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
    // Periodic device probe — logs Spotify's view of our device every 5s.
    // Surfaces "device drops out of active state" issues that some other
    // Spotify TUIs miss.
    let dev_client = client.clone();
    let dev_handle = tokio::spawn(async move {
        let mut interval = tokio::time::interval(Duration::from_secs(5));
        interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        interval.tick().await;
        loop {
            interval.tick().await;
            match dev_client.get_devices().await {
                Ok(devs) => {
                    let our_id = dev_client.device_id_for_log();
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
                }
                Err(e) => log::note("devices probe failed", Some(&format!("{e:#}"))),
            }
        }
    });

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
                Some(Ok(Event::Key(k))) if k.kind == KeyEventKind::Press => {
                    if k.modifiers.contains(KeyModifiers::CONTROL) && k.code == KeyCode::Char('c') {
                        log::key("ctrl-c", mode_name(&*state.lock().await));
                        log::note("quit", Some("ctrl-c"));
                        break;
                    }
                    let mode_before = mode_name(&*state.lock().await).to_string();
                    log::key(&key_label(k.code, k.modifiers), &mode_before);
                    let action = dispatch_key(k.code, k.modifiers, &state).await;
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
                        KeyAction::EnterSearch => enter_search(&client, &state).await,
                        KeyAction::SearchInputChanged => kick_search(&client, &state).await,
                        KeyAction::PlaySelection => {
                            play_selection(&client, &state, &art_loader).await;
                            // After a play, also poll /me/player briefly so we
                            // can pick up Spotify's view *if* it actually
                            // updates (it may not — librespot frequently fails
                            // to report state). Stale polls are silently
                            // dropped by apply_playback's should_accept check.
                            let c = client.clone();
                            let s = state.clone();
                            let a = art_loader.clone();
                            tokio::spawn(async move {
                                for _ in 0..6 {
                                    tokio::time::sleep(Duration::from_millis(250)).await;
                                    if let Ok(pb) = c.get_playback().await {
                                        apply_playback(&s, &a, pb).await;
                                    }
                                }
                            });
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

async fn wait_then_transfer(client: &SpotifyClient, device_id: &str) {
    for attempt in 0..24 {
        tokio::time::sleep(Duration::from_millis(500)).await;
        match client.get_devices().await {
            Ok(devices) => {
                let found = devices.iter().find(|d| d.id.as_deref() == Some(device_id));
                if let Some(d) = found {
                    log::note(
                        "device visible to Spotify",
                        Some(&format!(
                            "attempt={attempt} name={:?} is_active={}",
                            d.name, d.is_active
                        )),
                    );
                    match client.transfer_playback(device_id, false).await {
                        Ok(_) => log::note("transfer_playback ok", None),
                        Err(e) => log::error("transfer_playback", &format!("{e:#}")),
                    }
                    return;
                }
            }
            Err(e) => log::note("get_devices failed", Some(&format!("{e:#}"))),
        }
    }
    log::error(
        "wait_then_transfer",
        "device never appeared in /me/player/devices after 12s",
    );
}

fn mode_name(s: &AppState) -> &'static str {
    match s.mode {
        Mode::NowPlaying => "now_playing",
        Mode::Search(_) => "search",
        Mode::Help => "help",
    }
}

fn key_label(code: KeyCode, mods: KeyModifiers) -> String {
    let base = match code {
        KeyCode::Char(c) => format!("'{c}'"),
        KeyCode::Esc => "esc".into(),
        KeyCode::Enter => "enter".into(),
        KeyCode::Backspace => "backspace".into(),
        KeyCode::Left => "left".into(),
        KeyCode::Right => "right".into(),
        KeyCode::Up => "up".into(),
        KeyCode::Down => "down".into(),
        KeyCode::Tab => "tab".into(),
        other => format!("{other:?}"),
    };
    if mods.is_empty() {
        base
    } else {
        format!("{:?}+{}", mods, base)
    }
}

enum KeyAction {
    Stay,
    Quit,
    TogglePlayback,
    EnterSearch,
    SearchInputChanged,
    PlaySelection,
}

async fn dispatch_key(
    code: KeyCode,
    _mods: KeyModifiers,
    state: &Mutex<AppState>,
) -> KeyAction {
    let mut s = state.lock().await;
    match &mut s.mode {
        Mode::NowPlaying => match code {
            KeyCode::Char('q') | KeyCode::Esc => KeyAction::Quit,
            KeyCode::Char(' ') => KeyAction::TogglePlayback,
            KeyCode::Char('/') => KeyAction::EnterSearch,
            KeyCode::Char('?') => {
                s.mode = Mode::Help;
                KeyAction::Stay
            }
            _ => KeyAction::Stay,
        },
        Mode::Search(search) => match code {
            KeyCode::Esc => {
                if let Some(h) = search.debounce.take() {
                    h.abort();
                }
                s.mode = Mode::NowPlaying;
                KeyAction::Stay
            }
            KeyCode::Up => {
                if search.selected > 0 {
                    search.selected -= 1;
                }
                KeyAction::Stay
            }
            KeyCode::Down => {
                let max = visible_row_count(search).saturating_sub(1);
                if search.selected < max {
                    search.selected += 1;
                }
                KeyAction::Stay
            }
            KeyCode::Enter => KeyAction::PlaySelection,
            KeyCode::Backspace => {
                if search.cursor > 0 {
                    let byte = char_idx_to_byte(&search.input, search.cursor - 1);
                    search.input.remove(byte);
                    search.cursor -= 1;
                    refilter_in_context(search);
                    KeyAction::SearchInputChanged
                } else {
                    KeyAction::Stay
                }
            }
            KeyCode::Left => {
                if search.cursor > 0 {
                    search.cursor -= 1;
                }
                KeyAction::Stay
            }
            KeyCode::Right => {
                let max = search.input.chars().count();
                if search.cursor < max {
                    search.cursor += 1;
                }
                KeyAction::Stay
            }
            KeyCode::Char(c) => {
                let byte = char_idx_to_byte(&search.input, search.cursor);
                search.input.insert(byte, c);
                search.cursor += 1;
                refilter_in_context(search);
                KeyAction::SearchInputChanged
            }
            _ => KeyAction::Stay,
        },
        Mode::Help => match code {
            KeyCode::Esc | KeyCode::Char('?') | KeyCode::Char('q') => {
                s.mode = Mode::NowPlaying;
                KeyAction::Stay
            }
            _ => KeyAction::Stay,
        },
    }
}

fn visible_row_count(s: &SearchState) -> usize {
    let mut n = 0;
    if let Some(c) = &s.in_context {
        n += c.filtered.len();
    }
    n += s.results.tracks.len();
    n += s.results.albums.len();
    n += s.results.artists.len();
    n += s.results.playlists.len();
    n
}

fn refilter_in_context(s: &mut SearchState) {
    let Some(ctx) = &mut s.in_context else {
        return;
    };
    let q = s.input.to_lowercase();
    ctx.filtered = if q.is_empty() {
        Vec::new()
    } else {
        ctx.tracks
            .iter()
            .enumerate()
            .filter(|(_, t)| {
                t.name.to_lowercase().contains(&q)
                    || t.artists.iter().any(|a| a.name.to_lowercase().contains(&q))
            })
            .map(|(i, _)| i)
            .collect()
    };
}

fn char_idx_to_byte(s: &str, char_idx: usize) -> usize {
    s.char_indices()
        .nth(char_idx)
        .map(|(i, _)| i)
        .unwrap_or(s.len())
}

async fn enter_search(client: &Arc<SpotifyClient>, state: &Arc<Mutex<AppState>>) {
    let context_uri = {
        let s = state.lock().await;
        s.playback
            .as_ref()
            .and_then(|p| p.context.as_ref())
            .filter(|c| c.kind == "playlist")
            .map(|c| c.uri.clone())
    };
    log::note("enter_search", context_uri.as_deref());

    {
        let mut s = state.lock().await;
        let in_context = context_uri.as_ref().map(|uri| InContext {
            playlist_uri: uri.clone(),
            tracks: Vec::new(),
            filtered: Vec::new(),
        });
        s.mode = Mode::Search(SearchState::new(in_context));
    }

    if let Some(uri) = context_uri {
        if let Some(id) = playlist_id_from_uri(&uri) {
            let client = client.clone();
            let state = state.clone();
            tokio::spawn(async move {
                match client.get_playlist_tracks(&id).await {
                    Ok(tracks) => {
                        let mut s = state.lock().await;
                        if let Mode::Search(search) = &mut s.mode {
                            if let Some(ctx) = &mut search.in_context {
                                if ctx.playlist_uri == uri {
                                    ctx.tracks = tracks;
                                    refilter_in_context(search);
                                }
                            }
                        }
                    }
                    Err(e) => {
                        // Soft fail — most often a 403 because Spotify blocks API
                        // access to editorial playlists (URIs starting `37i9dQZ...`).
                        log::note(
                            "in-context tracks unavailable",
                            Some(&format!("uri={uri} err={e:#}")),
                        );
                        let mut s = state.lock().await;
                        if let Mode::Search(search) = &mut s.mode {
                            search.in_context = None;
                        }
                    }
                }
            });
        }
    }
}

fn playlist_id_from_uri(uri: &str) -> Option<String> {
    uri.strip_prefix("spotify:playlist:").map(|s| s.to_string())
}

async fn kick_search(client: &Arc<SpotifyClient>, state: &Arc<Mutex<AppState>>) {
    let (q, my_id) = {
        let mut s = state.lock().await;
        let Mode::Search(search) = &mut s.mode else {
            return;
        };
        if let Some(h) = search.debounce.take() {
            h.abort();
        }
        if search.input.is_empty() {
            search.results = SearchResults::default();
            search.last_query.clear();
            search.applied_id = search.request_id;
            search.selected = 0;
            return;
        }
        search.request_id += 1;
        (search.input.clone(), search.request_id)
    };
    log::note("search debounce scheduled", Some(&format!("id={my_id} q={q:?}")));

    let client_task = client.clone();
    let state_task = state.clone();
    let handle = tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(250)).await;
        match client_task.search(&q).await {
            Ok(results) => {
                let counts = format!(
                    "tracks={} albums={} artists={} playlists={}",
                    results.tracks.len(),
                    results.albums.len(),
                    results.artists.len(),
                    results.playlists.len()
                );
                let mut s = state_task.lock().await;
                if let Mode::Search(search) = &mut s.mode {
                    if my_id >= search.applied_id && my_id == search.request_id {
                        search.results = results;
                        search.last_query = q.clone();
                        search.applied_id = my_id;
                        search.selected = 0;
                        search.debounce = None;
                        log::note(
                            "search results applied",
                            Some(&format!("id={my_id} q={q:?} {counts}")),
                        );
                    } else {
                        log::note(
                            "search results dropped (stale)",
                            Some(&format!(
                                "id={my_id} request_id={} applied_id={}",
                                search.request_id, search.applied_id
                            )),
                        );
                    }
                }
            }
            Err(e) => {
                log::error("search task", &format!("id={my_id} q={q:?} err={e:#}"));
                let mut s = state_task.lock().await;
                let retry = e.downcast_ref::<RateLimited>().map(|r| r.0);
                s.error = Some(format!("{e:#}"));
                s.rate_limited_until = retry.map(|x| Instant::now() + Duration::from_secs(x));
                if let Mode::Search(search) = &mut s.mode {
                    search.debounce = None;
                }
            }
        }
    });

    let mut s = state.lock().await;
    if let Mode::Search(search) = &mut s.mode {
        if search.request_id == my_id {
            search.debounce = Some(handle.abort_handle());
        } else {
            // a newer kick raced ahead; cancel this one
            handle.abort();
        }
    } else {
        handle.abort();
    }
}

async fn play_selection(
    client: &Arc<SpotifyClient>,
    state: &Arc<Mutex<AppState>>,
    art_loader: &Arc<art::ArtLoader>,
) {
    // 1. Resolve action and capture the synth template now, while search
    //    state is still in AppState. We finalize its timestamp later (after
    //    a successful play) so it matches `last_local_action_ms` exactly.
    let (action, synth_template) = {
        let s = state.lock().await;
        let Mode::Search(search) = &s.mode else {
            return;
        };
        let Some(action) = resolve_selection(search) else {
            log::note("play_selection: nothing selected", None);
            return;
        };
        let template = synth_template_for(&action, search);
        (action, template)
    };

    let action_desc = match &action {
        PlayAction::Track(u) => format!("track: {u}"),
        PlayAction::Context { uri, offset } => {
            format!("context: uri={uri} offset={}", offset.as_deref().unwrap_or("-"))
        }
    };
    log::note("play_selection", Some(&action_desc));

    let result = match &action {
        PlayAction::Track(uri) => client.play_uris(&[uri.clone()]).await,
        PlayAction::Context { uri, offset } => {
            client.play_context(uri, offset.as_deref()).await
        }
    };

    // 2. On success: use a SINGLE timestamp for both last_local_action_ms
    //    and synth.timestamp so should_accept doesn't reject our own synth.
    let synth_to_apply = match &result {
        Ok(()) => {
            let ts = now_unix_ms();
            let mut s = state.lock().await;
            s.error = None;
            log::mode_change("search", "now_playing");
            s.mode = Mode::NowPlaying;
            s.last_local_action_ms = ts;
            synth_template.map(|mut pb| {
                pb.timestamp = Some(ts);
                pb
            })
        }
        Err(e) => {
            log::error("play_selection", &format!("{e:#}"));
            let mut s = state.lock().await;
            s.error = Some(format!("{e:#}"));
            None
        }
    };

    if let Some(pb) = synth_to_apply {
        log::note(
            "play_selection: applying synthetic playback",
            pb.item.as_ref().map(|t| t.name.as_str()),
        );
        apply_playback(state, art_loader, Some(pb)).await;
    }
}

/// Build a Playback template for the user's selection. Timestamp is filled
/// in later by `play_selection` (after the play succeeds) so it matches the
/// `last_local_action_ms` value the same call sets.
fn synth_template_for(action: &PlayAction, search: &SearchState) -> Option<Playback> {
    let template = |item: Track, context: Option<PlaybackContext>| Playback {
        is_playing: true,
        progress_ms: Some(0),
        item: Some(item),
        context,
        timestamp: None, // filled in by caller
    };
    match action {
        PlayAction::Track(uri) => Some(template(find_track_by_uri(search, uri)?, None)),
        PlayAction::Context { uri, offset } => {
            // "Skip to track within current playlist": we know which track is
            // starting. Album/playlist/artist plays without an offset don't
            // have specific track info; let polled data handle them.
            let off = offset.as_deref()?;
            let track = find_track_by_uri(search, off)?;
            Some(template(
                track,
                Some(PlaybackContext {
                    uri: uri.clone(),
                    kind: "playlist".into(),
                }),
            ))
        }
    }
}

fn find_track_by_uri(search: &SearchState, uri: &str) -> Option<Track> {
    if let Some(ctx) = &search.in_context {
        if let Some(t) = ctx
            .tracks
            .iter()
            .find(|t| t.uri.as_deref() == Some(uri))
            .cloned()
        {
            return Some(t);
        }
    }
    search
        .results
        .tracks
        .iter()
        .find(|t| t.uri.as_deref() == Some(uri))
        .cloned()
}

#[derive(Debug)]
enum PlayAction {
    Track(String),
    Context { uri: String, offset: Option<String> },
}

fn resolve_selection(s: &SearchState) -> Option<PlayAction> {
    let mut idx = s.selected;

    if let Some(ctx) = &s.in_context {
        if idx < ctx.filtered.len() {
            let track = &ctx.tracks[ctx.filtered[idx]];
            let track_uri = track.uri.clone()?;
            return Some(PlayAction::Context {
                uri: ctx.playlist_uri.clone(),
                offset: Some(track_uri),
            });
        }
        idx -= ctx.filtered.len();
    }

    if idx < s.results.tracks.len() {
        let t = &s.results.tracks[idx];
        let uri = t.uri.clone()?;
        return Some(PlayAction::Track(uri));
    }
    idx -= s.results.tracks.len();

    if idx < s.results.albums.len() {
        let uri = s.results.albums[idx].uri.clone()?;
        return Some(PlayAction::Context { uri, offset: None });
    }
    idx -= s.results.albums.len();

    if idx < s.results.artists.len() {
        let uri = s.results.artists[idx].uri.clone()?;
        return Some(PlayAction::Context { uri, offset: None });
    }
    idx -= s.results.artists.len();

    if idx < s.results.playlists.len() {
        let uri = s.results.playlists[idx].uri.clone();
        return Some(PlayAction::Context { uri, offset: None });
    }

    None
}

pub fn now_unix_ms() -> u64 {
    SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// Decide whether to accept incoming playback data over our current state.
/// Rejects polled data older than our last local action — librespot frequently
/// fails to push state updates to Spotify Connect, so `/me/player` keeps
/// returning whichever device reported last (often something from a prior
/// session, with a stale timestamp).
pub fn should_accept(s: &AppState, incoming: Option<&Playback>) -> bool {
    let local = s.last_local_action_ms;
    match incoming {
        Some(pb) => {
            // Accept if Spotify's state is at least as new as our last action.
            // Missing timestamps are treated as 0 (never trust them over a
            // recent local action).
            pb.timestamp.unwrap_or(0) >= local
        }
        None => {
            // Spotify reports no active session. Only believe it if our last
            // local action wasn't recent — otherwise it's the well-known 204
            // we get right after a play because librespot hasn't reported.
            now_unix_ms().saturating_sub(local) > 60_000
        }
    }
}

async fn apply_playback(
    state: &Arc<Mutex<AppState>>,
    art_loader: &Arc<art::ArtLoader>,
    pb: Option<Playback>,
) {
    // Skip stale data so we don't overwrite freshly-synthesized local state
    // (see should_accept).
    {
        let s = state.lock().await;
        if !should_accept(&s, pb.as_ref()) {
            log::note(
                "apply_playback: ignored stale poll",
                Some(&format!(
                    "polled_ts={:?} local_ts={}",
                    pb.as_ref().and_then(|p| p.timestamp),
                    s.last_local_action_ms
                )),
            );
            return;
        }
    }

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
                        if g.current_track_id == id_at_fetch {
                            g.art = Some(proto);
                        }
                    }
                    Err(_) => {}
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
            log::note("toggle_playback: skipped (rate-limited)", None);
            return;
        }
        s.playback.as_ref().map(|p| p.is_playing).unwrap_or(false)
    };
    log::note(
        "toggle_playback",
        Some(if was_playing { "pause" } else { "play" }),
    );
    let result = if was_playing {
        client.pause().await
    } else {
        client.play().await
    };
    let mut s = state.lock().await;
    match result {
        Ok(()) => {
            let ts = now_unix_ms();
            if let Some(p) = s.playback.as_mut() {
                p.is_playing = !was_playing;
                p.timestamp = Some(ts);
            }
            s.error = None;
            s.last_local_action_ms = ts;
        }
        Err(e) => {
            log::error("toggle_playback", &format!("{e:#}"));
            s.error = Some(format!("{e:#}"));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use api::{Album, Artist, Playlist, SearchResults, Track};

    fn track(uri: &str, name: &str) -> Track {
        Track {
            id: Some(uri.trim_start_matches("spotify:track:").to_string()),
            uri: Some(uri.to_string()),
            name: name.to_string(),
            duration_ms: 0,
            artists: vec![Artist {
                uri: None,
                name: "A".into(),
            }],
            album: Album {
                uri: None,
                name: "Alb".into(),
                artists: vec![],
                images: vec![],
            },
        }
    }

    fn search_state_with_results(results: SearchResults) -> SearchState {
        let mut s = SearchState::new(None);
        s.results = results;
        s
    }

    fn pb_with_ts(ts: Option<u64>, name: &str) -> Playback {
        Playback {
            is_playing: true,
            progress_ms: Some(0),
            item: Some(track("spotify:track:x", name)),
            context: None,
            timestamp: ts,
        }
    }

    #[test]
    fn should_accept_rejects_polled_older_than_local_action() {
        let mut s = AppState::default();
        s.last_local_action_ms = 10_000;
        // Polled ts is older — stale data from prior session.
        assert!(!should_accept(&s, Some(&pb_with_ts(Some(5_000), "stale"))));
    }

    #[test]
    fn should_accept_accepts_polled_newer_than_local_action() {
        let mut s = AppState::default();
        s.last_local_action_ms = 10_000;
        assert!(should_accept(&s, Some(&pb_with_ts(Some(20_000), "fresh"))));
    }

    #[test]
    fn should_accept_accepts_polled_equal_timestamp() {
        let mut s = AppState::default();
        s.last_local_action_ms = 10_000;
        assert!(should_accept(&s, Some(&pb_with_ts(Some(10_000), "same"))));
    }

    #[test]
    fn should_accept_rejects_missing_timestamp_when_we_recently_acted() {
        let mut s = AppState::default();
        s.last_local_action_ms = now_unix_ms();
        // Spotify omitted the timestamp — treat as 0, older than our action.
        assert!(!should_accept(&s, Some(&pb_with_ts(None, "no-ts"))));
    }

    #[test]
    fn should_accept_accepts_when_no_prior_local_action() {
        let s = AppState::default();
        // last_local_action_ms == 0 (default) — accept anything.
        assert!(should_accept(&s, Some(&pb_with_ts(Some(0), "first"))));
        assert!(should_accept(&s, None));
    }

    #[test]
    fn should_accept_rejects_none_right_after_play() {
        let mut s = AppState::default();
        s.last_local_action_ms = now_unix_ms();
        // 204 No Content right after a play — librespot hasn't reported yet.
        assert!(!should_accept(&s, None));
    }

    #[test]
    fn should_accept_accepts_none_after_long_idle() {
        let mut s = AppState::default();
        s.last_local_action_ms = now_unix_ms().saturating_sub(120_000); // 2 min ago
        // Plenty of time has passed; trust that nothing is playing.
        assert!(should_accept(&s, None));
    }

    #[test]
    fn synth_template_for_track_omits_timestamp() {
        let search = search_state_with_results(SearchResults {
            tracks: vec![track("spotify:track:abc", "T1")],
            ..Default::default()
        });
        let action = PlayAction::Track("spotify:track:abc".into());
        let synth = synth_template_for(&action, &search).expect("synth must build");
        assert!(synth.is_playing);
        assert_eq!(synth.item.as_ref().unwrap().name, "T1");
        assert_eq!(synth.progress_ms, Some(0));
        assert!(synth.context.is_none());
        // Timestamp is filled in by play_selection AFTER the play call,
        // so it matches last_local_action_ms exactly.
        assert!(synth.timestamp.is_none());
    }

    #[test]
    fn synth_template_for_in_context_includes_playlist_context() {
        let mut s = SearchState::new(Some(InContext {
            playlist_uri: "spotify:playlist:pl".into(),
            tracks: vec![track("spotify:track:in0", "in0")],
            filtered: vec![0],
        }));
        s.selected = 0;
        let action = PlayAction::Context {
            uri: "spotify:playlist:pl".into(),
            offset: Some("spotify:track:in0".into()),
        };
        let synth = synth_template_for(&action, &s).expect("synth must build");
        assert_eq!(synth.item.as_ref().unwrap().name, "in0");
        let ctx = synth.context.expect("context expected");
        assert_eq!(ctx.uri, "spotify:playlist:pl");
        assert_eq!(ctx.kind, "playlist");
    }

    #[test]
    fn synth_template_for_context_without_offset_is_none() {
        // Album/playlist/artist play with no specific track — we don't have
        // enough info to synthesize; polled data (if any) takes over.
        let search = search_state_with_results(SearchResults::default());
        let action = PlayAction::Context {
            uri: "spotify:album:a".into(),
            offset: None,
        };
        assert!(synth_template_for(&action, &search).is_none());
    }

    #[test]
    fn synth_with_matching_timestamp_is_accepted() {
        // The exact case that broke first time: synth and last_local_action
        // share a timestamp; should_accept must accept (>= comparison).
        let ts = now_unix_ms();
        let mut s = AppState::default();
        s.last_local_action_ms = ts;
        let mut synth = pb_with_ts(Some(ts), "POWER");
        synth.is_playing = true;
        assert!(should_accept(&s, Some(&synth)));
    }

    #[test]
    fn resolve_track_returns_track_uri() {
        let s = search_state_with_results(SearchResults {
            tracks: vec![track("spotify:track:abc", "T1")],
            ..Default::default()
        });
        match resolve_selection(&s) {
            Some(PlayAction::Track(uri)) => assert_eq!(uri, "spotify:track:abc"),
            other => panic!("expected Track, got {other:?}"),
        }
    }

    #[test]
    fn resolve_album_returns_context_no_offset() {
        let mut s = search_state_with_results(SearchResults {
            albums: vec![Album {
                uri: Some("spotify:album:1".into()),
                name: "A".into(),
                artists: vec![],
                images: vec![],
            }],
            ..Default::default()
        });
        s.selected = 0; // sections start at 0 since tracks is empty
        match resolve_selection(&s) {
            Some(PlayAction::Context { uri, offset }) => {
                assert_eq!(uri, "spotify:album:1");
                assert!(offset.is_none());
            }
            other => panic!("expected album context, got {other:?}"),
        }
    }

    #[test]
    fn resolve_walks_across_sections() {
        let mut s = search_state_with_results(SearchResults {
            tracks: vec![track("spotify:track:t0", "t0"), track("spotify:track:t1", "t1")],
            albums: vec![Album {
                uri: Some("spotify:album:al".into()),
                name: "Al".into(),
                artists: vec![],
                images: vec![],
            }],
            artists: vec![Artist {
                uri: Some("spotify:artist:ar".into()),
                name: "Ar".into(),
            }],
            playlists: vec![Playlist {
                uri: "spotify:playlist:p".into(),
                name: "P".into(),
                owner: None,
            }],
        });
        // index 0,1 → tracks; 2 → album; 3 → artist; 4 → playlist
        let cases: Vec<(usize, &str)> = vec![
            (0, "spotify:track:t0"),
            (1, "spotify:track:t1"),
            (2, "spotify:album:al"),
            (3, "spotify:artist:ar"),
            (4, "spotify:playlist:p"),
        ];
        for (idx, want) in cases {
            s.selected = idx;
            let got = match resolve_selection(&s) {
                Some(PlayAction::Track(u)) => u,
                Some(PlayAction::Context { uri, .. }) => uri,
                None => panic!("none at idx {idx}"),
            };
            assert_eq!(got, want, "at idx {idx}");
        }
    }

    #[test]
    fn resolve_returns_none_when_out_of_range() {
        let s = search_state_with_results(SearchResults::default());
        assert!(resolve_selection(&s).is_none());
    }

    #[test]
    fn resolve_in_context_uses_playlist_uri_with_offset() {
        let mut s = SearchState::new(Some(InContext {
            playlist_uri: "spotify:playlist:pl".into(),
            tracks: vec![track("spotify:track:in0", "in0")],
            filtered: vec![0],
        }));
        s.selected = 0;
        match resolve_selection(&s) {
            Some(PlayAction::Context { uri, offset }) => {
                assert_eq!(uri, "spotify:playlist:pl");
                assert_eq!(offset.as_deref(), Some("spotify:track:in0"));
            }
            other => panic!("expected in-context, got {other:?}"),
        }
    }

    #[test]
    fn refilter_in_context_matches_track_name_case_insensitive() {
        let mut s = SearchState::new(Some(InContext {
            playlist_uri: "spotify:playlist:p".into(),
            tracks: vec![
                track("spotify:track:1", "Strawberry Fields"),
                track("spotify:track:2", "Yesterday"),
            ],
            filtered: vec![],
        }));
        s.input = "STRAW".into();
        refilter_in_context(&mut s);
        let ctx = s.in_context.as_ref().unwrap();
        assert_eq!(ctx.filtered, vec![0]);
    }

    #[test]
    fn refilter_in_context_empty_input_clears() {
        let mut s = SearchState::new(Some(InContext {
            playlist_uri: "spotify:playlist:p".into(),
            tracks: vec![track("spotify:track:1", "X")],
            filtered: vec![0],
        }));
        s.input.clear();
        refilter_in_context(&mut s);
        assert!(s.in_context.as_ref().unwrap().filtered.is_empty());
    }

    #[test]
    fn char_idx_to_byte_ascii_and_multibyte() {
        assert_eq!(char_idx_to_byte("abc", 0), 0);
        assert_eq!(char_idx_to_byte("abc", 2), 2);
        assert_eq!(char_idx_to_byte("abc", 3), 3); // past end
        assert_eq!(char_idx_to_byte("abc", 99), 3);
        // "é" is 2 bytes in UTF-8
        assert_eq!(char_idx_to_byte("aéc", 1), 1);
        assert_eq!(char_idx_to_byte("aéc", 2), 3);
    }

    #[test]
    fn key_label_basic() {
        use crossterm::event::{KeyCode, KeyModifiers};
        assert_eq!(key_label(KeyCode::Char('a'), KeyModifiers::NONE), "'a'");
        assert_eq!(key_label(KeyCode::Esc, KeyModifiers::NONE), "esc");
        assert_eq!(key_label(KeyCode::Enter, KeyModifiers::NONE), "enter");
        // with modifier
        let s = key_label(KeyCode::Char('c'), KeyModifiers::CONTROL);
        assert!(s.contains("CONTROL") && s.contains("'c'"), "got: {s}");
    }

    #[test]
    fn playlist_id_extraction() {
        assert_eq!(
            playlist_id_from_uri("spotify:playlist:abc123"),
            Some("abc123".into())
        );
        assert!(playlist_id_from_uri("spotify:album:abc").is_none());
    }
}
