# netmon

A tiny terminal network monitor written in Rust. It shows live download and upload
speed for one network interface, with gauges and sparkline history charts.

```
┌ ◉ Network Monitor  │  interface: wlp2s0  │  iface 1/4  │  n/p: iface  │  i: auto-iface  │  r: reset  │  q: quit ┐
┌ ▼ DL / ▲ UL / CPU / MEM History (24h) ───────────────────────────────────────────┐
│ ▼ DL 1.2 MB/s  │⠀⠀⣀⡤⠖⠒⠒⠦⣄⡀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⢀⣠⠴⠒⠒⠲⢤⣀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⢀⡤⠴⠒⠒⠦⢤⡀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⣀⡤⠖⠒⠒⠦⣄⡀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⢀⣠⠴⠒⠒⠲⢤⣀⠀⠀⠀⠀⠀⠀⠀│
│ ▲ UL 128 KB/s  │⣀⣀⣀⣀⣀⣀⣀⣀⣀⣀⣀⣀⣀⣀⣀⣀⡏⢹⣀⣀⣀⣀⣀⣀⣀⣀⣀⣀⣀⣀⣀⣀⣀⣀⣀⣰⠋⣇⣀⣀⣀⣀⣀⣀⣀⣀⣀⣀⣀⣀⣀⣀⣀⣀⣀⣀⣀⣀⡼⢹⣀⣀⣀⣀⣀⣀⣀⣀⣀⣀⣀⣀⣀⣀⣀⣀⣀⣀⡏⢧⣀⣀⣀⣀⣀⣀⣀⣀⣀⣀⣀⣀⣀⣀⣀⣀⣀⣀⣀⣀⣀⣀⣀⣀│
│ ▲ UL 128 KB/s  │⠀⢀⡤⠖⠒⠦⣄⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⣠⠴⠒⠲⢤⡀⠀⠀⠀⠀⠀⠀⠀⠀⠀⢀⡤⠖⠒⠦⣄⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⣠⠴⠒⠲⢤⡀⠀⠀⠀⠀⠀⠀⠀⠀⠀⢀⡤⠖⠒⠦⣄⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⣠⠴⠒⠲⢤⡀⠀⠀⠀⠀⠀⠀⠀⠀⠀⢀⡤⠖⠒⠦│
│ ▌ CPU 23.4%   │⠀⠀⠀⠀⠀⢀⡴⠒⠉⠉⠉⠉⠉⠉⠉⠉⠉⠉⠉⠉⠉⠉⠉⠉⠉⠉⠉⠉⠉⠉⠉⠉⠉⠉⠉⠉⠉⠉⠉⠉⠉⠉⠉⠉⠉⠉⠉⠉⠉⠉⠉⠉⠉⠉⠉⠉⠉⠉⠉⠉⠉⠉⠉⠉⠉⠉⠉⠉⠉⠉⠉⠉⠉⠉⠉⠉⠉⠉⠉⠉⠉⠉⠉⠉⠉⠉⠉⠉⠉⠉⠉⠉⠉⠉⠉⠉⠉⠉⠉⠉⠉⠉⠉⠉⠉⠉⠉│
│ ▌ MEM 41.8%   │⠀⠤⠤⠤⠤⠤⠤⠤⠤⠤⠤⠤⠤⠤⠤⠤⠤⠤⠤⠤⠤⠤⠤⠤⠤⠤⠤⠤⠤⠤⠤⠤⠤⠤⠤⠤⠤⠤⠤⠤⠤⠤⠤⠤⠤⠤⠤⠤⠤⠤⠤⠤⠤⠤⠤⠤⠤⠤⠤⠤⠤⠤⠤⠤⠤⠤⠤⠤⠤⠤⠤⠤⠤⠤⠤⠤⠤⠤⠤⠤⠤⠤⠤⠤⠤⠤⠤⠤⠤⠤⠤⠤⠤⠤⠤⠤⠤⠤⠤⠤⠤⠤⠤⠤⠤⠤⠤⠤⠤⠤⠤⠤⠤⠤⠤⠤⠤⠤│
│             ├─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ┤
│ 06:00          12:00          18:00          now 14:21:03                          │
│ Session peak: 5.4 MB/s  │  Samples: 600                                          │
├ Top Connections (by throughput) ────────────────────────────────────────────────────┤
│ PROC(PID)      PRO  SRC             DST             HOST       SVC    RX      TX   │
│ node(26951)    tcp  127.0.0.1:51641 127.0.0.1:10808 localhost  -      6.1 KB/s 2.0 KB/s │
│ sshd(882)      tcp  10.0.0.2:22     10.0.0.9:55123  9.9.9.9    ssh    12.0 KB/s 4.0 KB/s │
│ resolver(1031) udp  127.0.0.53:53   8.8.8.8:53      dns.google domain n/a     n/a      │
│   ...more rows now fit because the four stats regions are compact...              │
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

All samples are stored in two ring buffers of 172800 entries, which covers 1 day
(24 hours) at a 0.5-second (2 Hz) tick. The waveform draws the full buffer, but the
X-axis **scale is dynamic**: until a full day of history has accumulated, the
available samples are stretched across the whole chart width so the (initially
sparse) curve is always clearly visible instead of being crushed into a sliver at
the left edge of a 24h window. As more history arrives the view gradually "zooms
out" until it spans the full day, after which it scrolls as new samples come in.
Because the line is one dot thick, it stays a thin *hollow* braille curve rather
than a solid block even when the buffer is full.

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
| `i` / `I` | Toggle automatic switching to the busiest interface |

Switching interfaces clears the history buffers, resets the session peak, and
re-reads the counters of the new interface. This makes the first sample on the
new interface a real measurement instead of a bogus delta between two different
counters. The title bar shows the position in the list, for example `iface 2/4`.

### Automatic interface selection

By default the program watches every interface and, once per second, adds up the
bytes transferred on each during that 1-second window (sampled twice at 2 Hz).
The interface with the most accumulated traffic is selected automatically; the
switch only happens when a *different* interface is ahead by more than ~4 KB/s, so
two interfaces with similar load will not thrash back and forth. Manual switching
with `n` / `p` turns auto mode off (shown as `auto: off` in the footer). Press `i`
to toggle it back on.

## Display

| Area | Meaning |
| --- | --- |
| Title bar | Program name, the interface in use, and its position in the list |
| DL / UL / CPU / MEM box | The current value of each metric on the left, with a hollow braille history waveform on the right spanning the last 24 hours (172800 samples @ 2 Hz) |
| Download waveform | Hollow braille line of download speed over the dynamic 24-hour window |
| Upload waveform | Hollow braille line of upload speed over the dynamic 24-hour window |
| CPU waveform | Hollow braille line of **system-wide** CPU utilisation (%) over the dynamic 24-hour window; the value shown is the latest reading (left-aligned under the `CPU` title) |
| MEM waveform | Hollow braille line of **system-wide** memory usage (%) — `(MemTotal − MemAvailable) / MemTotal` — over the dynamic 24-hour window |
| Footer | Session peak speed, number of stored samples, and auto-iface status (`auto: on/off`) |

The DL/UL waveform scale is the maximum value inside the current window plus 10
percent headroom, so it rescales as traffic changes. The CPU and MEM waveforms use
a fixed 0–100 % scale. The X-axis sits at the bottom of the chart: below the
baseline a `now HH:MM:SS` label (right edge) and adaptive `HH:MM` time labels are
drawn on the row beneath it. The number of `HH:MM` labels scales with the terminal
width so they never overlap, and all times are shown in **Asia/Shanghai (UTC+8)**
wall-clock time regardless of the host's timezone.

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
  fastest way to see *who* is using bandwidth when a process holds many sockets. The
  aggregate view also shows htop-style process metrics, read from `/proc/<pid>`:

  ```
  ├ Top Connections 1-3/3 [agg]  ↑↓ pg:scroll c:focus a:agg /:filter ──────────┤
  │ USER   PROC(PID)         CPU%    MEM%    TIME+        CONNS  RX          TX       │
  │ dj     node(26951)       75.0    40.0    02:11.48         4  6.1 KB/s    2.0 KB/s │
  │ root   sshd(882)          0.0     0.2    15:42.07         1  12.0 KB/s   4.0 KB/s │
  │ systemd-resolve(1031)     0.1     0.5    00:03.90         2  n/a         n/a      │
  └─────────────────────────────────────────────────────────────────────────────────────┘
  ```

  - **USER** — the process owner, resolved from `/proc/<pid>/status` `Uid` via the
    system password database (`getpwuid_r`). Shows `-` if it cannot be read.
  - **CPU%** — the process's CPU usage as a percentage of one core, measured over
    the sampling interval from `/proc/<pid>/stat` (`utime` + `stime` delta).
  - **MEM%** — the process's resident memory (`VmRSS`) as a percentage of total RAM
    (`/proc/meminfo` `MemTotal`).
  - **TIME+** — cumulative CPU time (htop format: `MM:SS.cc` under an hour,
    `HH:MM:SS` above), from `/proc/<pid>/stat`.

  The **system-wide** CPU and memory history are drawn as waveforms in the top box
  (below the DL/UL waveforms), not inline here.

  These columns only appear in the aggregate (by-process) view; the per-connection
  detail view keeps the `PROC(PID) / PRO / SRC / DST / HOST / SVC / RX / TX` layout.
- **Focus mode** — press `c` to let the Top Connections panel take over the whole
  screen; press `c` again to return to the normal layout.
- **Filter** — press `/` and type a substring; it matches process name, pid,
  address, host or service (case-insensitive). `Enter` or `Esc` accepts/exits.

Use `↑` / `↓` (or `j` / `k`) to scroll when the list is longer than the panel.

The data comes from `ss -tunpi`, sampled twice per second (2 Hz). For TCP sockets the
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
- Counters are read twice per second (2 Hz), so short bursts inside a 0.5 s tick
  are averaged into that interval.
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
