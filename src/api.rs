use anyhow::{Context as _, Result};
use async_trait::async_trait;
use serde::Deserialize;
use serde_json::json;
use std::sync::Mutex;
use std::time::Instant;

use crate::auth::Auth;
use crate::log;

const BASE: &str = "https://api.spotify.com/v1";

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
    pub height: Option<u32>,
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
struct PlaylistTracksPage {
    #[serde(default = "Vec::new")]
    items: Vec<PlaylistTrackItem>,
}

#[derive(Debug, Deserialize)]
struct PlaylistTrackItem {
    #[serde(default)]
    track: Option<Track>,
}

/// The set of Spotify operations the rest of the app needs. Splitting this
/// out lets tests inject a `FakeSpotify` (see `test_support`) that returns
/// programmed responses without touching the wire — every action handler
/// and the run loop is generic over this trait.
#[async_trait]
pub trait SpotifyApi: Send + Sync {
    fn set_device_id(&self, id: String);
    fn clear_device_id(&self);
    fn device_id_for_log(&self) -> Option<String>;
    fn rate_limited_until(&self) -> Option<Instant>;
    fn clear_rate_limit(&self);

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
}

#[async_trait]
impl SpotifyApi for SpotifyClient {
    fn set_device_id(&self, id: String) {
        SpotifyClient::set_device_id(self, id)
    }
    fn clear_device_id(&self) {
        SpotifyClient::clear_device_id(self)
    }
    fn device_id_for_log(&self) -> Option<String> {
        SpotifyClient::device_id_for_log(self)
    }
    fn rate_limited_until(&self) -> Option<Instant> {
        SpotifyClient::rate_limited_until(self)
    }
    fn clear_rate_limit(&self) {
        SpotifyClient::clear_rate_limit(self)
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
}

impl SpotifyClient {
    pub fn new(auth: Auth) -> Result<Self> {
        Ok(Self {
            http: reqwest::Client::builder().build()?,
            auth,
            device_id: Mutex::new(None),
            rate_limited_until: Mutex::new(None),
        })
    }

    /// Returns the current rate-limit deadline if one is in effect (i.e. in
    /// the future). Lazily clears expired deadlines so callers see `None`.
    pub fn rate_limited_until(&self) -> Option<Instant> {
        let mut guard = self.rate_limited_until.lock().expect("rate_limit poisoned");
        match *guard {
            Some(t) if t > Instant::now() => Some(t),
            _ => {
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
    }

    fn note_rate_limit(&self, secs: u64) {
        *self.rate_limited_until.lock().expect("rate_limit poisoned") =
            Some(Instant::now() + std::time::Duration::from_secs(secs));
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

    pub fn device_id_for_log(&self) -> Option<String> {
        self.device_id()
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
}
