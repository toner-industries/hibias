# Dependencies and Cargo Feature Flags

Survey of `spotify-player`'s dependency graph and feature surface, with notes on what is essential vs. optional for a similar Rust TUI music app.

Sources: `spotify-player/Cargo.toml` (workspace), `spotify-player/spotify_player/Cargo.toml`, `spotify-player/lyric_finder/Cargo.toml`, `spotify-player/default.nix`, `spotify-player/Dockerfile`, `spotify-player/Cross.toml`, `spotify-player/ci/Dockerfile-cross`, `spotify-player/README.md`.

## Workspace Layout

`Cargo.toml:1-3` declares a two-member workspace (`spotify_player`, `lyric_finder`) using resolver 2. Lints are pinned at the workspace level: `unsafe_code = "deny"` and clippy `pedantic` denied with `perf/style/complexity` warned (`Cargo.toml:8-24`). Release profile keeps `debug = 1` for usable backtraces (`Cargo.toml:5-6`).

## Feature Flags

All defined in `spotify_player/Cargo.toml:84-102`. Default is `["rodio-backend", "media-control"]`.

| Feature | Pulls in | What it does | Default? |
| --- | --- | --- | --- |
| `streaming` | `librespot-playback`, `librespot-connect`, `rustfft` | Embed librespot to act as a Spotify Connect device and play audio in-process. Required by all `*-backend` features and by `daemon`. `rustfft` powers the optional audio visualizer. | indirectly (via `rodio-backend`) |
| `rodio-backend` | `streaming` + `librespot-playback/rodio-backend` | Cross-platform audio via `rodio` (CPAL underneath). | yes |
| `alsa-backend` | `streaming` + `librespot-playback/alsa-backend` | Direct ALSA on Linux. | no |
| `pulseaudio-backend` | `streaming` + `librespot-playback/pulseaudio-backend` | PulseAudio/Pipewire on Linux. | no |
| `portaudio-backend` | `streaming` + `librespot-playback/portaudio-backend` | PortAudio. | no |
| `jackaudio-backend` | `streaming` + `librespot-playback/jackaudio-backend` | JACK. | no |
| `rodiojack-backend` | `streaming` + `librespot-playback/rodiojack-backend` | Rodio with the JACK host. | no |
| `sdl-backend` | `streaming` + `librespot-playback/sdl-backend` | SDL2 audio. | no |
| `gstreamer-backend` | `streaming` + `librespot-playback/gstreamer-backend` | GStreamer pipeline (broadest codec support, heaviest dep). | no |
| `media-control` | `souvlaki`, `winit`, `windows` (Win32) | OS-level media keys / now-playing integration. MPRIS on Linux, MediaSession on Windows, `MPNowPlayingInfoCenter` on macOS. `winit` is needed for the macOS event loop (`Cargo.toml:67-69`). | yes |
| `image` | `viuer`, `image` (`dep:`) | Render album art in supported terminals (Kitty, iTerm2, sixel, blocks). | no |
| `sixel` | `image` + `viuer/sixel` | Adds sixel output path inside `viuer` (links `libsixel`). | no |
| `pixelate` | `image` | Forces a low-res pixelated render style. | no |
| `notify` | `notify-rust` (with `d` = D-Bus only, no zbus) | Desktop notifications on track change. | no |
| `daemon` | `daemonize`, `streaming` | Detach into background mode (`spotify_player -d`); requires an audio backend. macOS users must drop `media-control` (README L322-326). | no |
| `fzf` | `fuzzy-matcher` | Adds fuzzy filtering inside list pages and pickers. | no |

### Common combinations

- Default install: `rodio-backend` + `media-control` (everything else off).
- Headless server: `--no-default-features --features daemon,rodio-backend`.
- Linux desktop maximal: `--features pulseaudio-backend,media-control,image,sixel,notify,fzf` with `--no-default-features` to avoid pulling rodio.
- Docker image: built with `--no-default-features` (`Dockerfile:4`), so no streaming, no media control — just the TUI controlling a remote Spotify Connect device.
- Nix package (`default.nix:25-110`) parameterises every feature; `withAudioBackend` is mutually exclusive across the eight backend strings.

### Non-feature-gated optional behavior

`enable_audio_visualization` and `enable_media_control` are *runtime config* keys (README L230, L236) — the features must be compiled in **and** turned on in `app.toml`. So shipping a binary with `media-control` does not force MPRIS to register.

## Core Dependencies

Versions cited from `spotify_player/Cargo.toml:12-82`.

### TUI / terminal

| Crate | Version | Why |
| --- | --- | --- |
| `ratatui` | 0.30.0 | Immediate-mode TUI rendering. |
| `crossterm` | 0.29.0 | Cross-platform terminal backend (input events, raw mode, colors). |
| `unicode-bidi` | 0.3.18 | Correct rendering of bidi text in track/artist names. |
| `viuer` | =0.9.2 (optional) | Image rendering in terminal. Pinned exactly (`L46`) due to a freeze regression tracked in issue #899. |
| `image` | 0.25.10 (optional) | Decoding album art before handing to `viuer`. |
| `clipboard-win` | 5.4.1 (Windows-only, `L72`) | Clipboard copy on Windows; Unix paths use `which`+`xclip`/`pbcopy` invocation patterns elsewhere. |

### Spotify / audio

| Crate | Version | Why |
| --- | --- | --- |
| `rspotify` | 0.15.3 (`features = ["cli"]`) | Web API client (search, playlists, library, player REST). The `cli` feature enables PKCE OAuth helpers. |
| `librespot-core` | 0.8.0 | Session, auth, mercury — always linked even without streaming because metadata uses it. |
| `librespot-oauth` | 0.8.0 | OAuth flow against Spotify accounts service. |
| `librespot-metadata` | 0.8.0 | Protobuf metadata fetches that the Web API can't deliver. |
| `librespot-playback` | 0.8.0 (optional, `default-features = false`, `+native-tls`) | The actual decoder/player. Disabled default features mean the consumer picks one backend feature. |
| `librespot-connect` | 0.8.0 (optional) | Makes the local librespot session show up as a Spotify Connect device. |
| `rustfft` | 6 (optional) | FFT for the audio visualizer; only needed when `streaming` is on. |
| `souvlaki` | 0.8.3 (optional) | Cross-platform OS media controls (MPRIS / SMTC / macOS). |
| `winit` | 0.30.13 (optional, macOS+Windows only, `L67-69`) | Required by `souvlaki` on macOS for an event loop; pulled in on Windows for window-handle plumbing. |
| `windows` | 0.58.0 (optional, Windows-only, `L74-82`) | `Win32_Foundation`, `Graphics_Gdi`, `LibraryLoader`, `WindowsAndMessaging` — the minimum surface `souvlaki` needs on Windows. |

### Async / concurrency

| Crate | Version | Why |
| --- | --- | --- |
| `tokio` | 1.50.0 (`rt`, `rt-multi-thread`, `macros`, `time`) | Async runtime for HTTP, librespot, socket server. Note: no `signal`, `fs`, or `net` features — those are pulled transitively. |
| `futures` | 0.3.32 | Stream/sink combinators for librespot event handling. |
| `async-trait` | 0.1.89 | Object-safe async traits for client abstractions. Listed in `cargo-machete` ignored set (`L111`) because macro use isn't detected. |
| `flume` | 0.12.0 | MPMC channel used to fan client/event traffic between threads (chosen over `tokio::sync::mpsc` for its sync-side `recv`). |
| `parking_lot` | 0.12.5 | Faster `Mutex`/`RwLock` for `SharedState`. |
| `maybe-async` | 0.2.10 | Lets `rspotify` compile in either sync or async mode. |

### Config / serialization

| Crate | Version | Why |
| --- | --- | --- |
| `serde` | 1.0.228 (`derive`) | Universal config + API model serde. |
| `serde_json` | 1.0.149 | rspotify responses, lyric finder JSON. |
| `toml` | 1.1.0 | `app.toml`, `keymap.toml`, `theme.toml` parsing. |
| `config_parser2` | 0.1.7 | Custom derive used to merge partial configs over defaults — a project-specific helper, not the popular `config` crate. |
| `dirs-next` | 2.0.0 | XDG-aware locations for config + cache. |

### CLI

| Crate | Version | Why |
| --- | --- | --- |
| `clap` | 4.6.0 (`derive`, `string`) | Subcommand parser for the CLI/RPC surface. |
| `clap_complete` | 4.6.0 | Generates shell completions (the `generate` subcommand wired up in `default.nix:122-125`). |

### Logging / diagnostics

| Crate | Version | Why |
| --- | --- | --- |
| `tracing` | 0.1.44 | Structured logging across async tasks. |
| `tracing-subscriber` | 0.3.23 (`env-filter`) | `RUST_LOG`-style filtering. |
| `log` | 0.4.29 | Compatibility for deps that emit through `log` rather than `tracing`. |
| `backtrace` | 0.3.76 | Manual backtrace capture for panic/error reporting. |

### Networking / TLS

| Crate | Version | Why |
| --- | --- | --- |
| `reqwest` | 0.13.2 (`json`, `query`) | Web API HTTP client (rspotify-internal + lyric/cover fetches). |
| `rustls` | 0.23.37 (`default-features = false` + `ring`) | Pulled explicitly to pin TLS to rustls/ring across the dep graph and avoid OpenSSL where possible. Note `librespot-playback` still uses `native-tls` (`L21`), so OpenSSL re-enters the build. |

### Misc utilities

| Crate | Version | Why |
| --- | --- | --- |
| `anyhow` | 1.0.102 | App-level error type. (No `thiserror` — errors are `anyhow::Error` end-to-end.) |
| `chrono` / `chrono-humanize` | 0.4.44 / 0.2.3 | Time rendering ("3 minutes ago"). |
| `rand` | 0.10.0 | OAuth state, shuffle helpers. |
| `regex` | 1.12.3 | Lyric/markup cleaning, keymap parsing. |
| `daemonize` | 0.5.0 (optional) | Fork/setsid for `--daemon`. |
| `notify-rust` | 4.12.0 (optional, `default-features=false`, `+d`) | Desktop notifications via D-Bus only — the `d` feature avoids pulling zbus or async runtimes the app doesn't need. |
| `fuzzy-matcher` | 0.3.7 (optional) | Skim-style fuzzy match scoring. |
| `ttl_cache` | 0.5.1 | Simple TTL cache for API responses. |
| `which` | 8.0.2 | Locate external binaries (e.g. clipboard helpers). |
| `html-escape` | 0.2.13 | Escape track/artist text before rendering into HTML notification bodies. |
| `vergen` | =9.0.6 | Embeds git/build info. Pinned because of issue #914 (`L64-65`); also in `cargo-machete` ignored list. |

## Build / Cross-Compile

### System libraries

Verified across `default.nix:76-97`, `flake.nix:30-35`, `ci/Dockerfile-cross:7`, README L67-84:

| Library | Required for | Notes |
| --- | --- | --- |
| `openssl` (libssl-dev) | librespot's `native-tls` path | Always needed when `streaming` is on. |
| `alsa-lib` (libasound2-dev) | `alsa-backend`, and `rodio-backend` on Linux | Linux only. |
| `libpulse` (libpulse-dev) | `pulseaudio-backend` | Linux. |
| `libdbus` (libdbus-1-dev) | `media-control` (souvlaki MPRIS), `notify-rust` | Linux. |
| `pkg-config` + `cmake` | rustls/ring builds, librespot-playback C deps | Linux/macOS host requirement. |
| `libsixel` | `sixel` feature | dlopen'd at runtime; macOS Nix wrapper sets `DYLD_LIBRARY_PATH` (`default.nix:117-120`). |
| `libxcb-shape0`, `libxcb-xfixes0` | Cross-compiled Linux builds (`Cross.toml`) | Required by `winit` even on headless Linux when cross-compiling. |
| `fontconfig` | macOS / Nix builds with image | Listed unconditionally in `default.nix:79`. |
| `autoconf`, `automake`, `libtool` | librespot-playback build scripts | Native build inputs in `default.nix:67-71`. |
| `gstreamer + plugins-base/good` | `gstreamer-backend` | Heaviest backend; only when explicitly chosen. |
| `SDL2`, `portaudio`, `libjack2` | respective backends | Each is exclusive in the Nix expression. |

### Platform quirks

- **macOS**: `winit` is conditionally pulled in only on macOS+Windows (`Cargo.toml:67-69`) because `souvlaki` needs an event loop on macOS. Daemon mode and media-control are mutually exclusive on macOS — the README explicitly tells users to drop `media-control` when daemonizing.
- **Windows**: Pulls `windows` 0.58 with a tight feature subset (`Cargo.toml:74-82`) and `clipboard-win`. `crossterm` handles VT input.
- **Cross-compilation**: `Cross.toml` defines only `aarch64-unknown-linux-gnu` and uses a custom Dockerfile that installs `:$arch` variants of ssl, alsa, dbus, and xcb dev libs. Other targets aren't first-class.
- **Docker**: The published image (`Dockerfile`) builds `--no-default-features` to avoid native audio libs entirely; it's a remote-control-only TUI inside the container.
- **Nix**: `default.nix` uses `rustPlatform.bindgenHook` (needed because some librespot backend crates use bindgen) and `writableTmpDirAsHomeHook` to work around shell-completion install on Darwin (`default.nix:64-71`).

## The `lyric_finder` Crate

Standalone library published separately (`lyric_finder/Cargo.toml:1-9`) that scrapes Genius for song lyrics. Tiny dep set:

- `reqwest` 0.12.28 with `default-features = false` + `json,http2` — leaves TLS choice to the consumer.
- `html5ever` =0.27.0 + `markup5ever_rcdom` 0.3.0 — parses Genius HTML to extract the lyric text. `html5ever` is exact-pinned, presumably for parser-API stability.
- `serde` (derive), `anyhow`, `log` for plumbing.
- Dev-only: `tokio` and `env_logger` for the example binary at `lyric_finder/examples/lyric-finder.rs`.

It is consumed by `spotify_player` only when the user enables lyrics in config; nothing in the main `Cargo.toml` lists it as a dep, suggesting it is vendored via path/local link or copied in-tree (worth confirming when wiring lyrics into hibias).

## Implications for `hibias`

### Effectively mandatory for any spotify-player-shaped app

- `tokio` + `futures` — async is unavoidable for HTTP + librespot.
- `ratatui` + `crossterm` — the obvious TUI choice.
- `rspotify` — Web API coverage that would take months to rebuild.
- `librespot-core` + `librespot-oauth` — even read-only clients need session/auth machinery for OAuth and metadata access.
- `serde` + `toml` + `dirs-next` — config baseline.
- `clap` (+`clap_complete`) — CLI parser; a music app benefits from RPC subcommands.
- `tracing` + `tracing-subscriber` — diagnostic logging.
- `anyhow` — the chosen error style; trivially swappable for `thiserror` if hibias prefers typed errors.
- `parking_lot` — small win, big convenience for sync `SharedState`.
- `reqwest` — pulled in transitively anyway.

### Optional / design-space

- **Audio backend choice**: `librespot-playback` is the only path to streaming. Backend feature can be left to the user, but picking a sensible default (rodio is the most portable) matters. `gstreamer-backend` is overkill unless you need its codecs.
- **Media controls**: `souvlaki` is the only credible cross-platform option. Skipping it removes a large surface (winit on macOS, windows-rs on Windows, dbus on Linux).
- **Image rendering**: `viuer` + `image` adds binary size and the sixel headache. A v1 hibias can defer this entirely.
- **Notifications**: `notify-rust` with the `d` feature is cheap; worth including.
- **Daemon mode**: `daemonize` is tiny but couples with the socket/RPC design; fine to defer.
- **Fuzzy search**: `fuzzy-matcher` is trivial to add later; consider `nucleo` (newer, used by Helix) as an alternative.
- **TLS**: spotify-player straddles rustls (explicit) and native-tls (via librespot). hibias could try to force rustls everywhere by patching librespot features, eliminating the OpenSSL system dep.
- **`config_parser2`**: a project-local crate; hibias can use plain `serde` + `figment` or `config` instead.
- **`flume` vs `tokio::sync`**: hibias could standardize on tokio channels if it doesn't need sync-side recv.
- **Lyrics**: `lyric_finder` is a small, self-contained crate; either depend on the published version or skip lyrics in v1.

### Things hibias can reasonably do differently

- Use `thiserror` for typed errors at API boundaries while keeping `anyhow` for app-level glue.
- Replace `config_parser2` with a more standard layered-config approach.
- Drop the `winit` macOS workaround if media-control is out of scope for v1.
- Pin to rustls end-to-end by forking/patching `librespot-playback` features or using a streaming-less mode.
- Consider `nucleo` over `fuzzy-matcher` and `ratatui-image` over `viuer` for image rendering — both are more actively maintained as of 2026.
