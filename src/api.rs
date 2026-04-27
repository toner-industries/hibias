use anyhow::{Context, Result};
use serde::Deserialize;

use crate::auth::Auth;

const BASE: &str = "https://api.spotify.com/v1";

pub struct SpotifyClient {
    http: reqwest::Client,
    auth: Auth,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Playback {
    pub is_playing: bool,
    pub progress_ms: Option<u64>,
    pub item: Option<Track>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Track {
    pub id: Option<String>,
    pub name: String,
    pub duration_ms: u64,
    pub artists: Vec<Artist>,
    pub album: Album,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Artist {
    pub name: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Album {
    pub name: String,
    #[serde(default)]
    pub images: Vec<Image>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Image {
    pub url: String,
    pub width: Option<u32>,
    pub height: Option<u32>,
}

impl Album {
    /// Pick a reasonable mid-size cover (~300px) — Spotify returns descending sizes.
    pub fn cover_url(&self) -> Option<&str> {
        let mid = self
            .images
            .iter()
            .min_by_key(|i| (i.width.unwrap_or(640) as i32 - 300).abs());
        mid.or_else(|| self.images.first()).map(|i| i.url.as_str())
    }
}

impl SpotifyClient {
    pub fn new(auth: Auth) -> Result<Self> {
        Ok(Self {
            http: reqwest::Client::builder().build()?,
            auth,
        })
    }

    async fn bearer(&self) -> Result<String> {
        Ok(format!("Bearer {}", self.auth.token().await?))
    }

    pub async fn get_playback(&self) -> Result<Option<Playback>> {
        let resp = self
            .http
            .get(format!("{BASE}/me/player"))
            .header("Authorization", self.bearer().await?)
            .send()
            .await
            .context("GET /me/player")?;
        if resp.status() == reqwest::StatusCode::NO_CONTENT {
            return Ok(None);
        }
        if resp.status() == reqwest::StatusCode::TOO_MANY_REQUESTS {
            let retry = retry_after_secs(&resp).unwrap_or(30);
            anyhow::bail!(RateLimited(retry));
        }
        if !resp.status().is_success() {
            let status = resp.status();
            let body = resp.text().await.unwrap_or_default();
            anyhow::bail!("GET /me/player: {status}: {body}");
        }
        let pb: Playback = resp.json().await.context("parse /me/player body")?;
        Ok(Some(pb))
    }

    pub async fn play(&self) -> Result<()> {
        self.put_command("play").await
    }

    pub async fn pause(&self) -> Result<()> {
        self.put_command("pause").await
    }

    async fn put_command(&self, cmd: &str) -> Result<()> {
        let resp = self
            .http
            .put(format!("{BASE}/me/player/{cmd}"))
            .header("Authorization", self.bearer().await?)
            .header("Content-Length", "0")
            .send()
            .await
            .with_context(|| format!("PUT /me/player/{cmd}"))?;
        if resp.status() == reqwest::StatusCode::TOO_MANY_REQUESTS {
            let retry = retry_after_secs(&resp).unwrap_or(30);
            anyhow::bail!(RateLimited(retry));
        }
        let status = resp.status();
        if !status.is_success() {
            let body = resp.text().await.unwrap_or_default();
            anyhow::bail!("PUT /me/player/{cmd}: {status}: {body}");
        }
        Ok(())
    }
}

#[derive(Debug)]
pub struct RateLimited(pub u64);

impl std::fmt::Display for RateLimited {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "rate limited by Spotify; retry after {}s", self.0)
    }
}

impl std::error::Error for RateLimited {}

fn retry_after_secs(resp: &reqwest::Response) -> Option<u64> {
    resp.headers()
        .get(reqwest::header::RETRY_AFTER)?
        .to_str()
        .ok()?
        .parse()
        .ok()
}
