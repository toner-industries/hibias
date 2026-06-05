# hifi UI screens — driven offline against a replay cassette.
#
# Regenerate (from repo root, after `cargo build`):
#   cargo run --bin hifi-cassette          # refresh cassette.json if needed
#   vhs vhs/screens.tape                    # writes PNGs to scratch/vhs/
#
# No Spotify, no auth, no rate limits — every screen is served from the cassette.
#
# Navigation notes (learned the hard way, verified against hifi.log.sqlite):
#   - Esc on Now Playing QUITS — only Esc to close a known-open overlay.
#   - Tab is global: switches top tab and closes any open overlay.
#   - `/` `d` `?` are global only when NOT typing in the search box.
#   - Always `Sleep` AFTER a `Screenshot`: VHS captures the frame lazily, so a
#     keypress immediately after a screenshot races it and you capture stale.

Output scratch/vhs/session.gif

Set Shell "zsh"
Set FontSize 13
Set Padding 8
Set Width 820
Set Height 700

# --- Launch offline against the cassette --------------------------------
Type "HIFI_REPLAY=cassette.json target/debug/hifi"
Enter
Sleep 3s
Screenshot scratch/vhs/01-now-playing.png
Sleep 1s

# --- Search tab: a query that's in the cassette -------------------------
Type "/"
Sleep 500ms
Type "beatles"
Sleep 2s
Screenshot scratch/vhs/02-search.png
Sleep 1s

# --- Library tab (Tab switches even from the search box) ----------------
Tab
Sleep 1s
Screenshot scratch/vhs/04-library.png
Sleep 1s

# --- Devices overlay (cassette has a recorded device list) --------------
Type "d"
Sleep 1s
Screenshot scratch/vhs/05-devices.png
Sleep 1s
Escape
Sleep 1s

# --- Help overlay -------------------------------------------------------
Type "?"
Sleep 1s
Screenshot scratch/vhs/06-help.png
Sleep 1s
Escape
Sleep 1s

# --- Browse overlay: select an album in search and open it --------------
Type "/"
Sleep 1s
Down Down Down Down Down Down Down Down Down
Enter
Sleep 2s
Screenshot scratch/vhs/03-browse.png
Sleep 1s

# --- Quit cleanly (Ctrl+C always quits) ---------------------------------
Ctrl+C
Sleep 500ms
