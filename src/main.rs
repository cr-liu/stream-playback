use clap::Parser;
use serde::Deserialize;
use std::fs;
use std::path::PathBuf;
use std::sync::atomic::{AtomicI32, AtomicU64, Ordering};

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

fn main() {
    let cli = Cli::parse();
    let cfg = match Config::resolve(cli) {
        Ok(c) => c,
        Err(e) => { eprintln!("Error: {}", e); std::process::exit(1); }
    };
    println!("Config: {:?}", cfg);
}
