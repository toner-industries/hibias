use anyhow::{anyhow, Context, Result};
use ratatui_image::{picker::Picker, protocol::StatefulProtocol};

pub struct ArtLoader {
    picker: Option<Picker>,
    http: reqwest::Client,
}

impl ArtLoader {
    pub fn new(http: reqwest::Client) -> Self {
        let picker = Picker::from_query_stdio().ok();
        Self { picker, http }
    }

    pub fn enabled(&self) -> bool {
        self.picker.is_some()
    }

    pub async fn load(&self, url: &str) -> Result<StatefulProtocol> {
        let picker = self
            .picker
            .as_ref()
            .ok_or_else(|| anyhow!("no image protocol detected for this terminal"))?;
        let bytes = self
            .http
            .get(url)
            .send()
            .await
            .context("download cover")?
            .error_for_status()?
            .bytes()
            .await?;
        let img = image::load_from_memory(&bytes).context("decode cover")?;
        Ok(picker.new_resize_protocol(img))
    }
}

/// Head-owned cache of the currently-decoded album art. Lives in the TUI run
/// loop, never in `AppState`, so the core stays free of ratatui types — the
/// core only signals *what* to fetch via [`crate::app::ArtRequest`].
///
/// Keyed by track id: the renderer asks for art by the track it's drawing, so
/// a stale image is never paired with a newly-changed track.
#[derive(Default)]
pub struct ArtCache {
    current: Option<(String, StatefulProtocol)>,
    loading: Option<String>,
}

impl ArtCache {
    pub fn new() -> Self {
        Self::default()
    }

    /// The decoded image for `track_id`, if that's the one we hold. Any other
    /// id (including `None`) yields `None`, so last track's art never lingers.
    pub fn get_for(&mut self, track_id: Option<&str>) -> Option<&mut StatefulProtocol> {
        match (&mut self.current, track_id) {
            (Some((id, proto)), Some(t)) if id == t => Some(proto),
            _ => None,
        }
    }

    /// True if this id is already decoded or mid-fetch — lets the run loop
    /// avoid respawning a fetch on every redraw tick.
    pub fn has_or_loading(&self, track_id: &str) -> bool {
        self.current.as_ref().map(|(id, _)| id == track_id).unwrap_or(false)
            || self.loading.as_deref() == Some(track_id)
    }

    pub fn begin_loading(&mut self, track_id: String) {
        self.loading = Some(track_id);
    }

    pub fn store(&mut self, track_id: String, proto: StatefulProtocol) {
        if self.loading.as_deref() == Some(track_id.as_str()) {
            self.loading = None;
        }
        self.current = Some((track_id, proto));
    }
}
