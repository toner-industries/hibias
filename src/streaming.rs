use anyhow::{anyhow, Context, Result};
use librespot_connect::{ConnectConfig, Spirc};
use librespot_core::{cache::Cache, config::DeviceType, config::SessionConfig, Session};
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

pub async fn start(device_name: &str) -> Result<Streaming> {
    let cache_dir = librespot_cache_dir();
    let cache = Cache::new(Some(&cache_dir), None, None, None).context("librespot cache")?;
    let creds = cache.credentials().ok_or_else(|| {
        anyhow!(
            "no librespot credentials at {}/credentials.json — run spotify-player once first",
            cache_dir.display()
        )
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
    if let Some(home) = dirs_next::home_dir() {
        return home.join(".cache").join("spotify-player");
    }
    PathBuf::from(".cache/spotify-player")
}
