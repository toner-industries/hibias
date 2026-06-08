#![allow(dead_code)] // Test scaffolding — not all helpers used by every test.

//! Headless test harness — `cfg(test)` only.
//!
//! Wires up an `AppState`, a programmable `FakeSpotify`, and ratatui's
//! `TestBackend` so we can drive the same code paths the run loop uses
//! (`dispatch_input` → `KeyAction` → action handler) and inspect the
//! resulting state + rendered screen, all without a TTY or the network.
//!
//! NOTE: `Harness::run` hand-mirrors the `match action` dispatch in
//! `main.rs::run`. They must stay in sync — a new `KeyAction` wired only into
//! `main.rs` won't be exercised by tests until it's added here too.

use anyhow::{anyhow, Result};
use async_trait::async_trait;
use ratatui::backend::TestBackend;
use ratatui::Terminal;
use std::collections::HashMap;
use std::sync::Mutex as StdMutex;
use std::sync::Arc;
use std::time::Instant;
use tokio::sync::Mutex;

use crate::api::{
    Album, Artist, Device, Playback, Playlist, SearchResults, SpotifyApi, Track,
};
use crate::app::{
    apply_playback_force, dispatch_input, enter_browse, enter_library, enter_search, kick_search,
    like_current_track, open_devices, play_browse_collection, play_browse_selection,
    play_library_selection, play_selection, refresh_queue, seek_relative, skip_track,
    toggle_playback, transfer_to_device, AppState, KeyAction,
};
use crate::input::{Input, Key, Mods};
use crate::{art, ui};

/// What a call to the fake recorded — used by tests to assert the right
/// endpoint was hit, with the right arguments.
#[derive(Debug, Clone, PartialEq)]
pub enum Call {
    GetPlayback,
    Play,
    Pause,
    GetDevices,
    TransferPlayback { device_id: String, play: bool },
    SeekTo(u64),
    NextTrack,
    PrevTrack,
    PlayUris(Vec<String>),
    PlayContext { uri: String, offset: Option<String> },
    Search(String),
    GetAlbumTracks(String),
    GetPlaylistTracks(String),
    GetRecentlyPlayed(u32),
    GetQueue,
    SaveTrack(String),
    GetSavedTracks(u32),
    GetSavedPlaylists(u32),
    GetSavedAlbums(u32),
    GetFollowedArtists(u32),
}

#[derive(Default)]
struct FakeState {
    device_id: Option<String>,
    rate_limited_until: Option<Instant>,
    playback: Option<Result<Option<Playback>, String>>,
    devices: Option<Result<Vec<Device>, String>>,
    recently_played: Option<Result<Vec<Track>, String>>,
    queue: Option<Result<Vec<Track>, String>>,
    /// Per-query programmable response.
    search: HashMap<String, Result<SearchResults, String>>,
    /// Per-id programmable response.
    album_tracks: HashMap<String, Result<Vec<Track>, String>>,
    /// Per-id programmable response.
    playlist_tracks: HashMap<String, Result<Vec<Track>, String>>,
    saved_tracks: Option<Result<Vec<Track>, String>>,
    saved_playlists: Option<Result<Vec<Playlist>, String>>,
    saved_albums: Option<Result<Vec<Album>, String>>,
    followed_artists: Option<Result<Vec<Artist>, String>>,
    /// Each `play()` pops the front; if empty, defaults to Ok(()).
    play_returns: Vec<Result<(), String>>,
    pause_returns: Vec<Result<(), String>>,
    seek_returns: Vec<Result<(), String>>,
    next_returns: Vec<Result<(), String>>,
    prev_returns: Vec<Result<(), String>>,
    play_uris_returns: Vec<Result<(), String>>,
    play_context_returns: Vec<Result<(), String>>,
    transfer_returns: Vec<Result<(), String>>,
    calls: Vec<Call>,
}

pub struct FakeSpotify {
    inner: StdMutex<FakeState>,
}

impl Default for FakeSpotify {
    fn default() -> Self {
        Self::new()
    }
}

impl FakeSpotify {
    pub fn new() -> Self {
        Self {
            inner: StdMutex::new(FakeState::default()),
        }
    }

    fn with<R>(&self, f: impl FnOnce(&mut FakeState) -> R) -> R {
        let mut g = self.inner.lock().expect("fake poisoned");
        f(&mut g)
    }

    fn record(&self, c: Call) {
        self.with(|s| s.calls.push(c));
    }

    fn pop_result(list: &mut Vec<Result<(), String>>) -> Result<()> {
        if list.is_empty() {
            return Ok(());
        }
        match list.remove(0) {
            Ok(()) => Ok(()),
            Err(e) => Err(anyhow!(e)),
        }
    }

    // ---- setters -------------------------------------------------------

    pub fn set_playback(&self, r: Result<Option<Playback>, String>) {
        self.with(|s| s.playback = Some(r));
    }

    pub fn set_devices(&self, r: Result<Vec<Device>, String>) {
        self.with(|s| s.devices = Some(r));
    }

    pub fn set_recently_played(&self, r: Result<Vec<Track>, String>) {
        self.with(|s| s.recently_played = Some(r));
    }

    pub fn set_queue(&self, r: Result<Vec<Track>, String>) {
        self.with(|s| s.queue = Some(r));
    }

    pub fn set_search(&self, query: &str, r: Result<SearchResults, String>) {
        self.with(|s| {
            s.search.insert(query.to_string(), r);
        });
    }

    pub fn set_album_tracks(&self, id: &str, r: Result<Vec<Track>, String>) {
        self.with(|s| {
            s.album_tracks.insert(id.to_string(), r);
        });
    }

    pub fn set_playlist_tracks(&self, id: &str, r: Result<Vec<Track>, String>) {
        self.with(|s| {
            s.playlist_tracks.insert(id.to_string(), r);
        });
    }

    pub fn set_saved_tracks(&self, r: Result<Vec<Track>, String>) {
        self.with(|s| s.saved_tracks = Some(r));
    }

    pub fn set_saved_playlists(&self, r: Result<Vec<Playlist>, String>) {
        self.with(|s| s.saved_playlists = Some(r));
    }

    pub fn set_saved_albums(&self, r: Result<Vec<Album>, String>) {
        self.with(|s| s.saved_albums = Some(r));
    }

    pub fn set_followed_artists(&self, r: Result<Vec<Artist>, String>) {
        self.with(|s| s.followed_artists = Some(r));
    }

    pub fn queue_play(&self, r: Result<(), String>) {
        self.with(|s| s.play_returns.push(r));
    }

    pub fn queue_play_context(&self, r: Result<(), String>) {
        self.with(|s| s.play_context_returns.push(r));
    }

    // ---- readers -------------------------------------------------------

    pub fn calls(&self) -> Vec<Call> {
        self.with(|s| s.calls.clone())
    }

    pub fn last_call(&self) -> Option<Call> {
        self.with(|s| s.calls.last().cloned())
    }

    pub fn clear_calls(&self) {
        self.with(|s| s.calls.clear());
    }
}

#[async_trait]
impl SpotifyApi for FakeSpotify {
    fn set_device_id(&self, id: String) {
        self.with(|s| s.device_id = Some(id));
    }
    fn clear_device_id(&self) {
        self.with(|s| s.device_id = None);
    }
    fn rate_limited_until(&self) -> Option<Instant> {
        self.with(|s| {
            // Mirror SpotifyClient's lazy clear.
            match s.rate_limited_until {
                Some(t) if t > Instant::now() => Some(t),
                _ => {
                    s.rate_limited_until = None;
                    None
                }
            }
        })
    }
    fn clear_rate_limit(&self) {
        self.with(|s| s.rate_limited_until = None);
    }
    fn background_throttled(&self) -> bool {
        // Tests don't simulate the sliding-window counter.
        false
    }

    async fn get_playback(&self) -> Result<Option<Playback>> {
        self.record(Call::GetPlayback);
        let resp = self.with(|s| s.playback.clone());
        match resp {
            Some(Ok(pb)) => Ok(pb),
            Some(Err(e)) => Err(anyhow!(e)),
            None => Ok(None),
        }
    }

    async fn play(&self) -> Result<()> {
        self.record(Call::Play);
        self.with(|s| Self::pop_result(&mut s.play_returns))
    }

    async fn pause(&self) -> Result<()> {
        self.record(Call::Pause);
        self.with(|s| Self::pop_result(&mut s.pause_returns))
    }

    async fn get_devices(&self) -> Result<Vec<Device>> {
        self.record(Call::GetDevices);
        let resp = self.with(|s| s.devices.clone());
        match resp {
            Some(Ok(d)) => Ok(d),
            Some(Err(e)) => Err(anyhow!(e)),
            None => Ok(Vec::new()),
        }
    }

    async fn transfer_playback(&self, device_id: &str, play: bool) -> Result<()> {
        self.record(Call::TransferPlayback {
            device_id: device_id.to_string(),
            play,
        });
        self.with(|s| Self::pop_result(&mut s.transfer_returns))
    }

    async fn seek_to(&self, position_ms: u64) -> Result<()> {
        self.record(Call::SeekTo(position_ms));
        self.with(|s| Self::pop_result(&mut s.seek_returns))
    }

    async fn next_track(&self) -> Result<()> {
        self.record(Call::NextTrack);
        self.with(|s| Self::pop_result(&mut s.next_returns))
    }

    async fn previous_track(&self) -> Result<()> {
        self.record(Call::PrevTrack);
        self.with(|s| Self::pop_result(&mut s.prev_returns))
    }

    async fn play_uris(&self, uris: &[String]) -> Result<()> {
        self.record(Call::PlayUris(uris.to_vec()));
        self.with(|s| Self::pop_result(&mut s.play_uris_returns))
    }

    async fn play_context(&self, context_uri: &str, offset_uri: Option<&str>) -> Result<()> {
        self.record(Call::PlayContext {
            uri: context_uri.to_string(),
            offset: offset_uri.map(|s| s.to_string()),
        });
        self.with(|s| Self::pop_result(&mut s.play_context_returns))
    }

    async fn search(&self, q: &str) -> Result<SearchResults> {
        self.record(Call::Search(q.to_string()));
        let resp = self.with(|s| s.search.get(q).cloned());
        match resp {
            Some(Ok(r)) => Ok(r),
            Some(Err(e)) => Err(anyhow!(e)),
            None => Ok(SearchResults::default()),
        }
    }

    async fn get_album_tracks(&self, album_id: &str) -> Result<Vec<Track>> {
        self.record(Call::GetAlbumTracks(album_id.to_string()));
        let resp = self.with(|s| s.album_tracks.get(album_id).cloned());
        match resp {
            Some(Ok(t)) => Ok(t),
            Some(Err(e)) => Err(anyhow!(e)),
            None => Ok(Vec::new()),
        }
    }

    async fn get_playlist_tracks(&self, playlist_id: &str) -> Result<Vec<Track>> {
        self.record(Call::GetPlaylistTracks(playlist_id.to_string()));
        let resp = self.with(|s| s.playlist_tracks.get(playlist_id).cloned());
        match resp {
            Some(Ok(t)) => Ok(t),
            Some(Err(e)) => Err(anyhow!(e)),
            None => Ok(Vec::new()),
        }
    }

    async fn get_recently_played(&self, limit: u32) -> Result<Vec<Track>> {
        self.record(Call::GetRecentlyPlayed(limit));
        let resp = self.with(|s| s.recently_played.clone());
        match resp {
            Some(Ok(t)) => Ok(t),
            Some(Err(e)) => Err(anyhow!(e)),
            None => Ok(Vec::new()),
        }
    }

    async fn get_queue(&self) -> Result<Vec<Track>> {
        self.record(Call::GetQueue);
        match self.with(|s| s.queue.clone()) {
            Some(Ok(t)) => Ok(t),
            Some(Err(e)) => Err(anyhow!(e)),
            None => Ok(Vec::new()),
        }
    }

    async fn save_track(&self, track_id: &str) -> Result<()> {
        self.record(Call::SaveTrack(track_id.to_string()));
        Ok(())
    }

    async fn get_saved_tracks(&self, limit: u32) -> Result<Vec<Track>> {
        self.record(Call::GetSavedTracks(limit));
        match self.with(|s| s.saved_tracks.clone()) {
            Some(Ok(t)) => Ok(t),
            Some(Err(e)) => Err(anyhow!(e)),
            None => Ok(Vec::new()),
        }
    }

    async fn get_saved_playlists(&self, limit: u32) -> Result<Vec<Playlist>> {
        self.record(Call::GetSavedPlaylists(limit));
        match self.with(|s| s.saved_playlists.clone()) {
            Some(Ok(t)) => Ok(t),
            Some(Err(e)) => Err(anyhow!(e)),
            None => Ok(Vec::new()),
        }
    }

    async fn get_saved_albums(&self, limit: u32) -> Result<Vec<Album>> {
        self.record(Call::GetSavedAlbums(limit));
        match self.with(|s| s.saved_albums.clone()) {
            Some(Ok(t)) => Ok(t),
            Some(Err(e)) => Err(anyhow!(e)),
            None => Ok(Vec::new()),
        }
    }

    async fn get_followed_artists(&self, limit: u32) -> Result<Vec<Artist>> {
        self.record(Call::GetFollowedArtists(limit));
        match self.with(|s| s.followed_artists.clone()) {
            Some(Ok(t)) => Ok(t),
            Some(Err(e)) => Err(anyhow!(e)),
            None => Ok(Vec::new()),
        }
    }
}

/// Headless test harness — owns an AppState and a programmable FakeSpotify,
/// and provides press/run/snapshot affordances that match what the real run
/// loop does. Album art is a head concern (see `ArtCache`) and is never
/// produced in tests, so the harness holds no art loader.
pub struct Harness {
    pub fake: Arc<FakeSpotify>,
    pub client: Arc<dyn SpotifyApi>,
    pub state: Arc<Mutex<AppState>>,
}

impl Default for Harness {
    fn default() -> Self {
        Self::new()
    }
}

impl Harness {
    pub fn new() -> Self {
        let fake = Arc::new(FakeSpotify::new());
        let client: Arc<dyn SpotifyApi> = fake.clone();
        let state = Arc::new(Mutex::new(AppState::default()));
        Self {
            fake,
            client,
            state,
        }
    }

    /// Dispatch one keypress and return what action the run loop would take.
    pub async fn press(&self, key: Key) -> KeyAction {
        dispatch_input(Input::new(key), &self.state).await
    }

    pub async fn press_with_mods(&self, key: Key, mods: Mods) -> KeyAction {
        dispatch_input(Input::with_mods(key, mods), &self.state).await
    }

    /// Equivalent to typing each character of `s` into whichever mode is
    /// active. Each char goes through `dispatch_input`; if the result is
    /// `SearchInputChanged`, the search-debounce side-effect runs.
    pub async fn type_str(&self, s: &str) {
        for c in s.chars() {
            let action = self.press(Key::Char(c)).await;
            self.run(action).await;
        }
    }

    /// Execute the side-effect of a `KeyAction` — the same dispatch the
    /// real run loop performs, minus the bits we don't want in tests
    /// (terminal teardown, real reconnect via librespot).
    pub async fn run(&self, action: KeyAction) {
        match action {
            KeyAction::Stay | KeyAction::Quit => {}
            KeyAction::TogglePlayback => toggle_playback(&self.client, &self.state).await,
            KeyAction::Seek(d) => seek_relative(&self.client, &self.state, d).await,
            KeyAction::NextTrack => {
                skip_track(&self.client, &self.state, true).await;
                refresh_queue(&self.client, &self.state).await;
            }
            KeyAction::PrevTrack => {
                skip_track(&self.client, &self.state, false).await;
                refresh_queue(&self.client, &self.state).await;
            }
            KeyAction::Reconnect => {
                // Tests don't drive the real librespot session — they assert
                // on the higher-level state machine instead.
            }
            KeyAction::EnterSearch => enter_search(&self.client, &self.state).await,
            KeyAction::SearchInputChanged => kick_search(&self.client, &self.state).await,
            KeyAction::PlaySelection => play_selection(&self.client, &self.state).await,
            KeyAction::OpenBrowse(c) => enter_browse(&self.client, &self.state, c).await,
            KeyAction::PlayBrowseSelection => {
                play_browse_selection(&self.client, &self.state).await
            }
            KeyAction::PlayBrowseCollection => {
                play_browse_collection(&self.client, &self.state).await
            }
            KeyAction::LikeCurrent => like_current_track(&self.client, &self.state).await,
            KeyAction::OpenLibrary => enter_library(&self.client, &self.state).await,
            KeyAction::PlayLibrarySelection => {
                play_library_selection(&self.client, &self.state).await
            }
            KeyAction::OpenDevices => open_devices(&self.client, &self.state).await,
            KeyAction::TransferToDevice(id) => {
                transfer_to_device(&self.client, &self.state, id).await
            }
        }
    }

    /// Press a key and execute the resulting action in one step — the
    /// common case in tests that read like "press p".
    pub async fn press_and_run(&self, key: Key) {
        let action = self.press(key).await;
        self.run(action).await;
    }

    /// Force-apply a `Playback` to the state, bypassing `should_accept`. Used
    /// to set up scenarios that assume a track is already loaded — `_force` so
    /// the seed lands regardless of any prior local action in the test.
    pub async fn seed_playback(&self, pb: Playback) {
        apply_playback_force(&self.state, Some(pb)).await;
    }

    pub async fn mode_name(&self) -> &'static str {
        crate::app::mode_name(&*self.state.lock().await)
    }

    /// Wait until all spawned background tasks (browse fetches, search
    /// debounce, post-play polls) have a chance to run. Tests call this
    /// after kicking off async work before asserting on state.
    pub async fn settle(&self) {
        // Search debounce sleeps 250ms before firing; post-play polls
        // tick every 250ms for up to 6 iterations. A few hundred ms covers
        // the common case; longer ones can call settle multiple times.
        for _ in 0..6 {
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
            tokio::task::yield_now().await;
        }
    }

    /// Render the current UI into a 100x40 in-memory buffer and return the
    /// concatenated text. (The UI canvas itself is 96 wide — `ui::FIXED_W` —
    /// so columns 96–99 are always blank padding.) Trailing whitespace on each
    /// line is trimmed for readability in assertions.
    pub async fn snapshot(&self) -> String {
        self.snapshot_sized(100, 40).await
    }

    pub async fn snapshot_sized(&self, width: u16, height: u16) -> String {
        let backend = TestBackend::new(width, height);
        let mut terminal = Terminal::new(backend).expect("test backend");
        {
            let mut s = self.state.lock().await;
            // Tests never produce art (no image protocol), so an empty cache
            // renders identically to production with art disabled.
            let mut art = art::ArtCache::new();
            terminal
                .draw(|f| ui::render(f, &mut s, &mut art))
                .expect("draw failed");
        }
        let buf = terminal.backend().buffer().clone();
        buffer_to_string(&buf, width, height)
    }
}

fn buffer_to_string(buf: &ratatui::buffer::Buffer, width: u16, height: u16) -> String {
    let mut out = String::with_capacity(width as usize * height as usize + height as usize);
    for y in 0..height {
        let mut row = String::with_capacity(width as usize);
        for x in 0..width {
            row.push_str(buf[(x, y)].symbol());
        }
        // Trim trailing spaces to keep assertion text shorter.
        let trimmed = row.trim_end();
        out.push_str(trimmed);
        out.push('\n');
    }
    out
}
