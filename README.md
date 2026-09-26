# netmon

A tiny terminal network monitor written in Rust. It shows live download and upload
speed for one network interface, with gauges and sparkline history charts.

```
┌ ◉ Network Monitor ── interface: wlp2s0 (1/4) ── n/p:iface r:reset q:quit ┐
│ ┌ ▼ DOWNLOAD ─────────┐ ┌ ▲ UPLOAD ──────────┐ │
│ │    1.2 MB/s         │ │    128.0 KB/s     │ │
│ └─────────────────────┘ └───────────────────┘ │
│ ┌ ▼ Download History (120s) ──────────────────┐ │
│ │      ▁▂▃▅▇▅▃▂▁▂▃▅▇█▇▅▃▂                     │ │
│ └─────────────────────────────────────────────┘ │
│ ┌ ▲ Upload History (120s) ────────────────────┐ │
│ │      ▁▁▂▂▃▃▂▂▁▁▂▂▃▃▂▂                       │ │
│ └─────────────────────────────────────────────┘ │
│ Session peak: 5.4 MB/s  │  Samples: 300  │  ↑/↓:scroll conns           │
├ Top Connections (by throughput) ────────────────────────────────────────────┤
│ PROC(PID)      PRO  SRC             DST             HOST       SVC    RX      TX   │
│ node(26951)    tcp  127.0.0.1:51641 127.0.0.1:10808 localhost  -      6.1 KB/s 2.0 KB/s │
│ sshd(882)      tcp  10.0.0.2:22     10.0.0.9:55123  9.9.9.9    ssh    12.0 KB/s 4.0 KB/s │
│ resolver(1031) udp  127.0.0.53:53   8.8.8.8:53      dns.google domain n/a     n/a      │
└────────────────────────────────────────────────────────────────────────────────┘
```

## How it works

The program reads the kernel counters for the selected interface from
`/proc/net/dev` once per second. It computes the byte delta between two samples
and divides by the measured elapsed time to get bytes per second. Download speed
comes from the `rx_bytes` field and upload speed from the `tx_bytes` field.

The default interface is detected from `/proc/net/route`: the program picks the
interface that owns the default route. If no default route exists, it falls back
to the first non-loopback interface listed in `/proc/net/dev`.

The program also builds a list of every non-loopback interface in
`/proc/net/dev`. Press `n` or `p` to move through that list while running. If you
name an interface on the command line and it is not in the list, the program
puts it at the front of the list.

All samples are stored in two ring buffers of 300 entries, which covers 5 minutes
at a 1-second tick. The sparklines draw the newest 120 points, so the waveform
window is about 2 minutes.

This tool is Linux-only because it depends on `/proc/net/dev` and
`/proc/net/route`.

## Requirements

- Linux
- Rust toolchain (edition 2021)
- `ss` from iproute2 (used to enumerate per-connection / per-process throughput).

## Build

```sh
git clone <repository-url>
cd network_mornitor
cargo build --release
```

The binary is written to `target/release/netmon`.

### Portable (static) build

The default build links against the build host's glibc, so the binary will refuse
to run on a machine with an *older* glibc (e.g. `GLIBC_2.39 not found`). To produce
a single static binary that runs on any x86_64 Linux, build against musl instead:

```sh
rustup target add x86_64-unknown-linux-musl
cargo build --release --target x86_64-unknown-linux-musl
```

The portable binary is at `target/x86_64-unknown-linux-musl/release/netmon` and is
statically linked (no glibc dependency). Use this one when deploying to other
machines.

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
| `n` / `N` / `Tab` | Switch to the next interface (wraps around) |
| `p` / `P` / `BackTab` | Switch to the previous interface (wraps around) |
| `↑` / `↓` / `j` / `k` | Scroll the Top Connections list by one row |
| `PageUp` / `PageDown` | Scroll the Top Connections list by one screen |
| `Home` / `End` | Jump to the first / last row |
| `/` | Enter filter mode (type to filter by process name, pid, address, host or service; `Enter`/`Esc` to finish) |
| `c` | Toggle focus mode — the Top Connections panel fills the whole screen |
| `a` | Toggle aggregate-by-process view (one row per process, with total rate and connection count) |
| `q` / `Q` / `Esc` | Quit |
| `r` / `R` | Reset history buffers, session peak, connection monitor and filter |

Switching interfaces clears the history buffers, resets the session peak, and
re-reads the counters of the new interface. This makes the first sample on the
new interface a real measurement instead of a bogus delta between two different
counters. The title bar shows the position in the list, for example `iface 2/4`.

## Display

| Area | Meaning |
| --- | --- |
| Title bar | Program name, the interface in use, and its position in the list |
| DOWNLOAD gauge | Current download speed in bytes/s (binary units) |
| UPLOAD gauge | Current upload speed in bytes/s (binary units) |
| Download History | Download speed sparkline, newest 120 samples |
| Upload History | Upload speed sparkline, newest 120 samples |
| Footer | Session peak speed and number of stored samples |

Gauge fill is relative to the session peak speed, which is the largest download
or upload value seen since start or since the last reset. The sparkline scale is
the maximum value inside its own 120-point window plus 10 percent headroom, so a
sparkline rescales as traffic changes.

## Process & Connection Throughput

Below the gauges and history charts, the bottom panel lists the connections that
are using the most bandwidth, sorted by total (RX + TX) throughput:

```
├ Top Connections (by throughput)  ↑/↓:scroll ──────────────────────────────────┤
│ PROC(PID)      PRO  SRC             DST             HOST       SVC    RX      TX │
│ node(26951)    tcp  127.0.0.1:51641 127.0.0.1:10808 localhost  -      6.1 KB/s 2.0 KB/s │
│ sshd(882)      tcp  10.0.0.2:22     10.0.0.9:55123  9.9.9.9    ssh    12.0 KB/s 4.0 KB/s │
│ resolver(1031) udp  127.0.0.53:53   8.8.8.8:53      dns.google domain n/a     n/a      │
└────────────────────────────────────────────────────────────────────────────────┘
```

Each row shows the owning process name and pid, the protocol, the local (source)
and remote (destination) `IP:port`, and the current RX/TX rates. Two extra columns
add human-readable context:

- **HOST** — the reverse-DNS name of the remote IP (resolved in a background
  thread and cached, so it never blocks the UI). It shows `-` until the lookup
  finishes, or permanently `-` when the IP has no PTR record.
- **SVC** — the service name for the remote port, looked up from `/etc/services`
  (e.g. `443 → https`, `53 → domain`). Shows `-` when unknown.

The panel title shows the visible range and total (e.g. `12-21/45`) and a few
hints. When there are more connections than fit, use `↑`/`↓` (or `j`/`k`) to move
one row, `PageUp`/`PageDown` to move a screen, and `Home`/`End` to jump to the
start/end. Rows are zebra-striped to stay readable.

Two more ways to cope with a long list:

- **Aggregate by process** — press `a` to collapse every socket of a process into a
  single row showing the summed RX/TX rate and the connection count. This is the
  fastest way to see *who* is using bandwidth when a process holds many sockets.
- **Focus mode** — press `c` to let the Top Connections panel take over the whole
  screen; press `c` again to return to the normal layout.
- **Filter** — press `/` and type a substring; it matches process name, pid,
  address, host or service (case-insensitive). `Enter` or `Esc` accepts/exits.

Use `↑` / `↓` (or `j` / `k`) to scroll when the list is longer than the panel.

The data comes from `ss -tunpi`, sampled once per second. For TCP sockets the
kernel exposes cumulative `bytes_received` / `bytes_sent` counters, so the rate is
the per-second delta between two samples. UDP is connectionless and the kernel
does **not** track cumulative per-socket bytes, so UDP rows are shown for context
but their RX/TX rates are always `n/a`. Only `ESTAB` TCP sockets are listed;
`LISTEN` / `TIME-WAIT` and other states are skipped.

If `ss` is not installed, the panel shows `ss unavailable` and the rest of the
program keeps running.

## Project layout

```
src/main.rs      TUI, counter reading, sampling, and layout
src/conns.rs     per-connection/process sampling: runs `ss`, parses its output,
                 and computes per-connection RX/TX rates
Cargo.toml       package manifest (ratatui 0.29, crossterm 0.28)
```

## Limitations

- Linux only (`/proc` filesystem required).
- One interface at a time; it does not aggregate all interfaces. The interface
  list is read once at start, so a hot-plugged interface does not appear until
  you restart the program.
- Counters are read once per second, so short bursts inside a tick are averaged
  into that interval.
- Sparkline scaling is per-window, so the vertical scale changes when traffic
  changes. Compare shapes, not absolute heights.
- No logging, no export, no configuration file.
- The Top Connections panel depends on `ss` (iproute2); it shows
  `ss unavailable` if `ss` is not installed, and the rest of the program keeps
  running.
- UDP connections have no per-socket throughput rate (the kernel does not track
  cumulative UDP bytes), so their RX/TX columns always read `n/a`.

## Dependencies

- [ratatui](https://crates.io/crates/ratatui) 0.29 with the `crossterm` feature
- [crossterm](https://crates.io/crates/crossterm) 0.28

## License

No license file is present in this repository. Add one before distributing the
project.
