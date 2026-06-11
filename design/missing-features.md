# Missing features & known rough edges

Notes from a full manual test pass on 2026-06-10 (live mode, driven over tmux;
every screen exercised: play/pause, seek, search→play, browse album/playlist,
library sub-tabs, device picker, command palette, help, like, replay mode,
restarts). Bugs found during the pass were fixed in the same session — they're
listed at the bottom for the record; everything else here is open.

## Missing features

- **No next/previous hotkeys.** Skipping tracks requires `:` → `next` /
  `previous` through the command palette. `n` / `b` (or `>` / `<`) on Now
  Playing would match every other player TUI. The handlers (`skip_track`)
  already exist — this is just dispatch + footer wiring.
- **No volume control.** Neither local (librespot mixer) nor Connect volume.
  `+`/`-` hotkeys would do.
- **No shuffle / repeat control or indicator.** `/me/player` returns
  `shuffle_state` and `repeat_state`; neither is shown nor settable.
- **Artists are dead ends.** `Library → Artists` rows are inert (documented
  in `library_enter_action`), and artist rows in search play the artist's
  context directly instead of opening top tracks / albums to browse.
- **Browse can't browse into search-result artists** (only albums and
  playlists open the Browse overlay).
- **No "play from Up Next".** The Up Next list is display-only; can't select
  or jump to a queued track. No "add to queue" action from search/browse
  rows either.
- **No pagination.** Library sections and browse track lists cap at the
  first page (50 items); long playlists are silently truncated. (The browse
  overlay shows "N tracks" from the first page only.)
- **No like state shown.** The current track doesn't indicate whether it's
  already in Liked Songs, and there's no unlike.
- **Resume-on-restart requires a keypress.** After a restart, the boot
  transfer lands paused (`play:false` by design, so the app never blasts
  audio unannounced). A `--resume` flag or a "press space to resume" hint on
  the seeded screen would smooth the restart story.
- **Replay-mode title.** Under `HIFI_REPLAY` the frame title stays
  "hifi · starting device..." forever; could say "hifi · replay" instead.

## Known inconsistencies (by design or upstream, worth knowing)

- **`:next` on a context-less play (bare track, empty queue) ends playback**,
  but the UI keeps showing the old track "playing" (progress advancing) for
  up to 60s before converging to "Nothing playing." — that's the
  `should_accept` freshness window; librespot's unreliable state reporting
  forces the fiction. Same window applies to the optimistic boot seed: it can
  show a stale "playing" state for ~10s after a restart whose transfer didn't
  retain the context.
- **Action errors are short-lived on screen.** Any accepted playback poll
  clears `state.error`, so e.g. a like 403 banner can vanish within a second
  or two if a poll lands right after. Worth a dedicated transient-error slot
  (like `notice`) with a minimum display time.
- **Silent no-op space.** Pressing space on "Nothing playing." PUTs a play
  that 200s but does nothing (no context server-side). A notice ("nothing to
  resume — pick a track") would explain the silence.
- **`just clippy` is red at baseline** (pre-existing: unused `mut` in
  `api.rs`, derivable `Default` impl, `field_reassign_with_default` in
  tests, dead `DEVICES_JSON` in two of the three binaries). Untouched here
  because `api.rs` is compiled per-binary and some "dead" items are live in
  only one — needs the careful `#[allow]` treatment CLAUDE.md warns about.

## Fixed during this pass (2026-06-10)

- ratatui-image's stdio capability probe leaked a blocked stdin-reader
  thread under tmux, which then raced crossterm and silently ate ~40% of
  all keypresses. The probe is now skipped inside tmux (halfblocks directly).
- Whitespace-only search queries no longer hit `/v1/search` (Spotify 400s
  "No search query").
- Opening a Browse overlay no longer leaves the Search tab claiming
  "loading… (showing …)" forever (request/applied id desync + leaked
  debounce handle).
- A transient 404 on a control endpoint no longer leaves the app convinced
  the Connect device is offline: routine `/me/player` polls now carry the
  reporting device and flip `device_present` back when our device shows up.
  The status-line copy also matched reality ("auto-reconnects on your next
  action" instead of "restart hifi").
- The boot/reconnect `transfer_playback` retries transient 5xx / device-
  not-found races within its 12s probe budget instead of giving up on the
  first error (a single Spotify 500 used to strand a restart at "Nothing
  playing").
- "Up Next" now populates right after starting a track/album/playlist from
  search, browse, or library (post-play poll refreshes the queue once it
  confirms real playback; browse/library plays now get the post-play poll
  burst at all).
