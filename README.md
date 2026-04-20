# stream-playback

Receive UDP audio from [mic2sock](https://github.com/cr-liu/mic2sock.rs) and play the first channel through the system default output device.

Cross-platform: macOS, Windows, Linux.

## Build

```bash
cargo build --release
```

On Linux, install ALSA headers first:
```bash
sudo apt-get install libasound2-dev pkg-config
```

macOS and Windows have no extra dependencies.

## Run

### Dynamic registration (default)

The client sends a `register` datagram to the server; server records the address and starts streaming.

```bash
./target/release/stream-playback --host 192.168.1.10 --n-channel 1
```

`--n-channel` is the **sender's total channel count** (used to compute expected packet size). Only ch0 is played.

### Fixed port (no register)

Use this when the server has your address in its `static_receivers` list:

```bash
./target/release/stream-playback --host 192.168.1.10 --listen-port 7999 --n-channel 1
```

### All options

```
--host <IP>                  Server host [required]
--port <N>                   Server port [default: 7998]
--sample-rate <HZ>           [default: 16000]
--n-channel <N>              Sender's total channels [default: 1]
--sample-per-packet <N>      [default: 32]
--pkt-len <BYTES>            Override packet size (else computed)
--listen-port <PORT>         Bind fixed UDP port; suppresses register
--config <PATH>              TOML config file
```

### Config file

```toml
[receiver]
host = "192.168.1.10"
port = 7998
n_channel = 1
listen_port = 7999   # optional

[gui]
enabled = true
bind_addr = "0.0.0.0"
port = 8081
password = "test"
```

CLI args override config values.

## Output

Stats print every second:

```
[12:34:57] recv/s=500   lost/s=0     total_recv=500   total_lost=0   pkt_id=499
```

Ctrl+C to stop.

## Web GUI

On startup the binary serves a web GUI at `http://<bind_addr>:<port>` (default `http://0.0.0.0:8081`). Log in with the password from `[gui] password` (default: `test`).

Features:
- Edit `config.toml` in the browser (all fields require restart)
- Live waveform of the incoming audio stream (channel 0)
- Red vertical **playhead line** showing current ring-buffer water level — the distance from the right edge is proportional to playback latency
- Stats panel (packets received, lost, current packet_id, ring occupancy)
- Restart button

Set `[gui] enabled = false` to disable, or `bind_addr = "127.0.0.1"` to restrict to localhost.
