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
use std::{
    path::PathBuf,
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    },
};

pub struct Streaming {
    spirc: Spirc,
    /// A handle on the same session Spirc is driving, purely so we can ask
    /// whether it's still alive (see [`Streaming::is_dead`]).
    session: Session,
    /// Set when the Spirc background task returns. librespot's Spirc loop
    /// exits as soon as its session goes invalid — a network drop, a laptop
    /// suspend, or Spotify's access point hanging up on an idle connection —
    /// and it does so *silently*: the device just disappears from Spotify
    /// Connect while hibias keeps believing it's registered. This flag is how
    /// the watchdog notices, and it costs zero Web API requests to check.
    task_done: Arc<AtomicBool>,
    /// Set by [`Streaming::shutdown`] so the watchdog can tell our own
    /// teardown (reconnect, quit) from a session that died under us.
    intentional: Arc<AtomicBool>,
    pub device_name: String,
    pub device_id: String,
}

impl Streaming {
    /// Tell librespot to disconnect from Spotify Connect and end its
    /// background task. The Spirc command is fire-and-forget; if the
    /// session is already broken this may error but we don't care —
    /// we're tearing it down either way.
    pub fn shutdown(&self) -> Result<()> {
        self.intentional.store(true, Ordering::SeqCst);
        self.spirc.shutdown().map_err(|e| anyhow!("{e}"))
    }

    /// True when this Connect device is gone from Spotify's point of view but
    /// we never asked for that — the state the watchdog exists to repair.
    /// Deliberately local-only (an atomic plus librespot's own session flag):
    /// health checks must never cost a request, or the watchdog becomes its
    /// own rate-limit problem.
    pub fn is_dead(&self) -> bool {
        !self.intentional.load(Ordering::SeqCst)
            && (self.task_done.load(Ordering::SeqCst) || self.session.is_invalid())
    }
}

/// The redirect URI librespot's own client id has registered — see
/// librespot's oauth example. Distinct from hibias's Web-API redirect (8989)
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
    eprintln!("One-time audio setup: hibias needs a second Spotify approval so it");
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
        anyhow!("no audio credentials cached — quit and relaunch hibias to set up audio output")
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

    let (spirc, spirc_task) = Spirc::new(connect_config, session.clone(), creds, player, mixer)
        .await
        .context("spirc init")?;

    // Wrap the Spirc task so its exit is observable. Bare `tokio::spawn` here
    // meant a dead session was indistinguishable from a healthy one until the
    // user pressed a key and got a 404 back from Spotify.
    let task_done = Arc::new(AtomicBool::new(false));
    let intentional = Arc::new(AtomicBool::new(false));
    {
        let task_done = task_done.clone();
        let intentional = intentional.clone();
        tokio::spawn(async move {
            spirc_task.await;
            task_done.store(true, Ordering::SeqCst);
            if !intentional.load(Ordering::SeqCst) {
                crate::log::note("spirc task exited (session died)", None);
            }
        });
    }

    Ok(Streaming {
        spirc,
        session,
        task_done,
        intentional,
        device_name: device_name.to_string(),
        device_id,
    })
}

fn librespot_cache_dir() -> PathBuf {
    if let Ok(p) = std::env::var("HIBIAS_LIBRESPOT_CACHE") {
        return PathBuf::from(p);
    }
    let home = dirs_next::home_dir().unwrap_or_else(|| PathBuf::from("."));
    let ours = home.join(".cache").join("hibias");
    // Earlier builds borrowed spotify-player's librespot cache instead of
    // owning one. Keep honoring it when it's the only place with credentials
    // so existing setups don't get re-prompted to authorize audio.
    let legacy = home.join(".cache").join("spotify-player");
    if !ours.join("credentials.json").exists() && legacy.join("credentials.json").exists() {
        return legacy;
    }
    ours
}
