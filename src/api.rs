//! Spotify Web API layer: domain types, the `SpotifyApi` trait, the live
//! `SpotifyClient`, the offline `ReplaySpotify`/`Cassette` record-replay system,
//! and the rate-limit circuit breaker.
//!
//! COMPILED THREE TIMES. As `crate::api` for the `hifi` binary (which only
//! *replays* — it never builds a cassette), and `#[path]`-included by
//! `bin/diag.rs` and `bin/cassette.rs` (hifi is a binary-only crate, no
//! lib.rs). Items marked `#[allow(dead_code)]` (e.g. `Cassette::from_log`,
//! `cassette_key`) are live in one binary and unused in another — do NOT delete
//! them as "dead" or strip the allow without checking all three build targets.

use anyhow::{Context as _, Result};
use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use serde_json::json;
use std::collections::{HashMap, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use crate::auth::Auth;
use crate::log;

const BASE: &str = "https://api.spotify.com/v1";

/// Sliding window we count outbound requests over to estimate our load on
/// Spotify's per-app limiter (Spotify uses ~30s rolling internally).
const THROTTLE_WINDOW: Duration = Duration::from_secs(30);

/// Soft cap for background traffic over `THROTTLE_WINDOW`. When exceeded,
/// background pollers skip their tick; user-initiated requests still go
/// through. Sized well below the 12/30s sustained load that historically
/// tripped a 429 — at 10/30s we sit at a fraction of the burn line with
/// ~7-9 req/30s of headroom on top of the steady-state baseline (1-3
/// /me/player polls per window, depending on play/pause state).
const BACKGROUND_SOFT_CAP: usize = 10;

pub struct SpotifyClient {
    http: reqwest::Client,
    auth: Auth,
    // Mutex (not OnceLock) because reconnect replaces the id when a fresh
    // librespot session comes up.
    device_id: Mutex<Option<String>>,
    /// Single source of truth for the "Spotify told us to back off" state.
    /// Set by `send_logged` on any 429 response, checked by `send_logged`
    /// before sending any new request. This is the hard circuit breaker —
    /// while it's set in the future, no HTTP requests reach Spotify, no
    /// matter which code path tries (poll loops, user actions, reconnect
    /// probes, retry bursts).
    ///
    /// We learned the hard way that piling extra requests on a 429 turns a
    /// short server-side back-off into a multi-hour one.
    rate_limited_until: Mutex<Option<Instant>>,
    /// Timestamps of recent outbound requests. Used by
    /// `background_throttled` to slow down pollers before we actually
    /// trigger a 429. Pruned on every read/write so it stays bounded by
    /// `THROTTLE_WINDOW`.
    recent_requests: Mutex<VecDeque<Instant>>,
    /// When `HIFI_RECORD` is set, every successful read response is teed
    /// (untruncated) into a replay cassette — see [`CassetteRecorder`]. `None`
    /// in normal operation.
    recorder: Option<CassetteRecorder>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Playback {
    pub is_playing: bool,
    pub progress_ms: Option<u64>,
    pub item: Option<Track>,
    #[serde(default)]
    pub context: Option<Context>,
    /// ms since epoch when Spotify last updated this state. Used to detect
    /// stale responses: when librespot doesn't report state, /me/player keeps
    /// returning whichever device reported last, with this timestamp frozen.
    #[serde(default)]
    pub timestamp: Option<u64>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Context {
    pub uri: String,
    #[serde(rename = "type")]
    pub kind: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Track {
    pub id: Option<String>,
    #[serde(default)]
    pub uri: Option<String>,
    pub name: String,
    #[serde(default)]
    pub duration_ms: u64,
    #[serde(default)]
    pub artists: Vec<Artist>,
    // `/albums/{id}/tracks` returns Track objects without a nested `album`
    // (the caller already knows it). Default to a blank Album so the parse
    // doesn't fail on those responses; callers that care fill it in.
    #[serde(default)]
    pub album: Album,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Artist {
    #[serde(default)]
    pub uri: Option<String>,
    pub name: String,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct Album {
    #[serde(default)]
    pub uri: Option<String>,
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub artists: Vec<Artist>,
    #[serde(default)]
    pub images: Vec<Image>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Playlist {
    pub uri: String,
    pub name: String,
    #[serde(default)]
    pub owner: Option<PlaylistOwner>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct PlaylistOwner {
    #[serde(default)]
    pub display_name: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Device {
    pub id: Option<String>,
    pub name: String,
    #[serde(default)]
    pub is_active: bool,
}

#[derive(Debug, Deserialize)]
struct DevicesPayload {
    #[serde(default)]
    devices: Vec<Device>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Image {
    pub url: String,
    pub width: Option<u32>,
}

impl Album {
    /// Pick a reasonable mid-size cover (~300px) — Spotify returns descending sizes.
    pub fn cover_url(&self) -> Option<&str> {
        let mid = self
            .images
            .iter()
            .min_by_key(|i| (i.width.unwrap_or(640) as i32 - 300).abs());
        mid.or_else(|| self.images.first()).map(|i| i.url.as_str())
    }
}

#[derive(Debug, Clone, Default)]
pub struct SearchResults {
    pub tracks: Vec<Track>,
    pub albums: Vec<Album>,
    pub artists: Vec<Artist>,
    pub playlists: Vec<Playlist>,
}

#[derive(Debug, Deserialize)]
struct SearchPayload {
    #[serde(default)]
    tracks: Option<Page<Track>>,
    #[serde(default)]
    albums: Option<Page<Album>>,
    #[serde(default)]
    artists: Option<Page<Artist>>,
    #[serde(default)]
    playlists: Option<Page<Playlist>>,
}

#[derive(Debug, Deserialize)]
struct Page<T> {
    #[serde(default = "Vec::new")]
    items: Vec<Option<T>>,
}

impl<T> Page<T> {
    fn into_items(self) -> Vec<T> {
        self.items.into_iter().flatten().collect()
    }
}

#[derive(Debug, Deserialize)]
struct RecentlyPlayedPage {
    #[serde(default = "Vec::new")]
    items: Vec<RecentlyPlayedItem>,
}

#[derive(Debug, Deserialize)]
struct RecentlyPlayedItem {
    #[serde(default)]
    track: Option<Track>,
}

#[derive(Debug, Deserialize)]
struct AlbumTracksPage {
    #[serde(default = "Vec::new")]
    items: Vec<Track>,
}

#[derive(Debug, Deserialize)]
struct QueueResponse {
    // `/me/player/queue` returns the upcoming tracks here (the currently
    // playing item is a separate `currently_playing` field we don't need).
    // Entries can be tracks or podcast episodes; episodes still parse into
    // Track since every field but `name` is defaulted.
    #[serde(default = "Vec::new")]
    queue: Vec<Track>,
}

#[derive(Debug, Deserialize)]
struct PlaylistTracksPage {
    #[serde(default = "Vec::new")]
    items: Vec<PlaylistTrackItem>,
}

#[derive(Debug, Deserialize)]
struct PlaylistTrackItem {
    #[serde(default)]
    track: Option<Track>,
}

/// `GET /me/tracks` → `{ items: [{ track: Track }] }`.
#[derive(Debug, Deserialize)]
struct SavedTracksPage {
    #[serde(default = "Vec::new")]
    items: Vec<SavedTrackItem>,
}

#[derive(Debug, Deserialize)]
struct SavedTrackItem {
    #[serde(default)]
    track: Option<Track>,
}

/// `GET /me/albums` → `{ items: [{ album: Album }] }`.
#[derive(Debug, Deserialize)]
struct SavedAlbumsPage {
    #[serde(default = "Vec::new")]
    items: Vec<SavedAlbumItem>,
}

#[derive(Debug, Deserialize)]
struct SavedAlbumItem {
    #[serde(default)]
    album: Option<Album>,
}

/// `GET /me/following?type=artist` → `{ artists: { items: [Artist] } }`.
#[derive(Debug, Deserialize)]
struct FollowedArtistsPayload {
    #[serde(default)]
    artists: Option<Page<Artist>>,
}

/// The set of Spotify operations the rest of the app needs. Splitting this
/// out lets tests inject a `FakeSpotify` (see `test_support`) that returns
/// programmed responses without touching the wire — every action handler
/// and the run loop is generic over this trait.
#[async_trait]
pub trait SpotifyApi: Send + Sync {
    fn set_device_id(&self, id: String);
    fn clear_device_id(&self);
    fn rate_limited_until(&self) -> Option<Instant>;
    fn clear_rate_limit(&self);
    /// True when our own 30s sliding-window request counter has crossed the
    /// background soft cap. Background pollers should skip their tick when
    /// this returns true; user-initiated requests ignore it.
    fn background_throttled(&self) -> bool;

    async fn get_playback(&self) -> Result<Option<Playback>>;
    async fn play(&self) -> Result<()>;
    async fn pause(&self) -> Result<()>;
    async fn get_devices(&self) -> Result<Vec<Device>>;
    async fn transfer_playback(&self, device_id: &str, play: bool) -> Result<()>;
    async fn seek_to(&self, position_ms: u64) -> Result<()>;
    async fn next_track(&self) -> Result<()>;
    async fn previous_track(&self) -> Result<()>;
    async fn play_uris(&self, uris: &[String]) -> Result<()>;
    async fn play_context(&self, context_uri: &str, offset_uri: Option<&str>) -> Result<()>;
    async fn search(&self, q: &str) -> Result<SearchResults>;
    async fn get_album_tracks(&self, album_id: &str) -> Result<Vec<Track>>;
    async fn get_playlist_tracks(&self, playlist_id: &str) -> Result<Vec<Track>>;
    async fn get_recently_played(&self, limit: u32) -> Result<Vec<Track>>;
    /// The user's upcoming play queue (`/me/player/queue`). Fetched on demand
    /// when the Now Playing tab is shown — never on a timer, to keep request
    /// volume low.
    async fn get_queue(&self) -> Result<Vec<Track>>;
    async fn save_track(&self, track_id: &str) -> Result<()>;
    /// The user's saved/library collections, for the Library tab. Each is
    /// fetched lazily, on first focus of its sub-tab.
    async fn get_saved_tracks(&self, limit: u32) -> Result<Vec<Track>>;
    async fn get_saved_playlists(&self, limit: u32) -> Result<Vec<Playlist>>;
    async fn get_saved_albums(&self, limit: u32) -> Result<Vec<Album>>;
    async fn get_followed_artists(&self, limit: u32) -> Result<Vec<Artist>>;
}

#[async_trait]
impl SpotifyApi for SpotifyClient {
    fn set_device_id(&self, id: String) {
        SpotifyClient::set_device_id(self, id)
    }
    fn clear_device_id(&self) {
        SpotifyClient::clear_device_id(self)
    }
    fn rate_limited_until(&self) -> Option<Instant> {
        SpotifyClient::rate_limited_until(self)
    }
    fn clear_rate_limit(&self) {
        SpotifyClient::clear_rate_limit(self)
    }
    fn background_throttled(&self) -> bool {
        SpotifyClient::background_throttled(self)
    }
    async fn get_playback(&self) -> Result<Option<Playback>> {
        SpotifyClient::get_playback(self).await
    }
    async fn play(&self) -> Result<()> {
        SpotifyClient::play(self).await
    }
    async fn pause(&self) -> Result<()> {
        SpotifyClient::pause(self).await
    }
    async fn get_devices(&self) -> Result<Vec<Device>> {
        SpotifyClient::get_devices(self).await
    }
    async fn transfer_playback(&self, device_id: &str, play: bool) -> Result<()> {
        SpotifyClient::transfer_playback(self, device_id, play).await
    }
    async fn seek_to(&self, position_ms: u64) -> Result<()> {
        SpotifyClient::seek_to(self, position_ms).await
    }
    async fn next_track(&self) -> Result<()> {
        SpotifyClient::next_track(self).await
    }
    async fn previous_track(&self) -> Result<()> {
        SpotifyClient::previous_track(self).await
    }
    async fn play_uris(&self, uris: &[String]) -> Result<()> {
        SpotifyClient::play_uris(self, uris).await
    }
    async fn play_context(&self, context_uri: &str, offset_uri: Option<&str>) -> Result<()> {
        SpotifyClient::play_context(self, context_uri, offset_uri).await
    }
    async fn search(&self, q: &str) -> Result<SearchResults> {
        SpotifyClient::search(self, q).await
    }
    async fn get_album_tracks(&self, album_id: &str) -> Result<Vec<Track>> {
        SpotifyClient::get_album_tracks(self, album_id).await
    }
    async fn get_playlist_tracks(&self, playlist_id: &str) -> Result<Vec<Track>> {
        SpotifyClient::get_playlist_tracks(self, playlist_id).await
    }
    async fn get_recently_played(&self, limit: u32) -> Result<Vec<Track>> {
        SpotifyClient::get_recently_played(self, limit).await
    }
    async fn get_queue(&self) -> Result<Vec<Track>> {
        SpotifyClient::get_queue(self).await
    }
    async fn save_track(&self, track_id: &str) -> Result<()> {
        SpotifyClient::save_track(self, track_id).await
    }
    async fn get_saved_tracks(&self, limit: u32) -> Result<Vec<Track>> {
        SpotifyClient::get_saved_tracks(self, limit).await
    }
    async fn get_saved_playlists(&self, limit: u32) -> Result<Vec<Playlist>> {
        SpotifyClient::get_saved_playlists(self, limit).await
    }
    async fn get_saved_albums(&self, limit: u32) -> Result<Vec<Album>> {
        SpotifyClient::get_saved_albums(self, limit).await
    }
    async fn get_followed_artists(&self, limit: u32) -> Result<Vec<Artist>> {
        SpotifyClient::get_followed_artists(self, limit).await
    }
}

impl SpotifyClient {
    pub fn new(auth: Auth) -> Result<Self> {
        // Rehydrate any persisted rate-limit deadline so a restart doesn't
        // wipe the gate. Spotify's penalty windows are measured in hours; if
        // we forget across `ctrl-c` we walk straight back into a fresh 429
        // on the very first request after relaunch.
        let rehydrated = load_rate_limit_until();
        if let Some(t) = rehydrated {
            let secs = t.saturating_duration_since(Instant::now()).as_secs();
            log::note(
                "rate_limit gate rehydrated from disk",
                Some(&format!("retry in {secs}s")),
            );
        }
        let recorder = std::env::var("HIFI_RECORD")
            .ok()
            .filter(|s| !s.is_empty())
            .map(CassetteRecorder::new);
        if let Some(rec) = &recorder {
            log::note("cassette recording enabled", Some(&rec.summary()));
        }
        Ok(Self {
            http: reqwest::Client::builder().build()?,
            auth,
            device_id: Mutex::new(None),
            rate_limited_until: Mutex::new(rehydrated),
            recent_requests: Mutex::new(VecDeque::new()),
            recorder,
        })
    }

    /// Append `now` to the sliding window and prune entries older than
    /// `THROTTLE_WINDOW`. Called once per outbound wire request.
    fn record_request(&self) {
        let now = Instant::now();
        let mut q = self.recent_requests.lock().expect("recent_requests poisoned");
        prune_window(&mut q, now);
        q.push_back(now);
    }

    /// Number of outbound requests within the last `THROTTLE_WINDOW`.
    /// Pruning here keeps the window honest even if no requests are being
    /// issued (e.g. while the rate-limit gate is engaged).
    fn recent_request_count(&self) -> usize {
        let now = Instant::now();
        let mut q = self.recent_requests.lock().expect("recent_requests poisoned");
        prune_window(&mut q, now);
        q.len()
    }

    /// Soft cap consumers (background pollers) check before issuing a
    /// request. Logging once-per-engagement is the caller's job — we don't
    /// want to spam the log every 5s while throttled.
    pub fn background_throttled(&self) -> bool {
        self.recent_request_count() >= BACKGROUND_SOFT_CAP
    }

    /// Returns the current rate-limit deadline if one is in effect (i.e. in
    /// the future). Lazily clears expired deadlines so callers see `None`.
    pub fn rate_limited_until(&self) -> Option<Instant> {
        let mut guard = self.rate_limited_until.lock().expect("rate_limit poisoned");
        match *guard {
            Some(t) if t > Instant::now() => Some(t),
            _ => {
                if guard.is_some() {
                    // Expired — also drop the file so a future restart
                    // doesn't re-rehydrate a stale deadline.
                    delete_rate_limit_file();
                }
                *guard = None;
                None
            }
        }
    }

    /// Wipes the rate-limit gate. The user's "get me unstuck" lever — wired
    /// to `:reconnect`. If Spotify is still rate-limiting us server-side,
    /// the next 429 will re-set the gate; this just gives the user a try.
    pub fn clear_rate_limit(&self) {
        *self.rate_limited_until.lock().expect("rate_limit poisoned") = None;
        delete_rate_limit_file();
    }

    fn note_rate_limit(&self, secs: u64) {
        let until = Instant::now() + Duration::from_secs(secs);
        *self.rate_limited_until.lock().expect("rate_limit poisoned") = Some(until);
        save_rate_limit_until(secs);
    }

    pub fn set_device_id(&self, id: String) {
        *self.device_id.lock().expect("device_id poisoned") = Some(id);
    }

    pub fn clear_device_id(&self) {
        *self.device_id.lock().expect("device_id poisoned") = None;
    }

    fn device_id(&self) -> Option<String> {
        self.device_id.lock().expect("device_id poisoned").clone()
    }

    async fn bearer(&self) -> Result<String> {
        Ok(format!("Bearer {}", self.auth.token().await?))
    }

    pub async fn get_playback(&self) -> Result<Option<Playback>> {
        let url = format!("{BASE}/me/player");
        let req = self
            .http
            .get(&url)
            .header("Authorization", self.bearer().await?);
        let (status, body) = self.send_logged(req, "GET", &url, None).await?;
        if status == reqwest::StatusCode::NO_CONTENT || body.is_empty() {
            return Ok(None);
        }
        let pb: Playback = serde_json::from_str(&body).context("parse /me/player body")?;
        Ok(Some(pb))
    }

    pub async fn play(&self) -> Result<()> {
        self.put_play(None).await
    }

    pub async fn get_devices(&self) -> Result<Vec<Device>> {
        let url = format!("{BASE}/me/player/devices");
        let req = self
            .http
            .get(&url)
            .header("Authorization", self.bearer().await?);
        let (_, body) = self.send_logged(req, "GET", &url, None).await?;
        let page: DevicesPayload = serde_json::from_str(&body).context("parse /me/player/devices")?;
        Ok(page.devices)
    }

    /// Tell Spotify to route playback to a given device (without starting playback).
    pub async fn transfer_playback(&self, device_id: &str, play: bool) -> Result<()> {
        let url = format!("{BASE}/me/player");
        let body = json!({ "device_ids": [device_id], "play": play });
        let req = self
            .http
            .put(&url)
            .header("Authorization", self.bearer().await?)
            .json(&body);
        self.send_logged(req, "PUT", &url, Some(&body.to_string()))
            .await
            .map(|_| ())
    }

    pub async fn seek_to(&self, position_ms: u64) -> Result<()> {
        let base = format!("{BASE}/me/player/seek?position_ms={position_ms}");
        let did = self.device_id();
        let url = with_device(&base, did.as_deref());
        let req = self
            .http
            .put(&url)
            .header("Authorization", self.bearer().await?)
            .header("Content-Length", "0");
        self.send_logged(req, "PUT", &url, None).await.map(|_| ())
    }

    pub async fn pause(&self) -> Result<()> {
        let did = self.device_id();
        let url = with_device(&format!("{BASE}/me/player/pause"), did.as_deref());
        let req = self
            .http
            .put(&url)
            .header("Authorization", self.bearer().await?)
            .header("Content-Length", "0");
        self.send_logged(req, "PUT", &url, None).await.map(|_| ())
    }

    pub async fn next_track(&self) -> Result<()> {
        let did = self.device_id();
        let url = with_device(&format!("{BASE}/me/player/next"), did.as_deref());
        let req = self
            .http
            .post(&url)
            .header("Authorization", self.bearer().await?)
            .header("Content-Length", "0");
        self.send_logged(req, "POST", &url, None).await.map(|_| ())
    }

    pub async fn previous_track(&self) -> Result<()> {
        let did = self.device_id();
        let url = with_device(&format!("{BASE}/me/player/previous"), did.as_deref());
        let req = self
            .http
            .post(&url)
            .header("Authorization", self.bearer().await?)
            .header("Content-Length", "0");
        self.send_logged(req, "POST", &url, None).await.map(|_| ())
    }

    pub async fn play_uris(&self, uris: &[String]) -> Result<()> {
        self.put_play(Some(json!({ "uris": uris }))).await
    }

    pub async fn play_context(&self, context_uri: &str, offset_uri: Option<&str>) -> Result<()> {
        let body = match offset_uri {
            Some(u) => json!({ "context_uri": context_uri, "offset": { "uri": u } }),
            None => json!({ "context_uri": context_uri }),
        };
        self.put_play(Some(body)).await
    }

    async fn put_play(&self, body: Option<serde_json::Value>) -> Result<()> {
        let did = self.device_id();
        let url = with_device(&format!("{BASE}/me/player/play"), did.as_deref());
        let mut req = self
            .http
            .put(&url)
            .header("Authorization", self.bearer().await?);
        let body_str = match &body {
            Some(b) => {
                req = req.json(b);
                Some(b.to_string())
            }
            None => {
                req = req.header("Content-Length", "0");
                None
            }
        };
        self.send_logged(req, "PUT", &url, body_str.as_deref())
            .await
            .map(|_| ())
    }

    /// Add a single track to the user's Liked Songs. Requires the
    /// `user-library-modify` scope — a stored token without that scope
    /// will 403; the user must delete `hifi-auth.json` and re-auth.
    pub async fn save_track(&self, track_id: &str) -> Result<()> {
        let url = format!("{BASE}/me/tracks?ids={}", urlencoding::encode(track_id));
        let req = self
            .http
            .put(&url)
            .header("Authorization", self.bearer().await?)
            .header("Content-Length", "0");
        self.send_logged(req, "PUT", &url, None).await.map(|_| ())
    }

    pub async fn search(&self, q: &str) -> Result<SearchResults> {
        let url = format!(
            "{BASE}/search?q={}&type=track,album,artist,playlist&limit=8",
            urlencoding::encode(q)
        );
        let req = self
            .http
            .get(&url)
            .header("Authorization", self.bearer().await?);
        let (_, body) = self.send_logged(req, "GET", &url, None).await?;
        let payload: SearchPayload =
            serde_json::from_str(&body).context("parse /search body")?;
        Ok(SearchResults {
            tracks: payload.tracks.map(Page::into_items).unwrap_or_default(),
            albums: payload.albums.map(Page::into_items).unwrap_or_default(),
            artists: payload.artists.map(Page::into_items).unwrap_or_default(),
            playlists: payload.playlists.map(Page::into_items).unwrap_or_default(),
        })
    }

    pub async fn get_recently_played(&self, limit: u32) -> Result<Vec<Track>> {
        let url = format!("{BASE}/me/player/recently-played?limit={limit}");
        let req = self
            .http
            .get(&url)
            .header("Authorization", self.bearer().await?);
        let (_, body) = self.send_logged(req, "GET", &url, None).await?;
        let page: RecentlyPlayedPage =
            serde_json::from_str(&body).context("parse recently-played")?;
        // Spotify can return the same track multiple times; dedup by uri while
        // preserving order.
        let mut seen = std::collections::HashSet::new();
        let mut out = Vec::new();
        for it in page.items.into_iter().filter_map(|i| i.track) {
            let key = it.uri.clone().unwrap_or_else(|| it.name.clone());
            if seen.insert(key) {
                out.push(it);
            }
        }
        Ok(out)
    }

    pub async fn get_queue(&self) -> Result<Vec<Track>> {
        let url = format!("{BASE}/me/player/queue");
        let req = self
            .http
            .get(&url)
            .header("Authorization", self.bearer().await?);
        let (status, body) = self.send_logged(req, "GET", &url, None).await?;
        // No active session yields a 204 / empty body — no queue to show.
        if status == reqwest::StatusCode::NO_CONTENT || body.is_empty() {
            return Ok(Vec::new());
        }
        let page: QueueResponse =
            serde_json::from_str(&body).context("parse /me/player/queue")?;
        Ok(page.queue)
    }

    pub async fn get_album_tracks(&self, album_id: &str) -> Result<Vec<Track>> {
        // The /albums/{id}/tracks endpoint returns Track objects without a
        // nested `album` (Track defaults that field). Limit 50 is plenty
        // for personal use.
        let url = format!("{BASE}/albums/{album_id}/tracks?limit=50");
        let req = self
            .http
            .get(&url)
            .header("Authorization", self.bearer().await?);
        let (_, body) = self.send_logged(req, "GET", &url, None).await?;
        let page: AlbumTracksPage =
            serde_json::from_str(&body).context("parse album tracks")?;
        Ok(page.items)
    }

    pub async fn get_playlist_tracks(&self, playlist_id: &str) -> Result<Vec<Track>> {
        let fields = urlencoding::encode(
            "items(track(name,uri,id,artists(name),album(name,images)))",
        );
        let url = format!("{BASE}/playlists/{playlist_id}/tracks?limit=100&fields={fields}");
        let req = self
            .http
            .get(&url)
            .header("Authorization", self.bearer().await?);
        let (_, body) = self.send_logged(req, "GET", &url, None).await?;
        let page: PlaylistTracksPage =
            serde_json::from_str(&body).context("parse playlist tracks")?;
        Ok(page.items.into_iter().filter_map(|i| i.track).collect())
    }

    pub async fn get_saved_tracks(&self, limit: u32) -> Result<Vec<Track>> {
        let url = format!("{BASE}/me/tracks?limit={limit}");
        let req = self
            .http
            .get(&url)
            .header("Authorization", self.bearer().await?);
        let (_, body) = self.send_logged(req, "GET", &url, None).await?;
        let page: SavedTracksPage =
            serde_json::from_str(&body).context("parse saved tracks")?;
        Ok(page.items.into_iter().filter_map(|i| i.track).collect())
    }

    pub async fn get_saved_playlists(&self, limit: u32) -> Result<Vec<Playlist>> {
        let url = format!("{BASE}/me/playlists?limit={limit}");
        let req = self
            .http
            .get(&url)
            .header("Authorization", self.bearer().await?);
        let (_, body) = self.send_logged(req, "GET", &url, None).await?;
        let page: Page<Playlist> =
            serde_json::from_str(&body).context("parse saved playlists")?;
        Ok(page.into_items())
    }

    pub async fn get_saved_albums(&self, limit: u32) -> Result<Vec<Album>> {
        let url = format!("{BASE}/me/albums?limit={limit}");
        let req = self
            .http
            .get(&url)
            .header("Authorization", self.bearer().await?);
        let (_, body) = self.send_logged(req, "GET", &url, None).await?;
        let page: SavedAlbumsPage =
            serde_json::from_str(&body).context("parse saved albums")?;
        Ok(page.items.into_iter().filter_map(|i| i.album).collect())
    }

    pub async fn get_followed_artists(&self, limit: u32) -> Result<Vec<Artist>> {
        let url = format!("{BASE}/me/following?type=artist&limit={limit}");
        let req = self
            .http
            .get(&url)
            .header("Authorization", self.bearer().await?);
        let (_, body) = self.send_logged(req, "GET", &url, None).await?;
        let payload: FollowedArtistsPayload =
            serde_json::from_str(&body).context("parse followed artists")?;
        Ok(payload.artists.map(Page::into_items).unwrap_or_default())
    }
}

pub(crate) fn with_device(url: &str, device_id: Option<&str>) -> String {
    match device_id {
        Some(id) if !id.is_empty() => {
            let sep = if url.contains('?') { '&' } else { '?' };
            format!("{url}{sep}device_id={}", urlencoding::encode(id))
        }
        _ => url.to_string(),
    }
}

impl SpotifyClient {
    /// Wrapping send: every HTTP request to Spotify goes through here. This
    /// is the hard rate-limit gate — if we've already received a 429, we
    /// short-circuit *without* hitting the network until the deadline
    /// passes. Callers don't have to remember to check.
    async fn send_logged(
        &self,
        req: reqwest::RequestBuilder,
        method: &'static str,
        url: &str,
        body_json: Option<&str>,
    ) -> Result<(reqwest::StatusCode, String)> {
        // Pre-flight: are we currently rate-limited?
        if let Some(until) = self.rate_limited_until() {
            let secs = until.saturating_duration_since(Instant::now()).as_secs();
            // Don't even log this at api_req — it never went on the wire.
            anyhow::bail!(RateLimited(secs.max(1)));
        }

        // Record the request *before* sending so the count reflects the
        // request we're about to make (matters for any concurrent caller
        // racing the same window).
        self.record_request();

        let req_id = log::next_request_id();
        log::api_req(req_id, method, url, body_json);
        let started = Instant::now();
        let resp = match req.send().await {
            Ok(r) => r,
            Err(e) => {
                let latency = started.elapsed().as_millis() as i64;
                log::api_err(req_id, latency, &format!("{e:#}"));
                return Err(e).with_context(|| format!("{method} {url}"));
            }
        };
        let status = resp.status();
        let latency = started.elapsed().as_millis() as i64;

        if status == reqwest::StatusCode::TOO_MANY_REQUESTS {
            let retry = retry_after_secs(&resp).unwrap_or(30);
            // Set the gate immediately so any concurrent in-flight requests
            // (e.g. from the other poll loop) refuse to fire when they wake.
            self.note_rate_limit(retry);
            log::api_resp(
                req_id,
                status.as_u16(),
                latency,
                Some(&format!("rate-limited; retry-after={retry}s")),
            );
            anyhow::bail!(RateLimited(retry));
        }

        let body_text = resp.text().await.unwrap_or_default();
        log::api_resp(req_id, status.as_u16(), latency, Some(&body_text));

        if !status.is_success() {
            anyhow::bail!("{method} {url}: {status}: {body_text}");
        }
        // Tee the full, untruncated body into the cassette when recording.
        // Unlike the SQLite log (capped at 32 KB), this keeps large library
        // pages intact, so recorded cassettes cover every screen.
        if let Some(rec) = &self.recorder {
            rec.record(method, url, status.as_u16(), &body_text);
        }
        Ok((status, body_text))
    }
}

#[derive(Debug)]
pub struct RateLimited(pub u64);

impl std::fmt::Display for RateLimited {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "rate limited by Spotify; retry after {}s", self.0)
    }
}

impl std::error::Error for RateLimited {}

fn retry_after_secs(resp: &reqwest::Response) -> Option<u64> {
    resp.headers()
        .get(reqwest::header::RETRY_AFTER)?
        .to_str()
        .ok()?
        .parse()
        .ok()
}

/// Drop entries older than `THROTTLE_WINDOW` from the front of `q`.
/// Shared by `record_request` and `recent_request_count` so both see the
/// same view of "what's still in the window".
fn prune_window(q: &mut VecDeque<Instant>, now: Instant) {
    while let Some(&front) = q.front() {
        if now.duration_since(front) > THROTTLE_WINDOW {
            q.pop_front();
        } else {
            break;
        }
    }
}

/// Path for the persisted rate-limit deadline. Honors `HIFI_RATELIMIT_FILE`
/// for tests / non-default deployments; otherwise sits alongside
/// `hifi-auth.json` in the working directory.
fn rate_limit_state_path() -> PathBuf {
    if let Ok(p) = std::env::var("HIFI_RATELIMIT_FILE") {
        return PathBuf::from(p);
    }
    PathBuf::from("hifi-ratelimit.json")
}

#[derive(serde::Serialize, serde::Deserialize)]
struct PersistedRateLimit {
    until_unix_ms: u64,
}

fn now_unix_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

fn save_rate_limit_until(secs_from_now: u64) {
    let until_unix_ms = now_unix_ms().saturating_add(secs_from_now.saturating_mul(1000));
    let path = rate_limit_state_path();
    let payload = PersistedRateLimit { until_unix_ms };
    match serde_json::to_string(&payload) {
        Ok(s) => {
            if let Err(e) = std::fs::write(&path, s) {
                log::error("persist rate_limit", &format!("{e:#}"));
            }
        }
        Err(e) => log::error("persist rate_limit (serialize)", &format!("{e:#}")),
    }
}

fn delete_rate_limit_file() {
    let path = rate_limit_state_path();
    // ENOENT is fine; anything else is unexpected but non-fatal.
    if let Err(e) = std::fs::remove_file(&path) {
        if e.kind() != std::io::ErrorKind::NotFound {
            log::error("clear rate_limit file", &format!("{e:#}"));
        }
    }
}

/// Read the persisted deadline (if any) and translate it from Unix-ms back
/// into an `Instant` measured against the current monotonic clock. Returns
/// `None` if the file is missing, malformed, or already expired.
fn load_rate_limit_until() -> Option<Instant> {
    let path = rate_limit_state_path();
    let s = std::fs::read_to_string(&path).ok()?;
    let persisted: PersistedRateLimit = serde_json::from_str(&s).ok()?;
    let now = now_unix_ms();
    if persisted.until_unix_ms <= now {
        // Stale; drop the file so we don't keep reading it on every boot.
        let _ = std::fs::remove_file(&path);
        return None;
    }
    let remaining_ms = persisted.until_unix_ms - now;
    Some(Instant::now() + Duration::from_millis(remaining_ms))
}

// ---------------------------------------------------------------------------
// Record / replay: an offline `SpotifyApi` backed by captured responses.
//
// The app already logs every request/response to `hifi.log.sqlite`. A cassette
// is just that data distilled into a `{logical-key -> response body}` map that
// `ReplaySpotify` serves without touching the network — so the UI can be
// exercised infinitely with zero rate-limit risk. See `Cassette::from_log`.
// ---------------------------------------------------------------------------

/// Maps a recorded `(method, url)` to the logical key its endpoint is stored
/// under. Returns `None` for requests we don't replay (mutations: play, pause,
/// seek, transfer, …) — those have no body worth serving back.
///
/// CONTRACT: the keys produced here MUST match the literals `ReplaySpotify`
/// reads back (`get_playback` → `"playback"`, `search` → `"search:{q}"`,
/// `get_album_tracks` → `"album_tracks:{id}"`, …). They are written and read in
/// two different places; if they drift, replay silently returns empty (a miss
/// is swallowed in `ReplaySpotify::parsed`), so the offline app just shows a
/// blank screen with no error. Change one side, change the other.
// Used by the `hifi-cassette` bin and tests; the `hifi` bin only replays.
#[allow(dead_code)]
fn cassette_key(method: &str, url: &str) -> Option<String> {
    let path = url.strip_prefix(BASE)?;
    let (path, query) = match path.split_once('?') {
        Some((p, q)) => (p, Some(q)),
        None => (path, None),
    };
    let key = match (method, path) {
        ("GET", "/me/player") => "playback".to_string(),
        ("GET", "/me/player/devices") => "devices".to_string(),
        ("GET", "/me/player/queue") => "queue".to_string(),
        ("GET", "/me/player/recently-played") => "recently_played".to_string(),
        ("GET", "/me/tracks") => "saved_tracks".to_string(),
        ("GET", "/me/playlists") => "saved_playlists".to_string(),
        ("GET", "/me/albums") => "saved_albums".to_string(),
        ("GET", "/me/following") => "followed_artists".to_string(),
        ("GET", "/search") => format!("search:{}", query.and_then(|q| query_param(q, "q"))?),
        ("GET", p) if p.starts_with("/albums/") && p.ends_with("/tracks") => {
            let id = p
                .trim_start_matches("/albums/")
                .trim_end_matches("/tracks");
            format!("album_tracks:{id}")
        }
        ("GET", p) if p.starts_with("/playlists/") && p.ends_with("/tracks") => {
            let id = p
                .trim_start_matches("/playlists/")
                .trim_end_matches("/tracks");
            format!("playlist_tracks:{id}")
        }
        _ => return None,
    };
    Some(key)
}

/// Extract and percent-decode a single query parameter value.
#[allow(dead_code)]
fn query_param(query: &str, name: &str) -> Option<String> {
    let prefix = format!("{name}=");
    query.split('&').find_map(|pair| {
        pair.strip_prefix(&prefix).map(|raw| {
            urlencoding::decode(raw)
                .map(|c| c.into_owned())
                .unwrap_or_else(|_| raw.to_string())
        })
    })
}

/// A set of recorded Spotify responses keyed by logical endpoint. Persisted as
/// plain JSON so cassettes are easy to inspect, hand-edit, and diff.
#[derive(Debug, Default, Clone, Serialize, Deserialize)]
pub struct Cassette {
    /// logical request key (see `cassette_key`) -> raw recorded JSON body
    entries: HashMap<String, String>,
}

// Several methods (new/insert/keys/save/from_log) are used only by the
// `hifi-cassette` bin and tests; the `hifi` bin uses just load/get/len.
#[allow(dead_code)]
impl Cassette {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    pub fn get(&self, key: &str) -> Option<&str> {
        self.entries.get(key).map(String::as_str)
    }

    pub fn insert(&mut self, key: impl Into<String>, body: impl Into<String>) {
        self.entries.insert(key.into(), body.into());
    }

    pub fn keys(&self) -> impl Iterator<Item = &String> {
        self.entries.keys()
    }

    pub fn load(path: impl AsRef<Path>) -> Result<Self> {
        let path = path.as_ref();
        let text = std::fs::read_to_string(path)
            .with_context(|| format!("read cassette {}", path.display()))?;
        serde_json::from_str(&text).context("parse cassette json")
    }

    /// Write the cassette as pretty JSON, atomically (temp file + rename) so a
    /// crash mid-write — the recorder rewrites this live — can't leave a
    /// truncated, unparseable cassette behind.
    pub fn save(&self, path: impl AsRef<Path>) -> Result<()> {
        let path = path.as_ref();
        let text = serde_json::to_string_pretty(self).context("serialize cassette")?;
        let tmp = path.with_extension("json.tmp");
        std::fs::write(&tmp, text).with_context(|| format!("write {}", tmp.display()))?;
        std::fs::rename(&tmp, path)
            .with_context(|| format!("rename {} -> {}", tmp.display(), path.display()))
    }

    /// Mine a cassette out of the SQLite event log the app writes. Pairs each
    /// `api_req` with its `api_resp`, keeps successful / untruncated / non-empty
    /// bodies, maps each to a logical key, and lets the most recent recording
    /// win.
    ///
    /// `request_id` resets to 1 on every process start, so it is *not* unique
    /// across runs. We therefore pair in row (`id`) order — a request stashes
    /// its `(method, url)` under its id; the next response carrying that id
    /// consumes it. A new run's `api_req` overwrites the stash before its own
    /// response arrives, so cross-run ids never mis-pair.
    pub fn from_log(db_path: impl AsRef<Path>) -> Result<Self> {
        let db_path = db_path.as_ref();
        let conn = rusqlite::Connection::open(db_path)
            .with_context(|| format!("open log db {}", db_path.display()))?;
        let mut stmt = conn.prepare(
            "SELECT kind, request_id, method, url, status, body \
             FROM events \
             WHERE kind IN ('api_req', 'api_resp') AND request_id IS NOT NULL \
             ORDER BY id ASC",
        )?;
        let rows = stmt.query_map([], |row| {
            Ok((
                row.get::<_, String>(0)?,         // kind
                row.get::<_, i64>(1)?,            // request_id
                row.get::<_, Option<String>>(2)?, // method
                row.get::<_, Option<String>>(3)?, // url
                row.get::<_, Option<i64>>(4)?,    // status
                row.get::<_, Option<String>>(5)?, // body
            ))
        })?;

        let mut pending: HashMap<i64, (String, String)> = HashMap::new();
        let mut cassette = Cassette::new();
        for row in rows {
            let (kind, request_id, method, url, status, body) = row?;
            match kind.as_str() {
                "api_req" => {
                    if let (Some(m), Some(u)) = (method, url) {
                        pending.insert(request_id, (m, u));
                    }
                }
                "api_resp" => {
                    let ok = matches!(status, Some(s) if (200..300).contains(&s));
                    let body = match body {
                        Some(b) => b,
                        None => continue,
                    };
                    if !ok || body.is_empty() || body.contains("…[truncated]") {
                        continue;
                    }
                    if let Some((m, u)) = pending.get(&request_id) {
                        if let Some(key) = cassette_key(m, u) {
                            cassette.entries.insert(key, body);
                        }
                    }
                }
                _ => {}
            }
        }
        Ok(cassette)
    }
}

/// Tees successful read responses into a [`Cassette`] on disk as the real
/// client runs. Enabled by `HIFI_RECORD=<path>`. Unlike mining the SQLite log,
/// this captures full untruncated bodies, so even the large library pages make
/// it into the cassette. Seeds from any existing file at `path`, so repeated
/// record sessions accumulate coverage rather than overwrite it.
pub struct CassetteRecorder {
    path: PathBuf,
    cassette: Mutex<Cassette>,
}

impl CassetteRecorder {
    pub fn new(path: impl Into<PathBuf>) -> Self {
        let path = path.into();
        let cassette = Cassette::load(&path).unwrap_or_default();
        Self {
            path,
            cassette: Mutex::new(cassette),
        }
    }

    fn summary(&self) -> String {
        let n = self.cassette.lock().expect("recorder poisoned").len();
        format!("{} (seeded with {n} endpoints)", self.path.display())
    }

    /// Record one response. No-op unless it's a successful, non-empty read we
    /// know how to replay (`cassette_key` returns `Some`). Rewrites the file
    /// only when the stored body actually changes, to avoid churn on identical
    /// polls.
    pub fn record(&self, method: &str, url: &str, status: u16, body: &str) {
        if !(200..300).contains(&status) || body.is_empty() {
            return;
        }
        let Some(key) = cassette_key(method, url) else {
            return;
        };
        let mut cassette = self.cassette.lock().expect("recorder poisoned");
        if cassette.get(&key) == Some(body) {
            return;
        }
        cassette.insert(key, body.to_string());
        if let Err(e) = cassette.save(&self.path) {
            log::note("cassette record failed", Some(&format!("{e:#}")));
        }
    }
}

/// An offline [`SpotifyApi`] that serves recorded responses from a [`Cassette`].
/// Reads return real captured JSON; mutations are no-ops, except play/pause
/// (and play_uris/play_context) which flip an in-memory `is_playing` flag so
/// the Now Playing screen visibly responds. It is never rate-limited — that is
/// the whole point.
pub struct ReplaySpotify {
    cassette: Cassette,
    device_id: Mutex<Option<String>>,
    /// Once the user toggles playback, overrides the recorded snapshot's
    /// `is_playing` so play/pause feel live offline.
    play_override: Mutex<Option<bool>>,
}

impl ReplaySpotify {
    pub fn new(cassette: Cassette) -> Self {
        Self {
            cassette,
            device_id: Mutex::new(None),
            play_override: Mutex::new(None),
        }
    }

    /// Parse the body stored under `key` into `T`, or `None` if the cassette
    /// has no such recording (parse failures are logged and treated as a miss
    /// so a single bad entry never crashes the offline app).
    fn parsed<T: serde::de::DeserializeOwned>(&self, key: &str) -> Option<T> {
        let body = self.cassette.get(key)?;
        match serde_json::from_str(body) {
            Ok(v) => Some(v),
            Err(e) => {
                log::note("replay parse failed", Some(&format!("key={key} err={e}")));
                None
            }
        }
    }
}

#[async_trait]
impl SpotifyApi for ReplaySpotify {
    fn set_device_id(&self, id: String) {
        *self.device_id.lock().expect("replay device_id poisoned") = Some(id);
    }
    fn clear_device_id(&self) {
        *self.device_id.lock().expect("replay device_id poisoned") = None;
    }
    fn rate_limited_until(&self) -> Option<Instant> {
        None
    }
    fn clear_rate_limit(&self) {}
    fn background_throttled(&self) -> bool {
        false
    }

    async fn get_playback(&self) -> Result<Option<Playback>> {
        let mut pb: Option<Playback> = self.parsed("playback");
        let over = *self.play_override.lock().expect("replay play_override poisoned");
        if let (Some(p), Some(playing)) = (pb.as_mut(), over) {
            p.is_playing = playing;
        }
        Ok(pb)
    }

    async fn play(&self) -> Result<()> {
        *self.play_override.lock().expect("replay play_override poisoned") = Some(true);
        Ok(())
    }
    async fn pause(&self) -> Result<()> {
        *self.play_override.lock().expect("replay play_override poisoned") = Some(false);
        Ok(())
    }

    async fn get_devices(&self) -> Result<Vec<Device>> {
        Ok(self
            .parsed::<DevicesPayload>("devices")
            .map(|p| p.devices)
            .unwrap_or_default())
    }

    async fn transfer_playback(&self, _device_id: &str, _play: bool) -> Result<()> {
        Ok(())
    }
    async fn seek_to(&self, _position_ms: u64) -> Result<()> {
        Ok(())
    }
    async fn next_track(&self) -> Result<()> {
        Ok(())
    }
    async fn previous_track(&self) -> Result<()> {
        Ok(())
    }
    async fn play_uris(&self, _uris: &[String]) -> Result<()> {
        *self.play_override.lock().expect("replay play_override poisoned") = Some(true);
        Ok(())
    }
    async fn play_context(&self, _context_uri: &str, _offset_uri: Option<&str>) -> Result<()> {
        *self.play_override.lock().expect("replay play_override poisoned") = Some(true);
        Ok(())
    }

    async fn search(&self, q: &str) -> Result<SearchResults> {
        let payload: SearchPayload = match self.parsed(&format!("search:{q}")) {
            Some(p) => p,
            None => return Ok(SearchResults::default()),
        };
        Ok(SearchResults {
            tracks: payload.tracks.map(Page::into_items).unwrap_or_default(),
            albums: payload.albums.map(Page::into_items).unwrap_or_default(),
            artists: payload.artists.map(Page::into_items).unwrap_or_default(),
            playlists: payload.playlists.map(Page::into_items).unwrap_or_default(),
        })
    }

    async fn get_album_tracks(&self, album_id: &str) -> Result<Vec<Track>> {
        Ok(self
            .parsed::<AlbumTracksPage>(&format!("album_tracks:{album_id}"))
            .map(|p| p.items)
            .unwrap_or_default())
    }

    async fn get_playlist_tracks(&self, playlist_id: &str) -> Result<Vec<Track>> {
        Ok(self
            .parsed::<PlaylistTracksPage>(&format!("playlist_tracks:{playlist_id}"))
            .map(|p| p.items.into_iter().filter_map(|i| i.track).collect())
            .unwrap_or_default())
    }

    async fn get_recently_played(&self, _limit: u32) -> Result<Vec<Track>> {
        let page: RecentlyPlayedPage = match self.parsed("recently_played") {
            Some(p) => p,
            None => return Ok(Vec::new()),
        };
        let mut seen = std::collections::HashSet::new();
        let mut out = Vec::new();
        for it in page.items.into_iter().filter_map(|i| i.track) {
            let key = it.uri.clone().unwrap_or_else(|| it.name.clone());
            if seen.insert(key) {
                out.push(it);
            }
        }
        Ok(out)
    }

    async fn get_queue(&self) -> Result<Vec<Track>> {
        Ok(self
            .parsed::<QueueResponse>("queue")
            .map(|p| p.queue)
            .unwrap_or_default())
    }

    async fn save_track(&self, _track_id: &str) -> Result<()> {
        Ok(())
    }

    async fn get_saved_tracks(&self, _limit: u32) -> Result<Vec<Track>> {
        Ok(self
            .parsed::<SavedTracksPage>("saved_tracks")
            .map(|p| p.items.into_iter().filter_map(|i| i.track).collect())
            .unwrap_or_default())
    }

    async fn get_saved_playlists(&self, _limit: u32) -> Result<Vec<Playlist>> {
        Ok(self
            .parsed::<Page<Playlist>>("saved_playlists")
            .map(Page::into_items)
            .unwrap_or_default())
    }

    async fn get_saved_albums(&self, _limit: u32) -> Result<Vec<Album>> {
        Ok(self
            .parsed::<SavedAlbumsPage>("saved_albums")
            .map(|p| p.items.into_iter().filter_map(|i| i.album).collect())
            .unwrap_or_default())
    }

    async fn get_followed_artists(&self, _limit: u32) -> Result<Vec<Artist>> {
        Ok(self
            .parsed::<FollowedArtistsPayload>("followed_artists")
            .and_then(|p| p.artists)
            .map(Page::into_items)
            .unwrap_or_default())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn search_payload_filters_null_items() {
        // Regression: Spotify pads playlists.items with `null` for editorial
        // playlists third-party apps can't access. We must drop those rather
        // than failing the whole parse.
        let body = r#"{
            "tracks": { "items": [
                {"id":"t1","uri":"spotify:track:t1","name":"T1","duration_ms":1000,
                 "artists":[{"name":"A"}],
                 "album":{"name":"Alb","images":[]}}
            ]},
            "albums": { "items": [
                null,
                {"uri":"spotify:album:a1","name":"Alb1","artists":[]}
            ]},
            "artists": { "items": [
                {"uri":"spotify:artist:x","name":"X"}
            ]},
            "playlists": { "items": [
                null,
                {"uri":"spotify:playlist:p1","name":"P1","owner":{"display_name":"o"}},
                null
            ]}
        }"#;
        let payload: SearchPayload = serde_json::from_str(body).expect("must parse");
        let results = SearchResults {
            tracks: payload.tracks.map(Page::into_items).unwrap_or_default(),
            albums: payload.albums.map(Page::into_items).unwrap_or_default(),
            artists: payload.artists.map(Page::into_items).unwrap_or_default(),
            playlists: payload.playlists.map(Page::into_items).unwrap_or_default(),
        };
        assert_eq!(results.tracks.len(), 1);
        assert_eq!(results.albums.len(), 1);
        assert_eq!(results.albums[0].uri.as_deref(), Some("spotify:album:a1"));
        assert_eq!(results.artists.len(), 1);
        assert_eq!(results.playlists.len(), 1);
        assert_eq!(results.playlists[0].uri, "spotify:playlist:p1");
    }

    #[test]
    fn search_payload_missing_sections_default_to_empty() {
        let body = r#"{ "tracks": { "items": [] } }"#;
        let p: SearchPayload = serde_json::from_str(body).unwrap();
        assert!(p.albums.is_none());
        assert!(p.artists.is_none());
        assert!(p.playlists.is_none());
    }

    #[test]
    fn playback_parses_with_context() {
        let body = r#"{
            "is_playing": true,
            "progress_ms": 1234,
            "item": {
                "id":"t1","uri":"spotify:track:t1","name":"T","duration_ms":1000,
                "artists":[{"name":"A"}],
                "album":{"name":"Alb","images":[]}
            },
            "context": {
                "uri":"spotify:playlist:abc",
                "type":"playlist",
                "href":"...",
                "external_urls":{}
            }
        }"#;
        let pb: Playback = serde_json::from_str(body).unwrap();
        assert!(pb.is_playing);
        let ctx = pb.context.expect("context present");
        assert_eq!(ctx.uri, "spotify:playlist:abc");
        assert_eq!(ctx.kind, "playlist");
    }

    #[test]
    fn playback_parses_without_context() {
        let body = r#"{
            "is_playing": false,
            "item": {
                "id":"t1","name":"T","duration_ms":0,
                "artists":[],
                "album":{"name":"Alb","images":[]}
            }
        }"#;
        let pb: Playback = serde_json::from_str(body).unwrap();
        assert!(!pb.is_playing);
        assert!(pb.context.is_none());
    }

    #[test]
    fn with_device_appends_query() {
        let out = with_device("https://api.spotify.com/v1/me/player/play", Some("abc"));
        assert_eq!(
            out,
            "https://api.spotify.com/v1/me/player/play?device_id=abc"
        );
    }

    #[test]
    fn with_device_appends_to_existing_query() {
        let out = with_device("https://x.test/path?q=1", Some("dev/1"));
        assert_eq!(out, "https://x.test/path?q=1&device_id=dev%2F1");
    }

    #[test]
    fn with_device_no_id_is_passthrough() {
        let out = with_device("https://x.test/path", None);
        assert_eq!(out, "https://x.test/path");
        let out = with_device("https://x.test/path", Some(""));
        assert_eq!(out, "https://x.test/path");
    }

    // --- circuit breaker --------------------------------------------------

    /// Exercise the gate's read/write/auto-clear semantics directly. We
    /// can't drive the HTTP-level short-circuit without a mock server, so
    /// these tests pin the state machine that `send_logged` depends on.
    /// Mirrors the three methods on `SpotifyClient` (`rate_limited_until`,
    /// `note_rate_limit`, `clear_rate_limit`) one-to-one.
    fn read_gate(gate: &Mutex<Option<Instant>>) -> Option<Instant> {
        let mut guard = gate.lock().unwrap();
        match *guard {
            Some(t) if t > Instant::now() => Some(t),
            _ => {
                *guard = None;
                None
            }
        }
    }

    #[test]
    fn rate_limit_gate_returns_none_when_unset() {
        let gate: Mutex<Option<Instant>> = Mutex::new(None);
        assert!(read_gate(&gate).is_none());
    }

    #[test]
    fn rate_limit_gate_holds_for_future_deadlines() {
        let gate: Mutex<Option<Instant>> = Mutex::new(Some(
            Instant::now() + std::time::Duration::from_secs(30),
        ));
        assert!(read_gate(&gate).is_some());
    }

    #[test]
    fn rate_limit_gate_auto_clears_past_deadlines() {
        let past = Instant::now()
            .checked_sub(std::time::Duration::from_secs(1))
            .expect("test box must have a 1s old Instant available");
        let gate: Mutex<Option<Instant>> = Mutex::new(Some(past));
        assert!(read_gate(&gate).is_none());
        // Lazy clear: the slot itself should now be None.
        assert!(gate.lock().unwrap().is_none());
    }

    #[test]
    fn rate_limit_gate_clear_wipes_state() {
        let gate: Mutex<Option<Instant>> = Mutex::new(Some(
            Instant::now() + std::time::Duration::from_secs(3600),
        ));
        *gate.lock().unwrap() = None; // mirrors clear_rate_limit
        assert!(read_gate(&gate).is_none());
    }

    /// Exercise the prune step directly: anything older than the window
    /// drops out, anything newer stays.
    #[test]
    fn prune_window_drops_old_entries_only() {
        let now = Instant::now();
        let mut q: VecDeque<Instant> = VecDeque::new();
        q.push_back(now - Duration::from_secs(60)); // way old
        q.push_back(now - Duration::from_secs(31)); // just outside window
        q.push_back(now - Duration::from_secs(29)); // inside
        q.push_back(now - Duration::from_secs(1)); // inside
        prune_window(&mut q, now);
        assert_eq!(q.len(), 2);
    }

    /// Counting + soft-cap engagement via `recent_request_count`-equivalent
    /// path (we drive the deque directly because we don't want this test to
    /// require a live `SpotifyClient` / HTTP).
    #[test]
    fn background_soft_cap_engages_at_threshold() {
        let now = Instant::now();
        let mut q: VecDeque<Instant> = VecDeque::new();
        for i in 0..(BACKGROUND_SOFT_CAP - 1) {
            q.push_back(now - Duration::from_secs(i as u64 % 10));
        }
        prune_window(&mut q, now);
        assert!(
            q.len() < BACKGROUND_SOFT_CAP,
            "len={} should be below soft cap {}",
            q.len(),
            BACKGROUND_SOFT_CAP
        );

        // One more pushes us to the cap.
        q.push_back(now);
        prune_window(&mut q, now);
        assert!(q.len() >= BACKGROUND_SOFT_CAP);
    }

    /// Single combined test for save / load / expired-purge / delete. Kept
    /// as one `#[test]` because `HIFI_RATELIMIT_FILE` is process-global and
    /// splitting into multiple tests would race under cargo's parallel
    /// runner.
    #[test]
    fn rate_limit_persistence_round_trip() {
        let path = std::env::temp_dir().join(format!(
            "hifi-ratelimit-test-{}.json",
            std::process::id()
        ));
        // SAFETY: env writes are unsafe in 2024 edition. Only this test
        // touches HIFI_RATELIMIT_FILE, so the access is effectively serial.
        unsafe {
            std::env::set_var("HIFI_RATELIMIT_FILE", &path);
        }
        let _ = std::fs::remove_file(&path);

        // (1) future deadline → save and reload survives.
        save_rate_limit_until(3600);
        let loaded = load_rate_limit_until().expect("future deadline should rehydrate");
        let remaining = loaded.saturating_duration_since(Instant::now()).as_secs();
        assert!(
            (3590..=3600).contains(&remaining),
            "remaining={remaining} should be ~3600s"
        );

        // (2) delete wipes the file; next load is empty.
        delete_rate_limit_file();
        assert!(load_rate_limit_until().is_none());

        // (3) a past deadline on disk is dropped (and the file removed) so
        // we don't re-rehydrate stale state on every boot.
        std::fs::write(
            &path,
            serde_json::to_string(&PersistedRateLimit {
                until_unix_ms: now_unix_ms().saturating_sub(60_000),
            })
            .unwrap(),
        )
        .unwrap();
        assert!(load_rate_limit_until().is_none());
        assert!(!path.exists(), "expired file should be deleted");

        unsafe {
            std::env::remove_var("HIFI_RATELIMIT_FILE");
        }
    }

    // ----- record / replay -------------------------------------------------

    const PLAYBACK_JSON: &str = r#"{
        "is_playing": true, "progress_ms": 1234,
        "item": {"id":"t1","uri":"spotify:track:t1","name":"My Song","duration_ms":1000,
                 "artists":[{"name":"A"}],"album":{"name":"Alb","images":[]}}
    }"#;
    const SEARCH_JSON: &str = r#"{
        "tracks": {"items": [
            {"id":"t1","uri":"spotify:track:t1","name":"Hit","duration_ms":1000,
             "artists":[{"name":"A"}],"album":{"name":"Alb","images":[]}}
        ]}
    }"#;
    const DEVICES_JSON: &str = r#"{"devices":[{"id":"d1","name":"hifi","is_active":true}]}"#;

    #[test]
    fn cassette_key_maps_read_endpoints() {
        let k = |m, u: &str| cassette_key(m, u);
        let b = "https://api.spotify.com/v1";
        assert_eq!(k("GET", &format!("{b}/me/player")).as_deref(), Some("playback"));
        assert_eq!(
            k("GET", &format!("{b}/me/player/devices")).as_deref(),
            Some("devices")
        );
        assert_eq!(k("GET", &format!("{b}/me/player/queue")).as_deref(), Some("queue"));
        assert_eq!(
            k("GET", &format!("{b}/me/tracks?limit=50")).as_deref(),
            Some("saved_tracks")
        );
        // search q is percent-decoded so it matches what `search(q)` builds.
        assert_eq!(
            k("GET", &format!("{b}/search?q=the%20beatles&type=track&limit=8")).as_deref(),
            Some("search:the beatles")
        );
        assert_eq!(
            k("GET", &format!("{b}/albums/abc123/tracks?limit=50")).as_deref(),
            Some("album_tracks:abc123")
        );
        assert_eq!(
            k("GET", &format!("{b}/playlists/p9/tracks?limit=100")).as_deref(),
            Some("playlist_tracks:p9")
        );
        // Mutations and unknown endpoints are not replayed.
        assert_eq!(k("PUT", &format!("{b}/me/player/play")), None);
        assert_eq!(k("POST", &format!("{b}/me/player/next")), None);
        assert_eq!(k("GET", "https://example.com/whatever"), None);
    }

    #[test]
    fn cassette_roundtrips_through_json() {
        let mut c = Cassette::new();
        c.insert("playback", PLAYBACK_JSON);
        c.insert("search:beatles", SEARCH_JSON);
        let path = std::env::temp_dir().join("hifi_cassette_roundtrip.json");
        c.save(&path).expect("save");
        let loaded = Cassette::load(&path).expect("load");
        assert_eq!(loaded.len(), 2);
        assert_eq!(loaded.get("playback"), Some(PLAYBACK_JSON));
        assert_eq!(loaded.get("search:beatles"), Some(SEARCH_JSON));
        let _ = std::fs::remove_file(&path);
    }

    #[tokio::test]
    async fn replay_serves_recorded_playback() {
        let mut c = Cassette::new();
        c.insert("playback", PLAYBACK_JSON);
        let replay = ReplaySpotify::new(c);
        let pb = replay.get_playback().await.unwrap().expect("playback");
        assert!(pb.is_playing);
        assert_eq!(pb.item.as_ref().unwrap().name, "My Song");
    }

    #[tokio::test]
    async fn replay_play_pause_overrides_is_playing() {
        let mut c = Cassette::new();
        c.insert("playback", PLAYBACK_JSON); // recorded as is_playing: true
        let replay = ReplaySpotify::new(c);

        replay.pause().await.unwrap();
        assert!(!replay.get_playback().await.unwrap().unwrap().is_playing);

        replay.play().await.unwrap();
        assert!(replay.get_playback().await.unwrap().unwrap().is_playing);
    }

    #[tokio::test]
    async fn replay_search_parses_recorded_body() {
        let mut c = Cassette::new();
        c.insert("search:beatles", SEARCH_JSON);
        let replay = ReplaySpotify::new(c);
        let results = replay.search("beatles").await.unwrap();
        assert_eq!(results.tracks.len(), 1);
        assert_eq!(results.tracks[0].name, "Hit");
    }

    #[tokio::test]
    async fn replay_missing_keys_return_empty_not_error() {
        let replay = ReplaySpotify::new(Cassette::new());
        assert!(replay.get_playback().await.unwrap().is_none());
        assert!(replay.get_devices().await.unwrap().is_empty());
        assert!(replay.get_queue().await.unwrap().is_empty());
        assert!(replay.search("nothing recorded").await.unwrap().tracks.is_empty());
        assert!(replay.get_album_tracks("missing").await.unwrap().is_empty());
        // Never rate-limited — that's the whole point of offline replay.
        assert!(replay.rate_limited_until().is_none());
    }

    #[test]
    fn from_log_pairs_by_run_and_prefers_latest_untruncated() {
        // Build a throwaway log DB shaped like the real one, with two "runs"
        // that both reuse request_id 1 (the counter resets per process), a
        // skipped mutation, a truncated body, and a 500 — then assert
        // extraction pairs correctly within each run and keeps the latest.
        let path = std::env::temp_dir().join("hifi_from_log_test.sqlite");
        for suffix in ["", "-wal", "-shm"] {
            let _ = std::fs::remove_file(format!("{}{suffix}", path.display()));
        }
        let b = "https://api.spotify.com/v1";
        {
            let conn = rusqlite::Connection::open(&path).expect("open");
            conn.execute_batch(
                "CREATE TABLE events (
                    id INTEGER PRIMARY KEY AUTOINCREMENT,
                    ts TEXT NOT NULL, ts_unix_ms INTEGER NOT NULL, kind TEXT NOT NULL,
                    request_id INTEGER, method TEXT, url TEXT, status INTEGER,
                    latency_ms INTEGER, body TEXT, detail TEXT);",
            )
            .unwrap();
            let mut ins = |kind: &str, rid: i64, method: Option<&str>, url: Option<&str>, status: Option<i64>, body: Option<&str>| {
                conn.execute(
                    "INSERT INTO events (ts, ts_unix_ms, kind, request_id, method, url, status, latency_ms, body, detail)
                     VALUES ('t', 0, ?1, ?2, ?3, ?4, ?5, 0, ?6, NULL)",
                    rusqlite::params![kind, rid, method, url, status, body],
                )
                .unwrap();
            };
            let old_pb = PLAYBACK_JSON.replace("My Song", "Old Song");
            // run 1
            ins("api_req", 1, Some("GET"), Some(&format!("{b}/me/player")), None, None);
            ins("api_resp", 1, None, None, Some(200), Some(&old_pb));
            ins("api_req", 2, Some("GET"), Some(&format!("{b}/search?q=beatles&type=track&limit=8")), None, None);
            ins("api_resp", 2, None, None, Some(200), Some(SEARCH_JSON));
            ins("api_req", 3, Some("PUT"), Some(&format!("{b}/me/player/play")), None, None); // mutation
            ins("api_resp", 3, None, None, Some(204), Some(""));
            ins("api_req", 4, Some("GET"), Some(&format!("{b}/me/player/devices")), None, None);
            ins("api_resp", 4, None, None, Some(200), Some("{\"devices\":[]} …[truncated]")); // dropped
            // run 2 (request_id resets to 1) — newer playback must win.
            ins("api_req", 1, Some("GET"), Some(&format!("{b}/me/player")), None, None);
            ins("api_resp", 1, None, None, Some(200), Some(PLAYBACK_JSON));
            ins("api_req", 2, Some("GET"), Some(&format!("{b}/me/player/devices")), None, None);
            ins("api_resp", 2, None, None, Some(500), Some("server error")); // non-2xx dropped
        }

        let cassette = Cassette::from_log(&path).expect("from_log");
        // playback: latest run wins ("My Song", not "Old Song").
        let pb = cassette.get("playback").expect("playback present");
        assert!(pb.contains("My Song") && !pb.contains("Old Song"));
        // search captured from run 1.
        assert!(cassette.get("search:beatles").is_some());
        // truncated devices + 500 devices + mutation are all excluded.
        assert!(cassette.get("devices").is_none());
        assert_eq!(cassette.len(), 2, "only playback + search should survive");

        for suffix in ["", "-wal", "-shm"] {
            let _ = std::fs::remove_file(format!("{}{suffix}", path.display()));
        }
    }

    #[test]
    fn recorder_captures_reads_skips_the_rest_and_persists() {
        let path = std::env::temp_dir().join("hifi_recorder_test.json");
        let _ = std::fs::remove_file(&path);
        let b = "https://api.spotify.com/v1";
        {
            let rec = CassetteRecorder::new(&path);
            // A successful read is captured under its logical key...
            rec.record("GET", &format!("{b}/search?q=beatles&type=track"), 200, SEARCH_JSON);
            // ...mutations, non-2xx, and empty bodies are skipped.
            rec.record("PUT", &format!("{b}/me/player/play"), 200, "{}");
            rec.record("GET", &format!("{b}/me/player"), 500, "err");
            rec.record("GET", &format!("{b}/me/player/queue"), 204, "");
        }
        // What landed on disk is a valid, replayable cassette.
        let loaded = Cassette::load(&path).expect("cassette written");
        assert_eq!(loaded.len(), 1);
        assert_eq!(loaded.get("search:beatles"), Some(SEARCH_JSON));

        // A second recorder seeds from the file (coverage accumulates) and a
        // changed body for an existing key overwrites it.
        {
            let rec = CassetteRecorder::new(&path);
            rec.record("GET", &format!("{b}/me/player"), 200, PLAYBACK_JSON);
        }
        let loaded = Cassette::load(&path).expect("cassette written");
        assert_eq!(loaded.len(), 2, "search retained + playback added");
        assert!(loaded.get("playback").unwrap().contains("My Song"));

        let _ = std::fs::remove_file(&path);
    }
}
