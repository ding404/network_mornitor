# netmon

A tiny terminal network monitor written in Rust. It shows live download and upload
speed for one network interface, with gauges and sparkline history charts.

```
┌ ◉ Network Monitor ── interface: wlp2s0 ── q: quit ┐
│ ┌ ▼ DOWNLOAD ─────────┐ ┌ ▲ UPLOAD ──────────┐ │
│ │    1.2 MB/s         │ │    128.0 KB/s     │ │
│ └─────────────────────┘ └───────────────────┘ │
│ ┌ ▼ Download History (120s) ──────────────────┐ │
│ │      ▁▂▃▅▇▅▃▂▁▂▃▅▇█▇▅▃▂                     │ │
│ └─────────────────────────────────────────────┘ │
│ ┌ ▲ Upload History (120s) ────────────────────┐ │
│ │      ▁▁▂▂▃▃▂▂▁▁▂▂▃▃▂▂                       │ │
│ └─────────────────────────────────────────────┘ │
│ Session peak: 5.4 MB/s  │  Samples: 300        │
└─────────────────────────────────────────────────┘
```

## How it works

The program reads the kernel counters for the selected interface from
`/proc/net/dev` once per second. It computes the byte delta between two samples
and divides by the measured elapsed time to get bytes per second. Download speed
comes from the `rx_bytes` field and upload speed from the `tx_bytes` field.

The default interface is detected from `/proc/net/route`: the program picks the
interface that owns the default route. If no default route exists, it falls back
to the first non-loopback interface listed in `/proc/net/dev`.

All samples are stored in two ring buffers of 300 entries, which covers 5 minutes
at a 1-second tick. The sparklines draw the newest 120 points, so the waveform
window is about 2 minutes.

This tool is Linux-only because it depends on `/proc/net/dev` and
`/proc/net/route`.

## Requirements

- Linux
- Rust toolchain (edition 2021)

## Build

```sh
git clone <repository-url>
cd network_mornitor
cargo build --release
```

The binary is written to `target/release/netmon`.

## Run

Use the auto-detected default interface:

```sh
cargo run --release
```

Pass an interface name explicitly:

```sh
cargo run --release -- eth0
```

You can also run the built binary directly:

```sh
./target/release/netmon wlp2s0
```

## Keys

| Key | Action |
| --- | --- |
| `q` / `Q` / `Esc` | Quit |
| `r` / `R` | Reset history buffers and session peak |

## Display

| Area | Meaning |
| --- | --- |
| Title bar | Program name and the interface in use |
| DOWNLOAD gauge | Current download speed in bytes/s (binary units) |
| UPLOAD gauge | Current upload speed in bytes/s (binary units) |
| Download History | Download speed sparkline, newest 120 samples |
| Upload History | Upload speed sparkline, newest 120 samples |
| Footer | Session peak speed and number of stored samples |

Gauge fill is relative to the session peak speed, which is the largest download
or upload value seen since start or since the last reset. The sparkline scale is
the maximum value inside its own 120-point window plus 10 percent headroom, so a
sparkline rescales as traffic changes.

## Project layout

```
src/main.rs      entire program: counter reading, sampling, and TUI rendering
Cargo.toml       package manifest (ratatui 0.29, crossterm 0.28)
```

## Limitations

- Linux only (`/proc` filesystem required).
- One interface at a time; it does not aggregate all interfaces.
- Counters are read once per second, so short bursts inside a tick are averaged
  into that interval.
- Sparkline scaling is per-window, so the vertical scale changes when traffic
  changes. Compare shapes, not absolute heights.
- No logging, no export, no configuration file.

## Dependencies

- [ratatui](https://crates.io/crates/ratatui) 0.29 with the `crossterm` feature
- [crossterm](https://crates.io/crates/crossterm) 0.28

## License

No license file is present in this repository. Add one before distributing the
project.
