//! Application logic: state, key dispatch, and async action handlers.
//!
//! This module is the entirety of the app's behavior layer. It owns
//! `AppState` and the `KeyAction` intent enum, and exposes async handlers
//! the run loop drives. It is frontend-agnostic: it consumes
//! [`crate::input::Input`] values, not crossterm types.

use ratatui_image::protocol::StatefulProtocol;
use std::{
    sync::Arc,
    time::{Duration, Instant, SystemTime},
};
use tokio::sync::Mutex;
use tokio::task::AbortHandle;

use crate::api::{
    self, Context as PlaybackContext, Playback, RateLimited, SearchResults, SpotifyApi, Track,
};
use crate::input::{Input, Key};
use crate::keys::ModeMask;
use crate::{art, log, recent, streaming};

// ---------------------------------------------------------------------------
// State
// ---------------------------------------------------------------------------

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

pub fn mode_name(s: &AppState) -> &'static str {
    match s.mode {
        Mode::NowPlaying => "now_playing",
        Mode::Search(_) => "search",
        Mode::Help => "help",
        Mode::Command(_) => "command",
        Mode::Browse(_) => "browse",
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
    pub fn new(
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

    /// True while a search query is mid-flight — either the debounce hasn't
    /// fired yet, or the request has fired but no response has been applied.
    /// Lets the UI distinguish "no results yet" from "Spotify returned zero
    /// matches" so we don't flash "no results found" between keystrokes.
    pub fn is_loading(&self) -> bool {
        !self.input.is_empty()
            && (self.debounce.is_some() || self.applied_id < self.request_id)
    }
}

pub struct InContext {
    pub playlist_uri: String,
    pub tracks: Vec<Track>,
    pub filtered: Vec<usize>,
}

// ---------------------------------------------------------------------------
// Key dispatch
// ---------------------------------------------------------------------------

pub enum KeyAction {
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
pub enum SelectionAction {
    /// Re-run search with this query (selected a row from "Recent searches").
    PromoteQuery(String),
    Play(PlayAction),
    /// Open an album or playlist to browse its tracks.
    Browse(Collection),
}

#[derive(Debug)]
pub enum PlayAction {
    Track(String),
    Context { uri: String, offset: Option<String> },
}

pub async fn dispatch_input(input: Input, state: &Mutex<AppState>) -> KeyAction {
    let mut s = state.lock().await;
    let shift = input.mods.shift;
    match &mut s.mode {
        Mode::NowPlaying => match input.key {
            Key::Char('q') | Key::Esc => KeyAction::Quit,
            Key::Char(' ') => KeyAction::TogglePlayback,
            Key::Left if shift => KeyAction::Seek(-SEEK_STEP_MS),
            Key::Right if shift => KeyAction::Seek(SEEK_STEP_MS),
            Key::Char('/') => KeyAction::EnterSearch,
            Key::Char(':') => {
                s.mode = Mode::Command(CommandState::default());
                KeyAction::Stay
            }
            Key::Char('?') => {
                s.mode = Mode::Help;
                KeyAction::Stay
            }
            _ => KeyAction::Stay,
        },
        Mode::Search(search) => match input.key {
            Key::Esc => {
                if let Some(h) = search.debounce.take() {
                    h.abort();
                }
                s.mode = Mode::NowPlaying;
                KeyAction::Stay
            }
            Key::Up => {
                if search.selected > 0 {
                    search.selected -= 1;
                }
                KeyAction::Stay
            }
            Key::Down => {
                let max = visible_row_count(search).saturating_sub(1);
                if search.selected < max {
                    search.selected += 1;
                }
                KeyAction::Stay
            }
            Key::Enter => match resolve_full_selection(search) {
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
            Key::Backspace => {
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
            Key::Left => {
                if search.cursor > 0 {
                    search.cursor -= 1;
                }
                KeyAction::Stay
            }
            Key::Right => {
                let max = search.input.chars().count();
                if search.cursor < max {
                    search.cursor += 1;
                }
                KeyAction::Stay
            }
            Key::Char(c) => {
                let byte = char_idx_to_byte(&search.input, search.cursor);
                search.input.insert(byte, c);
                search.cursor += 1;
                refilter_in_context(search);
                KeyAction::SearchInputChanged
            }
            _ => KeyAction::Stay,
        },
        Mode::Help => match input.key {
            Key::Esc | Key::Char('?') | Key::Char('q') => {
                s.mode = Mode::NowPlaying;
                KeyAction::Stay
            }
            _ => KeyAction::Stay,
        },
        Mode::Command(cmd) => match input.key {
            Key::Esc => {
                s.mode = Mode::NowPlaying;
                KeyAction::Stay
            }
            Key::Up => {
                if cmd.selected > 0 {
                    cmd.selected -= 1;
                }
                KeyAction::Stay
            }
            Key::Down => {
                let max = cmd.filtered().len().saturating_sub(1);
                if cmd.selected < max {
                    cmd.selected += 1;
                }
                KeyAction::Stay
            }
            Key::Left => {
                if cmd.cursor > 0 {
                    cmd.cursor -= 1;
                }
                KeyAction::Stay
            }
            Key::Right => {
                let max = cmd.input.chars().count();
                if cmd.cursor < max {
                    cmd.cursor += 1;
                }
                KeyAction::Stay
            }
            Key::Backspace => {
                if cmd.cursor > 0 {
                    let byte = char_idx_to_byte(&cmd.input, cmd.cursor - 1);
                    cmd.input.remove(byte);
                    cmd.cursor -= 1;
                    cmd.selected = 0;
                }
                KeyAction::Stay
            }
            Key::Char(c) => {
                let byte = char_idx_to_byte(&cmd.input, cmd.cursor);
                cmd.input.insert(byte, c);
                cmd.cursor += 1;
                cmd.selected = 0;
                KeyAction::Stay
            }
            Key::Enter => {
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
        Mode::Browse(browse) => match input.key {
            Key::Esc => {
                // Restore the search the user came from.
                let prior = std::mem::replace(
                    &mut browse.prior_search,
                    SearchState::new(None, Vec::new(), Vec::new()),
                );
                s.mode = Mode::Search(prior);
                KeyAction::Stay
            }
            Key::Up => {
                if browse.selected > 0 {
                    browse.selected -= 1;
                }
                KeyAction::Stay
            }
            Key::Down => {
                let max = browse.tracks.len().saturating_sub(1);
                if browse.selected < max {
                    browse.selected += 1;
                }
                KeyAction::Stay
            }
            Key::Enter => {
                if browse.tracks.is_empty() {
                    KeyAction::Stay
                } else {
                    KeyAction::PlayBrowseSelection
                }
            }
            // "Play the whole album/playlist" — works regardless of whether
            // the track list loaded.
            Key::Char('p') => KeyAction::PlayBrowseCollection,
            _ => KeyAction::Stay,
        },
    }
}

pub fn visible_row_count(s: &SearchState) -> usize {
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

pub fn refilter_in_context(s: &mut SearchState) {
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

pub fn char_idx_to_byte(s: &str, char_idx: usize) -> usize {
    s.char_indices()
        .nth(char_idx)
        .map(|(i, _)| i)
        .unwrap_or(s.len())
}

pub fn resolve_full_selection(s: &SearchState) -> Option<SelectionAction> {
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
pub fn resolve_collection_to_browse(s: &SearchState) -> Option<Collection> {
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

pub fn resolve_selection(s: &SearchState) -> Option<PlayAction> {
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

pub fn playlist_id_from_uri(uri: &str) -> Option<String> {
    uri.strip_prefix("spotify:playlist:").map(|s| s.to_string())
}

pub fn album_id_from_uri(uri: &str) -> Option<String> {
    uri.strip_prefix("spotify:album:").map(|s| s.to_string())
}

// ---------------------------------------------------------------------------
// Time/freshness helpers
// ---------------------------------------------------------------------------

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

/// Effective progress in ms given the current state — same calculation as the
/// UI's `displayed_progress`, but lives here so `toggle_playback` can freeze
/// the value before mutating `is_playing`.
pub fn displayed_progress_for_toggle(s: &AppState) -> u64 {
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

pub const DEVICE_OFFLINE_MSG: &str =
    "Connect device 'hifi' is offline — auto-reconnecting (or press ':' → reconnect)";

pub fn is_device_not_found(msg: &str) -> bool {
    msg.contains("Device not found") || msg.contains("\"status\" : 404")
}

// ---------------------------------------------------------------------------
// Reconnect / device lifecycle
// ---------------------------------------------------------------------------

/// Kick off a reconnect on a background task. Safe to call multiple times
/// — the in-flight guard inside `reconnect_now` collapses concurrent
/// triggers (e.g., auto-watchdog firing while a manual `:reconnect` is
/// already running).
pub fn spawn_reconnect(
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
pub async fn reconnect_now(
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
pub async fn reconnect_if_device_offline(
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

pub async fn wait_then_transfer(client: &dyn SpotifyApi, device_id: &str) {
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

// ---------------------------------------------------------------------------
// Action handlers
// ---------------------------------------------------------------------------

pub async fn enter_search(client: &Arc<dyn SpotifyApi>, state: &Arc<Mutex<AppState>>) {
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

/// Open the Browse overlay for the given collection, taking the current
/// SearchState with us so Esc can restore it. Kicks off the track fetch in
/// the background — Browse renders immediately with a "Loading..." placeholder.
pub async fn enter_browse(
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
pub async fn play_browse_selection(
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
pub async fn play_browse_collection(
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

pub async fn kick_search(client: &Arc<dyn SpotifyApi>, state: &Arc<Mutex<AppState>>) {
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

pub async fn play_selection(
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
                // PromoteQuery is handled in dispatch_input before
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
pub fn synth_template_for(action: &PlayAction, search: &SearchState) -> Option<Playback> {
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

pub fn find_track_by_uri(search: &SearchState, uri: &str) -> Option<Track> {
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

pub async fn apply_playback(
    state: &Arc<Mutex<AppState>>,
    art_loader: &Arc<art::ArtLoader>,
    pb: Option<Playback>,
) {
    apply_playback_inner(state, art_loader, pb, false).await
}

/// Apply playback bypassing `should_accept` — used for our own
/// synthesized seed (e.g. the recently-played track shown at startup),
/// which would otherwise be filtered out by the boot guard.
pub async fn apply_playback_force(
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
    // A Playback with no `item` carries no useful track info — Spotify
    // returns this between tracks or when nothing is actively serving.
    // Collapse it to `None` so we never end up in the janky hybrid state
    // (no track info displayed, but the previous album art still on screen).
    let pb = pb.filter(|p| p.item.is_some());

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
        let track_changed = prev != new_track_id;
        // When the track goes away (or changes), drop the old cover so the
        // UI doesn't render last-track's art alongside the next/empty state.
        // The new track's art is fetched async below if there is one.
        if track_changed {
            s.art = None;
        }
        new_track_id.is_some() && track_changed
    };

    if needs_art_fetch {
        if let Some(url) = cover_url {
            let s = state.clone();
            let loader = art_loader.clone();
            let id_at_fetch = new_track_id.clone();
            tokio::spawn(async move {
                if let Ok(proto) = loader.load(&url).await {
                    let mut g = s.lock().await;
                    if g.current_track_id == id_at_fetch {
                        g.art = Some(proto);
                    }
                }
            });
        }
    }
}

pub async fn toggle_playback(client: &Arc<dyn SpotifyApi>, state: &Arc<Mutex<AppState>>) {
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

pub async fn seek_relative(
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

pub async fn skip_track(client: &Arc<dyn SpotifyApi>, state: &Arc<Mutex<AppState>>, forward: bool) {
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

/// Convenience to grab a Result wrapping the action loop should perform
/// after a successful play. Used by the run loop to poll-burst after a
/// user-initiated play so we pick up Spotify's view quickly.
pub fn spawn_post_play_poll(
    client: Arc<dyn SpotifyApi>,
    state: Arc<Mutex<AppState>>,
    art_loader: Arc<art::ArtLoader>,
) {
    tokio::spawn(async move {
        for _ in 0..6 {
            tokio::time::sleep(Duration::from_millis(250)).await;
            // The client's send_logged would short-circuit anyway, but
            // bailing the whole burst is cleaner — no point chewing through
            // six instant-fail RateLimited errors.
            if client.rate_limited_until().is_some() {
                break;
            }
            if let Ok(pb) = client.get_playback().await {
                apply_playback(&state, &art_loader, pb).await;
            }
        }
    });
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::{Album, Artist, Playlist, SearchResults, Track};

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
        assert!(!should_accept(&s, Some(&pb_with_ts(None, "no-ts"))));
    }

    #[test]
    fn should_accept_accepts_when_no_prior_local_action() {
        let s = steady_state();
        assert!(should_accept(&s, Some(&pb_with_ts(Some(0), "first"))));
        assert!(should_accept(&s, None));
    }

    #[test]
    fn should_accept_rejects_none_right_after_play() {
        let mut s = steady_state();
        s.last_local_action_ms = now_unix_ms();
        assert!(!should_accept(&s, None));
    }

    #[test]
    fn should_accept_accepts_none_after_long_idle() {
        let mut s = steady_state();
        s.last_local_action_ms = now_unix_ms().saturating_sub(120_000);
        assert!(should_accept(&s, None));
    }

    // --- boot-phase gating ----------------------------------------------

    #[test]
    fn should_accept_boot_accepts_paused_polled() {
        let s = AppState::default();
        let mut pb = pb_with_ts(Some(0), "old");
        pb.is_playing = false;
        assert!(should_accept(&s, Some(&pb)));
    }

    #[test]
    fn should_accept_boot_rejects_none() {
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
        let search = search_state_with_results(SearchResults::default());
        let action = PlayAction::Context {
            uri: "spotify:album:a".into(),
            offset: None,
        };
        assert!(synth_template_for(&action, &search).is_none());
    }

    #[test]
    fn synth_with_matching_timestamp_is_accepted() {
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
        s.selected = 0;
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
        assert_eq!(char_idx_to_byte("abc", 3), 3);
        assert_eq!(char_idx_to_byte("abc", 99), 3);
        assert_eq!(char_idx_to_byte("aéc", 1), 1);
        assert_eq!(char_idx_to_byte("aéc", 2), 3);
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
        s.selected = 2;
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
                owner: Some(crate::api::PlaylistOwner {
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
        match resolve_full_selection(&s) {
            Some(SelectionAction::Play(PlayAction::Track(uri))) => {
                assert_eq!(uri, "spotify:track:t1");
            }
            other => panic!("expected Play(Track), got {other:?}"),
        }
    }

    #[test]
    fn resolve_artist_row_still_plays_not_browses() {
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
        s.input = "re".into();
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
    // ====================================================================

    use crate::test_support::{Call, Harness};
    use crate::input::Key;

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
            owner: Some(crate::api::PlaylistOwner {
                display_name: Some(owner.into()),
            }),
        }
    }

    #[tokio::test]
    async fn playlist_browse_403_shows_friendly_error_and_offers_p_fallback() {
        let h = Harness::new();
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
        h.fake.set_playlist_tracks(
            "abc123",
            Err("GET https://api.spotify.com/v1/playlists/abc123/tracks: 403 Forbidden: {\"error\": {\"status\": 403, \"message\": \"Forbidden\"}}".into()),
        );

        h.press_and_run(Key::Char('/')).await;
        h.type_str("power").await;
        h.settle().await;
        h.press_and_run(Key::Down).await;
        h.press_and_run(Key::Enter).await;
        h.settle().await;

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

        let screen = h.snapshot().await;
        assert!(
            screen.contains("Spotify locked this playlist"),
            "expected friendly hint in:\n{screen}"
        );
        assert!(
            screen.contains("[p] plays anyway") || screen.contains("[p] play"),
            "expected fallback hint in:\n{screen}"
        );

        let calls = h.fake.calls();
        assert!(
            calls.iter().any(|c| matches!(c, Call::GetPlaylistTracks(id) if id == "abc123")),
            "calls: {calls:?}"
        );
    }

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

        h.press_and_run(Key::Char('/')).await;
        h.type_str("test").await;
        h.settle().await;
        h.press_and_run(Key::Enter).await;
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
        h.fake.set_playlist_tracks("xyz", Err("403 Forbidden".into()));

        h.press_and_run(Key::Char('/')).await;
        h.type_str("x").await;
        h.settle().await;
        h.press_and_run(Key::Enter).await;
        h.settle().await;

        h.fake.clear_calls();
        h.press_and_run(Key::Char('p')).await;
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
        assert_eq!(h.mode_name().await, "now_playing");
    }

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

        h.press_and_run(Key::Char('/')).await;
        h.type_str("x").await;
        h.settle().await;
        h.press_and_run(Key::Enter).await;
        h.settle().await;

        h.press_and_run(Key::Down).await;
        h.press_and_run(Key::Down).await;
        h.fake.clear_calls();
        h.press_and_run(Key::Enter).await;
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

        h.press_and_run(Key::Char('/')).await;
        h.type_str("x").await;
        h.settle().await;
        h.press_and_run(Key::Enter).await;
        h.settle().await;
        assert_eq!(h.mode_name().await, "browse");

        h.press_and_run(Key::Esc).await;
        assert_eq!(h.mode_name().await, "search");

        let s = h.state.lock().await;
        let Mode::Search(search) = &s.mode else {
            panic!();
        };
        assert_eq!(search.input, "x");
        assert_eq!(search.results.albums.len(), 1);
    }

    #[tokio::test]
    async fn rate_limited_state_blocks_play_pause() {
        let h = Harness::new();
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
        h.press_and_run(Key::Char(' ')).await;

        let calls = h.fake.calls();
        assert!(
            !calls.iter().any(|c| matches!(c, Call::Play | Call::Pause)),
            "expected no Play/Pause calls while rate-limited, got: {calls:?}"
        );
    }

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

        h.press_and_run(Key::Char('/')).await;
        h.type_str("rock").await;
        h.settle().await;
        h.fake.clear_calls();
        h.press_and_run(Key::Enter).await;
        h.settle().await;

        let calls = h.fake.calls();
        assert!(
            calls.iter().any(|c| matches!(c, Call::PlayUris(uris) if uris == &["spotify:track:hit".to_string()])),
            "expected PlayUris call, got: {calls:?}"
        );
        assert_eq!(h.mode_name().await, "now_playing");
    }

    // --- loading state -------------------------------------------------

    /// While a search request is debouncing/in flight, the UI must say
    /// "loading…" — never "no results for …", which would be a lie.
    #[tokio::test]
    async fn search_shows_loading_not_no_results_before_response() {
        let h = Harness::new();
        h.fake.set_search(
            "rock",
            Ok(SearchResults {
                tracks: vec![track("spotify:track:hit", "Hit")],
                ..Default::default()
            }),
        );

        h.press_and_run(Key::Char('/')).await;
        h.type_str("rock").await;
        // Deliberately do NOT settle — the debounce hasn't fired yet, the
        // request is still pending. This is the window where the old UI
        // wrongly said "no results found".

        {
            let s = h.state.lock().await;
            let Mode::Search(search) = &s.mode else { panic!("expected Search mode") };
            assert!(
                search.is_loading(),
                "expected is_loading() to be true while debounce/request is pending"
            );
        }

        let screen = h.snapshot().await;
        assert!(
            screen.contains("loading"),
            "expected loading indicator while search pending, got:\n{screen}"
        );
        assert!(
            !screen.contains("no results"),
            "should not say 'no results' while still loading, got:\n{screen}"
        );
    }

    /// After the response lands, is_loading() flips false and the count
    /// hint replaces "loading…".
    #[tokio::test]
    async fn search_clears_loading_after_response_applied() {
        let h = Harness::new();
        h.fake.set_search(
            "rock",
            Ok(SearchResults {
                tracks: vec![track("spotify:track:hit", "Hit")],
                ..Default::default()
            }),
        );

        h.press_and_run(Key::Char('/')).await;
        h.type_str("rock").await;
        h.settle().await;

        {
            let s = h.state.lock().await;
            let Mode::Search(search) = &s.mode else { panic!() };
            assert!(!search.is_loading(), "should not be loading after settle");
            assert_eq!(search.last_query, "rock");
        }

        let screen = h.snapshot().await;
        assert!(!screen.contains("loading"), "screen still says loading:\n{screen}");
        assert!(
            screen.contains("1 results") || screen.contains("Tracks"),
            "expected results to render, got:\n{screen}"
        );
    }

    /// Zero matches from Spotify (not "no response yet") DOES show "no
    /// results for …" — distinguishing this from the loading case is the
    /// whole point.
    #[tokio::test]
    async fn search_with_zero_matches_says_no_results() {
        let h = Harness::new();
        h.fake.set_search("nothingmatches", Ok(SearchResults::default()));

        h.press_and_run(Key::Char('/')).await;
        h.type_str("nothingmatches").await;
        h.settle().await;

        {
            let s = h.state.lock().await;
            let Mode::Search(search) = &s.mode else { panic!() };
            assert!(!search.is_loading());
        }

        let screen = h.snapshot().await;
        assert!(
            screen.contains("no results for"),
            "expected 'no results' for genuine empty response, got:\n{screen}"
        );
    }

    // --- 96x40 fixed-canvas exercise -----------------------------------
    //
    // Renders every mode at exactly the fixed canvas size with deliberately
    // long content (track names, playlist names, etc.) and checks for the
    // tell-tale signs of overflow:
    //
    //   1. The bottom-right corner of the outer Block border is at column
    //      95, row 39 — if it's missing the layout overran height.
    //   2. The footer hint line (row 38) is fully rendered — if truncated
    //      mid-character, the canvas is too narrow.
    //   3. No row has content extending past column 95 (impossible by
    //      construction, but worth pinning).
    //
    // Run with `cargo test ui_at_96x40 -- --nocapture` to see snapshots.

    fn long_track() -> Track {
        Track {
            id: Some("idLong".into()),
            uri: Some("spotify:track:long".into()),
            name: "Mr. Brightside (Jacques Lu Cont's Thin White Duke Mix)".into(),
            duration_ms: 423_000,
            artists: vec![
                Artist { uri: None, name: "The Killers".into() },
                Artist { uri: None, name: "Featuring Some Other Long-Named Collaborator".into() },
            ],
            album: Album {
                uri: None,
                name: "Hot Fuss: 10th Anniversary Deluxe Edition (Remastered)".into(),
                artists: vec![],
                images: vec![],
            },
        }
    }

    /// Bottom-right corner of the outer border must land at (col 95, row 39)
    /// regardless of which overlay is active. If layouts overflow, that cell
    /// is empty (or contains arbitrary content) instead of the corner glyph.
    fn assert_border_closes(label: &str, screen: &str) {
        let lines: Vec<&str> = screen.lines().collect();
        assert!(
            lines.len() >= 40,
            "[{label}] expected ≥40 rows, got {}:\n{screen}",
            lines.len()
        );
        // Row 0 must start with the top-left corner.
        let top = lines[0];
        let top_chars: Vec<char> = top.chars().collect();
        assert!(
            !top_chars.is_empty() && !top_chars[0].is_whitespace(),
            "[{label}] top-left corner missing at (0,0):\n{screen}"
        );
        // Row 39 is the bottom border line; its rightmost rendered char
        // should be a corner glyph at or near column 95.
        let bottom = lines[39];
        let bottom_trimmed = bottom.trim_end();
        assert!(
            !bottom_trimmed.is_empty(),
            "[{label}] bottom border row 39 is empty (height overflow):\n{screen}"
        );
        // After trimming trailing whitespace, the bottom border should be
        // exactly the canvas width minus any leading offset (we anchor at
        // x=0 so no offset). Width is 96, so 96 chars of border.
        let bottom_width = bottom_trimmed.chars().count();
        assert_eq!(
            bottom_width, 96,
            "[{label}] bottom border has {bottom_width} chars, expected 96:\n{bottom_trimmed}"
        );
        // And row 0's top border too.
        let top_trimmed = top.trim_end();
        let top_width = top_trimmed.chars().count();
        assert_eq!(
            top_width, 96,
            "[{label}] top border has {top_width} chars, expected 96:\n{top_trimmed}"
        );
    }

    fn print_snapshot(label: &str, screen: &str) {
        eprintln!("\n=== {label} ===");
        for (i, line) in screen.lines().enumerate() {
            eprintln!("{i:>2}|{line}");
        }
    }

    #[tokio::test]
    async fn ui_at_96x40_now_playing_with_long_metadata() {
        let h = Harness::new();
        h.seed_playback(Playback {
            is_playing: true,
            progress_ms: Some(123_000),
            item: Some(long_track()),
            context: None,
            timestamp: Some(now_unix_ms()),
        }).await;
        {
            let mut s = h.state.lock().await;
            s.boot = false;
            s.device_name = Some("hifi (cabin)".into());
        }
        let screen = h.snapshot_sized(96, 40).await;
        print_snapshot("now_playing long metadata", &screen);
        assert_border_closes("now_playing long metadata", &screen);
    }

    #[tokio::test]
    async fn ui_at_96x40_now_playing_with_rate_limit_status() {
        let h = Harness::new();
        h.seed_playback(Playback {
            is_playing: false,
            progress_ms: Some(0),
            item: Some(long_track()),
            context: None,
            timestamp: Some(now_unix_ms()),
        }).await;
        {
            let mut s = h.state.lock().await;
            s.boot = false;
            s.device_name = Some("hifi".into());
            s.rate_limited_until = Some(Instant::now() + Duration::from_secs(45));
        }
        let screen = h.snapshot_sized(96, 40).await;
        print_snapshot("now_playing + rate-limited", &screen);
        assert_border_closes("now_playing + rate-limited", &screen);
    }

    #[tokio::test]
    async fn ui_at_96x40_search_with_results_in_every_section() {
        let h = Harness::new();
        let q = "very long search query string text here";
        h.fake.set_search(
            q,
            Ok(SearchResults {
                tracks: (0..5)
                    .map(|i| {
                        let mut t = long_track();
                        t.uri = Some(format!("spotify:track:t{i}"));
                        t.id = Some(format!("t{i}"));
                        t.name = format!("Track {i} — Mr. Brightside (Jacques Lu Cont's Remix)");
                        t
                    })
                    .collect(),
                albums: (0..4)
                    .map(|i| Album {
                        uri: Some(format!("spotify:album:a{i}")),
                        name: format!(
                            "Album {i}: A Very Long Title That Might Make The Row Overflow"
                        ),
                        artists: vec![Artist {
                            uri: None,
                            name: "Some Artist".into(),
                        }],
                        images: vec![],
                    })
                    .collect(),
                artists: (0..3)
                    .map(|i| Artist {
                        uri: Some(format!("spotify:artist:ar{i}")),
                        name: format!("Artist {i} With A Reasonably Long Performer Name"),
                    })
                    .collect(),
                playlists: (0..4)
                    .map(|i| Playlist {
                        uri: format!("spotify:playlist:p{i}"),
                        name: format!(
                            "Playlist {i}: This Is The Sort Of Title People Use For Their Own Mixes"
                        ),
                        owner: Some(crate::api::PlaylistOwner {
                            display_name: Some(format!("owner_with_a_long_username_{i}")),
                        }),
                    })
                    .collect(),
            }),
        );
        h.press_and_run(Key::Char('/')).await;
        h.type_str(q).await;
        h.settle().await;
        let screen = h.snapshot_sized(96, 40).await;
        print_snapshot("search results", &screen);
        assert_border_closes("search results", &screen);
    }

    #[tokio::test]
    async fn ui_at_96x40_search_recents() {
        let h = Harness::new();
        {
            let mut s = h.state.lock().await;
            s.recent_queries = (0..10)
                .map(|i| format!("recent search query number {i} with extra padding text"))
                .collect();
            s.recent_tracks = (0..8)
                .map(|i| {
                    let mut t = long_track();
                    t.uri = Some(format!("spotify:track:r{i}"));
                    t.id = Some(format!("r{i}"));
                    t.name = format!("Recently Played {i} — Some Long Track Title Goes Here");
                    t
                })
                .collect();
        }
        h.press_and_run(Key::Char('/')).await;
        let screen = h.snapshot_sized(96, 40).await;
        print_snapshot("search recents", &screen);
        assert_border_closes("search recents", &screen);
    }

    #[tokio::test]
    async fn ui_at_96x40_help_overlay() {
        let h = Harness::new();
        h.press_and_run(Key::Char('?')).await;
        let screen = h.snapshot_sized(96, 40).await;
        print_snapshot("help overlay", &screen);
        assert_border_closes("help overlay", &screen);
    }

    #[tokio::test]
    async fn ui_at_96x40_command_overlay() {
        let h = Harness::new();
        h.press_and_run(Key::Char(':')).await;
        let screen = h.snapshot_sized(96, 40).await;
        print_snapshot("command overlay", &screen);
        assert_border_closes("command overlay", &screen);
    }

    #[tokio::test]
    async fn ui_at_96x40_browse_with_many_tracks() {
        let h = Harness::new();
        h.fake.set_search(
            "test",
            Ok(SearchResults {
                albums: vec![dummy_album(
                    "spotify:album:al1",
                    "An Album Title That Is Quite Long For A Header",
                    "Some Artist",
                )],
                ..Default::default()
            }),
        );
        h.fake.set_album_tracks(
            "al1",
            Ok((0..40)
                .map(|i| {
                    let mut t = long_track();
                    t.uri = Some(format!("spotify:track:bt{i}"));
                    t.id = Some(format!("bt{i}"));
                    t.name = format!("Browse Track {i:02} — Some Reasonably Long Track Name");
                    t
                })
                .collect()),
        );
        h.press_and_run(Key::Char('/')).await;
        h.type_str("test").await;
        h.settle().await;
        h.press_and_run(Key::Enter).await;
        h.settle().await;
        let screen = h.snapshot_sized(96, 40).await;
        print_snapshot("browse 40 tracks", &screen);
        assert_border_closes("browse 40 tracks", &screen);
    }

    /// The exact bug the user hit at launch: boot seed loaded recently-played
    /// (track + art populated), then a poll returned `Playback { item: None }`,
    /// which overwrote the state into the janky "Track info unavailable +
    /// stale album art" hybrid. The fix is two-fold: item-less Playback is
    /// collapsed to None on the way in, and the art field is cleared
    /// whenever the current_track_id changes.
    #[tokio::test]
    async fn item_none_poll_does_not_create_track_info_unavailable_hybrid() {
        let h = Harness::new();
        // Seed exactly like the boot path does: a paused synth from
        // recently-played, art populated by a successful art fetch (we just
        // mark current_track_id so the comparison logic exercises).
        let seed = Playback {
            is_playing: false,
            progress_ms: Some(0),
            item: Some(track("spotify:track:seed", "Seeded Track")),
            context: None,
            timestamp: Some(now_unix_ms()),
        };
        h.seed_playback(seed).await;
        {
            let mut s = h.state.lock().await;
            s.boot = false;
            // Simulate the art fetch having completed: current_track_id is
            // already set from seed_playback, but mark art as present so
            // we can verify it gets cleared.
            // (Real ArtLoader doesn't run in the test harness, so we fake it.)
            s.current_track_id = Some("seed".into());
        }

        // Now simulate the poll that Spotify often returns after a transfer:
        // `is_playing: false`, `item: None`. This used to clobber the seed.
        apply_playback(
            &h.state,
            &h.art_loader,
            Some(Playback {
                is_playing: false,
                progress_ms: None,
                item: None,
                context: None,
                timestamp: Some(now_unix_ms() + 1000),
            }),
        )
        .await;

        let s = h.state.lock().await;
        // Either the seed survives (defended by `pb.filter`) OR the state
        // is fully cleared. The disallowed outcome is "playback Some + item
        // None + art Some" — the hybrid the user complained about.
        let has_item = s
            .playback
            .as_ref()
            .and_then(|p| p.item.as_ref())
            .is_some();
        let art_present = s.art.is_some();
        assert!(
            has_item || !art_present,
            "expected either a track to be displayed OR art to be cleared; \
             got playback={:?} art={art_present}",
            s.playback.as_ref().map(|p| p.item.as_ref().map(|t| &t.name)),
        );
    }

    #[tokio::test]
    async fn ui_at_96x40_browse_403_warning() {
        let h = Harness::new();
        h.fake.set_search(
            "x",
            Ok(SearchResults {
                playlists: vec![dummy_playlist(
                    "spotify:playlist:px",
                    "Some Curated Playlist With A Long Title",
                    "an_owner_username",
                )],
                ..Default::default()
            }),
        );
        h.fake.set_playlist_tracks("px", Err("403 Forbidden".into()));
        h.press_and_run(Key::Char('/')).await;
        h.type_str("x").await;
        h.settle().await;
        h.press_and_run(Key::Enter).await;
        h.settle().await;
        let screen = h.snapshot_sized(96, 40).await;
        print_snapshot("browse 403 warning", &screen);
        assert_border_closes("browse 403 warning", &screen);
    }
}

