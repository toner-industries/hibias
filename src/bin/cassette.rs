#![allow(dead_code, unused_imports)] // reuses a subset of the shared modules

// Builds a replay cassette from the SQLite event log the app already writes.
// No network, no auth, no Spotify calls — it just distills past recordings.
//
//   cargo run --bin hifi-cassette                       # hifi.log.sqlite -> cassette.json
//   cargo run --bin hifi-cassette -- <log.sqlite> <out.json>
//
// Then drive the UI offline against the recording:
//
//   HIFI_REPLAY=cassette.json cargo run --bin hifi

use anyhow::Result;
use std::env;

#[path = "../auth.rs"]
mod auth;
#[path = "../log.rs"]
mod log;
#[path = "../api.rs"]
mod api;

fn main() -> Result<()> {
    let args: Vec<String> = env::args().collect();
    let db = args.get(1).map(String::as_str).unwrap_or("hifi.log.sqlite");
    let out = args.get(2).map(String::as_str).unwrap_or("cassette.json");

    let cassette = api::Cassette::from_log(db)?;
    cassette.save(out)?;

    eprintln!("Wrote {} recorded endpoints: {db} -> {out}", cassette.len());
    let mut keys: Vec<String> = cassette.keys().cloned().collect();
    keys.sort();
    for key in keys {
        eprintln!("  {key}");
    }
    if cassette.is_empty() {
        eprintln!("(no replayable responses found — has the app made any GET requests yet?)");
    }
    Ok(())
}
