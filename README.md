# hifi

A terminal Spotify controller that is also its own speaker: it drives the
Spotify Web API for search/library/playback and embeds librespot as a Spotify
Connect device, so audio can play straight from the binary. No browser tab, no
desktop app.

## Requirements

- Spotify **Premium** (required by librespot and remote playback control)
- Rust toolchain (`cargo`)
- `tmux` and [`just`](https://github.com/casey/just) — optional, used by `just run`

## Setup

1. **Create a Spotify app** at <https://developer.spotify.com/dashboard>:
   - Add redirect URI `http://127.0.0.1:8989/login`
   - Select the **Web API** scope
2. **Give hifi the client id** — in the repo root create `hifi.toml`:

   ```toml
   client_id = "your-spotify-client-id"
   ```

   (or set `HIFI_CLIENT_ID` instead.)
3. **Audio-output credentials** (optional — only needed if hifi itself should
   play audio): hifi reuses [spotify-player](https://github.com/aome510/spotify-player)'s
   librespot credential cache. Run `spotify-player` once and log in, which
   writes `~/.cache/spotify-player/credentials.json`. Without it, hifi still
   works as a remote for your other Spotify devices.

## Run

```bash
just run        # build + launch in a tmux session (detach: Ctrl-b d, stop: just stop)
# or, without tmux/just:
cargo run --release --bin hifi
```

First launch opens your browser for Spotify login (OAuth/PKCE); the token is
cached in `hifi-auth.json`. To log in again later: `just reauth`.

Note: the UI is a fixed 96×40 character canvas — make the terminal at least
that size.
