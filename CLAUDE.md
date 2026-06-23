# hibias — Spotify TUI

A terminal Spotify controller. It uses the Spotify **Web API** for control/data
(search, library, playback state, transfer) **and** embeds **librespot** as a
Spotify Connect device so it can be the audio output itself — all in one
lightweight binary, no browser tab or desktop app required.

## Read first — orientation gotchas

- **`knowledge/` is NOT about hibias.** It documents the *reference* project
  [aome510/spotify-player](https://github.com/aome510/spotify-player) (cloned at
  `../spotify-player/`, gitignored), captured to inform hibias's design. Treat it
  as background only — its architecture (`flume`, `rspotify`, a Cargo workspace,
  `SharedState`) is **not** hibias's. Don't "fix" hibias to match it.
- **No `lib.rs`.** Binary-only crate with three `[[bin]]` targets (`Cargo.toml`):
  `hibias`, `hibias-diag`, `hibias-cassette`. The two helper bins re-compile shared
  modules via `#[path = "../*.rs"] mod …` (see `bin/diag.rs`, `bin/cassette.rs`).
  Consequence: each binary has its **own** copy of `log`'s global statics, and
  `api.rs` is compiled three times — items there marked `#[allow(dead_code)]`
  are live in one binary, unused in another; don't delete them as "dead."
- **The UI is a fixed 96×40 canvas** (`ui::FIXED_W`/`FIXED_H`). It clips, never
  reflows. Snapshot tests render 100×40 (cols 96–99 are blank padding).

## Architecture — four layers

| Layer | File | Responsibility |
|-------|------|----------------|
| **head** | `main.rs` | terminal setup, the auth-vs-replay boot branch, background tasks, the `select!` run loop |
| **logic** | `app/` | `AppState` + async handlers (`mod.rs`); `dispatch.rs` (`KeyAction`, pure `dispatch_input`); `freshness.rs` (`should_accept`); `tests.rs`. All re-exported, so callers use `crate::app::*` |
| **view** | `ui.rs` | ratatui rendering only — reads `AppState` + `ArtCache` |
| **data** | `api.rs` | `SpotifyApi` trait, `SpotifyClient` (live), `ReplaySpotify` (offline), rate-limit gate |

**Hard rule:** crossterm/ratatui types never cross into `app/` — it consumes
the frontend-neutral `input::Input`, not crossterm events. Decoded album art
lives in `art::ArtCache` owned by the run loop, **never** in `AppState` (keeps
the core free of ratatui types — the seam that lets a non-TUI head reuse it).

Supporting modules: `auth` (OAuth/PKCE), `log` (async SQLite event log),
`input` (frontend-neutral key type), `keys` (hotkey/footer tables + `ModeMask`),
`recent` (recent-search persistence), `streaming` (librespot Connect device —
mints its own credentials via librespot OAuth into `~/.cache/hibias`; falls back
to spotify-player's legacy cache at `~/.cache/spotify-player` if that's the
only one with credentials),
`art` (head-owned art cache/loader), `testmode` (the `HIBIAS_TEST` switch),
`test_support` (headless `Harness` + `FakeSpotify`, `cfg(test)` only).

## Boot sequence (`main.rs`)

auth-vs-replay branch → `streaming::ensure_credentials` (first-run librespot
OAuth, pre-TUI, skipped under replay) → `spawn_reconnect` (skipped under
replay) →
`spawn_boot_seed` → `run()` spawns `spawn_playback_poll` + an event-reader task
→ `select!(redraw tick 100ms, event channel)`. Keypresses arrive over an
`mpsc` channel (cancellation-safe) rather than a raw `EventStream`, so none are
lost to a redraw tick.

## The three orthogonal modes

- `HIBIAS_REPLAY=<cassette.json>` — serve recorded data offline (no auth, no
  librespot). "Where does data come from."
- `HIBIAS_RECORD=<path>` — tee live successful GET responses (untruncated) into a
  cassette as you browse.
- `HIBIAS_TEST=1` — "an automated harness is driving me": disables album art for
  deterministic, network-free frames. "Am I under test." Orthogonal to REPLAY;
  the VHS tape sets both. Truthy-valued (`0`/`false`/empty are off).

## All environment variables

| Var | Default | Purpose |
|-----|---------|---------|
| `HIBIAS_REPLAY` | — | offline replay from a cassette (`main.rs`) |
| `HIBIAS_RECORD` | — | record live responses into a cassette (`api.rs`) |
| `HIBIAS_TEST` | off | under-test mode; disables art (`testmode.rs`) |
| `HIBIAS_CLIENT_ID` | `hibias.toml` → auth file → stdin prompt | Spotify OAuth client id (`auth.rs`) |
| `HIBIAS_AUTH_FILE` | `hibias-auth.json` | stored OAuth token path (`auth.rs`) |
| `HIBIAS_RECENT_FILE` | `hibias-recent.json` | recent-search persistence (`recent.rs`) |
| `HIBIAS_RATELIMIT_FILE` | `hibias-ratelimit.json` | persisted 429 deadline (`api.rs`) |
| `HIBIAS_LIBRESPOT_CACHE` | `~/.cache/hibias` (legacy fallback: `~/.cache/spotify-player`) | librespot credential cache (`streaming.rs`) |
| `HIBIAS_DUMP_AUTH_PAGES` | off | debug: dump OAuth callback HTML (`auth.rs`) |

## Record / replay / screenshot workflow

```bash
cargo run --bin hibias-cassette          # mine hibias.log.sqlite → cassette.json (32KB body cap)
HIBIAS_RECORD=cassette.json cargo run    # OR record a live session (untruncated; visit every screen)
HIBIAS_REPLAY=cassette.json cargo run    # drive the app offline against it
vhs vhs/screens.tape                    # scripted screenshots → scratch/vhs/ (sets HIBIAS_TEST=1 + REPLAY)
```

Cassettes and `scratch/` are gitignored (real listening data). VHS gotchas (see
`vhs/screens.tape` header): **Esc on Now Playing QUITS**; `Tab` is global;
always `Sleep` AFTER a `Screenshot` or you capture a stale frame. VHS's xterm.js
mis-renders the album-art widget — hence `HIBIAS_TEST` disabling it; tmux
`capture-pane` is the reliable ground-truth for the actual cell grid.

## Build / test / run (justfile)

`just run` / `start` / `stop` / `attach` / `status` / `peek` (tmux session) ·
`just check` / `fmt` / `clippy` / `test` · `just logs [n]` / `logs-shell` /
`logs-clear` (the SQLite event log) · `just diag` / `diag-play <uri>` ·
`just reauth` (forget the cached token).

## Testing model

`test_support::Harness` drives `dispatch_input → KeyAction → handler` against
`FakeSpotify`, no TTY or network. **`Harness::run` hand-mirrors the action
dispatch in `main.rs::run`** — keep them in sync; a `KeyAction` wired only into
`main.rs` won't be exercised until it's added to the harness too.

## Conventions

- Tracked design artifacts (mockups, notes) go in `design/`; genuine throwaways
  (regenerable screenshots, scratch tapes) in `scratch/` (gitignored).
- Commit messages end with the `Co-Authored-By` trailer for the Claude model.
- This is a binary-only crate edited by more than one agent/machine — pull
  before large refactors; the working tree may carry others' uncommitted work.
