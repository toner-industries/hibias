# List available recipes
default:
    @just --list

# Build and run the TUI (release — needed for smooth audio + FFT)
run:
    cargo run --release

# Run a debug build (faster compile, may glitch audio)
run-debug:
    cargo run

# Release build without running
build:
    cargo build --release

# Fast type-check
check:
    cargo check

# Format
fmt:
    cargo fmt

# Lint
clippy:
    cargo clippy --all-targets -- -D warnings

# Tests
test:
    cargo test

# Forget cached Spotify token — next run re-does PKCE in browser
reauth:
    rm -f hifi-auth.json

# Tail the most recent log events from the SQLite log
logs n="50":
    sqlite3 -header -column hifi.log.sqlite "SELECT ts, kind, request_id, method, status, latency_ms, substr(coalesce(detail,body),1,120) AS info FROM events ORDER BY id DESC LIMIT {{n}};"

# Open the log DB in sqlite3 shell
logs-shell:
    sqlite3 hifi.log.sqlite

# Delete the SQLite log database
logs-clear:
    rm -f hifi.log.sqlite hifi.log.sqlite-shm hifi.log.sqlite-wal

# Wipe build artifacts
clean:
    cargo clean
