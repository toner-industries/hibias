use anyhow::{anyhow, Context, Result};
use librespot_connect::{ConnectConfig, Spirc};
use librespot_core::{cache::Cache, config::DeviceType, config::SessionConfig, Session};
use librespot_playback::{
    audio_backend,
    audio_backend::{Sink, SinkResult},
    config::{AudioFormat, Bitrate, PlayerConfig},
    convert::Converter,
    decoder::AudioPacket,
    mixer::{softmixer::SoftMixer, Mixer, MixerConfig},
    player,
};
use parking_lot::Mutex;
use rustfft::{num_complex::Complex, FftPlanner};
use std::{
    collections::VecDeque,
    path::PathBuf,
    sync::Arc,
    time::{Duration, Instant},
};

const FFT_SIZE: usize = 1024;
const HOP_SIZE: usize = 128;
pub const NUM_BANDS: usize = 128;

const DECAY_FACTOR: f32 = 0.985;
const DECAY_FACTOR_PEAK: f32 = 0.9985;
const SAMPLE_RATE: f32 = 44_100.0;

pub struct VisBands {
    pub values: [f32; NUM_BANDS],
    pub updated_at: Instant,
    pub peak_envelope: f32,
    pub is_active: bool,
}

impl Default for VisBands {
    fn default() -> Self {
        Self {
            values: [0.0; NUM_BANDS],
            updated_at: Instant::now(),
            peak_envelope: 1e-6,
            is_active: false,
        }
    }
}

pub fn decay_for_elapsed(elapsed: Duration) -> f32 {
    let elapsed_hops = elapsed.as_secs_f32() * SAMPLE_RATE / HOP_SIZE as f32;
    DECAY_FACTOR.powf(elapsed_hops)
}

pub fn peak_decay_for_elapsed(elapsed: Duration) -> f32 {
    let elapsed_hops = elapsed.as_secs_f32() * SAMPLE_RATE / HOP_SIZE as f32;
    DECAY_FACTOR_PEAK.powf(elapsed_hops)
}

pub struct Streaming {
    #[allow(dead_code)]
    spirc: Spirc,
    pub bands: Arc<Mutex<VisBands>>,
    pub device_name: String,
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
    // Don't call session.connect() here — Spirc::new performs the connect itself
    // when given a fresh Session + Credentials. Pre-connecting trips a
    // "Service unavailable { Session is not connected }" inside Spirc.

    let bands = Arc::new(Mutex::new(VisBands::default()));

    let connect_config = ConnectConfig {
        name: device_name.to_string(),
        device_type: DeviceType::Computer,
        initial_volume: 32_768,
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

    let bands_for_sink = Arc::clone(&bands);
    let player = player::Player::new(
        player_config,
        session.clone(),
        mixer.get_soft_volume(),
        move || -> Box<dyn Sink> {
            let real = backend(None, AudioFormat::default());
            Box::new(VisualizationSink::new(
                real,
                Arc::clone(&bands_for_sink),
                44_100.0,
            ))
        },
    );

    let bands_for_events = Arc::clone(&bands);
    let mut event_rx = player.get_player_event_channel();
    let events_task = tokio::spawn(async move {
        while let Some(ev) = event_rx.recv().await {
            match ev {
                player::PlayerEvent::Playing { .. } => {
                    bands_for_events.lock().is_active = true;
                }
                player::PlayerEvent::Paused { .. }
                | player::PlayerEvent::Stopped { .. }
                | player::PlayerEvent::EndOfTrack { .. } => {
                    bands_for_events.lock().is_active = false;
                }
                _ => {}
            }
        }
    });

    let (spirc, spirc_task) = Spirc::new(connect_config, session, creds, player, mixer)
        .await
        .context("spirc init")?;

    tokio::spawn(async move {
        tokio::select! {
            () = spirc_task => {}
            _ = events_task => {}
        }
    });

    Ok(Streaming {
        spirc,
        bands,
        device_name: device_name.to_string(),
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

struct VisualizationSink {
    inner: Box<dyn Sink>,
    sample_buf: VecDeque<f32>,
    bands: Arc<Mutex<VisBands>>,
    fft: Arc<dyn rustfft::Fft<f32>>,
    hann_window: Vec<f32>,
    fft_buf: Vec<Complex<f32>>,
    magnitudes: Vec<f32>,
    sample_rate: f32,
    band_ranges: Vec<(usize, usize)>,
    new_bands: [f32; NUM_BANDS],
    smooth_scratch: [f32; NUM_BANDS],
}

impl VisualizationSink {
    fn new(inner: Box<dyn Sink>, bands: Arc<Mutex<VisBands>>, sample_rate: f32) -> Self {
        let mut planner = FftPlanner::<f32>::new();
        let fft = planner.plan_fft_forward(FFT_SIZE);
        let hann_window: Vec<f32> = (0..FFT_SIZE)
            .map(|i| {
                0.5 * (1.0 - (2.0 * std::f32::consts::PI * i as f32 / (FFT_SIZE - 1) as f32).cos())
            })
            .collect();
        let band_ranges = precompute_band_ranges(FFT_SIZE / 2, NUM_BANDS);
        Self {
            inner,
            sample_buf: VecDeque::with_capacity(FFT_SIZE * 2),
            bands,
            fft,
            hann_window,
            fft_buf: vec![Complex::new(0.0, 0.0); FFT_SIZE],
            magnitudes: vec![0.0; FFT_SIZE / 2],
            sample_rate,
            band_ranges,
            new_bands: [0.0; NUM_BANDS],
            smooth_scratch: [0.0; NUM_BANDS],
        }
    }
}

impl Sink for VisualizationSink {
    fn start(&mut self) -> SinkResult<()> {
        self.inner.start()
    }

    fn stop(&mut self) -> SinkResult<()> {
        let mut g = self.bands.lock();
        g.values.fill(0.0);
        g.peak_envelope = 1e-6;
        g.updated_at = Instant::now();
        g.is_active = false;
        drop(g);
        self.sample_buf.clear();
        self.inner.stop()
    }

    fn write(&mut self, packet: AudioPacket, converter: &mut Converter) -> SinkResult<()> {
        if let AudioPacket::Samples(ref samples) = packet {
            self.sample_buf.extend(samples.chunks(2).map(|c| {
                if c.len() == 2 {
                    f64::midpoint(c[0], c[1]) as f32
                } else {
                    c[0] as f32
                }
            }));

            while self.sample_buf.len() >= FFT_SIZE {
                {
                    let (front, back) = self.sample_buf.as_slices();
                    if front.len() >= FFT_SIZE {
                        for (dst, (&s, &w)) in self
                            .fft_buf
                            .iter_mut()
                            .zip(front.iter().zip(self.hann_window.iter()))
                        {
                            *dst = Complex::new(s * w, 0.0);
                        }
                    } else {
                        let split = front.len();
                        for (dst, (&s, &w)) in self.fft_buf[..split]
                            .iter_mut()
                            .zip(front.iter().zip(self.hann_window[..split].iter()))
                        {
                            *dst = Complex::new(s * w, 0.0);
                        }
                        let remaining = FFT_SIZE - split;
                        for (dst, (&s, &w)) in self.fft_buf[split..].iter_mut().zip(
                            back[..remaining]
                                .iter()
                                .zip(self.hann_window[split..].iter()),
                        ) {
                            *dst = Complex::new(s * w, 0.0);
                        }
                    }
                }

                self.fft.process(&mut self.fft_buf);

                for (mag, c) in self.magnitudes.iter_mut().zip(self.fft_buf.iter()) {
                    *mag = c.norm();
                }

                fill_log_bands(&self.magnitudes, &self.band_ranges, &mut self.new_bands);
                smooth_bands(&mut self.new_bands, &mut self.smooth_scratch);

                let mut g = self.bands.lock();
                let elapsed_hops =
                    g.updated_at.elapsed().as_secs_f32() * self.sample_rate / HOP_SIZE as f32;
                let decay = DECAY_FACTOR.powf(elapsed_hops);
                let peak_decay = DECAY_FACTOR_PEAK.powf(elapsed_hops);
                let frame_peak = self.new_bands.iter().copied().fold(0.0f32, f32::max);
                for (stored, fresh) in g.values.iter_mut().zip(self.new_bands.iter()) {
                    *stored = (*stored * decay).max(*fresh);
                }
                g.peak_envelope = (g.peak_envelope * peak_decay).max(frame_peak);
                g.updated_at = Instant::now();
                drop(g);

                self.sample_buf.drain(..HOP_SIZE);
            }
        }

        self.inner.write(packet, converter)
    }
}

fn precompute_band_ranges(num_bins: usize, num_bands: usize) -> Vec<(usize, usize)> {
    let log_min = 1.0_f64;
    let log_max = num_bins as f64;
    let mut used_up_to: usize = 1;
    let mut ranges = Vec::with_capacity(num_bands);
    for band in 0..num_bands {
        if used_up_to >= num_bins {
            ranges.push((num_bins - 1, num_bins));
            continue;
        }
        let t_start = band as f64 / num_bands as f64;
        let t_end = (band + 1) as f64 / num_bands as f64;
        let natural_start = (log_min * (log_max / log_min).powf(t_start)) as usize;
        let natural_end = (log_min * (log_max / log_min).powf(t_end)) as usize;
        let start = natural_start.max(used_up_to).min(num_bins - 1);
        let end = natural_end.max(start + 1).min(num_bins);
        used_up_to = end;
        ranges.push((start, end));
    }
    ranges
}

fn fill_log_bands(magnitudes: &[f32], band_ranges: &[(usize, usize)], out: &mut [f32]) {
    for (band_val, &(start, end)) in out.iter_mut().zip(band_ranges.iter()) {
        let len = (end - start) as f32;
        let sum_sq: f32 = magnitudes[start..end].iter().map(|&v| v * v).sum();
        *band_val = (sum_sq / len).sqrt();
    }
}

fn smooth_bands(bands: &mut [f32], scratch: &mut [f32]) {
    let n = bands.len();
    if n < 3 {
        return;
    }
    scratch[..n].copy_from_slice(&bands[..n]);
    for i in 0..n {
        let prev = scratch[if i > 0 { i - 1 } else { 0 }];
        let next = scratch[if i + 1 < n { i + 1 } else { n - 1 }];
        bands[i] = prev * 0.25 + scratch[i] * 0.5 + next * 0.25;
    }
}
