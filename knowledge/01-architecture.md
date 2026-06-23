# 01 — Overall Architecture & Crate Layout

This document describes how `spotify-player` is structured: workspace layout, module
boundaries inside the binary crate, the runtime model, the threads/tasks spawned at
startup, the channels that connect them, and the shutdown flow. It is the
reference for designing the analogous bones of `hibias`.

## Workspace crates

The repo (`spotify-player/Cargo.toml:1-3`) is a 2-member Cargo workspace using
resolver 2:

- **`spotify_player`** — the actual binary crate. All UI, client, state, event,
  and streaming logic lives here. `spotify_player/Cargo.toml:1-9`.
- **`lyric_finder`** — a small standalone library crate that scrapes Genius
  for lyrics. It is a completely independent library (its only deps of note are
  `reqwest`, `html5ever`, `markup5ever_rcdom`) and is published separately to
  crates.io. `lyric_finder/Cargo.toml:1-18`.

Workspace-wide lints in `spotify-player/Cargo.toml:8-24` enforce
`unsafe_code = "deny"` and `clippy::pedantic = deny` with a curated set of
relaxations (`module_name_repetitions`, several `cast_*`, `similar_names`,
`too_many_lines`, `missing_errors_doc`).

Takeaway for `hibias`: a workspace is worth doing only when you have a genuinely
reusable, decoupled library. Otherwise a single crate is simpler. `lyric_finder`
is a good template for splitting out a network-bound, framework-free helper.

## Top-level dependencies (binary crate)

Key crates in `spotify_player/Cargo.toml`:

- **TUI**: `ratatui` 0.30, `crossterm` 0.29 (line 36, 16).
- **Async runtime**: `tokio` with `rt`, `rt-multi-thread`, `macros`, `time`
  (lines 29-34). Notably **no** `net`, `io-util`, `signal`, `sync`, etc. — only
  the runtime, timers, and macros are pulled in.
- **Channels**: `flume` 0.12 (line 51) — used for all in-process work queues
  (chosen because it is sync- and async-friendly on both ends, unlike
  `tokio::sync::mpsc`).
- **Sync primitives**: `parking_lot` 0.12 (line 40) for `Mutex`/`RwLock` over
  application state; `std::sync::OnceLock` for the global config singleton.
- **Spotify**: `rspotify` 0.15 (Web API client) and the `librespot-*` family
  (`-core`, `-oauth`, `-metadata`, `-connect`/`-playback` behind a feature)
  (lines 18-27).
- **HTTP**: `reqwest` 0.13, `rustls` (with `ring` provider, line 61).
- **Logging**: `tracing` + `tracing-subscriber` with env-filter (lines 41-42).
- **Config**: `toml`, `serde`, `config_parser2`, `clap` for CLI parsing.
- **Optional**: `souvlaki` (OS media-control), `viuer`/`image` (cover art),
  `notify-rust` (desktop notifications), `daemonize`, `rustfft` (FFT for
  visualizer), `fuzzy-matcher`, `winit` (Win/macOS event loop for media keys).

The default feature set (line 102) is `["rodio-backend", "media-control"]`
which transitively pulls in `streaming` (= `librespot-playback` +
`librespot-connect` + `rustfft`).

## Module structure of `spotify_player/src/`

Declared in `spotify_player/src/main.rs:1-17`:

```
spotify_player/src/
├── main.rs                  # entry point, wiring, runtime bootstrap
├── auth.rs                  # librespot Credentials acquisition + OAuth login
├── token.rs                 # rspotify Token <-> librespot Session bridge
├── log_layer.rs             # tracing Layer that buffers logs in-memory for the Logs page
├── playlist_folders.rs      # Spotify playlist-folder hierarchy fetching
├── command.rs               # Command/Action enums (UI-level intents)
├── key.rs                   # Key & KeySequence types
├── utils.rs                 # small helpers (parse_uri, map_join, etc.)
├── media_control.rs         # OS media-key integration (souvlaki) — feature-gated
├── streaming.rs             # librespot integrated player + Spirc Connect device — feature-gated
├── cli/
│   ├── mod.rs               # Request/Response wire types + `init_cli` clap command tree
│   ├── client.rs            # UDP socket server `start_socket` (in-process IPC)
│   ├── commands.rs          # clap subcommand definitions
│   └── handlers.rs          # `handle_cli_subcommand` — sends request to running daemon
├── client/
│   ├── mod.rs               # `AppClient` (Web API + librespot session); `handle_request`
│   ├── handlers.rs          # `start_client_handler` (consumes ClientRequest channel)
│   │                        # + `start_player_event_watcher` (100ms polling loop)
│   ├── request.rs           # `ClientRequest` and `PlayerRequest` enums
│   └── spotify.rs           # Custom `rspotify::BaseClient` impl wired to librespot session
├── config/
│   ├── mod.rs               # `Configs` struct + global `OnceLock` + override engine
│   ├── keymap.rs            # keymap config parsing
│   └── theme.rs             # theme config parsing
├── state/
│   ├── mod.rs               # `State { ui, player, data, ... }` + `SharedState = Arc<State>`
│   ├── constant.rs          # ID/URI constants for "liked tracks", "top tracks", etc.
│   ├── data.rs              # `AppData` (user data, in-memory caches, browse data)
│   ├── model.rs             # Domain types: Track, Album, Artist, Context, Playlist, …
│   ├── player.rs            # `PlayerState` (current playback, queue, buffered playback)
│   ├── queue.rs             # local queue model
│   └── ui/                  # `UIState`, `PageState`, `PopupState`, focus enums
├── event/
│   ├── mod.rs               # `start_event_handler` blocking on crossterm::event::read
│   ├── page.rs              # per-page key handling
│   ├── popup.rs             # per-popup key handling
│   ├── window.rs            # window/list key handling
│   └── clipboard.rs         # OS clipboard helpers
└── ui/
    ├── mod.rs               # `run` (render loop), `init_ui`, `clean_up`
    ├── page.rs              # render_*_page functions
    ├── popup.rs             # popup rendering
    ├── playback.rs          # playback bar rendering
    ├── single_line_input.rs # text-input widget
    ├── streaming.rs         # FFT VisualizationSink + visualization rendering — feature-gated
    └── utils.rs             # rendering helpers
```

Total LOC in the largest hubs: `client/mod.rs` ~2000 lines, `event/mod.rs`
~900, `state/model.rs` ~765, `state/queue.rs` ~660, `config/mod.rs` ~553. The
client and event handlers are by far the biggest accumulation points — they
fan in many small request types and many small key-bindings respectively.

The split is roughly **state / behavior on state**:
- `state/` owns the data structures (and exposes `parking_lot` guards).
- `client/` mutates state in response to async API calls.
- `event/` mutates state in response to terminal input.
- `ui/` reads state and draws frames.
- `config/` is read-only after startup.
- `cli/` is a separate IPC surface, not the interactive UI.

## main.rs walkthrough

`main` is a *synchronous* `fn main() -> Result<()>` (`main.rs:236`). It does
the following in order:

1. **Install rustls crypto provider** (`main.rs:239-241`). Required because
   `librespot` pulls in `hyper-rustls` which now demands an explicit provider.
2. **Parse CLI args** with clap via `cli::init_cli()?.get_matches()`
   (`main.rs:244`).
3. **Resolve & create config and cache folders** (`main.rs:247-266`).
4. **Build & install the global `Configs`** (`main.rs:269-285`). Applies any
   `-o key=value` overrides via `apply_config_override`. The result is stored
   in a process-wide `OnceLock<Configs>` (`config/mod.rs:28`, accessor at
   `config/mod.rs:513-520`).
5. **Branch on subcommand** (`main.rs:287-333`):
   - **No subcommand** → run the interactive app: initialize logging, create
     the shared `State`, optionally `daemonize`, then call `start_app(&state)`.
   - **Some subcommand** → call `cli::handle_cli_subcommand`, which speaks to
     a *running* `spotify_player` instance over a localhost UDP socket; if no
     instance is running it spawns a tiny tokio runtime + thread to handle
     the single request and exits (`cli/handlers.rs:146-181`).

`start_app` is annotated `#[tokio::main]` (`main.rs:98`), so the tokio
multi-thread runtime is *only* started in the interactive / daemon branch.
This is a deliberate split: one-shot CLI calls don't pay for a full runtime.

### Inside `start_app`

`main.rs:99-234`. After per-feature setup (`viuer` probing, PulseAudio env
vars), it does:

1. **Create the client request channel**:
   ```rust
   let (client_pub, client_sub) = flume::unbounded::<client::ClientRequest>();
   ```
   (`main.rs:112`). This unbounded MPMC flume channel is *the* spine of the
   app — every component that wants to mutate Spotify state sends on
   `client_pub`.

2. **Build the `AppClient`** (`main.rs:142-148`): constructs `reqwest::Client`,
   the librespot-backed `Spotify` adapter (`client/spotify.rs:36-50`),
   reads OAuth scopes, and runs the user OAuth flow if a user-provided
   client ID is configured. Then `client.new_session(...)` either opens a
   new librespot streaming connection (when `streaming` feature + enabled in
   config) or just connects an auth session.

3. **Seed initial data** via `init_spotify` (`main.rs:27-45`): fires off
   `GetCurrentUser`, `GetUserPlaylists`, `GetUserFollowedArtists`,
   `GetUserSavedAlbums`, the liked-tracks `GetContext`, and `GetUserSavedShows`
   on `client_pub`. None of these block — they just queue work for the
   client handler.

4. **Spawn 4–6 long-running workers** (covered below).

5. **Final block**: `loop { thread::sleep(1s); }` to keep the main thread
   alive (`main.rs:231-233`). On platforms with `media-control` enabled
   (Win/macOS), the main thread is instead handed to the `winit` event loop
   (`main.rs:217-227`) which is a hard requirement of those OSes for
   receiving system media-key events.

## Threading & concurrency model

The runtime is a hybrid: a tokio multi-thread runtime hosts all I/O-bound
work, while the UI loop, the terminal-event reader, and a polling watcher
each run on dedicated `std::thread` instances. Communication is exclusively
via `flume` channels and `parking_lot`-guarded shared state.

```
                       ┌────────────────────────────────────────┐
                       │            SharedState (Arc)           │
                       │  ui: parking_lot::Mutex<UIState>       │
                       │  player: parking_lot::RwLock<...>      │
                       │  data: parking_lot::RwLock<AppData>    │
                       │  vis_bands: Option<Arc<Mutex<...>>>    │
                       │  logs: Arc<Mutex<VecDeque<String>>>    │
                       └─────────▲────────────▲─────────▲───────┘
                                 │            │         │
                                 │ writes     │ reads   │ reads/writes
                                 │            │         │
   ┌─────────────────────────────┼────────────┼─────────┼───────────────┐
   │                       tokio multi-thread runtime                    │
   │                                                                     │
   │   ┌───────────────────────┐                                         │
   │   │ start_client_handler  │◄── flume::Receiver<ClientRequest>       │
   │   │  (spawns one tokio    │                                         │
   │   │   task per request)   │                                         │
   │   └───────────────────────┘                                         │
   │                                                                     │
   │   ┌───────────────────────┐    ┌────────────────────────────────┐   │
   │   │ start_socket          │    │ player_event_task (in          │   │
   │   │ UDP @127.0.0.1:port   │    │   streaming::new_connection):  │   │
   │   │ for CLI IPC           │    │   reads librespot PlayerEvents │   │
   │   └───────────────────────┘    └────────────────────────────────┘   │
   │                                                                     │
   │   ┌───────────────────────┐    ┌────────────────────────────────┐   │
   │   │ Spirc task (librespot │    │ ad-hoc tokio::spawn for each   │   │
   │   │ Connect device)       │    │   ClientRequest, update_playback│  │
   │   └───────────────────────┘    └────────────────────────────────┘   │
   └─────────────────────────────────────────────────────────────────────┘
                          ▲                ▲                ▲
                          │ ClientRequest  │ ClientRequest  │ ClientRequest
                          │                │                │
   ┌──────────────────────┴────┐ ┌─────────┴────────┐ ┌─────┴──────────┐
   │ "terminal-event-handler"  │ │ "player-event-   │ │ "media-control"│
   │   OS thread               │ │   watcher" thread │ │ OS thread      │
   │ blocks on crossterm::     │ │ 100ms polling loop│ │ souvlaki cb    │
   │   event::read()           │ │ (no async)        │ │  (also winit   │
   │ -> sends ClientRequest    │ │ -> sends          │ │   on main      │
   │                           │ │   ClientRequest   │ │   thread for   │
   │                           │ │                   │ │   Win/macOS)   │
   └───────────────────────────┘ └───────────────────┘ └────────────────┘

   ┌───────────────────────────┐
   │ "ui" OS thread            │
   │ ratatui draw loop @ N ms  │
   │ reads SharedState; never  │
   │ sends on client channel   │
   └───────────────────────────┘
```

### Tasks/threads spawned in `start_app`

Listed in source order:

| # | Where spawned (`main.rs` line) | Kind                  | Name                    | Function                                                | Communication                                |
|---|-------------------------------|-----------------------|-------------------------|---------------------------------------------------------|----------------------------------------------|
| 1 | 154-160                       | `tokio::task::spawn`  | (anon)                  | `cli::start_socket` — UDP server for IPC                | reads CLI requests, runs them on `AppClient` |
| 2 | 163-168                       | `tokio::task::spawn`  | (anon)                  | `client::start_client_handler`                          | consumes `client_sub`; spawns subtasks       |
| 3 | 171-179                       | `std::thread`         | `player-event-watcher`  | `client::start_player_event_watcher`                    | sends on `client_pub`                        |
| 4 | 183-191 (interactive only)    | `std::thread`         | `terminal-event-handler`| `event::start_event_handler`                            | sends on `client_pub`                        |
| 5 | 194-198 (interactive only)    | `std::thread`         | `ui`                    | `ui::run`                                               | reads `SharedState` only                     |
| 6 | 203-214 (feature `media-control`) | `std::thread`     | `media-control`         | `media_control::start_event_watcher`                    | sends on `client_pub`                        |

Plus tasks spawned later, *not* at startup:

- `client.initialize_playback(state)` spawns a tokio task that retries device
  transfer for ~5s after session creation (`client/mod.rs:128-177`).
- Each `ClientRequest` received by `start_client_handler` is processed in its
  own tokio task (`client/handlers.rs:37-44`). This means many requests run
  concurrently — there is no single serial worker.
- `update_playback` after every player request spawns a task that polls
  `retrieve_current_playback` 5x, 1 second apart, to absorb Spotify server
  propagation lag (`client/mod.rs:668-688`).
- When `streaming` is on, `streaming::new_connection` spawns a `player_event_task`
  reading librespot `PlayerEvent`s (`streaming.rs:217-263`) and a `select!`
  between the Spirc task and the player-event task (`streaming.rs:271-276`).

### What runs where

- **Tokio runtime**: `AppClient::handle_request` (all Web API calls via
  `rspotify`/`reqwest`), `start_socket` (UDP), `start_client_handler`,
  initial-playback retry, `update_playback`, librespot Spirc + player event
  loops, OAuth flows.
- **Dedicated OS threads**:
  - **UI render** (`ui::run`) — pure synchronous ratatui draws; sleeps for
    `app_refresh_duration_in_ms` between frames (`ui/mod.rs:36-76`,
    default in config). It blocks only on `parking_lot` locks; no async.
  - **Terminal events** (`event::start_event_handler`) — blocks on
    `crossterm::event::read()` (`event/mod.rs:36`), sync, fires
    `ClientRequest`s on `client_pub` and mutates `state.ui` directly via the
    `parking_lot::Mutex`.
  - **Player polling watcher** (`start_player_event_watcher`) — `loop {
    sleep(100ms); ... }` (`client/handlers.rs:188-216`). Pings
    `GetCurrentPlayback` every `playback_refresh_duration_in_ms` and reacts
    to context/page changes by enqueuing `GetContext` / `GetCurrentUserQueue`
    / `GetLyrics` requests. *This is how stale-state reconciliation is
    driven.*
  - **Media control watcher** (`media_control::start_event_watcher`) — owns
    a `souvlaki::MediaControls` and forwards events as `ClientRequest::Player(_)`
    on `client_pub` (`media_control.rs:72`+).
  - **Main thread** (Win/macOS with `media-control`) — runs `winit` event
    loop only; otherwise it sleeps in a 1-second loop.

### Why the split?

- The UI must not be blocked by network/IO — hence the render loop is its
  own thread and never awaits anything.
- `crossterm::event::read()` is a blocking call without an async equivalent
  in the chosen version, so it must live on a thread.
- `souvlaki` and `winit` have OS-thread-affinity requirements (especially on
  macOS / Windows where the system event loop *must* run on the main
  thread).
- Network and Spotify API work is naturally async and benefits from tokio's
  many concurrent in-flight requests; that is hosted by the runtime.
- The choice of `flume::unbounded` rather than `tokio::sync::mpsc` is what
  lets sync threads (terminal-event, player-watcher, media-control) and
  async tasks (CLI socket, client-handler) all `send` into the same queue
  without juggling a `Handle` or `block_on`.

### Shared state synchronization

`State` (in `state/mod.rs:26-40`) wraps three data domains in
`parking_lot` guards:
- `ui: Mutex<UIState>` — exclusive lock; written by event/UI handlers,
  read by the render loop.
- `player: RwLock<PlayerState>` — written by client tasks (after Web API
  calls and on librespot events), read by UI and event handlers.
- `data: RwLock<AppData>` — written by client tasks; read everywhere.

`parking_lot` is chosen over `std::sync` for performance and for the
non-poisoning API. There is no lock-ordering convention enforced; the
codebase relies on each handler taking locks for short, scoped regions.

`config::get_config()` (`config/mod.rs:513`) is a `&'static Configs`, so
config reads are lock-free across the whole process after startup.

## ClientRequest flow (one-paragraph summary)

A user keypress → `event::start_event_handler` decodes it via the keymap →
sends a `ClientRequest::Player(PlayerRequest::Foo)` (or any other variant)
on `client_pub` → `start_client_handler` receives on `client_sub`,
validates the session, spawns a tokio task that calls
`AppClient::handle_request(state, request)` (`client/mod.rs:361`) → that
hits Spotify Web API or librespot, then writes new data into
`state.player` / `state.data` under their guards → next UI tick observes
the new state and redraws. After mutating playback, the request handler
*also* spawns `update_playback` to poll the API a few more times and
overwrite the buffered state once Spotify catches up.

## CLI subcommand path (no daemon)

When the user runs e.g. `spotify_player playback play-pause`,
`main` takes the `Some((cmd, args))` branch and calls
`cli::handle_cli_subcommand` (`main.rs:332`). That function
(`cli/handlers.rs:183`):
1. Binds an ephemeral UDP socket on localhost.
2. Tries to connect to the configured `client_port`. If the port refuses
   the connection (no daemon running), it spins up a *throwaway*
   `tokio::runtime::Runtime`, creates an `AppClient`, opens its own
   `start_socket` server in a background thread (`cli/handlers.rs:157-174`),
   and continues.
3. Encodes the CLI command as a `cli::Request`, sends it to the server,
   waits for the `Response`, and exits.

So one binary is both daemon and client. The CLI never imports any UI
code (`cli/mod.rs:1-12`).

## Shutdown flow

There is no graceful global shutdown. The flow is:

1. The user presses the `Quit` key. `event/mod.rs:577-579` flips
   `state.ui.lock().is_running = false`.
2. The next iteration of the UI render loop checks the flag
   (`ui/mod.rs:46-50`), runs `clean_up(terminal)` to leave the alternate
   screen / disable raw mode / disable mouse capture / show the cursor
   (`ui/mod.rs:94-103`), and then calls `std::process::exit(0)`.
3. `exit(0)` tears down everything else: the tokio runtime hosting
   `start_client_handler` and `start_socket`, the player-event watcher
   thread (which is just sleeping), the media-control thread, and the
   librespot Spirc connection. None of these are joined or notified.

This is simple but has consequences:
- In-flight Spotify Web API requests are dropped mid-flight. That's fine
  for reads; for writes it relies on the server to either complete or
  reject without partial commit.
- The librespot Spirc shutdown (`Spirc::shutdown`) is *only* called when a
  new streaming session replaces the old one (`client/mod.rs:241-247`),
  not on quit. The integrated player just dies with the process.
- Logs are flushed because `tracing-subscriber`'s file writer is
  synchronous; the in-memory `BufferLayer` is dropped with the process.

For `hibias`, this is a design choice you can keep or improve. A more
graceful version would: signal each thread/task via a watch channel, await
the librespot Spirc shutdown future, drain any pending writes, then
cooperatively exit. The cost is more wiring and an explicit `Shutdown`
variant of `ClientRequest` (or a `tokio_util::sync::CancellationToken`).

## Logging

`main.rs:47-96` sets up tracing with two layers:
- A `tracing_subscriber::fmt::layer` writing to a per-run log file in the
  log folder, named `spotify-player-YY-MM-DD-HH-MM.log`, with ANSI off and
  the writer wrapped in a `std::sync::Mutex<File>` for thread safety.
- A custom `BufferLayer` (`log_layer.rs:7-43`) that pushes formatted lines
  into a `VecDeque<String>` capped at 1000. The same buffer is given to
  `State::new` so the in-app **Logs page** can display recent log lines.

A panic hook (`main.rs:88-93`) writes the panic message + a `backtrace::Backtrace`
into a sibling `.backtrace` file, so crashes leave a forensic trail.

`RUST_LOG` defaults to `spotify_player=info,librespot=info` if unset
(`main.rs:62-65`); setting it to `off` skips file/buffer setup entirely.

## Design notes worth lifting into `hibias`

1. **Single shared `State` Arc, three locked sub-structs.** Avoids one
   giant mutex while keeping ownership trivial. `parking_lot` over
   `std::sync` is the right default for TUI/audio code.
2. **One unbounded `flume` channel as the spine.** `flume` works for
   sync→async and async→async both directions, removing a class of
   "where does the runtime live" headaches.
3. **The client handler spawns one task per request.** Cheap fan-out,
   bounded only by Spotify rate limits. Stateful sequencing is unnecessary
   because all writes go through `parking_lot` guards on `State`.
4. **Polling watcher to compensate for eventual consistency.** Since the
   Spotify Web API is eventually consistent and there is no streaming
   playback push, a 100ms watcher fires periodic
   `GetCurrentPlayback` and reacts to UI page changes by enqueuing the
   right fetch. `hibias` will need an analogous watcher unless its backend
   pushes updates.
5. **CLI uses UDP localhost loopback rather than a Unix socket.** Cheap,
   cross-platform (Windows-friendly), and avoids filesystem cleanup. The
   "spawn a daemon if none exists" pattern in `cli/handlers.rs:146-181`
   is the cleanest single-binary daemonization I've seen in a TUI.
6. **`#[tokio::main]` only on the interactive path.** One-shot CLI
   subcommands like `generate` (shell completions) and `authenticate`
   never start a runtime. That keeps cold-start fast.
7. **OS-thread for `crossterm::event::read`.** Don't try to read terminal
   events from inside tokio — block on a dedicated thread and push into
   the channel.
8. **UI render loop is a sleep-driven redraw.** Trades a tiny bit of CPU
   for vastly simpler invalidation logic; every frame re-reads the locked
   state. At 60–120ms per frame this is invisible. (See
   `app_refresh_duration_in_ms` in config.) An event-driven redraw is
   possible but ratatui / crossterm don't reward the complexity.
9. **No explicit shutdown.** `process::exit(0)` from inside the UI loop
   after restoring the terminal. Acceptable for a TUI; revisit if you add
   any background work that *must* flush (e.g. a local DB).

## File-path reference

Every cited line in this doc:

- `spotify-player/Cargo.toml:1-3, 8-24` — workspace + lints
- `spotify_player/Cargo.toml:1-9, 12-65, 84-103` — deps & features
- `lyric_finder/Cargo.toml:1-18` — sub-crate
- `spotify_player/src/main.rs:1-17` — module declarations
- `spotify_player/src/main.rs:27-45` — `init_spotify` seeding
- `spotify_player/src/main.rs:47-96` — logging + panic hook
- `spotify_player/src/main.rs:98-234` — `start_app` body
- `spotify_player/src/main.rs:112` — flume channel creation
- `spotify_player/src/main.rs:154-198` — task & thread spawning
- `spotify_player/src/main.rs:203-227` — media-control + winit
- `spotify_player/src/main.rs:236-334` — `main` body and CLI dispatch
- `spotify_player/src/client/mod.rs:46-55` — `AppClient` struct
- `spotify_player/src/client/mod.rs:128-177` — `initialize_playback`
- `spotify_player/src/client/mod.rs:179-217` — `new_session`
- `spotify_player/src/client/mod.rs:361` — `handle_request` entry
- `spotify_player/src/client/mod.rs:668-688` — `update_playback`
- `spotify_player/src/client/handlers.rs:22-46` — `start_client_handler`
- `spotify_player/src/client/handlers.rs:188-216` — `start_player_event_watcher`
- `spotify_player/src/client/request.rs:5-62` — request enums
- `spotify_player/src/client/spotify.rs:13-50` — `Spotify` adapter
- `spotify_player/src/state/mod.rs:22-40` — `SharedState`/`State`
- `spotify_player/src/state/mod.rs:42-70` — `State::new`
- `spotify_player/src/state/ui/mod.rs:26-45, 89-92` — `UIState` + `is_running`
- `spotify_player/src/state/data.rs:30-76` — `AppData`, `MemoryCaches`
- `spotify_player/src/event/mod.rs:34-60` — `start_event_handler`
- `spotify_player/src/event/mod.rs:577-579` — quit flag flip
- `spotify_player/src/ui/mod.rs:36-76` — render loop
- `spotify_player/src/ui/mod.rs:79-103` — terminal init/cleanup
- `spotify_player/src/streaming.rs:142-281` — `new_connection`
- `spotify_player/src/media_control.rs:72-100` — media-control watcher
- `spotify_player/src/cli/mod.rs:127-141, 164-223` — wire types + clap tree
- `spotify_player/src/cli/client.rs:30-96` — UDP socket server
- `spotify_player/src/cli/handlers.rs:146-181` — `try_connect_to_client`
- `spotify_player/src/cli/handlers.rs:183-220` — `handle_cli_subcommand`
- `spotify_player/src/config/mod.rs:28, 513-520` — `Configs` singleton
- `spotify_player/src/log_layer.rs:7-43` — in-memory log buffer
