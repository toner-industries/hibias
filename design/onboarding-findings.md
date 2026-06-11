# Onboarding findings — from-scratch dry run, 2026-06-11

Method: wiped ALL local state (auth, hifi.toml, recents, event log, librespot
credentials — backed up to `scratch/onboarding-backup-2026-06-11/`), deleted
the Spotify dev app from the dashboard, then re-onboarded exactly as a new
user would: downloaded the v0.1.1 release, ran it quarantined, created the
Spotify app via the dashboard (driven over Playwright), and watched every
step. Fixes shipped the same day are marked ✅; open items are marked ☐.

## The walls, in the order a new user hits them

1. ☐ **The repo is private.** Every README link 404s for anyone who isn't a
   collaborator; `curl` of a release asset returns a 9-byte "Not Found".
   The entire distribution story currently works for an audience of one.
   Decision needed: make `toner-industries/hifi` public (or mirror releases
   somewhere public).
2. ✅ **README pointed at the wrong repo** (`github.com/chrisbolin/hifi` —
   the actual remote is `toner-industries/hifi`).
3. ✅ **Gatekeeper, macOS Sequoia edition.** A browser-downloaded binary is
   quarantined; running it freezes the terminal for ~30s (blocked in
   `_dyld_start` while syspolicyd assesses), then a dialog appears whose
   **primary button is "Move to Trash"** ("Apple could not verify hifi is
   free of malware"), then `zsh: killed`. There is no "Open Anyway" in the
   dialog (Sequoia removed the right-click→Open path; the escape hatch is
   System Settings → Privacy & Security, or `xattr -d`).
   Mitigation shipped: `install.sh` one-liner — `curl` downloads never get
   the quarantine xattr, so the dialog never appears. README rewritten
   around it (and the dead `chmod +x` step dropped — the tarball already
   carries the execute bit).
4. **Spotify's Feb 2026 developer-policy changes** (announced
   developer.spotify.com/blog/2026-02-06…, effective for new apps Feb 11):
   - Development mode requires **Premium** (hifi already requires it ✓)
   - **One development-mode app per account** — "Create app" is greyed out
     with a tooltip if you already have one. ✅ The first-run prompt now
     explains the reuse-your-existing-app path (add the redirect URI to it).
   - **Max 5 authorized users** per client id (fine for personal use)
   - ☐ New client ids get "a smaller set of supported endpoints" — endpoint
     enforcement was postponed for pre-existing apps but applies to apps
     created now. Everything hifi calls worked on an app created 2026-06-11,
     but this is worth re-checking whenever something 403s.
5. ✅ **Client-id mistakes surfaced as cryptic failures.** A typo'd id used
   to open a browser to an INVALID_CLIENT page while the terminal waited
   forever for a callback. Now `auth.rs` pre-flights the id against the
   token endpoint (`invalid_client` vs `invalid_grant` — works without a
   login session; the authorize page hides its errors behind the login
   wall, verified live) and fails fast with where-to-fix-it text. The
   unfixable-headlessly case (redirect URI not registered exactly) gets a
   pre-emptive hint printed at browser-open time.
6. ✅ **The audio-credentials wall was the worst step**: the README told
   users to install a *different* Spotify client (spotify-player), log in
   there, and let hifi steal its librespot cache. Replaced with native
   librespot OAuth (`streaming::ensure_credentials`, librespot-oauth 0.8):
   on first run the browser opens a second time ("Spotify for Desktop" →
   single "Continue to the app" button) and reusable credentials are minted
   into `~/.cache/hifi/credentials.json` via one `Session::connect(creds,
   store_credentials=true)`. Legacy spotify-player cache still honored as a
   fallback so existing setups aren't re-prompted. Declining is non-fatal —
   hifi stays a remote-only controller.
7. ✅ **The streaming-failure status line was truncated mid-sentence**
   ("⚠ streaming disabled: no librespot credentials at" — path clipped by
   the 96-col canvas) and unactionable. Messages shortened at the source;
   the status row now ellipsizes anything that still overflows.

## Addendum (2026-06-11, second from-scratch run)

- **App creation is rate-limited**: after two create/delete cycles in one
  day, the dashboard refused with "You have created too many apps recently.
  Please try again in 24 hours." Combined with deletion being irreversible
  and the one-app cap, the practical guidance is: **never delete a working
  dev app** — fix it in place (Edit) instead. A genuine first-time user
  creating their single app won't hit this, but anyone who fat-fingers a
  delete-and-retry can lock themselves out of the Web API for a day.
- v0.1.2's installed-from-the-one-liner first run was verified up to the
  client-id prompt (install → launch → wizard text → dashboard); the
  remainder of the run was blocked by the cooldown above and resumes once
  it lifts.

## Dashboard form notes (for prompt-text fidelity)

- The create form's real labels: "App name", "App description" (required),
  "Redirect URIs" with an **Add** button, "Which API/SDKs are you planning
  to use?" (checkboxes — "Web API"), and a required terms checkbox. The old
  prompt said "select the Web API scope", which matches nothing on screen;
  the prompt now mirrors the actual form and auto-opens
  `developer.spotify.com/dashboard/create`.
- Typing the redirect URI but forgetting to click **Add** is benign:
  Save commits the pending text (verified live).
- Clicking the *words* next to the terms checkbox follows the embedded
  Developer-Terms link and **discards the whole form** — click the box
  itself. (Spotify's bug; nothing we can do but know about it.)
- OAuth consent for the user's own app lists the 7 scopes; the librespot
  approval appears as "Spotify for Desktop" with a single Continue button.

## Sequencing gotcha learned the hard way

Replacing a previously-executed binary **in place** (`cp` over the same
inode) makes macOS SIGKILL it on next exec with zero output — the kernel
caches code signatures per vnode. `rm` first, then copy. (Relevant for any
self-update story later.)

## Open items

- ☐ Make the repo public (blocker for everything else)
- ☐ `install.sh` covers only `aarch64-apple-darwin`; add targets as releases
  grow (script already fails politely with build-from-source instructions)
- ☐ Real fix for Gatekeeper on manual downloads: Apple Developer ID signing
  + notarization ($99/yr) or a Homebrew tap
- ☐ State-in-CWD is still a trap ("run it from the same directory every
  time"); consider `~/.config/hifi`/`~/.local/share/hifi` defaults with
  CWD-files-win back-compat
- ☐ Cut a release containing this work (v0.1.1 still ships the tmux
  key-eating bug fixed on 2026-06-10 and the old onboarding)
