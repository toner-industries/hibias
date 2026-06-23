#![allow(dead_code, unused_imports)] // diag uses a subset of the shared modules

// Diagnostic CLI that uses the same auth/api code as the main app but skips
// the TUI and librespot. Run it in a second terminal while `hibias` is running.
//
//   cargo run --bin hibias-diag                  -> show /me/player + devices
//   cargo run --bin hibias-diag play <track_uri> -> play on the "hibias" device,
//                                                 then poll /me/player for 10s
//
// Logging goes to hibias.log.sqlite (same DB the main app uses).

use anyhow::{Context, Result};
use std::env;
use std::sync::Arc;
use std::time::Duration;

#[path = "../api.rs"]
mod api;
#[path = "../auth.rs"]
mod auth;
#[path = "../log.rs"]
mod log;

use api::SpotifyClient;

#[tokio::main]
async fn main() -> Result<()> {
    let result = diag_main().await;
    // Drain the log-writer thread before exit or the last events are lost.
    log::flush();
    result
}

async fn diag_main() -> Result<()> {
    let _ = log::init(&std::path::PathBuf::from("hibias.log.sqlite"));
    log::note("diag start", None);

    let mode = parse_args(env::args().collect::<Vec<_>>());
    eprintln!("Authenticating...");
    let a = auth::Auth::init().await.context("authenticate")?;
    let client = Arc::new(SpotifyClient::new(a)?);

    print_state(&client).await?;

    match mode {
        DiagMode::Inspect => Ok(()),
        DiagMode::Play(uri) => play_and_poll(&client, &uri).await,
    }
}

enum DiagMode {
    Inspect,
    Play(String),
}

fn parse_args(args: Vec<String>) -> DiagMode {
    let mut it = args.into_iter().skip(1);
    match it.next().as_deref() {
        Some("play") => {
            let uri = it.next().unwrap_or_else(|| {
                eprintln!("usage: hibias-diag play <spotify:track:...>");
                std::process::exit(2);
            });
            DiagMode::Play(uri)
        }
        _ => DiagMode::Inspect,
    }
}

async fn print_state(client: &SpotifyClient) -> Result<()> {
    println!("\n=== current playback (/me/player) ===");
    match client.get_playback().await {
        Ok(None) => println!("(no active session: 204 No Content)"),
        Ok(Some(pb)) => {
            println!("is_playing: {}", pb.is_playing);
            if let Some(t) = &pb.item {
                let artists = t
                    .artists
                    .iter()
                    .map(|a| a.name.as_str())
                    .collect::<Vec<_>>()
                    .join(", ");
                println!("track:      {} — {}", t.name, artists);
            }
            if let Some(c) = &pb.context {
                println!("context:    {} ({})", c.uri, c.kind);
            } else {
                println!("context:    none");
            }
        }
        Err(e) => println!("error: {e:#}"),
    }

    println!("\n=== devices (/me/player/devices) ===");
    match client.get_devices().await {
        Ok(devs) if devs.is_empty() => println!("(no devices visible)"),
        Ok(devs) => {
            for d in devs {
                println!(
                    "  - name={:<24} id={:<40} active={}",
                    d.name,
                    d.id.as_deref().unwrap_or("(none)"),
                    d.is_active
                );
            }
        }
        Err(e) => println!("error: {e:#}"),
    }
    Ok(())
}

async fn play_and_poll(client: &SpotifyClient, track_uri: &str) -> Result<()> {
    // Look up the "hibias" device's id.
    let devs = client.get_devices().await.context("get_devices")?;
    let hibias = devs.iter().find(|d| d.name == "hibias");
    let Some(d) = hibias else {
        eprintln!("\nno 'hibias' device visible. Start the TUI first (`just run`).");
        std::process::exit(1);
    };
    let Some(device_id) = d.id.clone() else {
        eprintln!("\nhibias device has no id (?)");
        std::process::exit(1);
    };
    println!("\nfound hibias device: id={device_id} active={}", d.is_active);

    // Set the client's target device so play_uris appends ?device_id=.
    client.set_device_id(device_id.clone());

    println!("\n=== sending play({track_uri}) ===");
    match client.play_uris(&[track_uri.to_string()]).await {
        Ok(()) => println!("play accepted"),
        Err(e) => {
            println!("play failed: {e:#}");
            return Ok(());
        }
    }

    println!("\n=== polling /me/player every 500ms for 10s ===");
    for i in 0..20 {
        tokio::time::sleep(Duration::from_millis(500)).await;
        match client.get_playback().await {
            Ok(None) => println!("[{:>4}ms] 204 no-content", (i + 1) * 500),
            Ok(Some(pb)) => {
                let track = pb
                    .item
                    .as_ref()
                    .map(|t| t.name.clone())
                    .unwrap_or_else(|| "(no item)".into());
                let ctx = pb
                    .context
                    .as_ref()
                    .map(|c| format!("{}/{}", c.kind, c.uri))
                    .unwrap_or_else(|| "none".into());
                println!(
                    "[{:>4}ms] is_playing={} track={track} ctx={ctx}",
                    (i + 1) * 500,
                    pb.is_playing
                );
            }
            Err(e) => println!("[{:>4}ms] error: {e:#}", (i + 1) * 500),
        }
    }

    println!("\n=== devices after play ===");
    if let Ok(devs) = client.get_devices().await {
        for d in devs {
            println!(
                "  - name={:<24} id={:<40} active={}",
                d.name,
                d.id.as_deref().unwrap_or("(none)"),
                d.is_active
            );
        }
    }
    Ok(())
}
