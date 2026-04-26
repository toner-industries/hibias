use anyhow::{anyhow, Context, Result};
use serde::{Deserialize, Serialize};
use std::{
    path::{Path, PathBuf},
    time::{Duration, SystemTime, UNIX_EPOCH},
};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::TcpListener,
    sync::Mutex,
};

const AUTH_URL: &str = "https://accounts.spotify.com/authorize";
const TOKEN_URL: &str = "https://accounts.spotify.com/api/token";
const REDIRECT_URI: &str = "http://127.0.0.1:8989/login";
const SCOPES: &str = "user-read-playback-state user-modify-playback-state";
const REFRESH_MARGIN_SECS: u64 = 60;

pub struct Auth {
    client_id: String,
    http: reqwest::Client,
    state: Mutex<StoredTokens>,
}

#[derive(Serialize, Deserialize, Clone)]
struct StoredTokens {
    access_token: String,
    refresh_token: String,
    expires_at_unix: u64,
}

impl Auth {
    pub async fn init() -> Result<Self> {
        let client_id = resolve_client_id()
            .ok_or_else(|| anyhow!("no client_id; set HIFI_CLIENT_ID or hifi.toml"))?;
        let http = reqwest::Client::builder().build()?;
        let path = auth_state_path();

        let tokens = match load_tokens(&path) {
            Some(t) => t,
            None => {
                eprintln!("First-run authentication needed.");
                let t = run_oauth_flow(&client_id, &http).await?;
                save_tokens(&path, &t)?;
                t
            }
        };

        Ok(Self {
            client_id,
            http,
            state: Mutex::new(tokens),
        })
    }

    pub async fn token(&self) -> Result<String> {
        let mut state = self.state.lock().await;
        if !is_expired(&state) {
            return Ok(state.access_token.clone());
        }
        let refreshed = refresh(&self.client_id, &state.refresh_token, &self.http)
            .await
            .context(
                "token refresh failed; if you revoked access, delete hifi-auth.json and rerun",
            )?;
        *state = refreshed;
        save_tokens(&auth_state_path(), &state)?;
        Ok(state.access_token.clone())
    }
}

fn is_expired(t: &StoredTokens) -> bool {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    t.expires_at_unix.saturating_sub(REFRESH_MARGIN_SECS) <= now
}

#[derive(Deserialize)]
struct TokenResponse {
    access_token: String,
    refresh_token: Option<String>,
    expires_in: u64,
}

async fn run_oauth_flow(client_id: &str, http: &reqwest::Client) -> Result<StoredTokens> {
    let listener = TcpListener::bind("127.0.0.1:8989")
        .await
        .context("bind 127.0.0.1:8989 (already in use?)")?;

    let verifier = pkce::random_verifier();
    let challenge = pkce::challenge(&verifier);
    let state = pkce::random_state();

    let auth_url = format!(
        "{AUTH_URL}?client_id={cid}&response_type=code&redirect_uri={redir}\
         &scope={scope}&code_challenge_method=S256&code_challenge={chal}&state={state}",
        cid = urlencoding::encode(client_id),
        redir = urlencoding::encode(REDIRECT_URI),
        scope = urlencoding::encode(SCOPES),
        chal = urlencoding::encode(&challenge),
        state = urlencoding::encode(&state),
    );

    eprintln!("Opening browser for Spotify authorization...");
    eprintln!("If it doesn't open, visit:\n  {auth_url}");
    let _ = open::that(&auth_url);

    let code = wait_for_callback(listener, &state).await?;

    let resp: TokenResponse = http
        .post(TOKEN_URL)
        .form(&[
            ("grant_type", "authorization_code"),
            ("code", &code),
            ("redirect_uri", REDIRECT_URI),
            ("client_id", client_id),
            ("code_verifier", &verifier),
        ])
        .send()
        .await
        .context("token exchange request failed")?
        .error_for_status()
        .context("token exchange returned error")?
        .json()
        .await
        .context("parse token response")?;

    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    Ok(StoredTokens {
        access_token: resp.access_token,
        refresh_token: resp
            .refresh_token
            .ok_or_else(|| anyhow!("token response missing refresh_token"))?,
        expires_at_unix: now + resp.expires_in,
    })
}

async fn refresh(
    client_id: &str,
    refresh_token: &str,
    http: &reqwest::Client,
) -> Result<StoredTokens> {
    let resp: TokenResponse = http
        .post(TOKEN_URL)
        .form(&[
            ("grant_type", "refresh_token"),
            ("refresh_token", refresh_token),
            ("client_id", client_id),
        ])
        .send()
        .await?
        .error_for_status()?
        .json()
        .await?;

    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    Ok(StoredTokens {
        access_token: resp.access_token,
        refresh_token: resp
            .refresh_token
            .unwrap_or_else(|| refresh_token.to_string()),
        expires_at_unix: now + resp.expires_in,
    })
}

async fn wait_for_callback(listener: TcpListener, expected_state: &str) -> Result<String> {
    let timeout = Duration::from_secs(300);
    let accept = listener.accept();
    let (mut stream, _) = tokio::time::timeout(timeout, accept)
        .await
        .context("timed out waiting for browser redirect")??;

    let mut buf = vec![0u8; 8192];
    let n = stream.read(&mut buf).await?;
    let req = std::str::from_utf8(&buf[..n]).context("non-utf8 request")?;
    let line = req.lines().next().ok_or_else(|| anyhow!("empty request"))?;
    let path = line
        .split_whitespace()
        .nth(1)
        .ok_or_else(|| anyhow!("malformed request line: {line}"))?;
    let query = path.split_once('?').map(|(_, q)| q).unwrap_or("");

    let mut code = None;
    let mut got_state = None;
    let mut error = None;
    for pair in query.split('&') {
        let Some((k, v)) = pair.split_once('=') else {
            continue;
        };
        let v = urlencoding::decode(v).unwrap_or_default().into_owned();
        match k {
            "code" => code = Some(v),
            "state" => got_state = Some(v),
            "error" => error = Some(v),
            _ => {}
        }
    }

    let body = if let Some(e) = &error {
        format!("<h1>Auth failed</h1><p>{e}</p>")
    } else if code.is_some() {
        "<h1>Logged in</h1><p>You can close this tab.</p>".to_string()
    } else {
        "<h1>Bad request</h1>".to_string()
    };
    let resp = format!(
        "HTTP/1.1 200 OK\r\nContent-Type: text/html; charset=utf-8\r\n\
         Content-Length: {}\r\nConnection: close\r\n\r\n{}",
        body.len(),
        body
    );
    let _ = stream.write_all(resp.as_bytes()).await;
    let _ = stream.shutdown().await;

    if let Some(e) = error {
        anyhow::bail!("Spotify auth error: {e}");
    }
    let code = code.ok_or_else(|| anyhow!("no code in callback"))?;
    if got_state.as_deref() != Some(expected_state) {
        anyhow::bail!("OAuth state mismatch (CSRF guard)");
    }
    Ok(code)
}

mod pkce {
    use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine};
    use rand::RngCore;
    use sha2::{Digest, Sha256};

    pub fn random_verifier() -> String {
        let mut bytes = [0u8; 64];
        rand::rng().fill_bytes(&mut bytes);
        URL_SAFE_NO_PAD.encode(bytes)
    }

    pub fn challenge(verifier: &str) -> String {
        let hash = Sha256::digest(verifier.as_bytes());
        URL_SAFE_NO_PAD.encode(hash)
    }

    pub fn random_state() -> String {
        let mut bytes = [0u8; 16];
        rand::rng().fill_bytes(&mut bytes);
        URL_SAFE_NO_PAD.encode(bytes)
    }
}

fn auth_state_path() -> PathBuf {
    if let Ok(p) = std::env::var("HIFI_AUTH_FILE") {
        return PathBuf::from(p);
    }
    PathBuf::from("hifi-auth.json")
}

fn load_tokens(path: &Path) -> Option<StoredTokens> {
    let s = std::fs::read_to_string(path).ok()?;
    serde_json::from_str(&s).ok()
}

fn save_tokens(path: &Path, tokens: &StoredTokens) -> Result<()> {
    let s = serde_json::to_string_pretty(tokens)?;
    std::fs::write(path, s).context("save tokens")?;
    Ok(())
}

#[derive(Default, Deserialize)]
struct ConfigFile {
    client_id: Option<String>,
}

fn resolve_client_id() -> Option<String> {
    if let Ok(id) = std::env::var("HIFI_CLIENT_ID") {
        return Some(id);
    }
    let path = Path::new("hifi.toml");
    if !path.exists() {
        return None;
    }
    let s = std::fs::read_to_string(path).ok()?;
    let cfg: ConfigFile = toml::from_str(&s).ok()?;
    cfg.client_id
}
