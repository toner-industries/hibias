# hifi

A terminal Spotify controller that is also its own speaker: it drives the
Spotify Web API for search/library/playback and embeds librespot as a Spotify
Connect device, so audio can play straight from the binary. No browser tab, no
desktop app.

## Requirements

- Spotify **Premium** (required by librespot, remote playback control, and —
  since Feb 2026 — Spotify's developer dashboard itself)
- A terminal at least **96×40** characters — the UI is a fixed-size canvas

## Install

### Option A: one-line install (macOS Apple Silicon)

```bash
curl -fsSL https://raw.githubusercontent.com/toner-industries/hifi/main/install.sh | sh
```

This downloads the latest release, installs it to `~/.local/bin`, and — because
the download never touches a browser — never triggers macOS Gatekeeper.

### Option B: manual download

1. Grab the binary for your platform from the
   [Releases page](https://github.com/toner-industries/hifi/releases) and
   unpack it: `tar -xzf hifi-*.tar.gz`
2. Browser downloads are quarantined, so macOS will refuse to run it
   ("Apple could not verify…" — **don't** click *Move to Trash*). Clear the
   flag and put the binary on your PATH:

   ```bash
   xattr -d com.apple.quarantine hifi
   mv hifi ~/.local/bin/
   ```

   (Alternatively: System Settings → Privacy & Security → "Open Anyway".)

### Option C: build from source

Requires the Rust toolchain (`cargo`); `tmux` and
[`just`](https://github.com/casey/just) are optional, used by `just run`.

```bash
git clone https://github.com/toner-industries/hifi.git
cd hifi
just run        # build + launch in a tmux session (detach: Ctrl-b d, stop: just stop)
# or, without tmux/just:
cargo run --release --bin hifi
```

## First launch

Run `hifi` from a directory you'll keep using — it stores its state (login
token `hifi-auth.json`, recent searches, event log) in the working directory:

```bash
mkdir -p ~/music && cd ~/music && hifi
```

hifi walks you through setup the first time; allow ~2 minutes:

1. **Spotify client id**: hifi opens the Spotify dashboard's *Create app*
   page and tells you exactly what to enter (the one field that must match
   exactly is the redirect URI, `http://127.0.0.1:8989/login`). Paste the
   new app's Client ID into the terminal; it's remembered after that.
   - Spotify allows **one** development-mode app per account. If *Create app*
     is greyed out, open the app you already have, add the redirect URI to
     it, and use its Client ID instead. Don't delete it to start over —
     deletion is permanent and app creation is rate-limited (you can be
     locked out for 24 hours).
   - hifi verifies the id and redirect URI up front and tells you what to
     fix if they don't match — no cryptic `INVALID_CLIENT` pages.
   - To skip the prompt, set `HIFI_CLIENT_ID` or put `client_id = "..."` in
     a `hifi.toml` in the working directory.
2. **Spotify login**: your browser opens for the Spotify approval
   (OAuth/PKCE); the token is cached in `hifi-auth.json`. To log in again
   later: `just reauth` (or delete that file).
3. **Audio output**: your browser opens a second time so hifi itself can
   stream audio (librespot needs its own approval). Credentials are cached
   in `~/.cache/hifi`. If you skip or cancel this, hifi still works as a
   remote control for your other Spotify devices.
