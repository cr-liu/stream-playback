use clap::Parser;
use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use cpal::{BufferSize, SampleRate, StreamConfig};
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
}

#[derive(Deserialize, Debug, Default)]
struct FileConfig {
    host: Option<String>,
    port: Option<u16>,
    sample_rate: Option<u32>,
    n_channel: Option<u16>,
    sample_per_packet: Option<usize>,
    pkt_len: Option<usize>,
}

#[derive(Debug)]
struct Config {
    host: String,
    port: u16,
    sample_rate: u32,
    n_channel: u16,
    sample_per_packet: usize,
    pkt_len: usize,
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
        let host = cli.host.or(file.host)
            .ok_or_else(|| "--host is required (or set in config)".to_string())?;
        let port = cli.port.or(file.port).unwrap_or(7998);
        let sample_rate = cli.sample_rate.or(file.sample_rate).unwrap_or(16000);
        let n_channel = cli.n_channel.or(file.n_channel).unwrap_or(1);
        let sample_per_packet = cli.sample_per_packet.or(file.sample_per_packet).unwrap_or(32);
        let pkt_len = cli.pkt_len.or(file.pkt_len)
            .unwrap_or(HEADER_LEN + n_channel as usize * sample_per_packet * 2);
        Ok(Config { host, port, sample_rate, n_channel, sample_per_packet, pkt_len })
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
    stats: Arc<Stats>,
    sample_tx: mpsc::UnboundedSender<Vec<i16>>,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let socket = UdpSocket::bind("0.0.0.0:0").await?;
    let server_addr = format!("{}:{}", host, port);
    socket.connect(&server_addr).await?;
    println!("Connected to {}", server_addr);
    socket.send(b"register").await?;

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

                        let payload = &buf[HEADER_LEN..n];
                        let samples: Vec<i16> = payload
                            .chunks_exact(2)
                            .map(|c| i16::from_le_bytes([c[0], c[1]]))
                            .collect();
                        let _ = sample_tx.send(samples);
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
            _ = reg_interval.tick() => { let _ = socket.send(b"register").await; }
        }
    }
}

fn start_playback(
    sample_rate: u32,
    n_channel: u16,
    mut consumer: HeapCons<i16>,
) -> Result<cpal::Stream, Box<dyn std::error::Error>> {
    let host = cpal::default_host();
    let device = host.default_output_device().ok_or("no default output device")?;
    println!("Playback device: {}", device.name().unwrap_or_default());

    let config = StreamConfig {
        channels: n_channel,
        sample_rate: SampleRate(sample_rate),
        buffer_size: BufferSize::Default,
    };

    let stream = device.build_output_stream(
        &config,
        move |data: &mut [i16], _| {
            let read = consumer.pop_slice(data);
            for s in &mut data[read..] { *s = 0; }
        },
        move |err| eprintln!("Playback error: {}", err),
        None,
    )?;
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

    let ring_capacity = 32 * cfg.sample_per_packet * cfg.n_channel as usize;
    let rb = HeapRb::<i16>::new(ring_capacity);
    let (mut producer, consumer) = rb.split();

    let recv_handle = {
        let stats = stats.clone();
        let host = cfg.host.clone();
        tokio::spawn(async move {
            if let Err(e) = recv_loop(host, cfg.port, cfg.pkt_len, stats, sample_tx).await {
                eprintln!("recv_loop error: {}", e);
            }
        })
    };

    let writer_handle = tokio::spawn(async move {
        while let Some(samples) = sample_rx.recv().await {
            producer.push_slice(&samples);
        }
    });

    let _stream = match start_playback(cfg.sample_rate, cfg.n_channel, consumer) {
        Ok(s) => s,
        Err(e) => { eprintln!("Failed to start playback: {}", e); std::process::exit(1); }
    };

    tokio::signal::ctrl_c().await.ok();
    stats_handle.abort();
    recv_handle.abort();
    writer_handle.abort();
    println!("Shutdown");
}
