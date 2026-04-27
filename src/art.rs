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
