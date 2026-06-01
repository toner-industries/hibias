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
mod recent;
mod streaming;
#[cfg(test)]
mod test_support;
mod ui;

use api::{Context as PlaybackContext, Playback, RateLimited, SearchResults, SpotifyApi, SpotifyClient, Track};
use keys::ModeMask;

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
    /// `None` = never probed; `Some(true)` = our device id is in `/me/player/devices`;
    /// `Some(false)` = probe succeeded but our id is missing. When `false`,
    /// the librespot Spirc session has lost its Connect cloud registration
    /// and play/pause will 404 until the user restarts the app.
    pub device_present: Option<bool>,
    /// Recently played tracks fetched from /me/player/recently-played.
    /// Shown in the search overlay when the input is empty.
    pub recent_tracks: Vec<Track>,
    /// Queries the user has searched for previously, most-recent-first.
    /// Persisted to `hifi-recent.json` so it survives restarts.
    pub recent_queries: Vec<String>,
    /// True until the user takes a local action OR we observe an
    /// actively-playing track. While true, paused/empty polled playback is
    /// ignored so the recently-played seed isn't clobbered by whatever stale
    /// state `/me/player` happens to return at boot.
    pub boot: bool,
    /// Background streaming startup status. None = still starting; Some(Ok)
    /// after Spirc is registered; Some(Err) on failure.
    pub streaming_failed: Option<String>,
    /// The active librespot session, kept here so reconnect can shut down
    /// the old one and replace it with a new one. None = not yet started
    /// or torn down for reconnect.
    pub streaming: Option<streaming::Streaming>,
    /// True while a reconnect is in flight — prevents the auto-reconnect
    /// watchdog from racing with a manual `:reconnect`, and lets the UI
    /// show a "Reconnecting..." indicator.
    pub reconnecting: bool,
}

impl Default for AppState {
    fn default() -> Self {
        Self {
            playback: None,
            last_poll: None,
            error: None,
            rate_limited_until: None,
            art: None,
            current_track_id: None,
            device_name: None,
            mode: Mode::default(),
            last_local_action_ms: 0,
            device_present: None,
            recent_tracks: Vec::new(),
            recent_queries: Vec::new(),
            boot: true,
            streaming_failed: None,
            streaming: None,
            reconnecting: false,
        }
    }
}

#[derive(Default)]
pub enum Mode {
    #[default]
    NowPlaying,
    Search(SearchState),
    Help,
    Command(CommandState),
    Browse(BrowseState),
}

impl Mode {
    pub fn mask(&self) -> ModeMask {
        match self {
            Mode::NowPlaying => ModeMask::NOW_PLAYING,
            Mode::Search(_) => ModeMask::SEARCH,
            Mode::Help => ModeMask::HELP,
            Mode::Command(_) => ModeMask::COMMAND,
            Mode::Browse(_) => ModeMask::BROWSE,
        }
    }
}

/// Album or playlist metadata, captured at the moment the user hit Enter
/// on a search row. The tracks themselves are fetched lazily.
#[derive(Debug, Clone)]
pub struct Collection {
    pub kind: CollectionKind,
    pub uri: String,
    pub name: String,
    pub subtitle: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CollectionKind {
    Album,
    Playlist,
}

impl CollectionKind {
    pub fn label(self) -> &'static str {
        match self {
            CollectionKind::Album => "album",
            CollectionKind::Playlist => "playlist",
        }
    }
}

pub struct BrowseState {
    pub collection: Collection,
    pub tracks: Vec<Track>,
    pub loading: bool,
    pub error: Option<String>,
    pub selected: usize,
    /// First visible track index, for scrolling long collections.
    pub scroll: usize,
    /// The Search state we came from, kept here so Esc can restore the
    /// user's query, results, and selection exactly as they left it.
    pub prior_search: SearchState,
    /// Monotonic id so a slow fetch that resolves after the user has
    /// already navigated away can be dropped instead of clobbering state.
    pub fetch_id: u64,
}

/// A discrete action runnable from the command menu.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Cmd {
    PlayPause,
    Next,
    Previous,
    Reconnect,
    Search,
    Help,
    Quit,
}

impl Cmd {
    pub const ALL: &'static [Cmd] = &[
        Cmd::PlayPause,
        Cmd::Next,
        Cmd::Previous,
        Cmd::Reconnect,
        Cmd::Search,
        Cmd::Help,
        Cmd::Quit,
    ];

    pub fn name(self) -> &'static str {
        match self {
            Cmd::PlayPause => "play / pause",
            Cmd::Next => "next",
            Cmd::Previous => "previous",
            Cmd::Reconnect => "reconnect",
            Cmd::Search => "search",
            Cmd::Help => "help",
            Cmd::Quit => "quit",
        }
    }

    pub fn description(self) -> &'static str {
        match self {
            Cmd::PlayPause => "toggle playback on the current device",
            Cmd::Next => "skip to the next track",
            Cmd::Previous => "skip back (or restart current track)",
            Cmd::Reconnect => "restart the 'hifi' Connect device",
            Cmd::Search => "open the Spotify search overlay",
            Cmd::Help => "show the hotkey help overlay",
            Cmd::Quit => "exit hifi",
        }
    }
}

pub struct CommandState {
    pub input: String,
    pub cursor: usize,
    pub selected: usize,
}

impl Default for CommandState {
    fn default() -> Self {
        Self {
            input: String::new(),
            cursor: 0,
            selected: 0,
        }
    }
}

impl CommandState {
    /// Commands matching the current input (case-insensitive substring on
    /// the name). With empty input, all commands are returned.
    pub fn filtered(&self) -> Vec<Cmd> {
        if self.input.is_empty() {
            return Cmd::ALL.to_vec();
        }
        let q = self.input.to_lowercase();
        Cmd::ALL
            .iter()
            .copied()
            .filter(|c| c.name().to_lowercase().contains(&q))
            .collect()
    }

    pub fn selected_cmd(&self) -> Option<Cmd> {
        self.filtered().get(self.selected).copied()
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
    /// Snapshot copied from AppState when search opens — shown only when
    /// `input` is empty so it doesn't fight the live search results.
    pub recent_queries: Vec<String>,
    pub recent_tracks: Vec<Track>,
}

impl SearchState {
    fn new(
        in_context: Option<InContext>,
        recent_queries: Vec<String>,
        recent_tracks: Vec<Track>,
    ) -> Self {
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
            recent_queries,
            recent_tracks,
        }
    }

    /// Whether the "empty input" view (recents) is what the user sees right now.
    pub fn showing_recents(&self) -> bool {
        self.input.is_empty()
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
    {
        let c = client.clone();
        let s = state.clone();
        let a = art_loader.clone();
        tokio::spawn(async move {
            match c.get_recently_played(20).await {
                Ok(tracks) => {
                    log::note(
                        "recently_played loaded",
                        Some(&format!("count={}", tracks.len())),
                    );
                    let first = tracks.first().cloned();
                    {
                        let mut g = s.lock().await;
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
                        apply_playback_force(&s, &a, Some(synth)).await;
                    }
                }
                Err(e) => {
                    log::note(
                        "recently_played unavailable",
                        Some(&format!("{e:#} (likely missing scope — run `just reauth`)")),
                    );
                    // Fall back to /me/player so users without the new scope
                    // still see *something* (paused, last-known) instead of
                    // an empty screen. Still routed through the boot guard
                    // via force=true: it's our deliberate seed.
                    if let Ok(Some(pb)) = c.get_playback().await {
                        log::note(
                            "boot seed via /me/player fallback",
                            pb.item.as_ref().map(|t| t.name.as_str()),
                        );
                        let mut seed = pb;
                        // Always show as paused — we don't actually know what's
                        // happening on whichever device this came from.
                        seed.is_playing = false;
                        seed.timestamp = None;
                        apply_playback_force(&s, &a, Some(seed)).await;
                    }
                }
            }
        });
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
    client: Arc<dyn SpotifyApi>,
    state: Arc<Mutex<AppState>>,
    art_loader: Arc<art::ArtLoader>,
) -> Result<()> {
    // Periodic device probe — logs Spotify's view of our device every 5s.
    // Surfaces "device drops out of active state" issues that some other
    // Spotify TUIs miss.
    let dev_client = client.clone();
    let dev_state = state.clone();
    let dev_handle = tokio::spawn(async move {
        let mut interval = tokio::time::interval(Duration::from_secs(5));
        interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        interval.tick().await;
        loop {
            interval.tick().await;
            // Honor an existing rate-limit window — pounding /me/player/devices
            // while Spotify is telling us to back off just extends the misery.
            {
                let s = dev_state.lock().await;
                if s.rate_limited_until
                    .map(|t| t > Instant::now())
                    .unwrap_or(false)
                {
                    continue;
                }
            }
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
                    // Update our "is the librespot device still registered?"
                    // flag. Only meaningful once we've actually started a
                    // device (our_id is set). We do NOT auto-reconnect here
                    // — the probe runs every 5s and repeated reconnect
                    // attempts hammer Spotify (we saw an 8-hour Retry-After
                    // come back from that loop). Instead, the next user
                    // play/pause/seek/skip will trigger the reconnect, or
                    // they can run `:reconnect` manually.
                    if let Some(id) = our_id {
                        let now_present = devs.iter().any(|d| d.id.as_deref() == Some(id.as_str()));
                        let mut s = dev_state.lock().await;
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
                    // If the probe itself trips a 429, mirror what the
                    // playback poller does so the rest of the app sees the
                    // back-off window and stops piling on.
                    if let Some(secs) = e.downcast_ref::<RateLimited>().map(|r| r.0) {
                        let mut s = dev_state.lock().await;
                        s.rate_limited_until =
                            Some(Instant::now() + Duration::from_secs(secs));
                    }
                    log::note("devices probe failed", Some(&format!("{e:#}")));
                }
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
            // Same back-off respect as the devices probe — see above.
            {
                let s = poll_state.lock().await;
                if s.rate_limited_until
                    .map(|t| t > Instant::now())
                    .unwrap_or(false)
                {
                    continue;
                }
            }
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
                        KeyAction::Seek(delta_ms) => seek_relative(&client, &state, delta_ms).await,
                        KeyAction::NextTrack => skip_track(&client, &state, true).await,
                        KeyAction::PrevTrack => skip_track(&client, &state, false).await,
                        KeyAction::Reconnect => {
                            spawn_reconnect(&client, &state, "user: :reconnect");
                        }
                        KeyAction::EnterSearch => enter_search(&client, &state).await,
                        KeyAction::OpenBrowse(coll) => enter_browse(&client, &state, coll).await,
                        KeyAction::PlayBrowseSelection => {
                            play_browse_selection(&client, &state, &art_loader).await;
                        }
                        KeyAction::PlayBrowseCollection => {
                            play_browse_collection(&client, &state, &art_loader).await;
                        }
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
                                    // The client's send_logged would
                                    // short-circuit anyway, but bailing the
                                    // whole burst is cleaner — no point
                                    // chewing through six instant-fail
                                    // RateLimited errors.
                                    if c.rate_limited_until().is_some() {
                                        break;
                                    }
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

/// Kick off a reconnect on a background task. Safe to call multiple times
/// — the in-flight guard inside `reconnect_now` collapses concurrent
/// triggers (e.g., auto-watchdog firing while a manual `:reconnect` is
/// already running).
fn spawn_reconnect(
    client: &Arc<dyn SpotifyApi>,
    state: &Arc<Mutex<AppState>>,
    reason: &'static str,
) {
    let client = client.clone();
    let state = state.clone();
    tokio::spawn(async move {
        reconnect_now(&client, &state, reason).await;
    });
}

/// Tear down the current librespot session (if any) and start a fresh one.
/// Used by both startup and reconnect — single code path keeps the device-id
/// lifecycle in one place.
async fn reconnect_now(
    client: &Arc<dyn SpotifyApi>,
    state: &Arc<Mutex<AppState>>,
    reason: &'static str,
) {
    // Skip if a reconnect is already underway.
    {
        let mut s = state.lock().await;
        if s.reconnecting {
            log::note("reconnect: already in flight, skipping", Some(reason));
            return;
        }
        s.reconnecting = true;
        // Reset visible state for the "starting" indicator. Also clear
        // any lingering rate-limit and error state — Spotify sometimes
        // hands out absurdly long Retry-After headers after a bad probe
        // loop, and a manual reconnect is the user's "get me unstuck" lever.
        s.streaming_failed = None;
        s.device_name = None;
        s.device_present = None;
        s.rate_limited_until = None;
        s.error = None;
    }
    client.clear_rate_limit();
    log::note("reconnect: starting", Some(reason));

    // Shut down the existing session (if any). This is fire-and-forget;
    // a broken Spirc may surface an error but we're tearing it down regardless.
    let old = {
        let mut s = state.lock().await;
        s.streaming.take()
    };
    if let Some(old) = old {
        match old.shutdown() {
            Ok(()) => log::note("reconnect: old spirc shutdown ok", None),
            Err(e) => log::note("reconnect: old spirc shutdown err", Some(&format!("{e:#}"))),
        }
        // Give librespot a moment to actually wind down before binding a
        // new session — otherwise Spotify Connect can return the old (dead)
        // device record from /me/player/devices for a few seconds.
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
    client.clear_device_id();

    match streaming::start("hifi").await {
        Ok(new) => {
            log::note(
                "reconnect: new spirc up",
                Some(&format!("name={} id={}", new.device_name, new.device_id)),
            );
            client.set_device_id(new.device_id.clone());
            let device_id_for_transfer = new.device_id.clone();
            {
                let mut s = state.lock().await;
                s.device_name = Some(new.device_name.clone());
                s.streaming = Some(new);
                s.reconnecting = false;
            }
            wait_then_transfer(client.as_ref(), &device_id_for_transfer).await;
        }
        Err(e) => {
            let msg = format!("{e:#}");
            log::error("reconnect: streaming::start failed", &msg);
            let mut s = state.lock().await;
            s.streaming_failed = Some(msg);
            s.reconnecting = false;
        }
    }
}

/// If we know the librespot device has dropped off Spotify Connect, kick a
/// reconnect and tell the caller to abort. The caller's API call would have
/// 404'd anyway, and re-doing the action after reconnect is the user's job
/// — they're already at the keyboard.
async fn reconnect_if_device_offline(
    client: &Arc<dyn SpotifyApi>,
    state: &Arc<Mutex<AppState>>,
    caller: &'static str,
) -> bool {
    let offline = {
        let s = state.lock().await;
        s.device_present == Some(false) && !s.reconnecting
    };
    if offline {
        log::note(
            "device offline — auto-reconnecting on user action",
            Some(caller),
        );
        spawn_reconnect(client, state, "user action while offline");
    }
    offline
}

async fn wait_then_transfer(client: &dyn SpotifyApi, device_id: &str) {
    for attempt in 0..24 {
        tokio::time::sleep(Duration::from_millis(500)).await;
        // Bail the whole loop if the rate-limit gate trips mid-probe; we
        // don't want to keep poking /me/player/devices every 500ms while
        // we're meant to be backing off.
        if client.rate_limited_until().is_some() {
            log::note("wait_then_transfer: aborted (rate-limited)", None);
            return;
        }
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
            // get_devices/transfer_playback returning RateLimited is the
            // signal that the gate is set; abort and let the user retry.
            Err(e) if e.downcast_ref::<RateLimited>().is_some() => {
                log::note("wait_then_transfer: aborted (RateLimited)", None);
                return;
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
        Mode::Command(_) => "command",
        Mode::Browse(_) => "browse",
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
    /// Open the Browse mode loaded with the given album or playlist.
    OpenBrowse(Collection),
    /// Play the currently selected track inside the active Browse view.
    PlayBrowseSelection,
    /// Play the active Browse collection from the start, ignoring the
    /// per-track selection. Used when the tracks endpoint is 403'd —
    /// the user can still kick off playback of the whole album/playlist.
    PlayBrowseCollection,
    /// Seek the current track by ±N milliseconds.
    Seek(i64),
    NextTrack,
    PrevTrack,
    Reconnect,
}

const SEEK_STEP_MS: i64 = 10_000;

/// What hitting Enter on the current selection means.
#[derive(Debug)]
enum SelectionAction {
    /// Re-run search with this query (selected a row from "Recent searches").
    PromoteQuery(String),
    Play(PlayAction),
    /// Open an album or playlist to browse its tracks.
    Browse(Collection),
}

async fn dispatch_key(
    code: KeyCode,
    mods: KeyModifiers,
    state: &Mutex<AppState>,
) -> KeyAction {
    let mut s = state.lock().await;
    match &mut s.mode {
        Mode::NowPlaying => match code {
            KeyCode::Char('q') | KeyCode::Esc => KeyAction::Quit,
            KeyCode::Char(' ') => KeyAction::TogglePlayback,
            KeyCode::Left if mods.contains(KeyModifiers::SHIFT) => KeyAction::Seek(-SEEK_STEP_MS),
            KeyCode::Right if mods.contains(KeyModifiers::SHIFT) => KeyAction::Seek(SEEK_STEP_MS),
            KeyCode::Char('/') => KeyAction::EnterSearch,
            KeyCode::Char(':') => {
                s.mode = Mode::Command(CommandState::default());
                KeyAction::Stay
            }
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
            KeyCode::Enter => match resolve_full_selection(search) {
                Some(SelectionAction::PromoteQuery(q)) => {
                    search.input = q.clone();
                    search.cursor = q.chars().count();
                    refilter_in_context(search);
                    KeyAction::SearchInputChanged
                }
                Some(SelectionAction::Play(_)) => KeyAction::PlaySelection,
                Some(SelectionAction::Browse(coll)) => KeyAction::OpenBrowse(coll),
                None => KeyAction::Stay,
            },
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
        Mode::Command(cmd) => match code {
            KeyCode::Esc => {
                s.mode = Mode::NowPlaying;
                KeyAction::Stay
            }
            KeyCode::Up => {
                if cmd.selected > 0 {
                    cmd.selected -= 1;
                }
                KeyAction::Stay
            }
            KeyCode::Down => {
                let max = cmd.filtered().len().saturating_sub(1);
                if cmd.selected < max {
                    cmd.selected += 1;
                }
                KeyAction::Stay
            }
            KeyCode::Left => {
                if cmd.cursor > 0 {
                    cmd.cursor -= 1;
                }
                KeyAction::Stay
            }
            KeyCode::Right => {
                let max = cmd.input.chars().count();
                if cmd.cursor < max {
                    cmd.cursor += 1;
                }
                KeyAction::Stay
            }
            KeyCode::Backspace => {
                if cmd.cursor > 0 {
                    let byte = char_idx_to_byte(&cmd.input, cmd.cursor - 1);
                    cmd.input.remove(byte);
                    cmd.cursor -= 1;
                    cmd.selected = 0;
                }
                KeyAction::Stay
            }
            KeyCode::Char(c) => {
                let byte = char_idx_to_byte(&cmd.input, cmd.cursor);
                cmd.input.insert(byte, c);
                cmd.cursor += 1;
                cmd.selected = 0;
                KeyAction::Stay
            }
            KeyCode::Enter => {
                let Some(chosen) = cmd.selected_cmd() else {
                    return KeyAction::Stay;
                };
                // Dispatch — most commands close the menu first, then the
                // run loop performs the action.
                s.mode = Mode::NowPlaying;
                match chosen {
                    Cmd::PlayPause => KeyAction::TogglePlayback,
                    Cmd::Next => KeyAction::NextTrack,
                    Cmd::Previous => KeyAction::PrevTrack,
                    Cmd::Reconnect => KeyAction::Reconnect,
                    Cmd::Search => KeyAction::EnterSearch,
                    Cmd::Help => {
                        s.mode = Mode::Help;
                        KeyAction::Stay
                    }
                    Cmd::Quit => KeyAction::Quit,
                }
            }
            _ => KeyAction::Stay,
        },
        Mode::Browse(browse) => match code {
            KeyCode::Esc => {
                // Restore the search the user came from.
                let prior = std::mem::replace(&mut browse.prior_search, SearchState::new(None, Vec::new(), Vec::new()));
                s.mode = Mode::Search(prior);
                KeyAction::Stay
            }
            KeyCode::Up => {
                if browse.selected > 0 {
                    browse.selected -= 1;
                }
                KeyAction::Stay
            }
            KeyCode::Down => {
                let max = browse.tracks.len().saturating_sub(1);
                if browse.selected < max {
                    browse.selected += 1;
                }
                KeyAction::Stay
            }
            KeyCode::Enter => {
                if browse.tracks.is_empty() {
                    KeyAction::Stay
                } else {
                    KeyAction::PlayBrowseSelection
                }
            }
            // "Play the whole album/playlist" — works regardless of whether
            // the track list loaded.
            KeyCode::Char('p') => KeyAction::PlayBrowseCollection,
            _ => KeyAction::Stay,
        },
    }
}

fn visible_row_count(s: &SearchState) -> usize {
    if s.showing_recents() {
        return s.recent_queries.len() + s.recent_tracks.len();
    }
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

async fn enter_search(client: &Arc<dyn SpotifyApi>, state: &Arc<Mutex<AppState>>) {
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
        let queries = s.recent_queries.clone();
        let tracks = s.recent_tracks.clone();
        s.mode = Mode::Search(SearchState::new(in_context, queries, tracks));
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

fn album_id_from_uri(uri: &str) -> Option<String> {
    uri.strip_prefix("spotify:album:").map(|s| s.to_string())
}

/// Open the Browse overlay for the given collection, taking the current
/// SearchState with us so Esc can restore it. Kicks off the track fetch in
/// the background — Browse renders immediately with a "Loading..." placeholder.
async fn enter_browse(
    client: &Arc<dyn SpotifyApi>,
    state: &Arc<Mutex<AppState>>,
    collection: Collection,
) {
    log::note(
        "enter_browse",
        Some(&format!(
            "kind={} uri={} name={:?}",
            collection.kind.label(),
            collection.uri,
            collection.name
        )),
    );
    let fetch_id = {
        let mut s = state.lock().await;
        // Pull the current SearchState out so we can stash it in Browse.
        let prior = match std::mem::replace(&mut s.mode, Mode::NowPlaying) {
            Mode::Search(prev) => prev,
            // Defensive: caller should only invoke from Search.
            other => {
                s.mode = other;
                log::error("enter_browse called outside Search mode", "ignored");
                return;
            }
        };
        let fetch_id = prior.request_id.wrapping_add(1);
        s.mode = Mode::Browse(BrowseState {
            collection: collection.clone(),
            tracks: Vec::new(),
            loading: true,
            error: None,
            selected: 0,
            scroll: 0,
            prior_search: prior,
            fetch_id,
        });
        fetch_id
    };

    // Fire the fetch.
    let client = client.clone();
    let state_bg = state.clone();
    tokio::spawn(async move {
        let result = match collection.kind {
            CollectionKind::Album => match album_id_from_uri(&collection.uri) {
                Some(id) => client.get_album_tracks(&id).await,
                None => Err(anyhow::anyhow!("bad album uri: {}", collection.uri)),
            },
            CollectionKind::Playlist => match playlist_id_from_uri(&collection.uri) {
                Some(id) => client.get_playlist_tracks(&id).await,
                None => Err(anyhow::anyhow!("bad playlist uri: {}", collection.uri)),
            },
        };
        let mut s = state_bg.lock().await;
        let Mode::Browse(browse) = &mut s.mode else {
            // User navigated away; drop the result.
            log::note(
                "browse fetch arrived but not in Browse mode anymore",
                Some(&collection.uri),
            );
            return;
        };
        if browse.fetch_id != fetch_id || browse.collection.uri != collection.uri {
            // Stale fetch — user already opened a different collection.
            log::note("browse fetch stale", Some(&collection.uri));
            return;
        }
        browse.loading = false;
        match result {
            Ok(tracks) => {
                log::note(
                    "browse loaded",
                    Some(&format!("uri={} count={}", collection.uri, tracks.len())),
                );
                browse.tracks = tracks;
            }
            Err(e) => {
                let msg = format!("{e:#}");
                log::error("browse fetch", &msg);
                browse.error = Some(msg);
            }
        }
    });
}

/// Play the currently-selected track inside the active Browse view, using
/// the collection as the playback context (so Next/Previous walk the
/// collection in order).
async fn play_browse_selection(
    client: &Arc<dyn SpotifyApi>,
    state: &Arc<Mutex<AppState>>,
    art_loader: &Arc<art::ArtLoader>,
) {
    if reconnect_if_device_offline(client, state, "play_browse_selection").await {
        return;
    }
    let (action, synth) = {
        let s = state.lock().await;
        let Mode::Browse(browse) = &s.mode else { return };
        if browse.tracks.is_empty() {
            return;
        }
        let track = match browse.tracks.get(browse.selected) {
            Some(t) => t.clone(),
            None => return,
        };
        let offset_uri = match track.uri.clone() {
            Some(u) => u,
            None => {
                log::note("play_browse_selection: track missing uri", None);
                return;
            }
        };
        let action = PlayAction::Context {
            uri: browse.collection.uri.clone(),
            offset: Some(offset_uri),
        };
        // Synthesize the now-playing display: stamp the collection name into
        // the track's album field for albums (it was blank from the API),
        // so the user sees something sensible while the real poll catches up.
        let mut track = track;
        if matches!(browse.collection.kind, CollectionKind::Album) && track.album.name.is_empty() {
            track.album.name = browse.collection.name.clone();
        }
        let synth = Playback {
            is_playing: true,
            progress_ms: Some(0),
            item: Some(track),
            context: Some(api::Context {
                uri: browse.collection.uri.clone(),
                kind: browse.collection.kind.label().to_string(),
            }),
            timestamp: None,
        };
        (action, Some(synth))
    };

    log::note("play_browse_selection", Some(&format!("{action:?}")));
    let result = match &action {
        PlayAction::Context { uri, offset } => {
            client.play_context(uri, offset.as_deref()).await
        }
        // Unreachable — we only construct Context above.
        PlayAction::Track(_) => unreachable!(),
    };

    let synth_to_apply = match &result {
        Ok(()) => {
            let ts = now_unix_ms();
            let mut s = state.lock().await;
            s.error = None;
            log::mode_change("browse", "now_playing");
            s.mode = Mode::NowPlaying;
            s.last_local_action_ms = ts;
            s.boot = false;
            synth.map(|mut pb| {
                pb.timestamp = Some(ts);
                pb
            })
        }
        Err(e) => {
            let msg = format!("{e:#}");
            log::error("play_browse_selection", &msg);
            let mut s = state.lock().await;
            if is_device_not_found(&msg) {
                s.device_present = Some(false);
                s.error = Some(DEVICE_OFFLINE_MSG.to_string());
            } else {
                s.error = Some(msg);
            }
            None
        }
    };
    if let Some(pb) = synth_to_apply {
        apply_playback(state, art_loader, Some(pb)).await;
    }
}

/// Play the active Browse collection from the start with no offset. This is
/// the fallback when `/playlists/{id}/tracks` or `/albums/{id}/tracks` 403s
/// (Spotify locked these endpoints down for newly-created apps in late
/// 2024); the user can still kick off playback of the whole thing.
async fn play_browse_collection(
    client: &Arc<dyn SpotifyApi>,
    state: &Arc<Mutex<AppState>>,
    art_loader: &Arc<art::ArtLoader>,
) {
    if reconnect_if_device_offline(client, state, "play_browse_collection").await {
        return;
    }
    let uri = {
        let s = state.lock().await;
        let Mode::Browse(browse) = &s.mode else { return };
        browse.collection.uri.clone()
    };
    log::note("play_browse_collection", Some(&uri));
    let result = client.play_context(&uri, None).await;
    match result {
        Ok(()) => {
            // No tracks list to synth from — let the next /me/player poll
            // populate the now-playing display.
            let ts = now_unix_ms();
            let mut s = state.lock().await;
            s.error = None;
            log::mode_change("browse", "now_playing");
            s.mode = Mode::NowPlaying;
            s.last_local_action_ms = ts;
            s.boot = false;
            // Force-clear `playback` so the poll's first hit definitely
            // gets applied (it'll be a different track than the seed).
        }
        Err(e) => {
            let msg = format!("{e:#}");
            log::error("play_browse_collection", &msg);
            let mut s = state.lock().await;
            if is_device_not_found(&msg) {
                s.device_present = Some(false);
                s.error = Some(DEVICE_OFFLINE_MSG.to_string());
            } else {
                s.error = Some(msg);
            }
        }
    }
    // Bump a poll burst so we pick up the new track quickly.
    let c = client.clone();
    let s = state.clone();
    let a = art_loader.clone();
    tokio::spawn(async move {
        for _ in 0..6 {
            tokio::time::sleep(Duration::from_millis(250)).await;
            if c.rate_limited_until().is_some() {
                break;
            }
            if let Ok(pb) = c.get_playback().await {
                apply_playback(&s, &a, pb).await;
            }
        }
    });
}

async fn kick_search(client: &Arc<dyn SpotifyApi>, state: &Arc<Mutex<AppState>>) {
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
                let any_hits = !results.tracks.is_empty()
                    || !results.albums.is_empty()
                    || !results.artists.is_empty()
                    || !results.playlists.is_empty();
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
                        if any_hits {
                            recent::push_query(&mut s.recent_queries, &q);
                            recent::save_queries(&s.recent_queries);
                        }
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
    client: &Arc<dyn SpotifyApi>,
    state: &Arc<Mutex<AppState>>,
    art_loader: &Arc<art::ArtLoader>,
) {
    // 1. Resolve action and capture the synth template now, while search
    //    state is still in AppState. We finalize its timestamp later (after
    //    a successful play) so it matches `last_local_action_ms` exactly.
    if reconnect_if_device_offline(client, state, "play_selection").await {
        return;
    }
    let (action, synth_template) = {
        let s = state.lock().await;
        let Mode::Search(search) = &s.mode else {
            return;
        };
        let resolved = resolve_full_selection(search);
        let action = match resolved {
            Some(SelectionAction::Play(a)) => a,
            Some(SelectionAction::PromoteQuery(_)) => {
                // PromoteQuery is handled in dispatch_key before
                // PlaySelection is requested; defensive guard.
                log::note("play_selection: skipped (promote-query selection)", None);
                return;
            }
            Some(SelectionAction::Browse(_)) => {
                // Browse is routed via KeyAction::OpenBrowse, not PlaySelection.
                log::note("play_selection: skipped (browse selection)", None);
                return;
            }
            None => {
                log::note("play_selection: nothing selected", None);
                return;
            }
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
            s.boot = false;
            synth_template.map(|mut pb| {
                pb.timestamp = Some(ts);
                pb
            })
        }
        Err(e) => {
            let msg = format!("{e:#}");
            log::error("play_selection", &msg);
            let mut s = state.lock().await;
            if is_device_not_found(&msg) {
                s.device_present = Some(false);
                s.error = Some(DEVICE_OFFLINE_MSG.to_string());
            } else {
                s.error = Some(msg);
            }
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
    if let Some(t) = search
        .results
        .tracks
        .iter()
        .find(|t| t.uri.as_deref() == Some(uri))
        .cloned()
    {
        return Some(t);
    }
    search
        .recent_tracks
        .iter()
        .find(|t| t.uri.as_deref() == Some(uri))
        .cloned()
}

#[derive(Debug)]
enum PlayAction {
    Track(String),
    Context { uri: String, offset: Option<String> },
}

fn resolve_full_selection(s: &SearchState) -> Option<SelectionAction> {
    if s.showing_recents() {
        let mut idx = s.selected;
        if idx < s.recent_queries.len() {
            return Some(SelectionAction::PromoteQuery(s.recent_queries[idx].clone()));
        }
        idx -= s.recent_queries.len();
        if idx < s.recent_tracks.len() {
            let uri = s.recent_tracks[idx].uri.clone()?;
            return Some(SelectionAction::Play(PlayAction::Track(uri)));
        }
        return None;
    }
    // Albums and playlists open Browse instead of playing straight away.
    if let Some(coll) = resolve_collection_to_browse(s) {
        return Some(SelectionAction::Browse(coll));
    }
    resolve_selection(s).map(SelectionAction::Play)
}

/// If the current selection points at an album or playlist row in the
/// search results, return that collection's metadata. Otherwise None —
/// e.g. tracks, artists, and in-context rows fall through to play directly.
fn resolve_collection_to_browse(s: &SearchState) -> Option<Collection> {
    let mut idx = s.selected;
    if let Some(ctx) = &s.in_context {
        if idx < ctx.filtered.len() {
            return None;
        }
        idx -= ctx.filtered.len();
    }
    if idx < s.results.tracks.len() {
        return None;
    }
    idx -= s.results.tracks.len();
    if idx < s.results.albums.len() {
        let a = &s.results.albums[idx];
        let uri = a.uri.clone()?;
        let subtitle = a
            .artists
            .iter()
            .map(|x| x.name.as_str())
            .collect::<Vec<_>>()
            .join(", ");
        return Some(Collection {
            kind: CollectionKind::Album,
            uri,
            name: a.name.clone(),
            subtitle: if subtitle.is_empty() {
                "album".to_string()
            } else {
                format!("album · {subtitle}")
            },
        });
    }
    idx -= s.results.albums.len();
    if idx < s.results.artists.len() {
        // Artists still play directly (Spotify's "play artist context" picks
        // top tracks). Browsing an artist would need a different endpoint
        // (top tracks vs albums) — out of scope for now.
        return None;
    }
    idx -= s.results.artists.len();
    if idx < s.results.playlists.len() {
        let p = &s.results.playlists[idx];
        let owner = p
            .owner
            .as_ref()
            .and_then(|o| o.display_name.clone())
            .unwrap_or_default();
        return Some(Collection {
            kind: CollectionKind::Playlist,
            uri: p.uri.clone(),
            name: p.name.clone(),
            subtitle: if owner.is_empty() {
                "playlist".to_string()
            } else {
                format!("playlist · {owner}")
            },
        });
    }
    None
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
    // Boot mode: accept any non-empty payload so we display *something*
    // (a "Nothing playing" screen is the worst possible outcome). Reject
    // empty 204s — those are the well-known race where /me/player hasn't
    // caught up to our transfer yet. The recently-played seed (applied
    // via force=true) still wins because it lands before the first poll.
    if s.boot {
        return incoming.is_some();
    }
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
    apply_playback_inner(state, art_loader, pb, false).await
}

/// Apply playback bypassing `should_accept` — used for our own
/// synthesized seed (e.g. the recently-played track shown at startup),
/// which would otherwise be filtered out by the boot guard.
async fn apply_playback_force(
    state: &Arc<Mutex<AppState>>,
    art_loader: &Arc<art::ArtLoader>,
    pb: Option<Playback>,
) {
    apply_playback_inner(state, art_loader, pb, true).await
}

async fn apply_playback_inner(
    state: &Arc<Mutex<AppState>>,
    art_loader: &Arc<art::ArtLoader>,
    pb: Option<Playback>,
    force: bool,
) {
    // Skip stale data so we don't overwrite freshly-synthesized local state
    // (see should_accept).
    if !force {
        let s = state.lock().await;
        if !should_accept(&s, pb.as_ref()) {
            log::note(
                "apply_playback: ignored stale poll",
                Some(&format!(
                    "polled_ts={:?} local_ts={} boot={}",
                    pb.as_ref().and_then(|p| p.timestamp),
                    s.last_local_action_ms,
                    s.boot,
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

    let has_track = pb.as_ref().and_then(|p| p.item.as_ref()).is_some();
    let needs_art_fetch = {
        let mut s = state.lock().await;
        let prev = s.current_track_id.clone();
        s.playback = pb;
        s.last_poll = Some(Instant::now());
        s.error = None;
        s.rate_limited_until = None;
        s.current_track_id = new_track_id.clone();
        // First displayed track ends the boot phase — subsequent polls go
        // through the normal `should_accept` freshness checks.
        if has_track {
            s.boot = false;
        }
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

async fn toggle_playback(client: &Arc<dyn SpotifyApi>, state: &Arc<Mutex<AppState>>) {
    if reconnect_if_device_offline(client, state, "toggle_playback").await {
        return;
    }
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
            // Freeze the progress anchor *before* flipping is_playing so the
            // currently-displayed time is preserved across pause/resume. The
            // displayed value = stored progress_ms + (elapsed since last_poll
            // if playing); we collapse that into a fresh anchor of
            // (new_progress, last_poll = now).
            let now_instant = Instant::now();
            let frozen_progress = displayed_progress_for_toggle(&s);
            if let Some(p) = s.playback.as_mut() {
                p.is_playing = !was_playing;
                p.progress_ms = Some(frozen_progress);
                p.timestamp = Some(ts);
            }
            s.last_poll = Some(now_instant);
            s.error = None;
            s.last_local_action_ms = ts;
            s.boot = false;
        }
        Err(e) => {
            let msg = format!("{e:#}");
            log::error("toggle_playback", &msg);
            if is_device_not_found(&msg) {
                s.device_present = Some(false);
                s.error = Some(DEVICE_OFFLINE_MSG.to_string());
            } else {
                s.error = Some(msg);
            }
        }
    }
}

async fn seek_relative(
    client: &Arc<dyn SpotifyApi>,
    state: &Arc<Mutex<AppState>>,
    delta_ms: i64,
) {
    if reconnect_if_device_offline(client, state, "seek_relative").await {
        return;
    }
    // Compute the target position from the currently-displayed value (which
    // already includes elapsed time when playing), clamped to track duration.
    let (target_ms, was_playing) = {
        let s = state.lock().await;
        if s.rate_limited_until
            .map(|t| t > Instant::now())
            .unwrap_or(false)
        {
            log::note("seek_relative: skipped (rate-limited)", None);
            return;
        }
        let Some(pb) = s.playback.as_ref() else {
            return;
        };
        let Some(track) = pb.item.as_ref() else {
            return;
        };
        let cur = displayed_progress_for_toggle(&s) as i64;
        let raw = cur + delta_ms;
        let clamped = raw.clamp(0, track.duration_ms as i64);
        (clamped as u64, pb.is_playing)
    };
    log::note(
        "seek_relative",
        Some(&format!("delta_ms={delta_ms} target_ms={target_ms}")),
    );
    let result = client.seek_to(target_ms).await;
    let mut s = state.lock().await;
    match result {
        Ok(()) => {
            let ts = now_unix_ms();
            if let Some(p) = s.playback.as_mut() {
                p.progress_ms = Some(target_ms);
                p.timestamp = Some(ts);
                p.is_playing = was_playing;
            }
            s.last_poll = Some(Instant::now());
            s.error = None;
            s.last_local_action_ms = ts;
            s.boot = false;
        }
        Err(e) => {
            let msg = format!("{e:#}");
            log::error("seek_relative", &msg);
            if is_device_not_found(&msg) {
                s.device_present = Some(false);
                s.error = Some(DEVICE_OFFLINE_MSG.to_string());
            } else {
                s.error = Some(msg);
            }
        }
    }
}

async fn skip_track(client: &Arc<dyn SpotifyApi>, state: &Arc<Mutex<AppState>>, forward: bool) {
    if reconnect_if_device_offline(client, state, "skip_track").await {
        return;
    }
    {
        let s = state.lock().await;
        if s.rate_limited_until
            .map(|t| t > Instant::now())
            .unwrap_or(false)
        {
            log::note("skip_track: skipped (rate-limited)", None);
            return;
        }
    }
    log::note(
        "skip_track",
        Some(if forward { "next" } else { "previous" }),
    );
    let result = if forward {
        client.next_track().await
    } else {
        client.previous_track().await
    };
    let mut s = state.lock().await;
    match result {
        Ok(()) => {
            // The track has changed; poll will pick up the new one. Clear
            // the boot guard so the next poll lands.
            s.last_local_action_ms = now_unix_ms();
            s.boot = false;
            s.error = None;
        }
        Err(e) => {
            let msg = format!("{e:#}");
            log::error("skip_track", &msg);
            if is_device_not_found(&msg) {
                s.device_present = Some(false);
                s.error = Some(DEVICE_OFFLINE_MSG.to_string());
            } else {
                s.error = Some(msg);
            }
        }
    }
}

const DEVICE_OFFLINE_MSG: &str =
    "Connect device 'hifi' is offline — auto-reconnecting (or press ':' → reconnect)";

fn is_device_not_found(msg: &str) -> bool {
    msg.contains("Device not found") || msg.contains("\"status\" : 404")
}

/// Effective progress in ms given the current state — same calculation as the
/// UI's `displayed_progress`, but lives here so `toggle_playback` can freeze
/// the value before mutating `is_playing`.
fn displayed_progress_for_toggle(s: &AppState) -> u64 {
    let Some(pb) = &s.playback else {
        return 0;
    };
    let base = pb.progress_ms.unwrap_or(0);
    if !pb.is_playing {
        return base;
    }
    match s.last_poll {
        Some(poll) => base + poll.elapsed().as_millis() as u64,
        None => base,
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
        let mut s = SearchState::new(None, Vec::new(), Vec::new());
        s.results = results;
        s
    }

    /// Test helper: build a SearchState wrapping a given InContext, with no
    /// recents. Cuts boilerplate from in-context tests.
    fn search_state_with_context(ctx: InContext) -> SearchState {
        SearchState::new(Some(ctx), Vec::new(), Vec::new())
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

    /// All the steady-state should_accept tests assume we're past the
    /// initial boot phase (the boot guard is exercised separately below).
    fn steady_state() -> AppState {
        AppState {
            boot: false,
            ..Default::default()
        }
    }

    #[test]
    fn should_accept_rejects_polled_older_than_local_action() {
        let mut s = steady_state();
        s.last_local_action_ms = 10_000;
        // Polled ts is older — stale data from prior session.
        assert!(!should_accept(&s, Some(&pb_with_ts(Some(5_000), "stale"))));
    }

    #[test]
    fn should_accept_accepts_polled_newer_than_local_action() {
        let mut s = steady_state();
        s.last_local_action_ms = 10_000;
        assert!(should_accept(&s, Some(&pb_with_ts(Some(20_000), "fresh"))));
    }

    #[test]
    fn should_accept_accepts_polled_equal_timestamp() {
        let mut s = steady_state();
        s.last_local_action_ms = 10_000;
        assert!(should_accept(&s, Some(&pb_with_ts(Some(10_000), "same"))));
    }

    #[test]
    fn should_accept_rejects_missing_timestamp_when_we_recently_acted() {
        let mut s = steady_state();
        s.last_local_action_ms = now_unix_ms();
        // Spotify omitted the timestamp — treat as 0, older than our action.
        assert!(!should_accept(&s, Some(&pb_with_ts(None, "no-ts"))));
    }

    #[test]
    fn should_accept_accepts_when_no_prior_local_action() {
        let s = steady_state();
        // last_local_action_ms == 0 — accept anything in steady state.
        assert!(should_accept(&s, Some(&pb_with_ts(Some(0), "first"))));
        assert!(should_accept(&s, None));
    }

    #[test]
    fn should_accept_rejects_none_right_after_play() {
        let mut s = steady_state();
        s.last_local_action_ms = now_unix_ms();
        // 204 No Content right after a play — librespot hasn't reported yet.
        assert!(!should_accept(&s, None));
    }

    #[test]
    fn should_accept_accepts_none_after_long_idle() {
        let mut s = steady_state();
        s.last_local_action_ms = now_unix_ms().saturating_sub(120_000); // 2 min ago
        // Plenty of time has passed; trust that nothing is playing.
        assert!(should_accept(&s, None));
    }

    // --- boot-phase gating ----------------------------------------------

    #[test]
    fn should_accept_boot_accepts_paused_polled() {
        // Booting and we have nothing displayed — accept any payload, even a
        // paused one. Better to show "wrong" stale state than empty screen.
        let s = AppState::default();
        let mut pb = pb_with_ts(Some(0), "old");
        pb.is_playing = false;
        assert!(should_accept(&s, Some(&pb)));
    }

    #[test]
    fn should_accept_boot_rejects_none() {
        // 204 No Content right after transfer — librespot hasn't reported.
        let s = AppState::default();
        assert!(!should_accept(&s, None));
    }

    #[test]
    fn should_accept_boot_accepts_playing_polled() {
        let s = AppState::default();
        let mut pb = pb_with_ts(Some(0), "live");
        pb.is_playing = true;
        assert!(should_accept(&s, Some(&pb)));
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
        let mut s = search_state_with_context(InContext {
            playlist_uri: "spotify:playlist:pl".into(),
            tracks: vec![track("spotify:track:in0", "in0")],
            filtered: vec![0],
        });
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
        let mut s = search_state_with_context(InContext {
            playlist_uri: "spotify:playlist:pl".into(),
            tracks: vec![track("spotify:track:in0", "in0")],
            filtered: vec![0],
        });
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
        let mut s = search_state_with_context(InContext {
            playlist_uri: "spotify:playlist:p".into(),
            tracks: vec![
                track("spotify:track:1", "Strawberry Fields"),
                track("spotify:track:2", "Yesterday"),
            ],
            filtered: vec![],
        });
        s.input = "STRAW".into();
        refilter_in_context(&mut s);
        let ctx = s.in_context.as_ref().unwrap();
        assert_eq!(ctx.filtered, vec![0]);
    }

    #[test]
    fn refilter_in_context_empty_input_clears() {
        let mut s = search_state_with_context(InContext {
            playlist_uri: "spotify:playlist:p".into(),
            tracks: vec![track("spotify:track:1", "X")],
            filtered: vec![0],
        });
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

    // --- recents in the search overlay ---------------------------------

    #[test]
    fn recents_resolve_first_to_promote_query() {
        let s = SearchState::new(
            None,
            vec!["the beatles".into(), "weezer".into()],
            vec![track("spotify:track:r1", "Recent1")],
        );
        // selected=0 → first recent query
        match resolve_full_selection(&s) {
            Some(SelectionAction::PromoteQuery(q)) => assert_eq!(q, "the beatles"),
            other => panic!("expected PromoteQuery, got {other:?}"),
        }
    }

    #[test]
    fn recents_resolve_walks_into_recently_played() {
        let mut s = SearchState::new(
            None,
            vec!["q1".into()],
            vec![
                track("spotify:track:r0", "R0"),
                track("spotify:track:r1", "R1"),
            ],
        );
        s.selected = 2; // queries=1 then tracks 0,1 → idx 2 is recent track[1]
        match resolve_full_selection(&s) {
            Some(SelectionAction::Play(PlayAction::Track(uri))) => {
                assert_eq!(uri, "spotify:track:r1");
            }
            other => panic!("expected Play(Track), got {other:?}"),
        }
    }

    #[test]
    fn recents_hidden_when_input_nonempty() {
        let mut s = SearchState::new(
            None,
            vec!["q1".into()],
            vec![track("spotify:track:r0", "R0")],
        );
        s.input = "anything".into();
        // No live results — falls through to resolve_selection → None.
        assert!(resolve_full_selection(&s).is_none());
    }

    #[test]
    fn visible_row_count_uses_recents_when_input_empty() {
        let s = SearchState::new(
            None,
            vec!["q1".into(), "q2".into()],
            vec![track("spotify:track:r0", "R0")],
        );
        assert_eq!(visible_row_count(&s), 3);
    }

    #[test]
    fn visible_row_count_ignores_recents_when_input_nonempty() {
        let mut s = SearchState::new(
            None,
            vec!["q1".into(), "q2".into()],
            vec![track("spotify:track:r0", "R0")],
        );
        s.input = "x".into();
        assert_eq!(visible_row_count(&s), 0);
    }

    // --- pause/resume progress anchor ----------------------------------

    fn state_with_playback(progress_ms: u64, is_playing: bool) -> AppState {
        AppState {
            playback: Some(Playback {
                is_playing,
                progress_ms: Some(progress_ms),
                item: Some(track("spotify:track:x", "X")),
                context: None,
                timestamp: None,
            }),
            last_poll: Some(Instant::now()),
            ..Default::default()
        }
    }

    #[test]
    fn displayed_progress_for_toggle_paused_returns_stored() {
        let s = state_with_playback(45_000, false);
        assert_eq!(displayed_progress_for_toggle(&s), 45_000);
    }

    #[test]
    fn displayed_progress_for_toggle_playing_adds_elapsed() {
        // last_poll is "now"; elapsed should be ~0
        let s = state_with_playback(45_000, true);
        let got = displayed_progress_for_toggle(&s);
        assert!(got >= 45_000 && got < 45_500, "got {got}");
    }

    // --- browse routing -------------------------------------------------

    #[test]
    fn resolve_album_row_returns_browse_action() {
        let mut s = search_state_with_results(SearchResults {
            albums: vec![Album {
                uri: Some("spotify:album:abc".into()),
                name: "Some Album".into(),
                artists: vec![Artist {
                    uri: None,
                    name: "Artist X".into(),
                }],
                images: vec![],
            }],
            ..Default::default()
        });
        // Non-empty input so showing_recents() is false and we hit the
        // results branch.
        s.input = "x".into();
        s.selected = 0;
        match resolve_full_selection(&s) {
            Some(SelectionAction::Browse(c)) => {
                assert_eq!(c.uri, "spotify:album:abc");
                assert!(matches!(c.kind, CollectionKind::Album));
                assert_eq!(c.name, "Some Album");
                assert!(c.subtitle.contains("Artist X"), "got: {}", c.subtitle);
            }
            other => panic!("expected Browse(album), got {other:?}"),
        }
    }

    #[test]
    fn resolve_playlist_row_returns_browse_action() {
        let mut s = search_state_with_results(SearchResults {
            playlists: vec![Playlist {
                uri: "spotify:playlist:p1".into(),
                name: "Mix".into(),
                owner: Some(api::PlaylistOwner {
                    display_name: Some("alice".into()),
                }),
            }],
            ..Default::default()
        });
        s.input = "x".into();
        s.selected = 0;
        match resolve_full_selection(&s) {
            Some(SelectionAction::Browse(c)) => {
                assert_eq!(c.uri, "spotify:playlist:p1");
                assert!(matches!(c.kind, CollectionKind::Playlist));
                assert!(c.subtitle.contains("alice"), "got: {}", c.subtitle);
            }
            other => panic!("expected Browse(playlist), got {other:?}"),
        }
    }

    #[test]
    fn resolve_track_row_still_plays_not_browses() {
        let mut s = search_state_with_results(SearchResults {
            tracks: vec![track("spotify:track:t1", "T1")],
            ..Default::default()
        });
        s.input = "x".into();
        // selected=0 is the track row; should remain a Play action.
        match resolve_full_selection(&s) {
            Some(SelectionAction::Play(PlayAction::Track(uri))) => {
                assert_eq!(uri, "spotify:track:t1");
            }
            other => panic!("expected Play(Track), got {other:?}"),
        }
    }

    #[test]
    fn resolve_artist_row_still_plays_not_browses() {
        // Artists should keep playing directly (top-tracks context) since
        // there's no album-level browse to open.
        let mut s = search_state_with_results(SearchResults {
            artists: vec![Artist {
                uri: Some("spotify:artist:a1".into()),
                name: "AR".into(),
            }],
            ..Default::default()
        });
        s.input = "x".into();
        s.selected = 0;
        match resolve_full_selection(&s) {
            Some(SelectionAction::Play(PlayAction::Context { uri, offset })) => {
                assert_eq!(uri, "spotify:artist:a1");
                assert!(offset.is_none());
            }
            other => panic!("expected Play(Context for artist), got {other:?}"),
        }
    }

    #[test]
    fn album_id_from_uri_extracts() {
        assert_eq!(
            album_id_from_uri("spotify:album:xyz"),
            Some("xyz".to_string())
        );
        assert!(album_id_from_uri("spotify:playlist:xyz").is_none());
    }

    // --- command menu ---------------------------------------------------

    #[test]
    fn cmd_filtered_empty_input_returns_all() {
        let s = CommandState::default();
        assert_eq!(s.filtered().len(), Cmd::ALL.len());
    }

    #[test]
    fn cmd_filtered_case_insensitive_substring() {
        let mut s = CommandState::default();
        s.input = "PaUs".into();
        let got: Vec<&'static str> = s.filtered().iter().map(|c| c.name()).collect();
        assert!(got.contains(&"play / pause"), "got: {got:?}");
    }

    #[test]
    fn cmd_selected_indexes_into_filtered() {
        let mut s = CommandState::default();
        s.input = "re".into(); // matches "previous", "reconnect"
        s.selected = 1;
        let chosen = s.selected_cmd().expect("must select");
        let names: Vec<&'static str> = s.filtered().iter().map(|c| c.name()).collect();
        assert_eq!(chosen.name(), names[1]);
    }

    #[test]
    fn cmd_selected_out_of_range_returns_none() {
        let mut s = CommandState::default();
        s.selected = 999;
        assert!(s.selected_cmd().is_none());
    }

    #[test]
    fn is_device_not_found_recognizes_404_message() {
        assert!(is_device_not_found(
            "PUT https://api.spotify.com/v1/me/player/play: 404 Not Found: Device not found"
        ));
        assert!(is_device_not_found("{\"error\": {\"status\" : 404, \"message\" : \"x\"}}"));
        assert!(!is_device_not_found("rate limited"));
    }

    // ====================================================================
    // End-to-end scenarios driven through the Harness (test_support.rs).
    //
    // Each test wires up a `FakeSpotify`, programs the responses the
    // scenario needs, then drives keypresses through `dispatch_key` +
    // action handlers — the same paths the run loop uses in production.
    // Assertions look at the resulting `AppState` and at the rendered UI
    // (via ratatui's `TestBackend`).
    // ====================================================================

    use crate::test_support::{Call, Harness};

    fn dummy_album(uri: &str, name: &str, artist: &str) -> Album {
        Album {
            uri: Some(uri.into()),
            name: name.into(),
            artists: vec![Artist {
                uri: None,
                name: artist.into(),
            }],
            images: vec![],
        }
    }

    fn dummy_playlist(uri: &str, name: &str, owner: &str) -> Playlist {
        Playlist {
            uri: uri.into(),
            name: name.into(),
            owner: Some(api::PlaylistOwner {
                display_name: Some(owner.into()),
            }),
        }
    }

    /// The exact scenario the user just hit:
    ///   1. search for "power"
    ///   2. arrow down to a playlist row
    ///   3. press Enter to open Browse
    ///   4. Spotify 403s the `/playlists/{id}/tracks` endpoint
    ///   5. the Browse overlay shows a friendly yellow warning and
    ///      offers `p` to play the whole playlist
    #[tokio::test]
    async fn playlist_browse_403_shows_friendly_error_and_offers_p_fallback() {
        let h = Harness::new();
        // The search returns one playlist row.
        h.fake.set_search(
            "power",
            Ok(SearchResults {
                playlists: vec![dummy_playlist(
                    "spotify:playlist:abc123",
                    "POWER",
                    "chrisbolin",
                )],
                ..Default::default()
            }),
        );
        // The playlist-tracks fetch fails with Spotify's actual 403 body.
        h.fake.set_playlist_tracks(
            "abc123",
            Err("GET https://api.spotify.com/v1/playlists/abc123/tracks: 403 Forbidden: {\"error\": {\"status\": 403, \"message\": \"Forbidden\"}}".into()),
        );

        // Drive the flow.
        h.press_and_run(KeyCode::Char('/')).await; // open search
        h.type_str("power").await; // each char triggers a debounce
        h.settle().await; // let the debounce fire
        h.press_and_run(KeyCode::Down).await; // move off track row to playlist row
        h.press_and_run(KeyCode::Enter).await; // open browse
        h.settle().await; // let the playlist-tracks fetch resolve

        // State assertions.
        assert_eq!(h.mode_name().await, "browse");
        {
            let s = h.state.lock().await;
            let Mode::Browse(b) = &s.mode else {
                panic!("expected Mode::Browse, got {}", mode_name(&s));
            };
            assert!(!b.loading, "loading flag should be cleared");
            assert!(b.error.is_some(), "error should be populated on 403");
            let e = b.error.as_ref().unwrap();
            assert!(e.contains("403"), "error must mention 403, got: {e}");
        }

        // UI assertion: rendered screen contains the friendly hint.
        let screen = h.snapshot().await;
        assert!(
            screen.contains("Spotify won't let us read this playlist's tracks"),
            "expected friendly hint in:\n{screen}"
        );
        assert!(
            screen.contains("[p] play whole"),
            "expected fallback hint in:\n{screen}"
        );

        // And that the fake actually saw the playlist-tracks call (not the
        // album-tracks endpoint).
        let calls = h.fake.calls();
        assert!(
            calls.iter().any(|c| matches!(c, Call::GetPlaylistTracks(id) if id == "abc123")),
            "calls: {calls:?}"
        );
    }

    /// Inverse of the above: when the fetch succeeds, Browse shows the
    /// tracks and the regular hint, not the warning.
    #[tokio::test]
    async fn album_browse_success_shows_track_list() {
        let h = Harness::new();
        h.fake.set_search(
            "test",
            Ok(SearchResults {
                albums: vec![dummy_album("spotify:album:al1", "Test Album", "Test Artist")],
                ..Default::default()
            }),
        );
        h.fake.set_album_tracks(
            "al1",
            Ok(vec![
                track("spotify:track:a", "Track One"),
                track("spotify:track:b", "Track Two"),
                track("spotify:track:c", "Track Three"),
            ]),
        );

        h.press_and_run(KeyCode::Char('/')).await;
        h.type_str("test").await;
        h.settle().await;
        h.press_and_run(KeyCode::Enter).await; // album is first row -> Browse
        h.settle().await;

        let s = h.state.lock().await;
        let Mode::Browse(b) = &s.mode else {
            panic!("expected Browse");
        };
        assert!(!b.loading);
        assert!(b.error.is_none(), "no error expected, got {:?}", b.error);
        assert_eq!(b.tracks.len(), 3);
        assert_eq!(b.collection.name, "Test Album");
    }

    /// Pressing `p` inside Browse plays the whole collection via
    /// `play_context` with no offset — even when tracks haven't loaded.
    #[tokio::test]
    async fn p_in_browse_plays_whole_collection() {
        let h = Harness::new();
        h.fake.set_search(
            "x",
            Ok(SearchResults {
                playlists: vec![dummy_playlist("spotify:playlist:xyz", "Mix", "alice")],
                ..Default::default()
            }),
        );
        // Playlist-tracks 403s — but that doesn't block `p`.
        h.fake.set_playlist_tracks("xyz", Err("403 Forbidden".into()));

        h.press_and_run(KeyCode::Char('/')).await;
        h.type_str("x").await;
        h.settle().await;
        h.press_and_run(KeyCode::Enter).await;
        h.settle().await;

        // Now press `p` to play the whole playlist.
        h.fake.clear_calls();
        h.press_and_run(KeyCode::Char('p')).await;
        h.settle().await;

        let calls = h.fake.calls();
        let played = calls.iter().find_map(|c| match c {
            Call::PlayContext { uri, offset } => Some((uri.clone(), offset.clone())),
            _ => None,
        });
        assert_eq!(
            played,
            Some(("spotify:playlist:xyz".into(), None)),
            "expected play_context(playlist, None), all calls: {calls:?}"
        );
        // And the mode flips back to NowPlaying.
        assert_eq!(h.mode_name().await, "now_playing");
    }

    /// Enter on a track row in Browse plays via play_context with the
    /// track's URI as the offset (so Next/Previous walk the collection).
    #[tokio::test]
    async fn enter_in_browse_plays_selected_with_context_offset() {
        let h = Harness::new();
        h.fake.set_search(
            "x",
            Ok(SearchResults {
                albums: vec![dummy_album("spotify:album:al1", "Album", "Artist")],
                ..Default::default()
            }),
        );
        h.fake.set_album_tracks(
            "al1",
            Ok(vec![
                track("spotify:track:t0", "T0"),
                track("spotify:track:t1", "T1"),
                track("spotify:track:t2", "T2"),
            ]),
        );

        h.press_and_run(KeyCode::Char('/')).await;
        h.type_str("x").await;
        h.settle().await;
        h.press_and_run(KeyCode::Enter).await; // open browse
        h.settle().await;

        // Move down twice and pick T2.
        h.press_and_run(KeyCode::Down).await;
        h.press_and_run(KeyCode::Down).await;
        h.fake.clear_calls();
        h.press_and_run(KeyCode::Enter).await;
        h.settle().await;

        let calls = h.fake.calls();
        let played = calls.iter().find_map(|c| match c {
            Call::PlayContext { uri, offset } => Some((uri.clone(), offset.clone())),
            _ => None,
        });
        assert_eq!(
            played,
            Some((
                "spotify:album:al1".into(),
                Some("spotify:track:t2".into()),
            )),
            "got calls: {calls:?}"
        );
    }

    /// Esc inside Browse returns to the search the user came from, with
    /// the query, results, and selection intact.
    #[tokio::test]
    async fn esc_in_browse_restores_prior_search() {
        let h = Harness::new();
        h.fake.set_search(
            "x",
            Ok(SearchResults {
                albums: vec![dummy_album("spotify:album:al1", "A", "Art")],
                ..Default::default()
            }),
        );
        h.fake.set_album_tracks("al1", Ok(vec![]));

        h.press_and_run(KeyCode::Char('/')).await;
        h.type_str("x").await;
        h.settle().await;
        h.press_and_run(KeyCode::Enter).await;
        h.settle().await;
        assert_eq!(h.mode_name().await, "browse");

        h.press_and_run(KeyCode::Esc).await;
        assert_eq!(h.mode_name().await, "search");

        let s = h.state.lock().await;
        let Mode::Search(search) = &s.mode else {
            panic!();
        };
        assert_eq!(search.input, "x");
        assert_eq!(search.results.albums.len(), 1);
    }

    /// The rate-limit gate, when set, prevents user actions from hitting
    /// the API. Confirms the circuit breaker is wired all the way through
    /// the action handler layer (not just the HTTP layer).
    #[tokio::test]
    async fn rate_limited_state_blocks_play_pause() {
        let h = Harness::new();
        // Seed a playback (paused) so toggle has something to flip.
        h.seed_playback(Playback {
            is_playing: false,
            progress_ms: Some(0),
            item: Some(track("spotify:track:x", "X")),
            context: None,
            timestamp: Some(now_unix_ms()),
        }).await;
        {
            let mut s = h.state.lock().await;
            s.boot = false;
            s.rate_limited_until = Some(Instant::now() + Duration::from_secs(300));
        }
        h.fake.clear_calls();
        h.press_and_run(KeyCode::Char(' ')).await;

        let calls = h.fake.calls();
        assert!(
            !calls.iter().any(|c| matches!(c, Call::Play | Call::Pause)),
            "expected no Play/Pause calls while rate-limited, got: {calls:?}"
        );
    }

    /// The status line surfaces the rate-limit countdown.
    #[tokio::test]
    async fn rate_limited_ui_shows_countdown() {
        let h = Harness::new();
        {
            let mut s = h.state.lock().await;
            s.rate_limited_until = Some(Instant::now() + Duration::from_secs(120));
        }
        let screen = h.snapshot().await;
        assert!(
            screen.contains("rate limited"),
            "expected rate-limit hint in:\n{screen}"
        );
    }

    /// Search → Enter on a track row plays it directly (no Browse opens).
    #[tokio::test]
    async fn enter_on_track_row_plays_directly() {
        let h = Harness::new();
        h.fake.set_search(
            "rock",
            Ok(SearchResults {
                tracks: vec![track("spotify:track:hit", "Hit")],
                ..Default::default()
            }),
        );

        h.press_and_run(KeyCode::Char('/')).await;
        h.type_str("rock").await;
        h.settle().await;
        h.fake.clear_calls();
        h.press_and_run(KeyCode::Enter).await;
        h.settle().await;

        let calls = h.fake.calls();
        assert!(
            calls.iter().any(|c| matches!(c, Call::PlayUris(uris) if uris == &["spotify:track:hit".to_string()])),
            "expected PlayUris call, got: {calls:?}"
        );
        // And we're back in NowPlaying.
        assert_eq!(h.mode_name().await, "now_playing");
    }
}
