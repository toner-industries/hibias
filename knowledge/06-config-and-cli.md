# 06 — Configuration and CLI

How `spotify-player` lays out its config files, how its CLI talks to a running
instance over a UDP socket, and how the same binary serves as both TUI app and
headless daemon. All references are to files under
`spotify-player/spotify_player/`.

## Config file layout

Three TOML files live in a single config folder. There are no XDG fallbacks —
the path is hard-coded to `$HOME/.config/spotify-player` and overridable only
via CLI flag.

| File          | Purpose                          | Required | Auto-created on first run            |
| ------------- | -------------------------------- | -------- | ------------------------------------ |
| `app.toml`    | Application settings             | No       | Yes — written from defaults          |
| `keymap.toml` | Key bindings + action bindings   | No       | No (defaults are baked in)           |
| `theme.toml`  | User-defined themes              | No       | No (`default` theme is baked in)     |

Constants and path resolution live in `src/config/mod.rs:4-8` and
`src/config/mod.rs:498-511`:

```rust
const DEFAULT_CONFIG_FOLDER: &str = ".config/spotify-player";
const DEFAULT_CACHE_FOLDER:  &str = ".cache/spotify-player";
const APP_CONFIG_FILE:    &str = "app.toml";
const THEME_CONFIG_FILE:  &str = "theme.toml";
const KEYMAP_CONFIG_FILE: &str = "keymap.toml";
```

`get_config_folder_path()` joins `DEFAULT_CONFIG_FOLDER` onto `dirs_next::home_dir()`
(`src/config/mod.rs:498`). On platforms without `$HOME`, startup fails with
`cannot find the $HOME folder`. There is no respect for `XDG_CONFIG_HOME` —
just `~/.config/spotify-player`.

The single `Configs` struct (`src/config/mod.rs:31-36`) is stuffed into a
`OnceLock` (`src/config/mod.rs:28`) and read globally via `config::get_config()`.
This is one-shot initialization: there is **no hot-reload**. Editing any TOML
file requires restarting the process.

`AppConfig::new` (`src/config/mod.rs:436-444`) calls `parse_config_file`, and if
the file is absent it serializes the in-memory defaults and writes them as a new
`app.toml`. `KeymapConfig` and `ThemeConfig` instead just log a warning and use
defaults if the file is missing — they don't write a stub
(`src/config/keymap.rs:349-356`, `src/config/theme.rs:147-154`).

The cache folder defaults to `~/.cache/spotify-player` and holds:

- `audio/`, `image/` subdirs (created by `main.rs:259-266`)
- `imports/` for playlist-import state (`src/cli/handlers.rs:577-578`)
- log files and panic backtraces (`src/main.rs:69-86`)
- librespot credentials (when `audio_cache=true` / streaming is enabled)

## Top-level CLI flags and config overrides

`init_cli()` in `src/cli/mod.rs:164-224` builds the clap command tree.
Top-level flags (apply both to TUI mode and any subcommand):

| Flag                     | Short | Default                                          | Source                         |
| ------------------------ | ----- | ------------------------------------------------ | ------------------------------ |
| `--theme <THEME>`        | `-t`  | (uses `app.toml` `theme`)                        | `src/cli/mod.rs:182-188`       |
| `--config-folder <DIR>`  | `-c`  | `~/.config/spotify-player`                       | `src/cli/mod.rs:189-196`       |
| `--cache-folder <DIR>`   | `-C`  | `~/.cache/spotify-player`                        | `src/cli/mod.rs:197-204`       |
| `--config-override K=V`  | `-o`  | (repeatable)                                     | `src/cli/mod.rs:205-212`       |
| `--daemon`               | `-d`  | (only with `daemon` feature)                     | `src/cli/mod.rs:214-221`       |

`--config-override` works by serializing `AppConfig` to TOML, dot-walking the
key (`device.volume`, `theme`), inserting the parsed value, then deserializing
back. Implementation: `apply_config_override` in `src/config/mod.rs:526-553`.
Driver loop in `src/main.rs:275-283`:

```rust
for override_str in overrides {
    let (key, value) = override_str.split_once('=')...;
    apply_config_override(&mut configs.app_config, key, value)?;
}
```

So `spotify_player -o device.volume=80 -o theme=dracula` is fully supported.

## `app.toml` schema

Defined as one flat struct `AppConfig` in `src/config/mod.rs:49-142`, with two
nested tables `[device]` and `[layout]` (and one optional `[notify_format]`).
Every field has a default in the `Default` impl at `src/config/mod.rs:286-397`.

Major groupings:

- **Auth / connect**: `client_id`, `client_id_command` (shell command whose
  stdout becomes the client id), `login_redirect_uri`, `client_port` (UDP
  socket port — see daemon section), `proxy`, `ap_port`.
- **Streaming / device**: `enable_streaming` (`Always`/`DaemonOnly`/`Never`,
  `src/config/mod.rs:236-271`, with bool back-compat), `default_device`, and
  the `[device]` subtable (`name`, `device_type`, `volume`, `bitrate`,
  `audio_cache`, `normalization`, `autoplay` — `DeviceConfig`,
  `src/config/mod.rs:202-212`).
- **Playback display**: `playback_format` template, `playback_metadata_fields`,
  `play_icon` `▶`, `pause_icon` `▌▌`, `liked_icon` `♥`, `explicit_icon` `(E)`.
- **Layout**: `border_type` (`Hidden|Plain|Rounded|Double|Thick`,
  `src/config/mod.rs:151-159`), `progress_bar_type` (`Line|Rectangle`),
  `progress_bar_position` (`Bottom|Right`), and the nested `[layout]` table:
  `library.{playlist_percent,album_percent}` (must sum to ≤99 — checked at
  `src/config/mod.rs:427-432`), `playback_window_position` (`Top|Bottom`),
  `playback_window_height`.
- **Refresh / scrolling**: `app_refresh_duration_in_ms` (default 32),
  `playback_refresh_duration_in_ms` (default 0 = event-driven only),
  `page_size_in_rows`, `seek_duration_secs`, `volume_scroll_step`,
  `enable_mouse_scroll_volume`.
- **Optional features (cfg-gated)**: `cover_img_*` (`image` feature),
  `enable_audio_visualization` (`streaming`), `enable_media_control`
  (`media-control`), `enable_notify` / `notify_format` /
  `notify_timeout_in_secs` / `notify_transient` / `notify_streaming_only`
  (`notify`).
- **Hooks**: `player_event_hook_command` — a `Command { command, args }`
  invoked on each player event with the event prepended as positional args
  (`src/config/mod.rs:175-200`, docs/config.md:91-123).
- **Misc**: `theme` (theme name), `log_folder` (defaults to cache folder
  via `src/main.rs:271-274`), `tracks_playback_limit`, `genre_num`,
  `sort_artist_albums_by_type`, `enable_cover_image_cache`, `custom_queue`.

Minimal `app.toml`:

```toml
theme = "dracula"
client_port = 8080
playback_format = "{status} {track} • {artists}\n{album}"
enable_streaming = "Always"

[device]
name       = "spotify-player"
device_type = "speaker"
volume     = 70
bitrate    = 320

[layout]
playback_window_position = "Top"
playback_window_height   = 6
library = { playlist_percent = 40, album_percent = 40 }
```

A full example ships at `spotify-player/examples/app.toml`.

## `keymap.toml`

Two arrays of tables: `[[keymaps]]` and `[[actions]]`. Type definitions in
`src/config/keymap.rs:8-31`.

```rust
pub struct Keymap   { pub key_sequence: KeySequence, pub command: Command }
pub struct ActionMap{ pub key_sequence: KeySequence, pub target: ActionTarget,
                      pub action: Action }
```

### Key syntax

`KeySequence` is a space-separated list of `Key`s
(`src/key.rs:154-165`). Each `Key`:

- A bare token: `a`, `Z`, `space`, `enter`, `tab`, `backtab`, `backspace`,
  `esc`, `left`/`right`/`up`/`down`, `insert`, `delete`, `home`, `end`,
  `page_up`, `page_down`, `f1`..`f12` (`src/key.rs:19-62`).
- `C-<key>` for Ctrl, `M-<key>` for Alt (`src/key.rs:65-81`). Shift is folded
  into the `KeyCode` itself (`src/key.rs:176-191`), so `Z` == shift+z and is
  written literally.

Examples: `g a`, `C-z`, `M-enter`, `C-c C-x /`.

A user keymap merges with the defaults. Identical `key_sequence`s in the user
file *replace* the default mapping; everything else is preserved
(`src/config/keymap.rs:362-382`). Setting `command = "None"` for a key (a
variant of `Command` at `src/command.rs:10`) is the canonical way to *remove*
a default binding (`src/config/keymap.rs:445-448`).

### Command names

`Command` enum lives at `src/command.rs:9-93`. Variants serialize either as
bare names (`"NextTrack"`, `"ResumePause"`) or as table form for variants with
fields:

```toml
command = { VolumeChange = { offset = 5 } }
command = { SeekForward  = { duration = 10 } }
command = { SeekBackward = {} }              # uses seek_duration_secs default
```

### Actions

`Action` (`src/command.rs:96-115`) plus `ActionTarget`
(`src/command.rs:130-135`, default `SelectedItem`):

```toml
[[actions]]
action = "GoToArtist"
key_sequence = "g A"

[[actions]]
action = "GoToAlbum"
target = "PlayingTrack"
key_sequence = "g B"
```

### Minimal `keymap.toml`

```toml
[[keymaps]]
command = "None"               # unbind default `q` quit
key_sequence = "q"

[[keymaps]]
command = { VolumeChange = { offset = 1 } }
key_sequence = "-"

[[actions]]
action = "ToggleLiked"
key_sequence = "C-l"
```

The complete default keymap is in `src/config/keymap.rs:33-338`; it covers
~70 bindings including vim-style motion (`j`/`k`/`g g`/`G`), `g <x>` page
jumps, `s <x>` sort prefixes, and `u <x>` user-library shortcuts.

## `theme.toml`

`ThemeConfig` is just `themes: Vec<Theme>` (`src/config/theme.rs:7-21`). Each
`Theme` has a `name`, a `palette`, and a `component_style` table.

### Palette

16 ANSI slots plus optional `background`/`foreground`
(`src/config/theme.rs:23-61`). Values are color names (`"red"`,
`"bright_blue"`) or `#RRGGBB` hex (`src/config/theme.rs:494-507`). Missing
slots fall back to terminal defaults (`src/config/theme.rs:592-616`). A theme
named `default` is always present (`src/config/theme.rs:574-590`).

### Component styles

`ComponentStyle` (`src/config/theme.rs:63-84`) is 19 named slots, each
optional, each accepting a `Style { fg, bg, modifiers }`
(`src/config/theme.rs:86-92`). Slots cover block borders/titles, the playback
bar (`playback_status`, `playback_track`, `playback_artists`, `playback_album`,
`playback_genres`, `playback_metadata`, `playback_progress_bar`,
`playback_progress_bar_unfilled`), list/table chrome (`current_playing`,
`page_desc`, `playlist_desc`, `table_header`, `selection`, `secondary_row`),
and lyrics (`lyrics_played`, `lyrics_playing`).

Style colors must be enum names (`"Cyan"`, `"BrightBlack"`) or hex
(`src/config/theme.rs:412-453`). Modifiers: `Bold`, `Dim`, `Italic`,
`Underlined`, `RapidBlink`, `Reversed`, `Hidden`, `CrossedOut`
(`src/config/theme.rs:115-125`).

### Minimal `theme.toml`

```toml
[[themes]]
name = "dracula"

[themes.palette]
background = "#1e1f29"
foreground = "#f8f8f2"
red        = "#ff5555"
green      = "#50fa7b"
yellow     = "#f1fa8c"
blue       = "#bd93f9"
magenta    = "#ff79c6"
cyan       = "#8be9fd"

[themes.component_style]
selection     = { modifiers = ["Reversed", "Bold"] }
playback_track = { fg = "Cyan", modifiers = ["Bold"] }
```

User themes merge with defaults; same-name conflicts skip the user version
(`src/config/theme.rs:160-164`). A bundled theme set is in
`spotify-player/examples/theme.toml` (dracula, gruvbox, solarized, tokyonight,
catppuccin, etc.).

## CLI subcommands

Built in `src/cli/commands.rs`. Top-level subcommands and what they map to:

| Subcommand     | Args / flags                                                   | Wire request                              | Output                                | Source                          |
| -------------- | -------------------------------------------------------------- | ----------------------------------------- | ------------------------------------- | ------------------------------- |
| `authenticate` | —                                                              | (local; no socket)                        | OAuth flow, exits                     | `commands.rs:153`, `handlers.rs:188-192` |
| `generate <SHELL>` | bash/zsh/fish/elvish/powershell                            | (local)                                   | shell completion to stdout            | `commands.rs:157-166`           |
| `features`     | —                                                              | (local)                                   | compile-time feature list             | `commands.rs:242-244`, `handlers.rs:202-205` |
| `get key <KEY>`| `Playback`, `Devices`, `UserPlaylists`, `UserLikedTracks`, `UserSavedAlbums`, `UserFollowedArtists`, `UserTopTracks`, `Queue` | `Get(Key(...))`                | JSON                                  | `commands.rs:12-30`, `handlers.rs:202-241` |
| `get item <TYPE> [-i ID \| -n NAME]` | `Playlist`/`Album`/`Artist`/`Track`               | `Get(Item(type, id_or_name))`             | JSON                                  | `handlers.rs:328-340`           |
| `playback start context <TYPE> [-i\|-n] [-s]` | `Playlist`/`Album`/`Artist`, `--shuffle`        | `Playback(StartContext{...})`             | empty                                 | `commands.rs:36-51`             |
| `playback start track [-i\|-n]`           | —                                                  | `Playback(StartTrack(..))`                | empty                                 | `commands.rs:52-54`             |
| `playback start liked [-l N] [-r]`        | `--limit`, `--random`                              | `Playback(StartLikedTracks{..})`          | empty                                 | `commands.rs:55-75`             |
| `playback start radio <TYPE> [-i\|-n]`    | item-type seed                                     | `Playback(StartRadio(..))`                | empty                                 | `commands.rs:76-80`             |
| `playback play-pause` / `play` / `pause`  | —                                                  | `Playback(PlayPause/Play/Pause)`          | empty                                 | `commands.rs:102-104`           |
| `playback next` / `previous`              | —                                                  | `Playback(Next/Previous)`                 | empty                                 | `commands.rs:105-106`           |
| `playback shuffle` / `repeat`             | —                                                  | `Playback(Shuffle/Repeat)`                | empty                                 | `commands.rs:107-108`           |
| `playback volume <PERCENT> [--offset]`    | `-100..=100`                                       | `Playback(Volume{percent,is_offset})`     | empty                                 | `commands.rs:109-123`           |
| `playback seek <MS>`                      | i64 offset (ms)                                    | `Playback(Seek(ms))`                      | empty                                 | `commands.rs:124-132`           |
| `connect [-i\|-n]`                        | device id or name                                  | `Connect(IdOrName)`                       | empty                                 | `commands.rs:8-10`              |
| `like [-u]`                               | `--unlike`                                         | `Like { unlike }`                         | empty                                 | `commands.rs:141-151`           |
| `search <QUERY>`                          | —                                                  | `Search { query }`                        | JSON                                  | `commands.rs:135-139`           |
| `lyrics [-i\|-n]`                         | optional id/name; defaults to current track        | `Lyrics { id_or_name }`                   | plain text                            | `commands.rs:246-253`, `handlers.rs:868-916` |
| `playlist new <name> [<desc>] [-p] [-c]`  | `--public`, `--collab`                             | `Playlist(New{..})`                       | text                                  | `commands.rs:172-188`           |
| `playlist delete <id>`                    | —                                                  | `Playlist(Delete{id})`                    | text                                  | `commands.rs:189-191`           |
| `playlist list`                           | —                                                  | `Playlist(List)`                          | text (`<id>: <name>`)                 | `commands.rs:203`               |
| `playlist import <from> <to> [-d]`        | `--delete`                                         | `Playlist(Import{..})`                    | text diff                             | `commands.rs:192-202`           |
| `playlist fork <id>`                      | —                                                  | `Playlist(Fork{id})`                      | text                                  | `commands.rs:204-206`           |
| `playlist sync [<id>] [-d]`               | optional playlist id                               | `Playlist(Sync{..})`                      | text                                  | `commands.rs:207-215`           |
| `playlist edit <ADD\|DELETE> <pl_id> -t TRACK \| -a ALBUM` | mutually exclusive `--track-id`/`--album-id` | `Playlist(Edit{..})`                  | text                                  | `commands.rs:216-239`           |

`get` and `search` always emit serde-JSON of the underlying rspotify model
(`handle_get_key_request` at `src/cli/handlers.rs:202-241`,
`handle_search_request` at `src/cli/handlers.rs:342-346`). Most playback /
playlist commands return either an empty body or a small text receipt.

`-i ID` accepts a Spotify ID (`PlaylistId::from_id(...)`); `-n NAME` triggers
a `search_specific_type` API call and uses the first hit
(`src/cli/handlers.rs:243-326`). The two are an `ArgGroup` named `id_or_name`
defined in `src/cli/commands.rs:83-95`.

## Daemon mode and the IPC socket

### Wire format

The IPC primitive is **a single UDP socket bound to `127.0.0.1:<client_port>`**
(default `8080`, set via `app.toml` `client_port`). This is not a Unix domain
socket — it is plain UDP loopback (`src/cli/client.rs:42`).

Frames are JSON-encoded `Request`/`Response` enums
(`src/cli/mod.rs:127-141`):

```rust
pub enum Request {
    Get(GetRequest),
    Playback(Command),
    Connect(IdOrName),
    Like { unlike: bool },
    Playlist(PlaylistCommand),
    Search { query: String },
    Lyrics { id_or_name: Option<IdOrName> },
}

pub enum Response { Ok(Vec<u8>), Err(Vec<u8>) }
```

Request size cap is `MAX_REQUEST_SIZE = 4096` bytes (`src/cli/mod.rs:9`,
asserted at send in `src/cli/handlers.rs:235`).

### Server loop (in the running app)

`start_socket` (`src/cli/client.rs:30-96`):

1. Bind UDP at `127.0.0.1:client_port`.
2. `recv_from` into a 4096-byte buffer.
3. If `n_bytes == 0`, the peer is sending a *connection probe*; reply with
   an empty datagram. (`src/cli/client.rs:58-63`)
4. Otherwise, deserialize as `Request`, dispatch via
   `handle_socket_request` (`src/cli/client.rs:129-200`), serialize result.
5. **Chunked response**: split JSON into 4096-byte chunks, send each as its
   own datagram, then send a final empty datagram as EOF marker
   (`src/cli/client.rs:98-113`). Used because UDP datagrams are bounded.

The TUI app spawns this loop as a tokio task during startup
(`src/main.rs:154-160`):

```rust
tokio::task::spawn({
    let client = client.clone();
    let state  = state.clone();
    async move { cli::start_socket(&client, Some(&state), None).await; }
});
```

Note the `Some(&state)`: when running inside the TUI, the handler reads from
the shared state cache (`current_playback` from `state.player`), avoiding a
network round-trip. When running as a CLI-spawned worker (no TUI), `state` is
`None` and every read goes to the Spotify Web API.

### Client side (CLI subcommand)

`handle_cli_subcommand` (`src/cli/handlers.rs:183-249`):

1. Bind a transient UDP socket on an ephemeral port (`UdpSocket::bind("127.0.0.1:0")`).
2. `try_connect_to_client` (`src/cli/handlers.rs:146-181`): `connect()` to
   `127.0.0.1:client_port`, send an empty datagram (the probe), `recv` one
   byte.
3. If recv yields `ConnectionRefused`, no instance is running — spin up a
   tokio runtime in this process, build an `AppClient`, bind the server
   socket, and `std::thread::spawn` `start_socket` to handle the
   request locally before exiting.
4. Otherwise the running instance answered the probe; send the JSON-encoded
   `Request`.
5. `receive_response` (`src/cli/handlers.rs:13-28`) reassembles chunks until
   it sees a zero-byte datagram, then deserializes as `Response`.
6. Print `Ok` to stdout, `Err` to stderr with exit code 1.

### Daemon flag

With the `daemon` cargo feature enabled, `--daemon`/`-d` triggers
`daemonize::Daemonize::new().start()` in `src/main.rs:307-322` *before*
`start_app`. After daemonization, `start_app` notices `state.is_daemon == true`
and skips two thread spawns: `terminal-event-handler` and `ui`
(`src/main.rs:181-198`):

```rust
if !state.is_daemon {
    // terminal event handler thread
    // UI thread
}
```

Everything else — the API client, the player-event watcher, the IPC socket,
optional media-control loop — runs in both modes. So a daemon is just
"the app minus the TUI and terminal-input handler"; the same `start_socket`
serves both.

Daemon caveats from the README (lines 309-327):

- Only with `cargo install spotify_player --features daemon`.
- Not on Windows.
- Requires `streaming` and an audio backend.
- On macOS, must build without `media-control` (which requires a window).

### Single-binary architecture summary

```
                       cli::init_cli (clap)
                              │
                       parse args + load config
                              │
                ┌─────────────┴────────────┐
                │                          │
   subcommand?  │                          │  no subcommand
                ▼                          ▼
   handle_cli_subcommand        is_daemon? ── yes ── daemonize(), then ↓
                │                          │
                │                          ▼
                │                     start_app()
                │                       ├── AppClient + librespot session
                │                       ├── client request channel
                │                       ├── tokio task: cli::start_socket  ◄──┐
                │                       ├── thread:    player-event-watcher    │
                │                       ├── (if !daemon) thread: TUI event     │
                │                       └── (if !daemon) thread: ui::run       │
                │                                                              │
                ▼ probe 127.0.0.1:client_port                                  │
        ┌── refused ──► spawn local AppClient + start_socket in this process ──┘
        └── answered ──► send JSON Request, read chunked Response, print
```

The TUI and the headless daemon share `start_app` end-to-end; the only branch
is whether to spin up a UI thread. The CLI is a thin marshalling layer that
hits the same UDP endpoint regardless of who's serving.

## Notes for `hibias`'s design

- A single `OnceLock` config initialized from disk + CLI overrides is enough.
  No reload machinery; document that restart is required.
- The `--config-override KEY=VALUE` pattern using TOML round-tripping
  (`src/config/mod.rs:526-553`) is small, generic, and pleasant to use.
- The 4096-byte UDP loopback design is simple but quirky: chunked responses
  with empty-frame EOF, 4 KiB request cap, and the empty-datagram "probe"
  pattern. A length-prefixed Unix domain socket is probably a cleaner default
  for a fresh project (also avoids the open port on every machine).
- Keep CLI vs. server protocol as a typed enum (`Request`/`Response`) shared
  by both ends — the `serde_json::to_vec` boundary is the entire wire
  contract.
- Daemon = "TUI app minus UI thread" is a clean factoring; the IPC server is
  always running, even in TUI mode, which means the CLI Just Works against
  any live instance.
- Default keymap baked into the binary, with user `keymap.toml` *merging* on
  top (and `Command::None` to unbind), is a good UX pattern worth copying
  verbatim.
