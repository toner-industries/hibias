# Spotify Integration in `spotify-player`

A reference for the `hibias` design. Covers auth, the API client, librespot streaming, and OS media controls. All file paths are relative to `spotify-player/` (the repo we're mirroring).

---

## Crate Choice & Feature Layout

`spotify_player/Cargo.toml` pulls together three Spotify-shaped crate families:

- `rspotify` 0.15.x: typed Web API client. Used in two ways: a custom `Spotify` struct that implements `rspotify::clients::BaseClient` + `OAuthClient` (`spotify_player/src/client/spotify.rs:69`, `:114`), and an optional second `rspotify::AuthCodePkceSpotify` for users who supply their own client ID (`spotify_player/src/client/mod.rs:90`).
- `librespot-core` (always-on, `spotify_player/Cargo.toml:19`) + `librespot-oauth` (`:20`) + `librespot-metadata` (`:22`) for session, OAuth login, and Mercury-based metadata (lyrics, etc.).
- `librespot-connect` and `librespot-playback` (optional, gated behind `streaming` feature, `Cargo.toml:18,21,93`) for becoming a Spotify Connect device.

Feature flags worth noting (`Cargo.toml:84-102`):

- `streaming` enables `librespot-connect` + `librespot-playback` + `rustfft` (visualization).
- Each audio backend is its own feature: `rodio-backend` (default), `alsa-backend`, `pulseaudio-backend`, `portaudio-backend`, `jackaudio-backend`, `rodiojack-backend`, `sdl-backend`, `gstreamer-backend`. Compile-time check at `streaming.rs:19-39` errors if `streaming` is on with no backend selected.
- `media-control` pulls in `souvlaki` + `winit` + `windows` (default on).
- `daemon` requires `streaming` (no UI mode).

> **hibias implication:** keep the same coarse split — a always-compiled API/auth layer, a feature-gated streaming layer, and a feature-gated media-control layer. Match the audio-backend matrix if you want feature parity; otherwise pick one (e.g. just `rodio`) for v1 simplicity.

---

## Authentication

### Strategy

`spotify-player` does **not** use the public Spotify Web API OAuth Authorization Code flow as its primary path. Instead it:

1. Runs a local OAuth flow against Spotify via `librespot-oauth` to get an access token.
2. Wraps the resulting token as a `librespot_core::authentication::Credentials` and feeds it into a `librespot_core::Session`.
3. From that session, it uses `librespot`'s internal `login5` mechanism to mint short-lived Web-API tokens on demand (`spotify_player/src/token.rs:8-46`).

This means: **a single `librespot` session is the source of truth**, and the `rspotify` Web API client is driven by tokens minted from that session — not by an independent OAuth refresh loop.

A second, optional, fully-standard `rspotify::AuthCodePkceSpotify` client is constructed only if the user has put a `client_id` in their config (`client/mod.rs:72-101`). It is used via `Deref` (`client/mod.rs:57-64`) for endpoints rspotify covers cleanly — but for token refresh, even that client uses the librespot-derived token (because `Spotify::refetch_token` is what rspotify calls into; see "Token refresh" below).

### Constants & scopes

`spotify_player/src/auth.rs:6-34`:

- `SPOTIFY_CLIENT_ID = "65b708073fc0480ea92a077233ca87bd"` — the Spotify web app's public client ID, used as default.
- `NCSPOT_CLIENT_ID` is also defined (unused at the read-time of this doc but kept for compatibility).
- `OAUTH_SCOPES` lists 17 scopes covering Connect, playback, playlists, follow, history, and library. The `user-personalized` scope is unsupported by user-provided client IDs and is filtered out at `client/mod.rs:79`.

### `AuthConfig`

`spotify_player/src/auth.rs:36-79` defines:

```rust
pub struct AuthConfig {
    pub cache: librespot_core::cache::Cache,
    pub session_config: librespot_core::config::SessionConfig,
    pub login_redirect_uri: String, // default "http://127.0.0.1:8989/login"
}
```

`AuthConfig::new` (`auth.rs:59-78`) wires the librespot `Cache` to `<cache_folder>` for credentials and (optionally) `<cache_folder>/audio` for audio files when `device.audio_cache` is enabled.

### Login flow

`auth::get_creds(auth_config, reauth, use_cached)` (`auth.rs:87-119`):

1. If `use_cached`, read credentials from `auth_config.cache.credentials()` (librespot's on-disk `credentials.json`).
2. If absent and `reauth` is true: build an `OAuthClientBuilder` (`auth.rs:100-104`) with the Spotify client ID, the redirect URI (`127.0.0.1:8989/login`), and the scope list, call `.open_in_browser()` and `.build()`, then `oauth_client.get_access_token()`. Wrap as `Credentials::with_access_token(t.access_token)`.
3. If absent and `reauth` is false: `bail!`.

`librespot-oauth` handles the local HTTP listener, browser launch, and PKCE; `spotify-player` just invokes it.

### Where credentials live

- The librespot `Cache` writes a `credentials.json` (and friends) under the configured cache folder. Default path is whatever `dirs-next` chooses for `cache_folder` (see `Configs::new` initialization in `main.rs`, plus `cache_folder.join("audio")` for audio cache).
- The user-provided rspotify client (when used) stores its own token at `<cache_folder>/user_client_token.json` (`client/mod.rs:87`).

### Token refresh

The `Spotify` struct enables `Config { token_refreshing: true, .. }` (`client/spotify.rs:43`) so rspotify will call `refetch_token` automatically when a token is expired before each request.

`Spotify::refetch_token` (`client/spotify.rs:86-102`) does:

1. Grab the librespot `Session`.
2. If session is invalid, return the stale token (caller will then create a new session via `check_valid_session` on the next `ClientRequest` — see `handlers.rs:28`).
3. Otherwise call `token::get_token_rspotify(&session)`.

`token::get_token_rspotify` (`token.rs:8-46`):

1. `session.login5().auth_token()` with a 5-second timeout (`token.rs:6`).
2. On timeout, calls `session.shutdown()` to force re-init on the next attempt (`token.rs:21-25`).
3. Converts the librespot token (`access_token`, `expires_in`) into a `rspotify::Token` with `expires_at = now + expires_in`, empty scopes, no refresh token.

`AppClient::token()` (`client/mod.rs:114-125`) is the main accessor — it calls `self.auto_reauth()` (provided by rspotify's `BaseClient` trait) which transparently triggers `refetch_token` when needed, then pulls the access string out from the cached `Token`.

> **hibias implication:** the cleanest port is to copy this sandwich: librespot OAuth for first login + persistent credentials, librespot session + login5 for ongoing token refresh, and a thin Web-API client that you give a `&str` token to. Don't try to do "real" Web API OAuth refresh — Spotify rate-limits PKCE refresh harshly and librespot's tokens are easier and longer-lived.

---

## API Client (`AppClient`)

### Shape

`spotify_player/src/client/mod.rs:46-55`:

```rust
pub struct AppClient {
    http: reqwest::Client,
    spotify: Arc<spotify::Spotify>,           // librespot-backed rspotify client
    auth_config: AuthConfig,
    user_client: Option<rspotify::AuthCodePkceSpotify>, // user-supplied client ID, for endpoints not on default
    #[cfg(feature = "streaming")]
    stream_conn: Arc<Mutex<Option<librespot_connect::Spirc>>>,
}
```

`AppClient` `Deref`s to `&rspotify::AuthCodePkceSpotify` (`mod.rs:57-64`) — when the user provides a client ID, all "standard" rspotify methods route through the user client; otherwise the `expect` panics. *Note*: this means **`user_client` is effectively assumed to be `Some`** for many calls; the bare default-client-id path may panic on those methods. (Possibly a latent bug — keep in mind for hibias.)

The `spotify` field is the **internal** client used in three places:

1. Session management (`set_session`, `session()`).
2. As the implementor of `BaseClient::refetch_token` (so rspotify will use librespot tokens).
3. For low-level Mercury queries — radio/autoplay (`mod.rs:960-996`) and lyrics (`mod.rs:644-661`) hit `session.mercury()` and `librespot_metadata::Lyrics::get` directly.

### Construction & session lifecycle

`AppClient::new()` (`mod.rs:68-112`):

1. Build `AuthConfig` from configs.
2. If user has a custom `client_id`, build `AuthCodePkceSpotify` with the OAuth scopes (minus `user-personalized`), `token_cached: true`, and a cache path. Call `prompt_for_token` immediately.
3. Create empty `Spotify`, `reqwest::Client`, no streaming connection.

`AppClient::new_session(state, reauth)` (`mod.rs:180-217`):

1. Build a fresh `librespot_core::Session` (`auth.rs:55-57`).
2. Call `auth::get_creds`.
3. Set the session on `self.spotify`.
4. If streaming enabled in state: call `new_streaming_connection` (which itself connects the session via `Spirc::new`).
5. Otherwise call `session.connect(creds, true)` directly.
6. Call `self.refresh_token()` (rspotify's, which goes through `refetch_token`).
7. Reset memory caches and run `initialize_playback`.

`AppClient::check_valid_session` (`mod.rs:220-228`) is called before every `ClientRequest` (see `handlers.rs:28`) and rebuilds the session if `session.is_invalid()`.

### Request types

`spotify_player/src/client/request.rs` defines the two request enums fired into the client channel:

- `PlayerRequest` (`request.rs:7-20`): `NextTrack`, `PreviousTrack`, `Resume`, `Pause`, `ResumePause`, `SeekTrack(Duration)`, `Repeat`, `Shuffle`, `Volume(u8)`, `ToggleMute`, `TransferPlayback(String, bool)`, `StartPlayback(Playback, Option<bool>)`.
- `ClientRequest` (`request.rs:24-62`): everything else — `GetCurrentUser`, `GetDevices`, `GetBrowseCategories`, `GetUserPlaylists`, `GetContext(ContextId)`, `Search(String)`, `AddToLibrary`, `DeleteFromLibrary`, `Player(PlayerRequest)`, `GetCurrentUserQueue`, `GetLyrics{track_id}`, `RestartIntegratedClient`, `CreatePlaylist{...}`, plus playlist mutation requests. All variants are `Clone + Debug`.

### Request handler

`client::start_client_handler` (`handlers.rs:22-46`) is the central event loop:

```rust
while let Ok(request) = client_sub.recv_async().await {
    if let Err(err) = client.check_valid_session(state).await { continue; }
    tokio::task::spawn(async move {
        client.handle_request(&state, request).await
    });
}
```

Each request is handled in its own spawned task — no per-request mutex, but the underlying state is `parking_lot`/`tokio::Mutex` guarded.

`AppClient::handle_request` (`mod.rs:361-640`) is a giant `match` that maps each `ClientRequest` variant to:

- One or more rspotify (Deref) calls or `http_get` calls.
- An update to `state.data` or `state.player` (under `RwLock`s).
- Optionally a write to the on-disk file cache (`store_data_into_file_cache`).

### Player request handling

`AppClient::handle_player_request` (`mod.rs:252-358`) is its own dispatcher for `PlayerRequest`. Notable behaviors:

- `TransferPlayback` and `StartPlayback` are handled *before* the "must have an active playback" check (`mod.rs:258-281`).
- For `StartPlayback` it manually re-applies shuffle state because the integrated client doesn't honor the initial shuffle (`mod.rs:273-277`).
- Repeat cycles `Off → Track → Context → Off` (`mod.rs:314-318`).
- `ToggleMute` stashes the previous volume in `playback.mute_state` and restores it (`mod.rs:335-348`).
- Returns `Option<PlaybackMetadata>` so the caller can re-store the optimistic state.

After a player request completes, `AppClient::update_playback` (`mod.rs:668-688`) spawns a task that polls `retrieve_current_playback` 5x with 1s delays — Spotify's server doesn't update state instantaneously.

### Pagination & throttling

`all_paging_items<T>` (`mod.rs:1514-1569`) is the workhorse:

- `PAGE_LIMIT = 50`, `MAX_PARALLEL = 8`.
- Issues up to 8 pages in parallel via `futures::future::try_join_all`.
- Stops early when a page comes back empty (handles Spotify's "infinite trailing empty pages" bug).
- Uses `market=from_token` automatically.

`all_cursor_based_paging_items<T>` (`mod.rs:1572-1589`) is sequential — used for cursor endpoints (recently played, followed artists).

There is **no global throttle/rate-limiter** — the design relies on rspotify's exponential backoff plus the parallelism cap.

### Caching layers

Three distinct caches:

1. **In-memory TTL caches** in `state.data.caches` (`MemoryCaches`). Used for `context`, `search`, `lyrics`, `genres`, `images`. TTL = `*TTL_CACHE_DURATION` (a global const). Checked-then-written pattern in handle_request.
2. **In-memory user-data lists** in `state.data.user_data` for playlists, saved albums, saved tracks (HashMap), saved shows, followed artists. Mutated in-place on add/delete/reorder.
3. **On-disk file cache** under `cache_folder` via `store_data_into_file_cache(FileCacheKey, &cache_folder, &data)` for `Playlists`, `FollowedArtists`, `SavedAlbums`, `SavedShows`, `SavedTracks`. Allows offline startup with stale data.
4. **Optional disk audio cache** (librespot, when `device.audio_cache = true`).
5. **Optional disk image cache** at `<cache_folder>/image/` (`mod.rs:1741-1745`).

> **hibias implication:** the three-tier model (TTL memcache for read-only API responses, per-user-data lists with manual mutation, file cache for fast startup) is solid — replicate it, but consider a unified abstraction (`Cache<Key, Val>` with TTL + persistence) instead of the ad-hoc HashMap-plus-`store_data_into_file_cache` mix.

### Error propagation

- All async client methods return `anyhow::Result<T>`.
- `anyhow::Context as _` is imported at top so `.context("...")` decorates errors at every callsite.
- Inside `start_client_handler`, errors are logged (`tracing::error!`) but **not** propagated — the handler loop continues.
- Inside `handle_player_event` (`handlers.rs:175-185`), errors are also just logged.
- `tracing::info_span!` wraps each request task (`handlers.rs:35`) so logs are structured by request.

### Custom HTTP path

`http_get<T>` (`mod.rs:1477-1512`) is a direct `reqwest` GET that bypasses rspotify. Used for endpoints rspotify can't model cleanly (paginated paths with custom params, browse playlists due to a known rspotify bug at `mod.rs:701-735`). Manually adds `Authorization: Bearer <token>` from `self.token().await`.

### Player event watcher (separate from request handler)

`client::start_player_event_watcher` (`handlers.rs:188-216`) runs in its own thread (spawned in `main.rs:171-179`):

- Loops every 100ms.
- Every `playback_refresh_duration_in_ms` (configurable), pushes `ClientRequest::GetCurrentPlayback`.
- Each tick also calls `handle_player_event`, which:
  - On UI page change, fires `GetContext(ctx_id)` if the new context isn't cached and the timer-throttle (5s, `handlers.rs:140`) has elapsed.
  - On lyrics page, fetches lyrics if track changed.
  - Detects track-end (progress >= duration) and fires `GetCurrentPlayback`.
  - Detects queue drift and fires `GetCurrentUserQueue`.

> **hibias implication:** the polling-based playback watcher is a workaround for Spotify's lack of push events for non-Connect playback. If hibias only ever drives its own integrated player, you can rely on `librespot::PlayerEvent` and skip the polling — see Streaming section below.

---

## Streaming via `librespot` (`streaming.rs`)

### Goal

When the `streaming` feature is enabled, `spotify-player` itself becomes a Spotify Connect device — visible in the Spotify mobile/desktop client's device picker, controllable from any other Spotify client, and able to play audio locally.

### The Spirc protocol (Spotify Connect)

`librespot_connect::Spirc` is the entry point. Spirc is Spotify's Connect-device protocol (over the librespot session's MQTT-like transport). It handles device announcement, command dispatch (play/pause/seek/transfer), and state sync.

### `new_connection` flow

`streaming::new_connection(client, state, session, creds)` (`streaming.rs:143-281`):

1. **Build `ConnectConfig`** (`streaming.rs:157-167`): name from `device.name`, type from `device.device_type` parsed via `DeviceType::from_str` (defaults if invalid), initial volume converted from 0-100 (config) to 0-65535 (librespot's u16 range).
2. **Open a soft mixer** (`SoftMixer::open(MixerConfig::default())`, `streaming.rs:171-174`). Volume goes through the mixer.
3. **Find audio backend** via `audio_backend::find(None)` (`streaming.rs:176`). Returns a factory closure for `Box<dyn Sink>`. The actual backend (rodio, alsa, etc.) is determined by the compiled-in feature.
4. **Build `PlayerConfig`** (`streaming.rs:177-185`): bitrate (96/160/320 kbps from config), normalization on/off, defaults otherwise.
5. **Build `Player`** via `Player::new(player_config, session, mixer.get_soft_volume(), sink_factory)` (`streaming.rs:192-215`). The sink factory closure either returns the raw backend sink, or wraps it in a `VisualizationSink` that intercepts samples for the FFT visualizer (`streaming.rs:200-213`, see `ui::streaming::VisualizationSink`).
6. **Spawn the player event task** (`streaming.rs:217-263`): consumes `player.get_player_event_channel()` async stream, converts each `librespot::player::PlayerEvent` into a local `PlayerEvent` enum (`streaming.rs:42-57, 103-131`), updates `state.player.buffered_playback.is_playing`, kicks the visualizer's `is_active` flag, calls `client.update_playback(&state)`, and optionally executes a configured shell hook (`player_event_hook_command`).
7. **Initialize Spirc** with `Spirc::new(connect_config, session, creds, player, mixer).await` (`streaming.rs:267-269`). Returns `(Spirc, spirc_task: impl Future)`.
8. **Spawn the Spirc task** alongside the player-event task in a `tokio::select!` so either ending shuts down both (`streaming.rs:271-276`).
9. Return the `Spirc` handle to the caller.

`AppClient::new_streaming_connection` (`mod.rs:232-249`) then stores the `Spirc` in `self.stream_conn`, calling `.shutdown()` on the previous one if present.

### Threads/tasks

When streaming is enabled, the runtime spawns:

- A tokio task running `Spirc` (Connect protocol loop).
- A tokio task running the player-event consumer.
- librespot internally spawns its own audio I/O thread per the chosen backend.
- Plus the existing client-handler tokio task and the player-event-watcher OS thread (which still runs but mostly idles since librespot now drives playback state).

### Mapped `PlayerEvent`

`PlayerEvent` enum (`streaming.rs:42-57`):

- `Changed { playable_id }`
- `Playing { playable_id, position_ms }`
- `Paused { playable_id, position_ms }`
- `EndOfTrack { playable_id }`

Conversion from `librespot::player::PlayerEvent` at `streaming.rs:103-131` ignores most internal events (loading, preloading, volume, etc.) and only forwards these four. Each is also serialized to `Vec<String>` args for the user shell hook (`streaming.rs:60-87`).

### Local-device awareness in the API client

When streaming is on, `AppClient` augments device-related operations:

- `GetDevices` (`mod.rs:411-440`) appends a synthetic `Device { id: session.device_id(), name: device.name }` if not already in the API response.
- `find_available_device` (`mod.rs:739-784`) appends the same synthetic local device, then sorts so the user's `default_device` config wins.
- `initialize_playback` (`mod.rs:128-177`) retries 5x to ensure Spotify's server registers the new device before transferring playback.

### Audio backend abstraction

The whole abstraction is `librespot_playback::audio_backend::Sink` and the `audio_backend::find(name) -> impl Fn(Option<String>, AudioFormat) -> Box<dyn Sink>` factory. The `streaming` feature requires exactly one of the eight backend features to be enabled (compile_error at `streaming.rs:19-39`).

> **hibias implication:** copy librespot's Sink-factory pattern verbatim — it's how `VisualizationSink` slots in cleanly. If hibias wants visualization or audio FX, intercepting at the Sink level is the right seam. Default to `rodio-backend` for cross-platform simplicity.

---

## OS Media Controls (`media_control.rs`)

### Library: `souvlaki`

`souvlaki` 0.8.3 (`Cargo.toml:44`) is a cross-platform media-controls crate with three backends:

- **Linux/BSD**: MPRIS over D-Bus.
- **macOS**: `MPNowPlayingInfoCenter` + `MPRemoteCommandCenter`.
- **Windows**: `SystemMediaTransportControls` (SMTC). Requires an HWND, hence the dummy window dance at `media_control.rs:160-262`.

Gated behind `media-control` feature (`Cargo.toml:94`).

### Lifecycle

`media_control::start_event_watcher(state, client_pub)` (`media_control.rs:72-156`) runs on its own OS thread (spawned at `main.rs:201-214`):

1. **Platform setup** (`media_control.rs:78-86`):
   - Linux/macOS: `hwnd = None`.
   - Windows: spawn a `DummyWindow` (`media_control.rs:160-246`) — registers a window class, creates an invisible message-only window with `CreateWindowExW`, returns its `HWND`. Required because SMTC binds to a window handle.
2. **Build `PlatformConfig`** with `dbus_name = "spotify_player"`, `display_name = "Spotify Player"`, `hwnd`.
3. **Construct `MediaControls::new(config)?`**.
4. **Attach event handler** (`media_control.rs:95-137`): a closure that maps each `MediaControlEvent` (`Play`, `Pause`, `Toggle`, `SetPosition`, `Next`, `Previous`, `SetVolume`) to a `ClientRequest::Player(PlayerRequest::*)` and pushes it into the channel.
5. **Set initial playback state** to `Playing { progress: None }` (`media_control.rs:140`) — without this, macOS won't show metadata in the menu bar on startup.
6. **Refresh loop** (`media_control.rs:145-155`): every 1000ms (must be ≥ 1s on Linux because `souvlaki`'s D-Bus impl rate-limits to one event/sec, citation at `media_control.rs:142-144`), call `update_control_metadata` which:
   - Reads current playback from state.
   - Calls `controls.set_playback(MediaPlayback::Playing | Paused { progress })`.
   - Calls `controls.set_metadata(MediaMetadata{ title, album, artist, duration, cover_url })` only if track/episode info changed (string-key dedup via `prev_info`).
7. On Windows, also calls `windows::pump_event_queue()` each iteration to drain `WM_*` messages.

### macOS / Windows main-thread requirement

Because both platforms require their event loops on the main thread, after spawning the media-control thread `main.rs:217-227` runs a `winit::event_loop::EventLoop` on the main thread (no-op handler). This is what keeps macOS/Windows responsive to media keys. Linux doesn't need this.

> **hibias implication:** the souvlaki + winit dance is the standard recipe — copy it. Note especially the macOS startup hack (set `Playing` once for metadata to appear), the Linux 1s rate limit, and the Windows DummyWindow + pump. If hibias targets only Linux, the code becomes much simpler (drop winit, drop Windows module).

---

## Key Types & Traits Surface

For a `hibias`-side designer choosing what to mimic:

### From `auth.rs` / `token.rs`

- `pub struct AuthConfig { cache, session_config, login_redirect_uri }` — auth is configurable but flat.
- `pub fn get_creds(&AuthConfig, reauth: bool, use_cached: bool) -> Result<Credentials>` — the only login entry point.
- `pub async fn get_token_rspotify(&Session) -> Result<rspotify::Token>` — the bridge from librespot session to Web API token.

### From `client/`

- `pub struct AppClient { ... }` (`mod.rs:46`) — clonable, holds Arc'd internals.
- `Deref<Target = AuthCodePkceSpotify>` for ergonomic rspotify method access.
- `pub enum ClientRequest { ... }` — the public command surface; UI sends, handler receives.
- `pub enum PlayerRequest { ... }` — playback sub-surface; nested under `ClientRequest::Player`.
- `pub async fn handle_request(&self, &SharedState, ClientRequest) -> Result<()>` — central dispatcher.
- `pub fn start_client_handler(&SharedState, &AppClient, &Receiver<ClientRequest>)` — async loop.
- `pub fn start_player_event_watcher(&SharedState, &Sender<ClientRequest>)` — sync polling loop on its own thread.

### From `streaming.rs` (feature-gated)

- `pub async fn new_connection(AppClient, SharedState, Session, Credentials) -> Result<Spirc>` — single entry point.
- Internal `PlayerEvent` enum — translation layer between librespot and the rest of the app's playback state.

### From `media_control.rs` (feature-gated)

- `pub fn start_event_watcher(&SharedState, Sender<ClientRequest>) -> Result<(), souvlaki::Error>` — single entry point, blocks the calling thread.

### Channels & shared state

- `flume::Sender<ClientRequest>` / `Receiver<ClientRequest>` — unbounded, cloneable, lock-free.
- `SharedState` (in `state/`, not covered here) holds `player` (`RwLock<PlayerState>`), `data` (`RwLock<DataState>`), and `ui` (`Mutex<UiState>`). The client mutates `player.playback`, `player.buffered_playback`, `data.user_data.*`, `data.caches.*`.

> **hibias implication:** the request-channel pattern is the most reusable bit. UI thread → enum request → flume → tokio handler → mutate `RwLock`-guarded state → UI thread re-reads on next frame. It scales well and avoids any tokio-in-the-UI complexity. Keep it.

---

## Init Order (`main.rs:142-228`)

The exact bring-up sequence for reference:

1. `AppClient::new().await` — builds client, optionally prompts for user-client OAuth.
2. `client.new_session(Some(state), reauth=true).await` — librespot session, credentials, optional Spirc, refresh token.
3. `init_spotify(...)` — fires initial `ClientRequest`s (user, playlists, devices, etc.) into the channel.
4. Spawn `cli::start_socket` task (CLI command server).
5. Spawn `start_client_handler` task.
6. Spawn `start_player_event_watcher` thread.
7. (Non-daemon) Spawn terminal-event-handler thread, UI thread.
8. (If `media-control` enabled) Spawn `media_control::start_event_watcher` thread.
9. (macOS/Windows) Run `winit::EventLoop` on main thread.
10. Otherwise, sleep loop on main thread.
