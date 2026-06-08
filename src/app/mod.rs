//! Application logic: state, key dispatch, and async action handlers.
//!
//! `mod.rs` owns `AppState`, the state types, and the async action handlers
//! (reconnect, playback, library, devices). Two submodules carry the rest and
//! are re-exported, so external callers still use `crate::app::*`:
//!   - [`dispatch`] — the `KeyAction` enum + pure/sync `dispatch_input` routing.
//!   - [`freshness`] — the boot/staleness `should_accept` gate + time helpers.
//!
//! It is frontend-agnostic: it consumes [`crate::input::Input`] values, not
//! crossterm types.

mod dispatch;
mod freshness;
pub use dispatch::*;
pub use freshness::*;

use std::{
    sync::Arc,
    time::{Duration, Instant},
};
use tokio::sync::Mutex;
use tokio::task::AbortHandle;

use crate::api::{
    self, Album, Artist, Context as PlaybackContext, Device, Playback, Playlist, RateLimited,
    SearchResults, SpotifyApi, Track,
};
use crate::keys::ModeMask;
use crate::{log, recent, streaming};

// ---------------------------------------------------------------------------
// State
// ---------------------------------------------------------------------------

pub struct AppState {
    pub playback: Option<Playback>,
    pub last_poll: Option<Instant>,
    pub error: Option<String>,
    pub rate_limited_until: Option<Instant>,
    /// What album art the head should display, as plain data (no ratatui
    /// types). The TUI head owns the decoded image cache (see
    /// [`crate::art::ArtCache`]) and fetches against this; an HTTP head could
    /// serve the URL directly. Set on track change, cleared when nothing
    /// useful is playing.
    pub art_request: Option<ArtRequest>,
    pub current_track_id: Option<String>,
    pub device_name: Option<String>,
    /// The persistent top-level tab the user is on (Now Playing / Search /
    /// Library). Switching tabs never destroys a tab's state — Search keeps
    /// its query, Library keeps its loaded sections.
    pub tab: Tab,
    /// A transient surface stacked on top of the active tab (help, command
    /// palette, device picker, or a browsed collection). `None` = just the
    /// tab. Closing an overlay (Esc) reveals the tab underneath unchanged.
    pub overlay: Option<Overlay>,
    /// Search tab state. Persistent so tabbing away and back keeps the query.
    pub search: SearchState,
    /// Library tab state — four lazily-loaded sections.
    pub library: LibraryState,
    /// Vertical focus within the current tab (content rows vs. the tab strip).
    pub focus: Focus,
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
    /// Upcoming tracks from /me/player/queue, shown as "Up Next" on the Now
    /// Playing screen. Fetched on demand (on entering the tab / after a skip),
    /// never on a timer — so it can be slightly stale until the tab is
    /// re-entered. Empty when nothing is queued or nothing is playing.
    pub queue: Vec<Track>,
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
    /// Transient success/info message with an expiry. Used by commands
    /// whose effect is invisible from the now-playing view (e.g. `:like`).
    /// Status line renders it green; expiry is checked lazily on read.
    pub notice: Option<(String, Instant)>,
}

impl Default for AppState {
    fn default() -> Self {
        Self {
            playback: None,
            last_poll: None,
            error: None,
            rate_limited_until: None,
            art_request: None,
            current_track_id: None,
            device_name: None,
            tab: Tab::default(),
            overlay: None,
            search: SearchState::new(None, Vec::new(), Vec::new()),
            library: LibraryState::default(),
            focus: Focus::default(),
            last_local_action_ms: 0,
            device_present: None,
            recent_tracks: Vec::new(),
            queue: Vec::new(),
            recent_queries: Vec::new(),
            boot: true,
            streaming_failed: None,
            streaming: None,
            reconnecting: false,
            notice: None,
        }
    }
}

/// Frontend-neutral "show this cover" signal. The core sets it; the head
/// decides how to render (TUI decodes into [`crate::art::ArtCache`], an HTTP
/// head could proxy the URL). Plain data so `AppState` imports nothing from
/// any UI framework.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ArtRequest {
    pub track_id: String,
    pub url: String,
}

/// The persistent top-level tabs, shown as a strip across the top of every
/// screen (mirrors `design/mockups.html`).
#[derive(Default, Clone, Copy, PartialEq, Eq, Debug)]
pub enum Tab {
    #[default]
    NowPlaying,
    Search,
    Library,
}

impl Tab {
    pub const ALL: &'static [Tab] = &[Tab::NowPlaying, Tab::Search, Tab::Library];

    pub fn next(self) -> Tab {
        let i = Self::ALL.iter().position(|t| *t == self).unwrap_or(0);
        Self::ALL[(i + 1) % Self::ALL.len()]
    }

    pub fn prev(self) -> Tab {
        let i = Self::ALL.iter().position(|t| *t == self).unwrap_or(0);
        Self::ALL[(i + Self::ALL.len() - 1) % Self::ALL.len()]
    }

    pub fn name(self) -> &'static str {
        match self {
            Tab::NowPlaying => "now_playing",
            Tab::Search => "search",
            Tab::Library => "library",
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Tab::NowPlaying => "Now Playing",
            Tab::Search => "Search",
            Tab::Library => "Library",
        }
    }

    pub fn mask(self) -> ModeMask {
        match self {
            Tab::NowPlaying => ModeMask::NOW_PLAYING,
            Tab::Search => ModeMask::SEARCH,
            Tab::Library => ModeMask::LIBRARY,
        }
    }
}

/// Where vertical (up/down) navigation currently sits within a tab. Arrowing
/// up past the first content row moves focus onto the top tab strip, where
/// left/right switch tabs and down/enter drop back into the content.
#[derive(Default, Clone, Copy, PartialEq, Eq, Debug)]
pub enum Focus {
    /// On the top tab strip — left/right cycle tabs, down/enter enters content.
    Tabs,
    /// Inside the active tab's body — up/down walk its rows.
    #[default]
    Content,
}

/// A transient surface drawn over the active tab. Help/Command/Devices render
/// as centered boxes; Browse fills the body. Each carries its own state inline.
pub enum Overlay {
    Help,
    Command(CommandState),
    Devices(DevicesState),
    Browse(BrowseState),
}

impl Overlay {
    pub fn name(&self) -> &'static str {
        match self {
            Overlay::Help => "help",
            Overlay::Command(_) => "command",
            Overlay::Devices(_) => "devices",
            Overlay::Browse(_) => "browse",
        }
    }

    pub fn mask(&self) -> ModeMask {
        match self {
            Overlay::Help => ModeMask::HELP,
            Overlay::Command(_) => ModeMask::COMMAND,
            Overlay::Devices(_) => ModeMask::DEVICES,
            Overlay::Browse(_) => ModeMask::BROWSE,
        }
    }
}

/// Name of the surface the user is looking at — overlay if one is open, else
/// the active tab. Used for logging and tests.
pub fn mode_name(s: &AppState) -> &'static str {
    match &s.overlay {
        Some(ov) => ov.name(),
        None => s.tab.name(),
    }
}

/// True when the Now Playing screen is the visible surface — its tab is active
/// and no overlay covers it. The run loop watches this across a keypress: when
/// it flips false → true, Now Playing just came into view and its Up Next queue
/// is (re)fetched. This catches every path uniformly — tab nav, Esc out of
/// Search/Library, and closing an overlay — without threading an action through
/// each.
pub fn now_playing_visible(s: &AppState) -> bool {
    s.tab == Tab::NowPlaying && s.overlay.is_none()
}

/// The keymask whose footer/help should show — the overlay's if one is open
/// (its keys replace the tab's), else the active tab's.
pub fn active_mask(s: &AppState) -> ModeMask {
    match &s.overlay {
        Some(ov) => ov.mask(),
        None => s.tab.mask(),
    }
}

/// True when the focused surface is capturing typed characters, so the global
/// launcher keys (`/ : ? d l tab`) must NOT steal them: the Search tab (no
/// overlay) and the Command palette. Everywhere else those keys are live.
pub fn is_capturing_text(s: &AppState) -> bool {
    matches!(s.overlay, Some(Overlay::Command(_))) || (s.tab == Tab::Search && s.overlay.is_none())
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
    /// Monotonic id so a slow fetch that resolves after the user has
    /// already navigated away can be dropped instead of clobbering state.
    pub fetch_id: u64,
}

/// One lazily-loaded list in the Library tab. `loaded` distinguishes "never
/// fetched" from "fetched, empty" so we don't refetch a genuinely-empty
/// section on every focus.
pub struct Section<T> {
    pub items: Vec<T>,
    pub loaded: bool,
    pub loading: bool,
    pub error: Option<String>,
    /// Bumped per fetch so a slow response that lands after the user moved on
    /// is dropped instead of clobbering newer state.
    pub fetch_id: u64,
}

// Hand-written so `Section<T>` is Default for any `T` (the derive would
// wrongly require `T: Default` for the `Vec<T>` field).
impl<T> Default for Section<T> {
    fn default() -> Self {
        Self {
            items: Vec::new(),
            loaded: false,
            loading: false,
            error: None,
            fetch_id: 0,
        }
    }
}

#[derive(Default, Clone, Copy, PartialEq, Eq, Debug)]
pub enum LibraryTab {
    #[default]
    Liked,
    Playlists,
    Albums,
    Artists,
}

impl LibraryTab {
    pub const ALL: &'static [LibraryTab] = &[
        LibraryTab::Liked,
        LibraryTab::Playlists,
        LibraryTab::Albums,
        LibraryTab::Artists,
    ];

    pub fn next(self) -> LibraryTab {
        let i = Self::ALL.iter().position(|t| *t == self).unwrap_or(0);
        Self::ALL[(i + 1) % Self::ALL.len()]
    }

    pub fn prev(self) -> LibraryTab {
        let i = Self::ALL.iter().position(|t| *t == self).unwrap_or(0);
        Self::ALL[(i + Self::ALL.len() - 1) % Self::ALL.len()]
    }

    pub fn label(self) -> &'static str {
        match self {
            LibraryTab::Liked => "Liked",
            LibraryTab::Playlists => "Playlists",
            LibraryTab::Albums => "Albums",
            LibraryTab::Artists => "Artists",
        }
    }
}

/// Library tab state: the active sub-tab plus its four independently-loaded
/// sections. Each section is fetched on first focus only (rate-limit hygiene).
#[derive(Default)]
pub struct LibraryState {
    pub tab: LibraryTab,
    pub selected: usize,
    pub liked: Section<Track>,
    pub playlists: Section<Playlist>,
    pub albums: Section<Album>,
    pub artists: Section<Artist>,
}

impl LibraryState {
    /// Number of rows in the active sub-tab — used to bound selection.
    pub fn row_count(&self) -> usize {
        match self.tab {
            LibraryTab::Liked => self.liked.items.len(),
            LibraryTab::Playlists => self.playlists.items.len(),
            LibraryTab::Albums => self.albums.items.len(),
            LibraryTab::Artists => self.artists.items.len(),
        }
    }
}

/// Device-picker overlay state. Devices are fetched once when the overlay
/// opens — never on a timer (background device polling was removed for
/// rate-limit reasons).
#[derive(Default)]
pub struct DevicesState {
    pub devices: Vec<Device>,
    pub loading: bool,
    pub error: Option<String>,
    pub selected: usize,
    pub fetch_id: u64,
}

/// A discrete action runnable from the command menu.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Cmd {
    PlayPause,
    Next,
    Previous,
    Like,
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
        Cmd::Like,
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
            Cmd::Like => "like",
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
            Cmd::Like => "save the current track to Liked Songs",
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
        !self.input.is_empty() && (self.debounce.is_some() || self.applied_id < self.request_id)
    }
}

pub struct InContext {
    pub playlist_uri: String,
    pub tracks: Vec<Track>,
    pub filtered: Vec<usize>,
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
            // Transferring playback to our freshly-registered device makes
            // Spotify resume the track at its retained server-side position.
            // The periodic poll wouldn't pick that up for up to 30s, so the
            // now-playing view would sit at the stale seed (often 0:00) until
            // then. Re-poll immediately so the real position lands within ~1.5s.
            if wait_then_transfer(client.as_ref(), &device_id_for_transfer).await {
                spawn_post_play_poll(client.clone(), state.clone());
            }
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

/// Returns `true` once playback has been transferred to our device — the
/// caller uses this to know the server-side position is now authoritative for
/// us and worth re-polling immediately.
pub async fn wait_then_transfer(client: &dyn SpotifyApi, device_id: &str) -> bool {
    for attempt in 0..24 {
        tokio::time::sleep(Duration::from_millis(500)).await;
        // Bail the whole loop if the rate-limit gate trips mid-probe; we
        // don't want to keep poking /me/player/devices every 500ms while
        // we're meant to be backing off.
        if client.rate_limited_until().is_some() {
            log::note("wait_then_transfer: aborted (rate-limited)", None);
            return false;
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
                        Ok(_) => {
                            log::note("transfer_playback ok", None);
                            return true;
                        }
                        Err(e) => {
                            log::error("transfer_playback", &format!("{e:#}"));
                            return false;
                        }
                    }
                }
            }
            // get_devices/transfer_playback returning RateLimited is the
            // signal that the gate is set; abort and let the user retry.
            Err(e) if e.downcast_ref::<RateLimited>().is_some() => {
                log::note("wait_then_transfer: aborted (RateLimited)", None);
                return false;
            }
            Err(e) => log::note("get_devices failed", Some(&format!("{e:#}"))),
        }
    }
    log::error(
        "wait_then_transfer",
        "device never appeared in /me/player/devices after 12s",
    );
    false
}

// ---------------------------------------------------------------------------
// Action handlers
// ---------------------------------------------------------------------------

/// Focus the Search tab. Search state is persistent, so we only reset and
/// re-seed recents when the box is empty — tabbing away and back mid-query
/// keeps the user's in-progress search intact.
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

    let reseeded = {
        let mut s = state.lock().await;
        s.tab = Tab::Search;
        s.overlay = None;
        if s.search.input.is_empty() {
            let in_context = context_uri.as_ref().map(|uri| InContext {
                playlist_uri: uri.clone(),
                tracks: Vec::new(),
                filtered: Vec::new(),
            });
            let queries = s.recent_queries.clone();
            let tracks = s.recent_tracks.clone();
            s.search = SearchState::new(in_context, queries, tracks);
            true
        } else {
            false
        }
    };

    // Only kick the in-context playlist fetch when we actually (re)opened a
    // fresh search with that context attached.
    if !reseeded {
        return;
    }
    if let Some(uri) = context_uri {
        if let Some(id) = playlist_id_from_uri(&uri) {
            let client = client.clone();
            let state = state.clone();
            tokio::spawn(async move {
                match client.get_playlist_tracks(&id).await {
                    Ok(tracks) => {
                        let mut s = state.lock().await;
                        let search = &mut s.search;
                        if let Some(ctx) = &mut search.in_context {
                            if ctx.playlist_uri == uri {
                                ctx.tracks = tracks;
                                refilter_in_context(search);
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
                        if let Some(ctx) = &s.search.in_context {
                            if ctx.playlist_uri == uri {
                                s.search.in_context = None;
                            }
                        }
                    }
                }
            });
        }
    }
}

/// Open the Browse overlay for the given collection. Browse stacks on top of
/// whatever tab the user was on (Search or Library); Esc just closes it,
/// revealing that tab unchanged — no need to stash/restore search state, since
/// the Search tab is persistent. Kicks off the track fetch in the background;
/// Browse renders immediately with a "Loading..." placeholder.
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
        // Monotonic per-open id so a re-open of the same collection still
        // supersedes the prior fetch. Reuses the search request counter (it
        // only ever moves forward) so we don't need a second counter.
        let fetch_id = s.search.request_id.wrapping_add(1);
        s.search.request_id = fetch_id;
        s.overlay = Some(Overlay::Browse(BrowseState {
            collection: collection.clone(),
            tracks: Vec::new(),
            loading: true,
            error: None,
            selected: 0,
            fetch_id,
        }));
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
        let Some(Overlay::Browse(browse)) = s.overlay.as_mut() else {
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

// ---------------------------------------------------------------------------
// Library
// ---------------------------------------------------------------------------

/// Lazily load the active Library sub-tab. Fetches happen ONLY on first focus
/// of a sub-tab (never on a timer) — re-focusing a loaded section is a no-op.
/// User-initiated, so it ignores the background soft cap, but still bails on
/// the hard rate-limit gate. Sub-tabs load independently; a 403 on one (a
/// missing scope) leaves the others usable.
pub async fn enter_library(client: &Arc<dyn SpotifyApi>, state: &Arc<Mutex<AppState>>) {
    let (which, fetch_id) = {
        let mut s = state.lock().await;
        s.tab = Tab::Library;
        let which = s.library.tab;
        let (loaded, loading) = match which {
            LibraryTab::Liked => (s.library.liked.loaded, s.library.liked.loading),
            LibraryTab::Playlists => (s.library.playlists.loaded, s.library.playlists.loading),
            LibraryTab::Albums => (s.library.albums.loaded, s.library.albums.loading),
            LibraryTab::Artists => (s.library.artists.loaded, s.library.artists.loading),
        };
        if loaded || loading {
            return;
        }
        // Bail on the hard gate; the section stays unloaded and the user can
        // retry once the limit clears.
        if s.rate_limited_until
            .map(|t| t > Instant::now())
            .unwrap_or(false)
        {
            log::note("library load skipped (rate-limited)", Some(which.label()));
            return;
        }
        let fetch_id = bump_library_fetch(&mut s.library, which);
        (which, fetch_id)
    };

    log::note("library load", Some(which.label()));
    let client = client.clone();
    let state = state.clone();
    tokio::spawn(async move {
        let result = match which {
            LibraryTab::Liked => client.get_saved_tracks(50).await.map(LibraryItems::Tracks),
            LibraryTab::Playlists => client
                .get_saved_playlists(50)
                .await
                .map(LibraryItems::Playlists),
            LibraryTab::Albums => client.get_saved_albums(50).await.map(LibraryItems::Albums),
            LibraryTab::Artists => client
                .get_followed_artists(50)
                .await
                .map(LibraryItems::Artists),
        };
        let mut s = state.lock().await;
        // Drop the result if the user moved to a different sub-tab or a newer
        // fetch superseded this one.
        if s.tab != Tab::Library || current_library_fetch(&s.library, which) != fetch_id {
            log::note("library result dropped (stale)", Some(which.label()));
            return;
        }
        apply_library_result(&mut s.library, which, result);
    });
}

/// Play the selected track from the Library "Liked" list. Search-independent
/// (unlike `play_selection`), so Library doesn't depend on search state.
pub async fn play_library_selection(client: &Arc<dyn SpotifyApi>, state: &Arc<Mutex<AppState>>) {
    if reconnect_if_device_offline(client, state, "play_library_selection").await {
        return;
    }
    let (uri, synth) = {
        let s = state.lock().await;
        let lib = &s.library;
        let Some(track) = lib.liked.items.get(lib.selected) else {
            return;
        };
        let Some(uri) = track.uri.clone() else {
            log::note("play_library_selection: track missing uri", None);
            return;
        };
        let synth = Playback {
            is_playing: true,
            progress_ms: Some(0),
            item: Some(track.clone()),
            context: None,
            timestamp: None,
        };
        (uri, synth)
    };
    log::note("play_library_selection", Some(&uri));
    let result = client.play_uris(&[uri]).await;
    let synth_to_apply = match &result {
        Ok(()) => {
            let ts = now_unix_ms();
            let mut s = state.lock().await;
            s.error = None;
            log::mode_change("library", "now_playing");
            s.overlay = None;
            s.tab = Tab::NowPlaying;
            s.last_local_action_ms = ts;
            s.boot = false;
            let mut pb = synth;
            pb.timestamp = Some(ts);
            Some(pb)
        }
        Err(e) => {
            let msg = format!("{e:#}");
            log::error("play_library_selection", &msg);
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
        apply_playback(state, Some(pb)).await;
    }
}

/// Possible result payloads for a library section fetch, so one spawn site can
/// handle all four sub-tabs.
enum LibraryItems {
    Tracks(Vec<Track>),
    Playlists(Vec<Playlist>),
    Albums(Vec<Album>),
    Artists(Vec<Artist>),
}

fn bump_library_fetch(lib: &mut LibraryState, which: LibraryTab) -> u64 {
    match which {
        LibraryTab::Liked => {
            lib.liked.loading = true;
            lib.liked.fetch_id = lib.liked.fetch_id.wrapping_add(1);
            lib.liked.fetch_id
        }
        LibraryTab::Playlists => {
            lib.playlists.loading = true;
            lib.playlists.fetch_id = lib.playlists.fetch_id.wrapping_add(1);
            lib.playlists.fetch_id
        }
        LibraryTab::Albums => {
            lib.albums.loading = true;
            lib.albums.fetch_id = lib.albums.fetch_id.wrapping_add(1);
            lib.albums.fetch_id
        }
        LibraryTab::Artists => {
            lib.artists.loading = true;
            lib.artists.fetch_id = lib.artists.fetch_id.wrapping_add(1);
            lib.artists.fetch_id
        }
    }
}

fn current_library_fetch(lib: &LibraryState, which: LibraryTab) -> u64 {
    match which {
        LibraryTab::Liked => lib.liked.fetch_id,
        LibraryTab::Playlists => lib.playlists.fetch_id,
        LibraryTab::Albums => lib.albums.fetch_id,
        LibraryTab::Artists => lib.artists.fetch_id,
    }
}

fn apply_library_result(
    lib: &mut LibraryState,
    which: LibraryTab,
    result: anyhow::Result<LibraryItems>,
) {
    // 403 on a section means a missing OAuth scope — surface the same re-auth
    // hint the like flow uses, scoped to just this section.
    let friendly = |msg: String| -> String {
        if msg.contains("403") {
            "locked: missing scope — delete hifi-auth.json and re-auth".into()
        } else {
            msg
        }
    };
    match (which, result) {
        (LibraryTab::Liked, Ok(LibraryItems::Tracks(v))) => {
            lib.liked.items = v;
            lib.liked.loaded = true;
            lib.liked.loading = false;
            lib.liked.error = None;
        }
        (LibraryTab::Playlists, Ok(LibraryItems::Playlists(v))) => {
            lib.playlists.items = v;
            lib.playlists.loaded = true;
            lib.playlists.loading = false;
            lib.playlists.error = None;
        }
        (LibraryTab::Albums, Ok(LibraryItems::Albums(v))) => {
            lib.albums.items = v;
            lib.albums.loaded = true;
            lib.albums.loading = false;
            lib.albums.error = None;
        }
        (LibraryTab::Artists, Ok(LibraryItems::Artists(v))) => {
            lib.artists.items = v;
            lib.artists.loaded = true;
            lib.artists.loading = false;
            lib.artists.error = None;
        }
        (LibraryTab::Liked, Err(e)) => {
            lib.liked.loading = false;
            lib.liked.error = Some(friendly(format!("{e:#}")));
        }
        (LibraryTab::Playlists, Err(e)) => {
            lib.playlists.loading = false;
            lib.playlists.error = Some(friendly(format!("{e:#}")));
        }
        (LibraryTab::Albums, Err(e)) => {
            lib.albums.loading = false;
            lib.albums.error = Some(friendly(format!("{e:#}")));
        }
        (LibraryTab::Artists, Err(e)) => {
            lib.artists.loading = false;
            lib.artists.error = Some(friendly(format!("{e:#}")));
        }
        // Mismatched payload/sub-tab can't happen (the spawn maps 1:1), but be
        // defensive rather than panic.
        (_, Ok(_)) => log::error("library", "result/sub-tab mismatch"),
    }
}

// ---------------------------------------------------------------------------
// Devices
// ---------------------------------------------------------------------------

/// Open the device-picker overlay and fetch the device list ONCE. Never polls
/// on a timer (background device probing was removed for rate-limit reasons).
pub async fn open_devices(client: &Arc<dyn SpotifyApi>, state: &Arc<Mutex<AppState>>) {
    let fetch_id = {
        let mut s = state.lock().await;
        let gated = s
            .rate_limited_until
            .map(|t| t > Instant::now())
            .unwrap_or(false);
        let mut dev = DevicesState {
            loading: !gated,
            ..Default::default()
        };
        if gated {
            dev.error = Some("rate-limited — try again shortly".into());
        }
        dev.fetch_id = 1;
        s.overlay = Some(Overlay::Devices(dev));
        if gated {
            return;
        }
        1u64
    };

    log::note("open_devices", None);
    let client = client.clone();
    let state = state.clone();
    tokio::spawn(async move {
        let result = client.get_devices().await;
        let mut s = state.lock().await;
        let Some(Overlay::Devices(dev)) = s.overlay.as_mut() else {
            return; // overlay closed before the fetch landed
        };
        if dev.fetch_id != fetch_id {
            return;
        }
        dev.loading = false;
        match result {
            Ok(list) => {
                // Default selection to the active device if there is one.
                dev.selected = list.iter().position(|d| d.is_active).unwrap_or(0);
                dev.devices = list;
            }
            Err(e) => {
                log::error("get_devices", &format!("{e:#}"));
                dev.error = Some(format!("{e:#}"));
            }
        }
    });
}

/// Transfer playback to the chosen device, close the picker, and poll-burst so
/// Now Playing reflects the new device quickly.
pub async fn transfer_to_device(
    client: &Arc<dyn SpotifyApi>,
    state: &Arc<Mutex<AppState>>,
    device_id: String,
) {
    log::note("transfer_to_device", Some(&device_id));
    let result = client.transfer_playback(&device_id, true).await;
    {
        let mut s = state.lock().await;
        s.overlay = None;
        match &result {
            Ok(()) => {
                s.error = None;
                s.last_local_action_ms = now_unix_ms();
                s.boot = false;
            }
            Err(e) => {
                s.error = Some(format!("{e:#}"));
                log::error("transfer_playback", &format!("{e:#}"));
            }
        }
    }
    if result.is_ok() {
        spawn_post_play_poll(client.clone(), state.clone());
    }
}

/// Play the currently-selected track inside the active Browse view, using
/// the collection as the playback context (so Next/Previous walk the
/// collection in order).
pub async fn play_browse_selection(client: &Arc<dyn SpotifyApi>, state: &Arc<Mutex<AppState>>) {
    if reconnect_if_device_offline(client, state, "play_browse_selection").await {
        return;
    }
    let (action, synth) = {
        let s = state.lock().await;
        let Some(Overlay::Browse(browse)) = &s.overlay else {
            return;
        };
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
        PlayAction::Context { uri, offset } => client.play_context(uri, offset.as_deref()).await,
        // Unreachable — we only construct Context above.
        PlayAction::Track(_) => unreachable!(),
    };

    let synth_to_apply = match &result {
        Ok(()) => {
            let ts = now_unix_ms();
            let mut s = state.lock().await;
            s.error = None;
            log::mode_change("browse", "now_playing");
            s.overlay = None;
            s.tab = Tab::NowPlaying;
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
        apply_playback(state, Some(pb)).await;
    }
}

/// Play the active Browse collection from the start with no offset. This is
/// the fallback when `/playlists/{id}/tracks` or `/albums/{id}/tracks` 403s
/// (Spotify locked these endpoints down for newly-created apps in late
/// 2024); the user can still kick off playback of the whole thing.
pub async fn play_browse_collection(client: &Arc<dyn SpotifyApi>, state: &Arc<Mutex<AppState>>) {
    if reconnect_if_device_offline(client, state, "play_browse_collection").await {
        return;
    }
    let uri = {
        let s = state.lock().await;
        let Some(Overlay::Browse(browse)) = &s.overlay else {
            return;
        };
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
            s.overlay = None;
            s.tab = Tab::NowPlaying;
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
    tokio::spawn(async move {
        for _ in 0..6 {
            tokio::time::sleep(Duration::from_millis(250)).await;
            if c.rate_limited_until().is_some() {
                break;
            }
            if let Ok(pb) = c.get_playback().await {
                apply_playback(&s, pb).await;
            }
        }
    });
}

pub async fn kick_search(client: &Arc<dyn SpotifyApi>, state: &Arc<Mutex<AppState>>) {
    let (q, my_id) = {
        let mut s = state.lock().await;
        let search = &mut s.search;
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
    log::note(
        "search debounce scheduled",
        Some(&format!("id={my_id} q={q:?}")),
    );

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
                let search = &mut s.search;
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
            Err(e) => {
                log::error("search task", &format!("id={my_id} q={q:?} err={e:#}"));
                let mut s = state_task.lock().await;
                let retry = e.downcast_ref::<RateLimited>().map(|r| r.0);
                s.error = Some(format!("{e:#}"));
                s.rate_limited_until = retry.map(|x| Instant::now() + Duration::from_secs(x));
                s.search.debounce = None;
            }
        }
    });

    let mut s = state.lock().await;
    let search = &mut s.search;
    if search.request_id == my_id {
        search.debounce = Some(handle.abort_handle());
    } else {
        // a newer kick raced ahead; cancel this one
        handle.abort();
    }
}

pub async fn play_selection(client: &Arc<dyn SpotifyApi>, state: &Arc<Mutex<AppState>>) {
    // 1. Resolve action and capture the synth template now, while search
    //    state is still in AppState. We finalize its timestamp later (after
    //    a successful play) so it matches `last_local_action_ms` exactly.
    if reconnect_if_device_offline(client, state, "play_selection").await {
        return;
    }
    let (action, synth_template) = {
        let s = state.lock().await;
        let search = &s.search;
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
            format!(
                "context: uri={uri} offset={}",
                offset.as_deref().unwrap_or("-")
            )
        }
    };
    log::note("play_selection", Some(&action_desc));

    let result = match &action {
        PlayAction::Track(uri) => client.play_uris(&[uri.clone()]).await,
        PlayAction::Context { uri, offset } => client.play_context(uri, offset.as_deref()).await,
    };

    // 2. On success: use a SINGLE timestamp for both last_local_action_ms
    //    and synth.timestamp so should_accept doesn't reject our own synth.
    let synth_to_apply = match &result {
        Ok(()) => {
            let ts = now_unix_ms();
            let mut s = state.lock().await;
            s.error = None;
            log::mode_change("search", "now_playing");
            s.overlay = None;
            s.tab = Tab::NowPlaying;
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
        apply_playback(state, Some(pb)).await;
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

pub async fn apply_playback(state: &Arc<Mutex<AppState>>, pb: Option<Playback>) {
    apply_playback_inner(state, pb, false).await
}

/// Apply a polled playback result and, if it advanced to a new track while the
/// user is watching Now Playing, refresh the Up Next queue — it's now stale.
/// This is the only background queue refresh: it's event-driven (a real track
/// change the poll observed), not a timer, and gated to the Now Playing tab so
/// we never spend a request on a queue the user can't see. The playback poll
/// uses this instead of bare [`apply_playback`].
pub async fn apply_polled_playback(
    client: &Arc<dyn SpotifyApi>,
    state: &Arc<Mutex<AppState>>,
    pb: Option<Playback>,
) {
    let before = { state.lock().await.current_track_id.clone() };
    apply_playback(state, pb).await;
    let advanced_in_view = {
        let s = state.lock().await;
        s.current_track_id != before && s.current_track_id.is_some() && now_playing_visible(&s)
    };
    if advanced_in_view {
        refresh_queue(client, state).await;
    }
}

/// Apply playback bypassing `should_accept` — used for our own
/// synthesized seed (e.g. the recently-played track shown at startup),
/// which would otherwise be filtered out by the boot guard.
pub async fn apply_playback_force(state: &Arc<Mutex<AppState>>, pb: Option<Playback>) {
    apply_playback_inner(state, pb, true).await
}

async fn apply_playback_inner(state: &Arc<Mutex<AppState>>, pb: Option<Playback>, force: bool) {
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
    // On track change, point the head at the new cover (or clear it when
    // nothing useful is playing). The head owns the decoded-image cache and
    // fetches against this — the core stays framework-free. Art for the same
    // track across repeated polls keeps the same request, so the head's
    // `has_or_loading` check means it's fetched at most once.
    if prev != new_track_id {
        s.art_request = match (&new_track_id, &cover_url) {
            (Some(id), Some(url)) => Some(ArtRequest {
                track_id: id.clone(),
                url: url.clone(),
            }),
            _ => None,
        };
    }
}

/// Save the currently-playing track to the user's Liked Songs. Reads the
/// track id from `state.playback.item.id` — if there's nothing playing,
/// returns silently. On success, sets a transient green notice; on
/// failure (typically 403 when `user-library-modify` scope is missing),
/// sets `error`. Skips when the rate-limit gate is engaged, like the
/// other action handlers.
pub async fn like_current_track(client: &Arc<dyn SpotifyApi>, state: &Arc<Mutex<AppState>>) {
    let (track_id, label) = {
        let s = state.lock().await;
        if s.rate_limited_until
            .map(|t| t > Instant::now())
            .unwrap_or(false)
        {
            log::note("like: skipped (rate-limited)", None);
            return;
        }
        let Some(track) = s.playback.as_ref().and_then(|p| p.item.as_ref()) else {
            log::note("like: skipped (no current track)", None);
            return;
        };
        let Some(id) = track.id.clone() else {
            log::note("like: skipped (track has no id)", None);
            return;
        };
        (id, track.name.clone())
    };
    log::note("like", Some(&format!("track={label} id={track_id}")));
    match client.save_track(&track_id).await {
        Ok(()) => {
            let mut s = state.lock().await;
            s.error = None;
            s.notice = Some((
                format!("♥ liked: {label}"),
                Instant::now() + std::time::Duration::from_secs(3),
            ));
        }
        Err(e) => {
            let msg = format!("{e:#}");
            log::error("like", &msg);
            let mut s = state.lock().await;
            s.error = Some(if msg.contains("403") {
                "like failed: missing scope — delete hifi-auth.json and re-auth".into()
            } else {
                msg
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

/// Refresh the "Up Next" queue shown on Now Playing. Called on demand — when
/// the user lands on the Now Playing tab, or right after a skip — never on a
/// timer, to keep request volume low. Respects the hard rate-limit gate but
/// ignores the soft background-throttle cap, since it's user-initiated. The
/// queue is non-critical: the fetch runs on a spawned task so it never blocks
/// the UI, and a failure is logged (not surfaced) and leaves the prior list
/// untouched.
pub async fn refresh_queue(client: &Arc<dyn SpotifyApi>, state: &Arc<Mutex<AppState>>) {
    {
        let s = state.lock().await;
        if s.rate_limited_until
            .map(|t| t > Instant::now())
            .unwrap_or(false)
        {
            log::note("refresh_queue: skipped (rate-limited)", None);
            return;
        }
        // With nothing playing there's no queue to show, and Now Playing
        // renders its placeholder instead — don't spend a request on it.
        if s.playback.as_ref().and_then(|p| p.item.as_ref()).is_none() {
            return;
        }
    }
    let client = client.clone();
    let state = state.clone();
    tokio::spawn(async move {
        match client.get_queue().await {
            Ok(queue) => {
                log::note("refresh_queue", Some(&format!("len={}", queue.len())));
                state.lock().await.queue = queue;
            }
            Err(e) => log::note("refresh_queue failed", Some(&format!("{e:#}"))),
        }
    });
}

/// Convenience to grab a Result wrapping the action loop should perform
/// after a successful play. Used by the run loop to poll-burst after a
/// user-initiated play so we pick up Spotify's view quickly.
pub fn spawn_post_play_poll(client: Arc<dyn SpotifyApi>, state: Arc<Mutex<AppState>>) {
    tokio::spawn(async move {
        for _ in 0..6 {
            tokio::time::sleep(Duration::from_millis(250)).await;
            // The client's send_logged would short-circuit anyway, but
            // bailing the whole burst is cleaner — no point chewing through
            // six instant-fail RateLimited errors.
            if client.rate_limited_until().is_some() {
                break;
            }
            // Only apply a *real* track. An empty (204/None) result here is the
            // "nothing to resume / hasn't caught up" race — applying it would
            // clobber the seed with "Nothing playing." After a user-initiated
            // play, should_accept already rejects None for 60s; at boot
            // (last_local_action_ms == 0) it wouldn't, so filter explicitly.
            if let Ok(Some(pb)) = client.get_playback().await {
                if pb.item.is_some() {
                    apply_playback(&state, Some(pb)).await;
                }
            }
        }
    });
}

#[cfg(test)]
mod tests;
