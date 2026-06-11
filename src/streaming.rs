use anyhow::{anyhow, Context, Result};
use librespot_connect::{ConnectConfig, Spirc};
use librespot_core::{
    authentication::Credentials, cache::Cache, config::DeviceType, config::SessionConfig, Session,
};
use librespot_playback::{
    audio_backend,
    config::{AudioFormat, Bitrate, PlayerConfig},
    mixer::{softmixer::SoftMixer, Mixer, MixerConfig},
    player,
};
use std::{path::PathBuf, sync::Arc};

pub struct Streaming {
    spirc: Spirc,
    pub device_name: String,
    pub device_id: String,
}

impl Streaming {
    /// Tell librespot to disconnect from Spotify Connect and end its
    /// background task. The Spirc command is fire-and-forget; if the
    /// session is already broken this may error but we don't care —
    /// we're tearing it down either way.
    pub fn shutdown(&self) -> Result<()> {
        self.spirc.shutdown().map_err(|e| anyhow!("{e}"))
    }
}

/// The redirect URI librespot's own client id has registered — see
/// librespot's oauth example. Distinct from hifi's Web-API redirect (8989)
/// so the two flows can never collide on a port.
const OAUTH_REDIRECT: &str = "http://127.0.0.1:8898/login";

/// Make sure reusable librespot credentials exist in the cache, minting them
/// via Spotify's OAuth flow if missing. First run only; afterwards the cached
/// credentials.json short-circuits. Must run BEFORE the TUI owns the terminal:
/// it prints instructions to stderr and opens a browser.
pub async fn ensure_credentials() -> Result<()> {
    let cache_dir = librespot_cache_dir();
    let cache = Cache::new(Some(&cache_dir), None, None, None).context("librespot cache")?;
    if cache.credentials().is_some() {
        return Ok(());
    }

    eprintln!();
    eprintln!("One-time audio setup: hifi needs a second Spotify approval so it");
    eprintln!("can play audio itself (the first approval covered search/control).");
    eprintln!("Opening your browser...");

    let session_config = SessionConfig::default();
    let oauth = librespot_oauth::OAuthClientBuilder::new(
        &session_config.client_id,
        OAUTH_REDIRECT,
        vec!["streaming"],
    )
    .open_in_browser()
    .build()
    .context("build librespot oauth client")?;
    let token = oauth
        .get_access_token_async()
        .await
        .map_err(|e| anyhow!("librespot oauth: {e}"))?;

    // The token is short-lived; one real login with store_credentials=true
    // converts it into reusable stored credentials (credentials.json in the
    // cache). The session is dropped right after — the Connect device proper
    // is brought up later by `start` on the run loop's reconnect path.
    let session = Session::new(session_config, Some(cache));
    session
        .connect(Credentials::with_access_token(token.access_token), true)
        .await
        .context("librespot login")?;
    session.shutdown();

    eprintln!(
        "Audio output ready — credentials cached in {}.",
        cache_dir.display()
    );
    Ok(())
}

pub async fn start(device_name: &str) -> Result<Streaming> {
    let cache_dir = librespot_cache_dir();
    let cache = Cache::new(Some(&cache_dir), None, None, None).context("librespot cache")?;
    let creds = cache.credentials().ok_or_else(|| {
        anyhow!("no audio credentials cached — quit and relaunch hifi to set up audio output")
    })?;

    let session = Session::new(SessionConfig::default(), Some(cache));
    let device_id = session.device_id().to_string();
    // Don't call session.connect() here — Spirc::new performs the connect itself
    // when given a fresh Session + Credentials. Pre-connecting trips a
    // "Service unavailable { Session is not connected }" inside Spirc.

    let connect_config = ConnectConfig {
        name: device_name.to_string(),
        device_type: DeviceType::Computer,
        // Default to 100% — the user controls volume via their system mixer.
        initial_volume: u16::MAX,
        is_group: false,
        disable_volume: false,
        volume_steps: 64,
    };

    let mixer = Arc::new(SoftMixer::open(MixerConfig::default()).context("softmixer")?);
    mixer.set_volume(connect_config.initial_volume);

    let backend =
        audio_backend::find(None).ok_or_else(|| anyhow!("no audio backend compiled in"))?;

    let player_config = PlayerConfig {
        bitrate: Bitrate::default(),
        ..Default::default()
    };

    let player = player::Player::new(
        player_config,
        session.clone(),
        mixer.get_soft_volume(),
        move || backend(None, AudioFormat::default()),
    );

    let (spirc, spirc_task) = Spirc::new(connect_config, session, creds, player, mixer)
        .await
        .context("spirc init")?;

    tokio::spawn(spirc_task);

    Ok(Streaming {
        spirc,
        device_name: device_name.to_string(),
        device_id,
    })
}

fn librespot_cache_dir() -> PathBuf {
    if let Ok(p) = std::env::var("HIFI_LIBRESPOT_CACHE") {
        return PathBuf::from(p);
    }
    let home = dirs_next::home_dir().unwrap_or_else(|| PathBuf::from("."));
    let ours = home.join(".cache").join("hifi");
    // Earlier builds borrowed spotify-player's librespot cache instead of
    // owning one. Keep honoring it when it's the only place with credentials
    // so existing setups don't get re-prompted to authorize audio.
    let legacy = home.join(".cache").join("spotify-player");
    if !ours.join("credentials.json").exists() && legacy.join("credentials.json").exists() {
        return legacy;
    }
    ours
}
