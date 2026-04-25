# 04 - Events and Commands

How `spotify-player` turns a keystroke into a state mutation or an async API call. Three independent threads cooperate via two `flume` channels and a shared `parking_lot::Mutex`-guarded UI state.

## High-Level Flow

```
+-----------------+        +------------------+        +-----------------+
| crossterm read  | -----> | event handler    | -----> |  ui::run loop   |
| (blocking)      |  Key   | (resolves cmd,   | mutate |  (ratatui draw, |
| terminal-events |        |  mutates ui)     |  ui    |   ~32ms tick)   |
| thread          |        |                  |        |                 |
+-----------------+        +-------+----------+        +-----------------+
                                   |
                                   |  ClientRequest (flume::Sender)
                                   v
                           +-------+----------+        +-----------------+
                           | client_handler   | -----> |  Spotify Web /  |
                           | (tokio task,     |        |  librespot      |
                           |  per-request     | <----- |  responses      |
                           |  spawned)        |  data  |                 |
                           +------------------+        +-----------------+
                                   ^
                                   |  ClientRequest (e.g. GetCurrentPlayback)
                           +-------+----------+
                           | player-event-    |
                           | watcher (100 ms  |
                           | poll thread)     |
                           +------------------+
```

Spawn points are all in `spotify_player/src/main.rs:154-198`:

- `client::start_client_handler` — tokio task, drains `client_sub`.
- `client::start_player_event_watcher` — OS thread, 100 ms polling tick (`spotify_player/src/client/handlers.rs:191`).
- `event::start_event_handler` — OS thread, blocking `crossterm::event::read()`.
- `ui::run` — OS thread, `app_refresh_duration_in_ms` ticked render loop (default 32 ms; `spotify_player/src/config/mod.rs:332`).

A single `flume::unbounded::<ClientRequest>` channel is created at `spotify_player/src/main.rs:112` and cloned to every producer.

## The Terminal Event Loop

`spotify_player/src/event/mod.rs:35-60`:

```rust
pub fn start_event_handler(state: &SharedState, client_pub: &flume::Sender<ClientRequest>) {
    while let Ok(event) = crossterm::event::read() { ... }
}
```

- Library: **`crossterm`** (raw mode + alternate screen + mouse capture enabled in `ui::init_ui`, `spotify_player/src/ui/mod.rs:79-90`).
- The handler is a blocking `read()` loop, not async. It runs on a dedicated OS thread so it never starves tokio.
- Three event kinds are dispatched: `Mouse`, `Resize`, `Key`. Anything else (paste, focus) is ignored.
- Key events filter on `KeyEventKind::Press` only (`event/mod.rs:45-53`) to dodge crossterm's duplicate-press bug on Windows (issue references in source comment).
- **No debouncing.** Throughput is throttled implicitly by the terminal driver and by the render loop's 32 ms sleep — the event thread itself fires per-keystroke and mutates the shared `UIState` immediately.
- **Resize handling** is trivial: `state.ui.lock().orientation = Orientation::from_size(columns, rows)` (`event/mod.rs:40-43`). The render loop also caches `last_terminal_size` and resets the cover-image render on change (`ui/mod.rs:52-60`).

## The `Key` and `KeySequence` Model

`spotify_player/src/key.rs`:

```rust
pub enum Key { Unknown, None(KeyCode), Ctrl(KeyCode), Alt(KeyCode) }
pub struct KeySequence { pub keys: Vec<Key> }
```

- `Key` is a flat enum over crossterm's `KeyCode` plus a modifier tag. **Only `NONE`, `CTRL`, `ALT` are recognised**; `Shift` is folded into the keycode itself (`key.rs:181-183`); any other modifier combo collapses to `Key::Unknown`. There is no Meta/Super/Hyper.
- Stringly format: `enter`, `space`, `tab`, `backspace`, `esc`, `f1..f12`, arrows, `page_up`/`page_down`, `home`/`end`, plus single-char letters. Modifier prefix is `C-` or `M-` (e.g. `C-r`, `M-x`). Parser at `key.rs:65-81`. Sequences are space-separated: `"g c"` is g-then-c (`key.rs:156-165`).
- `KeySequence::is_prefix` (`key.rs:168-173`) is the basis of multi-key chord support.

### Keymap and ActionMap

`spotify_player/src/config/keymap.rs:8-31`:

```rust
pub struct KeymapConfig { pub keymaps: Vec<Keymap>, pub actions: Vec<ActionMap> }
pub struct Keymap   { key_sequence: KeySequence, command: Command }
pub struct ActionMap{ key_sequence: KeySequence, target: ActionTarget, action: Action }
```

Two parallel tables: one binds keys to `Command`s (the global verb set), the other binds keys to context-sensitive `Action`s (verbs that apply to a focused or playing item). Both are flat `Vec`s; lookup is linear. Defaults live in `KeymapConfig::default()` (`config/keymap.rs:33-338`) and a user `keymap.toml` is merged in: user entries override defaults by `key_sequence` equality (`config/keymap.rs:362-383`).

Lookup helpers (`config/keymap.rs:388-444`):

- `find_matched_prefix_keymaps` / `find_matched_prefix_actions` — used to test if a partial sequence is "still in flight."
- `has_matched_prefix` — combined prefix existence test.
- `find_command_from_key_sequence` — exact-match command lookup.
- `find_action_from_key_sequence` — exact-match action lookup.
- `find_command_or_action_from_key_sequence` — tries command first, then action; returns `CommandOrAction`.

### Chord Resolution Algorithm

In `event/mod.rs:119-193`, on each keypress:

1. Translate `KeyEvent` -> `Key`.
2. Push onto `ui.input_key_sequence` (the **pending chord buffer**, lives in `UIState` at `state/ui/mod.rs:30`).
3. **If `keymap_config.has_matched_prefix(&seq)` is false, reset `seq` to just the new key** — i.e. abandon a stale chord rather than waiting for a timeout. There is no chord timeout; the buffer is reset by either a successful match, an unmatchable continuation, or a digit key.
4. Dispatch in priority order:
   1. If a popup is open: `popup::handle_key_sequence_for_popup`.
   2. Else: `page::handle_key_sequence_for_page`.
   3. If neither claimed it, fall through to `handle_global_command` / `handle_global_action`.
5. If handled: clear `input_key_sequence` and `count_prefix`.
6. If not handled and the key is an ASCII digit: accumulate into `count_prefix` (vim-style multiplier — `5j` selects 5 down). Otherwise leave the buffer in place as a possible future prefix.

`count_prefix` is consumed by `handle_navigation_command` (`event/page.rs:567-612`) and multiplied with `page_size_in_rows` for `PageSelectNextOrScrollDown` etc.

## The `Command` Enum

`spotify_player/src/command.rs:8-93`. Roughly 60 variants; the abstraction sits **above keystrokes but below client-API verbs** — they describe user intent in terms of the app's own UI vocabulary, then `event/mod.rs` translates them into either a state mutation or a `ClientRequest`.

| Category | Examples | Where handled |
|---|---|---|
| Playback control | `NextTrack`, `PreviousTrack`, `ResumePause`, `Repeat`, `Shuffle`, `VolumeChange{offset}`, `Mute`, `SeekStart`, `SeekForward{duration}`, `SeekBackward{duration}` | `handle_global_command`, all dispatched as `ClientRequest::Player(PlayerRequest::*)` (`event/mod.rs:580-631`) |
| App lifecycle | `Quit`, `OpenCommandHelp`, `OpenLogs`, `RefreshPlayback`, `RestartIntegratedClient` (cfg `streaming`) | `handle_global_command` |
| Navigation in lists/tables | `SelectNext/PreviousOrScrollDown/Up`, `PageSelect…`, `SelectFirst/LastOrScrollTo…`, `ChooseSelected` | `handle_navigation_command` (`event/page.rs:567`); each window handler calls into it |
| Page navigation | `LibraryPage`, `SearchPage`, `BrowsePage`, `Queue`, `LyricsPage`, `LikedTrackPage`, `TopTrackPage`, `RecentlyPlayedTrackPage`, `CurrentlyPlayingContextPage`, `PreviousPage` | `handle_global_command` — pushes onto `ui.history` and may fire a `ClientRequest::GetContext` |
| Popup openers | `BrowseUserPlaylists`, `BrowseUserFollowedArtists`, `BrowseUserSavedAlbums`, `SwitchTheme`, `SwitchDevice`, `Search`, `CreatePlaylist` | `handle_global_command`; sets `ui.popup` and often sends a fetch request |
| Item actions on selection | `ShowActionsOnSelectedItem`, `ShowActionsOnCurrentTrack`, `ShowActionsOnCurrentContext`, `AddSelectedItemToQueue` | Per-window handlers in `event/window.rs` |
| Focus / window | `FocusNextWindow`, `FocusPreviousWindow` | `handle_global_command` |
| Context ordering / library | `SortTrackBy{Title,Artists,Album,Duration,AddedDate}`, `ReverseTrackOrder`, `SortLibrary{Alphabetically,ByRecent}`, `MovePlaylistItem{Up,Down}` | Mutate `state.data.write()` directly (`event/window.rs:118-145`, `event/page.rs:104-159`); `MovePlaylist*` sends `ClientRequest::ReorderPlaylistItems` |
| Misc | `OpenSpotifyLinkFromClipboard`, `JumpToCurrentTrackInContext`, `JumpToHighlightTrackInContext`, `ClosePopup`, `PlayRandom` | `handle_global_command` and per-window |

Each command has a human-readable `desc()` used by `OpenCommandHelp` (`command.rs:296-379`).

### `Action`, `ActionContext`, `ActionTarget`

`command.rs:95-140`. `Action` is the verb set for **operations on a specific item** (track / album / artist / playlist / show / episode): `GoToArtist`, `GoToAlbum`, `AddToQueue`, `AddToPlaylist`, `AddToLiked`, `DeleteFromLiked`, `CopyLink`, `Follow`, `Unfollow`, `GoToRadio`, `ToggleLiked`, etc.

- `ActionContext` enumerates the *type* of item the action applies to. The big match in `event::handle_action_in_context` (`event/mod.rs:195-469`) is the source of truth for which action × type pairs are valid.
- `ActionTarget` selects between `SelectedItem` (default) and `PlayingTrack`. The latter routes through `handle_global_action` (`event/mod.rs:524-567`) which substitutes the currently-playing track/episode for the cursor selection.
- Per-type "available actions" lists are built by `construct_track_actions`, `construct_album_actions`, etc. (`command.rs:202-294`); the result drives the `ActionList` popup (`PopupState::ActionList(Box<ActionListItem>, ListState)`).

## `ClientRequest` Channel

`spotify_player/src/client/request.rs`:

```rust
pub enum ClientRequest {
    GetCurrentUser, GetDevices, GetBrowseCategories, GetBrowseCategoryPlaylists(Category),
    GetUserPlaylists, GetUserSavedAlbums, GetUserSavedShows, GetUserFollowedArtists,
    GetContext(ContextId), GetCurrentPlayback, Search(String),
    AddPlayableToQueue(PlayableId<'static>), AddAlbumToQueue(AlbumId<'static>),
    AddPlayableToPlaylist(PlaylistId<'static>, PlayableId<'static>),
    DeleteTrackFromPlaylist(PlaylistId<'static>, TrackId<'static>),
    ReorderPlaylistItems { playlist_id, insert_index, range_start, range_length, snapshot_id },
    AddToLibrary(Item), DeleteFromLibrary(ItemId),
    Player(PlayerRequest),                  // playback control sub-verbs
    GetCurrentUserQueue, GetLyrics { track_id }, CreatePlaylist { ... },
    RestartIntegratedClient, // cfg streaming
}

pub enum PlayerRequest {
    NextTrack, PreviousTrack, Resume, Pause, ResumePause,
    SeekTrack(chrono::Duration), Repeat, Shuffle, Volume(u8), ToggleMute,
    TransferPlayback(String, bool), StartPlayback(Playback, Option<bool>),
}
```

### Producer side

Every `client_pub.send(ClientRequest::…)` in the codebase is fire-and-forget. Producers:

- The **event handler** (UI side) — primary user-driven traffic.
- The **mouse handler** for scroll-volume and progress-bar seek (`event/mod.rs:62-116`).
- The **player-event watcher** (`client/handlers.rs:48-216`) — periodic `GetCurrentPlayback`, plus reactive `GetContext` / `GetCurrentUserQueue` when the active page or playing item changes.
- `init_spotify` at startup (`main.rs:33-44`).
- The CLI socket task (`cli::start_socket`) for IPC commands.
- The media-control thread (cfg `media-control`).

### Consumer side

`client::start_client_handler` (`client/handlers.rs:22-46`):

```rust
while let Ok(request) = client_sub.recv_async().await {
    if let Err(err) = client.check_valid_session(state).await { ... continue; }
    let state = state.clone(); let client = client.clone();
    tokio::task::spawn(async move {
        client.handle_request(&state, request).await
    }.instrument(span));
}
```

Each request becomes its **own tokio task**, so requests run concurrently — there's no FIFO guarantee. The handler refreshes the auth session before dispatch.

### Reply path

There is **no reverse channel**. The client mutates `SharedState` directly (writes go through `state.data.write()` / `state.player.write()`, both `parking_lot::RwLock`s). The render loop sees the new data on its next 32 ms tick. This means: no `oneshot`, no future to await on the UI side, no request-id correlation. Loading states are derived implicitly from "data not present yet" checks (e.g. `data.caches.context.contains_key(&id.uri())` in `client/handlers.rs:138`).

## Popup / Page State Machine

The `UIState` (`state/ui/mod.rs:26-45`) holds:

- `history: Vec<PageState>` — back-stack of pages. `PreviousPage` pops, `new_page` pushes (and clears `popup`).
- `popup: Option<PopupState>` — at most one popup at a time.

`PageState` variants (`state/ui/page.rs:8-39`): `Library`, `Context`, `Search`, `Lyrics`, `Browse`, `Queue`, `CommandHelp`, `Logs`. Each carries its own UI sub-state (cursor positions, scroll offsets, focused sub-window).

`PopupState` variants (`state/ui/popup.rs:19-39`): `Search`, `UserPlaylistList`, `UserFollowedArtistList`, `UserSavedAlbumList`, `DeviceList`, `ArtistList`, `ThemeList`, `ActionList`, `PlaylistCreate`, `ConfirmAction`.

### Dispatch precedence

`event/mod.rs:141-162`:

1. **If a popup is open**, `popup::handle_key_sequence_for_popup` (`event/popup.rs:7-316`) gets first shot. Some popups consume raw keys before keymap lookup:
   - `Search` popup — types into the filter query, backspace on empty closes (`event/popup.rs:364-399`).
   - `PlaylistCreate` — Enter submits, Tab/BackTab toggles field, otherwise routed to a `LineInput` (`event/popup.rs:318-362`).
   - `ActionList` — first-character digit `0-9` immediately invokes the n-th action (`event/popup.rs:494-531`).
   - `UserPlaylistList` — text characters extend a fuzzy search query; falls through if not text.
   - `ConfirmAction` — only `y` confirms; any other key cancels (`event/popup.rs:617-644`).
   List-style popups go through `handle_command_for_list_popup` (`event/popup.rs:457-492`) which reduces commands to `SelectPrev/Next`, `ChooseSelected`, `ClosePopup`.
2. **Else** `page::handle_key_sequence_for_page` (`event/page.rs:8-47`) routes by `PageType`:
   - `Search` page bypasses keymap lookup entirely so the user can type free-form into the input field (`event/page.rs:198-357`).
   - `Library`, `Context`, `Browse` route both commands and `SelectedItem` actions.
   - `Lyrics` rejects everything (read-only page).
   - `Queue`, `CommandHelp`, `Logs` only accept navigation commands.
   - `Context` further dispatches to a focused sub-window (`window::handle_command_for_focused_context_window`, `event/window.rs:104-207`); the focused-window pattern lets the same key (e.g. `enter`) mean different things on the album-list vs the related-artists list of an artist page.
3. **Global fallback** (`handle_global_command` / `handle_global_action`) — playback, page navigation, popup openers, app lifecycle. This is also the only level that handles `ActionTarget::PlayingTrack`.

`has_focused_popup()` (`state/ui/mod.rs:71-76`) treats the `Search` popup as non-focused — `FocusNextWindow` etc. work *through* the search filter.

## Mouse Handling

`event/mod.rs:63-116`. Crossterm mouse capture is enabled at startup. Three behaviours:

- **Scroll up/down** -> volume `+= volume_scroll_step` (gated by `app_config.enable_mouse_scroll_volume`, default off).
- **Left click on the playback progress bar** -> compute seek position from x-offset within `state.ui.lock().playback_progress_bar_rect` (set by the renderer in `ui::playback`), send `PlayerRequest::SeekTrack`.
- Everything else is ignored. **No click-to-select on lists, no drag.** The progress-bar rect is the only mouse-aware region.

## Concrete Example: Pressing `Enter` on a Track in a Playlist

1. `crossterm::event::read()` returns `Key(Enter)` on the event thread.
2. `handle_key_event` -> seq `[Enter]` matches `Command::ChooseSelected` (default keymap, `config/keymap.rs:87-89`).
3. `ui.popup.is_none()`, current page is `PageState::Context { id: Some(Playlist(...)), ... }`. Dispatch -> `page::handle_command_for_context_page` -> `window::handle_command_for_focused_context_window`.
4. Context type resolves to `Playlist { tracks, ... }` -> `handle_command_for_track_table_window` (`event/window.rs:261-372`).
5. After `handle_navigation_command` declines (it doesn't handle `ChooseSelected`), the `match command` arm builds a `Playback::Context(playlist_id, Some(uri_offset))` and sends `ClientRequest::Player(PlayerRequest::StartPlayback(...))`.
6. On the tokio side, `start_client_handler` spawns a task; `client.handle_request` calls librespot/Web-API to start playback.
7. Independently, `start_player_event_watcher` notices the playback change on its next 100 ms tick and queues `GetCurrentPlayback` and possibly `GetCurrentUserQueue` (`client/handlers.rs:48-89`). Those tasks update `state.player` / `state.data`.
8. Within ~32 ms `ui::run` redraws using the new `SharedState`.

## Notes for `hifi`'s Design

- The two-channel model (UI -> client `flume::unbounded`, no reverse channel; render-on-poll instead) is simple and avoids future-correlation, at the cost of any explicit "request finished / failed" notifications. Errors only land in `tracing::error!` logs.
- Keymap lookup is O(N) over a `Vec` per keypress; fine at default size (~80 entries) but a `HashMap<KeySequence, Command>` would be trivial to swap in.
- The chord buffer has no timeout — chords are aborted only by a non-prefix continuation or by a successful match. For `hifi`, consider adding a configurable `chord_timeout_ms`.
- The popup-vs-page-vs-global dispatch ladder is implemented as plain function calls; there's no Trait-object `EventHandler` interface. Each `PageType` and each `PopupState` has its own bespoke handler function. Adding a new page = adding another arm in `handle_key_sequence_for_page`.
- `Action` vs `Command` is a useful split — verbs that need an object (a track) vs verbs that don't (next track, quit). Worth preserving.
- Mouse support is minimal; not a bad starting target for `hifi`.

## File Index (cited)

- `spotify_player/src/event/mod.rs` — top-level event loop, key/mouse dispatch, global command/action handlers.
- `spotify_player/src/event/page.rs` — per-page dispatch, navigation primitive.
- `spotify_player/src/event/popup.rs` — per-popup dispatch, list-popup helper.
- `spotify_player/src/event/window.rs` — context-page focused-window handlers.
- `spotify_player/src/event/clipboard.rs` — clipboard provider abstraction (pbcopy, wl-copy, xclip, xsel, Win32).
- `spotify_player/src/key.rs` — `Key`, `KeySequence`, parsing, `From<KeyEvent>`.
- `spotify_player/src/command.rs` — `Command`, `Action`, `ActionContext`, `ActionTarget`, available-action constructors, `desc()`.
- `spotify_player/src/config/keymap.rs` — `KeymapConfig`, default bindings, lookup methods.
- `spotify_player/src/client/request.rs` — `ClientRequest`, `PlayerRequest`.
- `spotify_player/src/client/handlers.rs` — `start_client_handler`, `start_player_event_watcher`, change-detection logic.
- `spotify_player/src/state/ui/mod.rs` — `UIState`, `input_key_sequence`, `count_prefix`, `history`, `popup`.
- `spotify_player/src/state/ui/page.rs` — `PageState`, `PageType`.
- `spotify_player/src/state/ui/popup.rs` — `PopupState`, `ActionListItem`, `PlaylistPopupAction`, `ConfirmableAction`.
- `spotify_player/src/main.rs` — thread/task spawn topology, `flume` channel construction.
- `spotify_player/src/ui/mod.rs` — render loop, terminal init/cleanup.
