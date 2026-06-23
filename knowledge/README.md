# spotify-player Knowledge Base

> ⚠️ **This documents a DIFFERENT project, not hibias.** These notes describe the
> reference implementation `spotify-player`. hibias does not share its crate
> layout, dependencies, or architecture. Do not treat anything here as a
> description of hibias's code — see the repo root `CLAUDE.md` for hibias itself.

Reference notes on the [aome510/spotify-player](https://github.com/aome510/spotify-player) codebase, captured to inform the design of `hibias`. The cloned source lives at `../spotify-player/` (gitignored).

## Index

1. [`01-architecture.md`](01-architecture.md) — Workspace layout, crates, top-level wiring in `main.rs`, threading model.
2. [`02-spotify-integration.md`](02-spotify-integration.md) — Auth, token refresh, the `Client` wrapper, `librespot` streaming, OS media controls.
3. [`03-state.md`](03-state.md) — Shared `SharedState`, locking, model for player/UI/data state.
4. [`04-events-and-commands.md`](04-events-and-commands.md) — Terminal/client event loops, key → command mapping, how commands mutate state and call the client.
5. [`05-ui.md`](05-ui.md) — `ratatui` render loop, pages, popups, theming, the `Frame` lifecycle.
6. [`06-config-and-cli.md`](06-config-and-cli.md) — `app.toml`/`keymap.toml`/`theme.toml`, CLI subcommands, daemon mode, socket protocol.
7. [`07-dependencies.md`](07-dependencies.md) — Notable dependencies and Cargo feature flags, what each enables.

## How to use this

Each file is intended to stand alone. Cross-references use relative links. Where a doc cites code, it uses `path/from/spotify-player/repo/root.rs:line` so you can jump directly.
