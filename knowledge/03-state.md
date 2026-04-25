# State Management in `spotify-player`

This doc traces how application state is shaped, shared, mutated, and read across `spotify-player` so that `hifi` can make informed choices. All references are relative to the cloned reference repo at `/Users/chris/repos/hifi/spotify-player/`.

## TL;DR

- One global `Arc<State>` is cloned into every task/thread.
- `State` is **not** an actor. It is a struct of `parking_lot::RwLock` / `Mutex` cells. Anyone with a clone of the `Arc` mutates anything by taking the appropriate lock.
- There is no event-bus / reducer / single writer. Mutations originate in three independent places: the client task (HTTP responses), the librespot streaming task (player events), and the terminal-event handler (UI input).
- The UI thread is a fixed-rate render loop (`app_refresh_duration_in_ms`, default ~32ms). It takes the locks each frame, reads what it needs, and drops them.
- Caches are layered: `ttl_cache::TtlCache` for in-memory hot data (1h TTL, capacity 64), `serde_json` files in `cache_folder` for cold persistence of user library data.

## Top-level `State` / `SharedState`

Defined at `spotify_player/src/state/mod.rs:22-40`.

```text
pub type SharedState = Arc<State>;

pub struct State {
    pub ui:        Mutex<UIState>,        // parking_lot::Mutex
    pub player:    RwLock<PlayerState>,   // parking_lot::RwLock
    pub data:      RwLock<AppData>,       // parking_lot::RwLock
    pub is_daemon: bool,
    #[cfg(feature = "streaming")]
    pub vis_bands: Option<Arc<Mutex<VisBands>>>,
    pub logs:      Arc<Mutex<VecDeque<String>>>,
}
```

Key facts:

- `parking_lot` is re-exported from `state/mod.rs:20` as the project's lock primitives. No `std::sync::Mutex`/`RwLock` and no `tokio::sync::*` for app state.
- `SharedState = Arc<State>` (`state/mod.rs:23`). Cheap `clone()` is the only sharing mechanism. There is **no interior `Arc`** on the sub-state cells — `RwLock<PlayerState>` lives inline, and is reached via `state.player.read()`.
- The single instance is constructed in `main.rs:329` (`Arc::new(state::State::new(...))`) and cloned once per task in `main.rs:154-198`.

## Sub-state modules

| Module                                           | Public type      | Lock                   | Role |
|--------------------------------------------------|------------------|------------------------|------|
| `state/player.rs`                                | `PlayerState`    | `RwLock`               | Spotify-side playback context, devices, queue, custom queue |
| `state/data.rs`                                  | `AppData`        | `RwLock`               | User library + memory caches + browse data |
| `state/ui/mod.rs`                                | `UIState`        | `Mutex` only           | Page history stack, popup, theme, key sequence, mouse-hit rects |
| `state/ui/page.rs`                               | `PageState` enum | (lives inside `UIState`) | Per-page navigation state (list/table cursor, focus) |
| `state/ui/popup.rs`                              | `PopupState`     | (lives inside `UIState`) | Popup payloads + their own list cursors |
| `state/queue.rs`                                 | `CustomQueue`    | (lives inside `PlayerState`) | App-managed batched queue for librespot |
| `state/model.rs`                                 | domain types     | —                      | Plain data: `Track`, `Album`, `Context`, `ContextId`, ... |
| `state/constant.rs`                              | URI constants    | —                      | `USER_LIKED_TRACKS_ID`, etc. |

Convenience aliases for guards are exported at `state/ui/mod.rs:8` (`UIStateGuard<'a> = parking_lot::MutexGuard<'a, UIState>`) and `state/data.rs:13` (`DataReadGuard<'a> = parking_lot::RwLockReadGuard<'a, AppData>`). These flow through the function signatures of `event::*` and `ui::*` so guards are passed by reference rather than re-acquired.

### `PlayerState` (`state/player.rs:7-26`)

```text
PlayerState {
    devices: Vec<Device>,
    playback: Option<rspotify::CurrentPlaybackContext>,
    playback_last_updated_time: Option<Instant>,
    buffered_playback: Option<PlaybackMetadata>,    // smooths out laggy server reflection
    queue: Option<rspotify::CurrentUserQueue>,
    currently_playing_tracks_id: Option<TracksId>,  // for ad-hoc tracks contexts
    custom_queue: Option<CustomQueue>,              // local batch queue
}
```

Two non-obvious things:

- `buffered_playback` exists because the Spotify Web API can take seconds to reflect a change. The app optimistically writes the *expected* metadata to `buffered_playback` (`client/mod.rs:402-406`), then `current_playback()` (`state/player.rs:34-58`) merges the canonical `playback` with `buffered_playback` overrides so the UI feels responsive. See `https://github.com/aome510/spotify-player/issues/109`.
- `playback_progress()` (`state/player.rs:64-80`) extrapolates progress from `playback_last_updated_time` so the bar advances every render even if the server hasn't been re-polled. This means **the UI thread mutates nothing but produces fresh-looking output** — drift is recomputed on each `read()`.

### `AppData` (`state/data.rs:30-34`)

```text
AppData { user_data: UserData, caches: MemoryCaches, browse: BrowseData }
```

- `UserData` (`state/data.rs:36-46`): authoritative library lists — playlists, followed artists, saved albums/shows, saved tracks (as `HashMap<uri, Track>`).
- `MemoryCaches` (`state/data.rs:49-56`): five `ttl_cache::TtlCache` maps, each with capacity 64 and 1-hour TTL (`TTL_CACHE_DURATION` at `state/data.rs:26`):
  - `context: TtlCache<String, Context>` — full playlist/album/artist/show payloads keyed by URI.
  - `search: TtlCache<String, SearchResults>` — keyed by query string.
  - `lyrics: TtlCache<String, Option<Lyrics>>` — `None` is cached so repeat misses don't re-query.
  - `genres: TtlCache<String, Vec<String>>` — keyed by artist name.
  - `images: TtlCache<String, image::DynamicImage>` (cfg `image`).
- `BrowseData` (`state/data.rs:58-63`): Spotify "Browse" categories and category->playlist map. No TTL — populated on demand and never expires within a session.

### `UIState` (`state/ui/mod.rs:26-45`)

```text
UIState {
    is_running: bool,
    theme: Theme,
    input_key_sequence: KeySequence,
    orientation: Orientation,
    history: Vec<PageState>,           // navigation stack (Library is always at idx 0)
    popup: Option<PopupState>,
    playback_progress_bar_rect: Rect,  // for mouse hit-testing
    count_prefix: Option<usize>,       // vim-style 5j / 10k
    last_cover_image_render_info: ImageRenderInfo,  // cfg image
}
```

- Navigation is a **stack** — pushing a new page (`new_page`, `state/ui/mod.rs:63`) appends to `history`; "back" pops. Search filters items via `search_filtered_items` (`state/ui/mod.rs:79`).
- `PageState` (`state/ui/page.rs:8-39`) encodes per-page selection (ratatui `ListState` / `TableState`) inline — losing the page = losing the cursor; that's intentional.
- `MutableWindowState` (`state/ui/page.rs:137-141`) is a small enum unifying `&mut TableState | &mut ListState | &mut usize` so navigation handlers don't care which list widget is focused.
- `Focusable` (`state/ui/page.rs:343-346`) plus a `impl_focusable!` macro (`state/ui/page.rs:396-441`) generate cyclic next/previous focus logic for `LibraryFocusState`, `ArtistFocusState`, `SearchFocusState`.

### `CustomQueue` (`state/queue.rs:55-85`)

A self-contained app-managed queue used when the integrated librespot player is active. It owns the full track list, sends *batches* of URIs to Spotify, and acts at batch boundaries. Worth reading top-to-bottom if `hifi` plans on similar queue control — the file has unit tests (`state/queue.rs:400-660`) covering shuffle, repeat, batch transitions.

## How state is mutated

There is no central reducer. Three unrelated tasks all write to the same locks.

### 1. The client task — most data writes

Spawned at `main.rs:163-168` via `client::start_client_handler`. It pulls `ClientRequest` enum values off a `flume::Receiver` (created at `main.rs:112`) and `tokio::task::spawn`s `handle_request` per message (`client/handlers.rs:37-44`).

`handle_request` (`client/mod.rs:361-640`) is one big `match` over `ClientRequest`. Each arm makes the HTTP call and writes the result back, e.g.:

- `ClientRequest::GetUserPlaylists` writes `state.data.write().user_data.playlists` (`client/mod.rs:458`) AND persists via `store_data_into_file_cache` (`client/mod.rs:452-457`).
- `ClientRequest::GetContext` populates `state.data.write().caches.context` with TTL (`client/mod.rs:539-545`).
- `ClientRequest::GetCurrentPlayback` calls `retrieve_current_playback` which writes `state.player.write()` (`client/mod.rs:1606-1648`).

Each `tokio::task::spawn` per request can run concurrently, so simultaneous writes to different cells (e.g. `data` vs `player`) interleave; per-cell ordering is whatever happens to acquire the write lock first. The code is generally a quick `let x = self.api(...).await?; state.X.write().field = x;` pattern — locks are NOT held across the `.await` boundary, so writers don't starve.

### 2. The librespot streaming task — audio-driven player updates

`streaming.rs:217-260` listens on the librespot player event channel and writes directly:

- `PlayerEvent::Playing` => `state.player.write().buffered_playback.is_playing = true` (`streaming.rs:229-232`).
- `PlayerEvent::Paused` => similarly false (`streaming.rs:237-241`).
- Same task also flips `state.vis_bands.lock().is_active` for the FFT visualizer (`streaming.rs:233-235, 242-244`).

After every event the task calls `client.update_playback(&state)` which spawns 5 GetCurrentPlayback retries on a 1s cadence (`client/mod.rs:668-693`) to reconcile with Spotify's view.

### 3. The terminal-event handler — UI mutations

Spawned at `main.rs:183-192` (`event::start_event_handler`). Blocking-reads `crossterm::event::read()` and dispatches synchronously.

`handle_key_event` (`event/mod.rs:119-193`) takes `state.ui.lock()` once at top and **holds it for the entire dispatch chain** (passed as `&mut UIStateGuard` through `event::page`, `event::popup`, `event::window`). All UI structural changes (pushing pages, opening popups, moving cursors) happen under that single lock.

For data the handler is mostly read-only: `event/page.rs:163` `let data = state.data.read();` then passes `&DataReadGuard` down. There are a handful of writes too — e.g. sorting library lists in place (`event/page.rs:105-159`) takes a write lock just for the sort.

A handler can also send `ClientRequest`s on the flume channel to the client task; those return as future writes by the client.

### Summary of writers per cell

| Cell | Writers |
|------|---------|
| `state.player`  | `client::handle_request`, `client::retrieve_current_playback`, `streaming` task (Playing/Paused), some event-handler arms (`currently_playing_tracks_id`) |
| `state.data`    | `client::handle_request` (HTTP responses, lyrics, search, contexts, library), event-handler sort commands |
| `state.ui`      | event-handler (key/mouse/resize), UI render loop (cover image render info under the same lock it already holds) |
| `state.vis_bands` | streaming task, audio sink |

## How readers (the UI) avoid lock pain

`ui::run` (`ui/mod.rs:36-76`) is a fixed-cadence loop:

```text
loop {
    {
        let mut ui = state.ui.lock();          // exclusive UI lock
        if !ui.is_running { ... exit ... }
        terminal.draw(|frame| {
            render_application(frame, state, &mut ui, rect);
        });
    } // ui guard dropped here
    std::thread::sleep(ui_refresh_duration);   // ~32ms default
}
```

Three patterns of note:

1. **The UI lock is held for the duration of one frame**, including all the `terminal.draw` work. This is fine because the event handler is the only other writer of `ui`, and it runs on a separate thread that simply blocks on `lock()` until the frame ends.
2. **Data/player locks are taken page-locally and short-lived.** Each page renderer does `let data = state.data.read();` (`ui/page.rs:49, 290, 394, 520, 576`) and `let player = state.player.read();` (`ui/playback.rs:30`, `ui/page.rs:753`). Read guards live for the duration of that one section's rendering, which is the only thing happening on the UI thread for that frame, so they're released by the time the next render section runs.
3. **The render loop never `.write()`s to `data` or `player`.** It only mutates `ui` (which it already exclusively holds) — most notably `ui.last_cover_image_render_info` (`ui/playback.rs:85`) and `ui.playback_progress_bar_rect`. This matters: the UI never starves the writers on `data`/`player`, and writers only contend with the brief read scopes.

`parking_lot::RwLock` is *not* writer-preferring by default, but writes here are short and rare enough that starvation hasn't bitten the project. With many readers (multiple page sections all `.read()`ing `data` per frame) and short writers, this works.

### Implication for `hifi`

If `hifi` wants reactive updates instead of a polling redraw, it needs something the reference doesn't have:

- A `Notify` / broadcast channel (e.g. `tokio::sync::Notify` or `tokio::sync::watch`) signalled on every state mutation, with the UI awaiting the notify before it redraws.
- Or a true actor: one task owns mutable state, others send messages and receive snapshots (e.g. via `tokio::sync::watch::Sender<StateSnapshot>` per sub-domain).

Either approach also lets `hifi` ditch the 32ms polling redraw, which currently spins regardless of whether anything changed.

## Caching layer

### In-memory: `ttl_cache::TtlCache`

`MemoryCaches` (`state/data.rs:49-56, 65-76`): five caches, each capacity 64, 1-hour TTL. Eviction is LRU-on-overflow + TTL-on-access. The 64-entry cap means navigating around a large library evicts older contexts.

Inserts always go through `state.data.write().caches.<name>.insert(key, value, *TTL_CACHE_DURATION)` — see `client/mod.rs:391, 544, 556, 1702, 1768`.

Read-before-write is the universal idiom — every cache-populating arm checks `contains_key` under a read lock first to avoid duplicate API calls (`client/mod.rs:384, 494, 548, 1748`).

### On-disk: `serde_json`

`store_data_into_file_cache` / `load_data_from_file_cache` (`state/data.rs:200-232`). Files live under `configs.cache_folder` and are named `{key:?}_cache.json`.

`FileCacheKey` (`state/data.rs:16-23`): `Playlists`, `PlaylistFolders`, `FollowedArtists`, `SavedShows`, `SavedAlbums`, `SavedTracks`.

These are populated on startup in `UserData::new_from_file_caches` (`state/data.rs:122-143`), so the app's initial render shows last-session data instantly, then the client task overwrites both file and in-memory copy on the next API success.

There is also a separate cover-image disk cache (`{cache_folder}/image/<filename>.jpg`, `client/mod.rs:1741-1745`) gated by `enable_cover_image_cache`.

### No DB / no embedded store

There is no SQLite, sled, redb, etc. Everything is JSON files plus in-memory `HashMap`/`Vec`/`TtlCache`.

## Pain points and non-obvious patterns

1. **Locks-everywhere is the architecture.** Every task and thread holds an `Arc<State>` and grabs locks on demand. There is no single source of mutation truth and no replay mechanism. Bugs from forgotten/stale fields (the `buffered_playback` story) are common — note the explicit "Related issue: #109" comment at `state/player.rs:14`.
2. **`UIState` uses `Mutex`, not `RwLock`.** The justification (implicit) is that the renderer mutates UI state every frame for things like `last_cover_image_render_info` and `playback_progress_bar_rect`. If `hifi` wants concurrent UI readers (e.g. for testing/snapshotting), it would need to refactor.
3. **Cross-cell consistency is not guaranteed.** Code that reads both `data` and `player` (e.g. `client/handlers.rs:96-145` page-change handler) takes them as separate guards and can observe a torn view if a writer slips in between. In practice the writes are compatible enough that nobody notices.
4. **Drop ordering matters.** The codebase carefully scopes guards via `let player = state.player.read();` followed by `let curr_item = ...; drop(player);` (see `client/mod.rs:1603-1652`). A `hifi` clippy lint or convention to keep `RwLock`/`Mutex` guard scopes minimal would prevent deadlocks.
5. **No deadlock-by-construction.** Because the same task can take, say, `state.ui.lock()` and `state.data.read()` in different orders across handlers, a deadlock is technically possible. The reference avoids it by convention (UI guard first, then data, then player) but it isn't enforced.
6. **The custom queue lives inside `PlayerState`** despite being app-managed. Mutating shuffle/repeat means reaching into `state.player.write().custom_queue.as_mut()` — verbose and easy to miss. `hifi` could promote this to its own `RwLock` cell.
7. **TTL of 1h on the memory cache is aggressive for a long-running daemon.** If `hifi` targets longer sessions (or always-on use), consider event-driven invalidation (e.g. on a "playlist updated" webhook) rather than time-based expiry.
8. **File cache writes happen on the client task with no debouncing.** Every successful library refresh writes the entire JSON. For very large libraries this is meaningful blocking I/O on the tokio task — `hifi` should either offload this to `tokio::task::spawn_blocking` or batch.
9. **The `vis_bands` Mutex is in the audio hot path.** It's locked from both the audio sink (high-rate writes) and the UI thread (per-frame reads). The reference uses `parking_lot::Mutex` precisely for its low overhead. If `hifi` wants visualisation, this contention pattern is a known design point.
10. **No undo / no event log.** Because mutations are direct lock-and-write, there's nothing to replay. If `hifi` wants Spotify-style "Recently played" or "back to last view" beyond the page stack, it has to build that itself.

## Design implications for `hifi`

- If reactive UI updates and a clearer mutation model are priorities, consider a single-writer pattern: an `actor` task owns `State`, exposes `tokio::sync::watch::Receiver`s for each sub-domain, and accepts mutations via an `mpsc` channel of typed commands. This eliminates the lock soup and makes time-travel/debug snapshots trivial.
- If staying with the locks-everywhere model, at least:
  - Use `parking_lot::RwLock` everywhere (skip `Mutex` for `UIState` if the renderer can be made non-mutating).
  - Add a thin `state::write_*` / `state::read_*` API surface so you can later add change-notification, metrics, and tracing in one place.
  - Consider `arc_swap::ArcSwap<Snapshot>` for read-mostly cells where you're OK cloning whole sub-state into the consumer.
- For caching, pick one tier strategy up front. The reference's mix of `TtlCache + JSON files` works but is ad hoc. `hifi` could use `redb` / `sled` / SQLite for both tiers and get atomic transactions, evictions, and querying for free.
- Keep the page-stack model from `UIState` (`state/ui/mod.rs:33`) — it's clean and lets you implement "back" trivially. But pull selection state out of `PageState` into a per-page-id keyed map if you want cursors to survive re-navigation.
- Treat `buffered_playback` as a lesson: any client-side optimistic state needs to be explicit and named, not papered over with retries. Consider an `OptimisticPlayback` wrapper with a TTL of its own.
