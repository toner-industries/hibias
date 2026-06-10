# hifi

A terminal Spotify controller that is also its own speaker: it drives the
Spotify Web API for search/library/playback and embeds librespot as a Spotify
Connect device, so audio can play straight from the binary. No browser tab, no
desktop app.

## Requirements

- Spotify **Premium** (required by librespot and remote playback control)
- A terminal at least **96×40** characters — the UI is a fixed-size canvas

## Install

### Option A: download a release

1. Grab the binary for your platform from the
   [Releases page](https://github.com/chrisbolin/hifi/releases) and unpack it.
2. Make it executable and put it somewhere convenient:

   ```bash
   chmod +x hifi
   mv hifi ~/.local/bin/    # or anywhere on your PATH
   ```

   On macOS, if Gatekeeper blocks the unsigned binary, clear the quarantine
   flag first: `xattr -d com.apple.quarantine hifi`.
3. Run it:

   ```bash
   cd ~/music            # any directory you'll keep using — see note below
   hifi
   ```

   Note: hifi stores its state (login token `hifi-auth.json`, recent searches,
   event log) in the directory you run it from, so launch it from the same
   directory each time.

### Option B: build from source

Requires the Rust toolchain (`cargo`); `tmux` and
[`just`](https://github.com/casey/just) are optional, used by `just run`.

```bash
git clone https://github.com/chrisbolin/hifi.git
cd hifi
just run        # build + launch in a tmux session (detach: Ctrl-b d, stop: just stop)
# or, without tmux/just:
cargo run --release --bin hifi
```

## First launch

No configuration is needed up front — hifi walks you through setup the first
time you run it:

1. **Spotify client id**: hifi prompts you to create a (free) Spotify app at
   <https://developer.spotify.com/dashboard> — add redirect URI
   `http://127.0.0.1:8989/login`, select the **Web API** scope — and paste the
   app's Client ID into the terminal. It's remembered for future launches.
   (To skip the prompt, set `HIFI_CLIENT_ID` or put
   `client_id = "..."` in a `hifi.toml` in the working directory.)
2. **Spotify login**: your browser opens for Spotify login (OAuth/PKCE); the
   token is cached in `hifi-auth.json`. To log in again later: `just reauth`
   (or delete that file).
3. **Audio-output credentials** (optional — only needed if hifi itself should
   play audio): hifi reuses [spotify-player](https://github.com/aome510/spotify-player)'s
   librespot credential cache. Run `spotify-player` once and log in, which
   writes `~/.cache/spotify-player/credentials.json`. Without it, hifi still
   works as a remote for your other Spotify devices.
