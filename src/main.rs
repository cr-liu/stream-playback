use clap::Parser;
use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use cpal::SampleFormat;
use ringbuf::traits::{Consumer, Producer, Split};
use ringbuf::{HeapCons, HeapRb};
use serde::Deserialize;
use std::fs;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicI32, AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};
use tokio::net::UdpSocket;
use tokio::sync::mpsc;
use tokio::time::{self, Duration};

const HEADER_LEN: usize = 12;

#[derive(Parser, Debug)]
#[command(version, about = "UDP audio stream playback test tool")]
struct Cli {
    /// Optional path to TOML config file
    #[arg(long)]
    config: Option<PathBuf>,
    /// Server host (IP or hostname)
    #[arg(long)]
    host: Option<String>,
    /// Server port
    #[arg(long)]
    port: Option<u16>,
    /// Sample rate (Hz)
    #[arg(long)]
    sample_rate: Option<u32>,
    /// Number of channels
    #[arg(long)]
    n_channel: Option<u16>,
    /// Samples per channel per packet
    #[arg(long)]
    sample_per_packet: Option<usize>,
    /// Override packet size in bytes (else computed)
    #[arg(long)]
    pkt_len: Option<usize>,
    /// Bind local UDP port to this value (else ephemeral); when set, suppresses register
    #[arg(long)]
    listen_port: Option<u16>,
}

#[derive(Deserialize, Debug, Default)]
struct FileConfig {
    #[serde(default)]
    receiver: ReceiverSection,
    #[serde(default)]
    gui: GuiSection,
}

#[derive(Deserialize, Debug, Default)]
struct ReceiverSection {
    host: Option<String>,
    port: Option<u16>,
    sample_rate: Option<u32>,
    n_channel: Option<u16>,
    sample_per_packet: Option<usize>,
    pkt_len: Option<usize>,
    listen_port: Option<u16>,
}

#[derive(Deserialize, Debug, Default)]
struct GuiSection {
    enabled: Option<bool>,
    bind_addr: Option<String>,
    port: Option<u16>,
    password: Option<String>,
}

#[derive(Debug)]
struct Config {
    host: String,
    port: u16,
    sample_rate: u32,
    sample_per_packet: usize,
    pkt_len: usize,
    listen_port: Option<u16>,
    gui_enabled: bool,
    gui_bind_addr: String,
    gui_port: u16,
    gui_password: String,
}

impl Config {
    fn resolve(cli: Cli) -> Result<Config, String> {
        let file = match cli.config.as_ref() {
            Some(path) => {
                let text = fs::read_to_string(path)
                    .map_err(|e| format!("failed to read config {}: {}", path.display(), e))?;
                toml::from_str::<FileConfig>(&text)
                    .map_err(|e| format!("failed to parse config: {}", e))?
            }
            None => FileConfig::default(),
        };

        let host = cli.host.or(file.receiver.host)
            .ok_or_else(|| "--host is required (or set in config)".to_string())?;
        let port = cli.port.or(file.receiver.port).unwrap_or(7998);
        let sample_rate = cli.sample_rate.or(file.receiver.sample_rate).unwrap_or(16000);
        // n_channel is only used to compute the default pkt_len — it represents
        // the sender's total channel count, not the playback channel count.
        let n_channel = cli.n_channel.or(file.receiver.n_channel).unwrap_or(1);
        let sample_per_packet = cli.sample_per_packet.or(file.receiver.sample_per_packet).unwrap_or(32);
        let pkt_len = cli.pkt_len.or(file.receiver.pkt_len)
            .unwrap_or(HEADER_LEN + n_channel as usize * sample_per_packet * 2);
        let listen_port = cli.listen_port.or(file.receiver.listen_port);

        let gui_enabled = file.gui.enabled.unwrap_or(true);
        let gui_bind_addr = file.gui.bind_addr.unwrap_or_else(|| "0.0.0.0".to_string());
        let gui_port = file.gui.port.unwrap_or(8081);
        let gui_password = file.gui.password.unwrap_or_else(|| "test".to_string());

        Ok(Config {
            host, port, sample_rate, sample_per_packet, pkt_len, listen_port,
            gui_enabled, gui_bind_addr, gui_port, gui_password,
        })
    }
}

#[derive(Default)]
struct Stats {
    received: AtomicU64,
    lost: AtomicU64,
    latest_pkt_id: AtomicI32,
}

impl Stats {
    fn snapshot(&self) -> (u64, u64, i32) {
        (
            self.received.load(Ordering::Relaxed),
            self.lost.load(Ordering::Relaxed),
            self.latest_pkt_id.load(Ordering::Relaxed),
        )
    }
}

async fn stats_task(stats: Arc<Stats>) {
    let mut interval = time::interval(Duration::from_secs(1));
    interval.tick().await;
    let mut last_received: u64 = 0;
    let mut last_lost: u64 = 0;
    loop {
        interval.tick().await;
        let (received, lost, pkt_id) = stats.snapshot();
        let delta_recv = received.saturating_sub(last_received);
        let delta_lost = lost.saturating_sub(last_lost);
        last_received = received;
        last_lost = lost;

        let secs = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_secs();
        let hh = (secs / 3600) % 24;
        let mm = (secs / 60) % 60;
        let ss = secs % 60;

        println!(
            "[{:02}:{:02}:{:02}] recv/s={:<5} lost/s={:<5} total_recv={:<8} total_lost={:<6} pkt_id={}",
            hh, mm, ss, delta_recv, delta_lost, received, lost, pkt_id,
        );
    }
}

async fn recv_loop(
    host: String,
    port: u16,
    pkt_len: usize,
    sample_per_packet: usize,
    listen_port: Option<u16>,
    stats: Arc<Stats>,
    sample_tx: mpsc::UnboundedSender<Vec<i16>>,
    waveform_tap: tokio::sync::broadcast::Sender<Vec<i16>>,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let bind_addr = match listen_port {
        Some(p) => format!("0.0.0.0:{}", p),
        None => "0.0.0.0:0".to_string(),
    };
    let socket = UdpSocket::bind(&bind_addr).await?;
    let server_addr = format!("{}:{}", host, port);
    socket.connect(&server_addr).await?;
    println!("Connected to {} (local bind {})", server_addr, bind_addr);

    // Only send register when the server doesn't know our address.
    let use_register = listen_port.is_none();
    if use_register {
        socket.send(b"register").await?;
    }

    let mut buf = vec![0u8; pkt_len];
    let mut reg_interval = time::interval(Duration::from_secs(2));
    reg_interval.tick().await;

    let mut last_pkt_id: Option<i32> = None;
    let mut last_warn = std::time::Instant::now() - Duration::from_secs(10);

    loop {
        tokio::select! {
            result = socket.recv(&mut buf) => {
                match result {
                    Ok(n) if n == pkt_len => {
                        let pkt_id = i32::from_le_bytes(buf[8..12].try_into().unwrap());

                        if let Some(last) = last_pkt_id {
                            let diff = pkt_id.wrapping_sub(last);
                            if diff == 1 {
                                // normal
                            } else if diff > 1 && diff < 1000 {
                                stats.lost.fetch_add((diff - 1) as u64, Ordering::Relaxed);
                            } else {
                                continue;
                            }
                        }
                        last_pkt_id = Some(pkt_id);

                        stats.received.fetch_add(1, Ordering::Relaxed);
                        stats.latest_pkt_id.store(pkt_id, Ordering::Relaxed);

                        // Channel-major format: ch0 is the first block after the header.
                        // Extract only ch0 samples and discard the rest.
                        let ch0_end = HEADER_LEN + sample_per_packet * 2;
                        if n >= ch0_end {
                            let ch0_bytes = &buf[HEADER_LEN..ch0_end];
                            let samples: Vec<i16> = ch0_bytes
                                .chunks_exact(2)
                                .map(|c| i16::from_le_bytes([c[0], c[1]]))
                                .collect();
                            let _ = waveform_tap.send(samples.clone());
                            let _ = sample_tx.send(samples);
                        }
                    }
                    Ok(n) => {
                        if last_warn.elapsed() > Duration::from_secs(1) {
                            eprintln!("unexpected packet size {} (expected {})", n, pkt_len);
                            last_warn = std::time::Instant::now();
                        }
                    }
                    Err(e) => eprintln!("UDP recv error: {}", e),
                }
            }
            _ = reg_interval.tick() => {
                if use_register {
                    let _ = socket.send(b"register").await;
                }
            }
        }
    }
}

/// Start cpal playback.
///
/// Adapts to the device's default output config: queries sample rate,
/// channel count, and sample format, and converts the incoming mono
/// i16 stream on the fly.
///
/// Resampling is nearest-neighbor (fine for a test tool; not hi-fi).
/// Mono → N channels is done by duplicating the sample to every channel.
fn start_playback(
    stream_rate: u32,
    mut consumer: HeapCons<i16>,
    playback_pos: Arc<std::sync::atomic::AtomicU64>,
) -> Result<cpal::Stream, Box<dyn std::error::Error>> {
    let host = cpal::default_host();
    let device = host.default_output_device().ok_or("no default output device")?;
    println!("Playback device: {}", device.name().unwrap_or_default());

    let supported = device.default_output_config()?;
    let sample_format = supported.sample_format();
    let output_rate = supported.sample_rate().0;
    let output_channels = supported.channels();
    let config = supported.config();

    println!(
        "Device output: {} Hz, {} ch, {:?}; stream source: {} Hz mono",
        output_rate, output_channels, sample_format, stream_rate,
    );

    // Nearest-neighbor resample: for every output frame, advance a
    // fractional source index by (stream_rate / output_rate).
    // When the accumulator crosses 1.0, pop one source sample.
    let step = stream_rate as f64 / output_rate as f64;
    let channels = output_channels as usize;

    let err_fn = |err| eprintln!("Playback stream error: {}", err);

    let stream = match sample_format {
        SampleFormat::I16 => {
            let mut src_pos = 1.0_f64; // force first pop on first frame
            let mut current: i16 = 0;
            let playback_pos_cap = playback_pos.clone();
            device.build_output_stream(
                &config,
                move |data: &mut [i16], _: &cpal::OutputCallbackInfo| {
                    let frames = data.len() / channels;
                    let mut played_this_call: u64 = 0;
                    for f in 0..frames {
                        while src_pos >= 1.0 {
                            src_pos -= 1.0;
                            let mut buf = [0i16; 1];
                            let got = consumer.pop_slice(&mut buf);
                            played_this_call += got as u64;
                            current = if got == 1 { buf[0] } else { 0 };
                        }
                        for c in 0..channels {
                            data[f * channels + c] = current;
                        }
                        src_pos += step;
                    }
                    playback_pos_cap.fetch_add(played_this_call, std::sync::atomic::Ordering::Relaxed);
                },
                err_fn,
                None,
            )?
        }
        SampleFormat::F32 => {
            let mut src_pos = 1.0_f64;
            let mut current: f32 = 0.0;
            let playback_pos_cap = playback_pos.clone();
            device.build_output_stream(
                &config,
                move |data: &mut [f32], _: &cpal::OutputCallbackInfo| {
                    let frames = data.len() / channels;
                    let mut played_this_call: u64 = 0;
                    for f in 0..frames {
                        while src_pos >= 1.0 {
                            src_pos -= 1.0;
                            let mut buf = [0i16; 1];
                            let got = consumer.pop_slice(&mut buf);
                            played_this_call += got as u64;
                            current = if got == 1 {
                                buf[0] as f32 / 32768.0
                            } else {
                                0.0
                            };
                        }
                        for c in 0..channels {
                            data[f * channels + c] = current;
                        }
                        src_pos += step;
                    }
                    playback_pos_cap.fetch_add(played_this_call, std::sync::atomic::Ordering::Relaxed);
                },
                err_fn,
                None,
            )?
        }
        other => return Err(format!("unsupported sample format: {:?}", other).into()),
    };

    stream.play()?;
    Ok(stream)
}

#[tokio::main(flavor = "current_thread")]
async fn main() {
    let cli = Cli::parse();
    let cfg = match Config::resolve(cli) {
        Ok(c) => c,
        Err(e) => { eprintln!("Error: {}", e); std::process::exit(1); }
    };
    println!("Config: {:?}", cfg);
    println!("Expected packet size: {} bytes", cfg.pkt_len);

    let stats = Arc::new(Stats::default());
    let stats_handle = tokio::spawn(stats_task(stats.clone()));

    let (sample_tx, mut sample_rx) = mpsc::unbounded_channel::<Vec<i16>>();

    let (wave_tap_tx, _) = tokio::sync::broadcast::channel::<Vec<i16>>(4);
    let wave_tap_for_gui = wave_tap_tx.clone();

    let playback_pos = Arc::new(std::sync::atomic::AtomicU64::new(0));
    let playback_pos_for_gui = playback_pos.clone();
    let playback_pos_for_cpal = playback_pos.clone();

    let samples_produced = Arc::new(std::sync::atomic::AtomicU64::new(0));
    let samples_produced_for_gui = samples_produced.clone();
    let samples_produced_for_writer = samples_produced.clone();

    // Suppress unused warnings until GUI task wires these in.
    let _ = wave_tap_for_gui;
    let _ = playback_pos_for_gui;
    let _ = samples_produced_for_gui;

    // Ring buffer sized for mono (ch0) only — 32 packets of jitter headroom.
    let ring_capacity = 32 * cfg.sample_per_packet;
    let rb = HeapRb::<i16>::new(ring_capacity);
    let (mut producer, consumer) = rb.split();

    let recv_handle = {
        let stats = stats.clone();
        let host = cfg.host.clone();
        let spp = cfg.sample_per_packet;
        let lp = cfg.listen_port;
        let wt = wave_tap_tx.clone();
        tokio::spawn(async move {
            if let Err(e) = recv_loop(host, cfg.port, cfg.pkt_len, spp, lp, stats, sample_tx, wt).await {
                eprintln!("recv_loop error: {}", e);
            }
        })
    };

    let writer_handle = tokio::spawn(async move {
        while let Some(samples) = sample_rx.recv().await {
            let n = samples.len();
            producer.push_slice(&samples);
            samples_produced_for_writer.fetch_add(n as u64, std::sync::atomic::Ordering::Relaxed);
        }
    });

    // Always play mono (ch0 only).
    let _stream = match start_playback(cfg.sample_rate, consumer, playback_pos_for_cpal) {
        Ok(s) => s,
        Err(e) => { eprintln!("Failed to start playback: {}", e); std::process::exit(1); }
    };

    tokio::signal::ctrl_c().await.ok();
    stats_handle.abort();
    recv_handle.abort();
    writer_handle.abort();
    println!("Shutdown");
}
