# CODEBUDDY.md

This file provides guidance to CodeBuddy Code when working with code in this repository.

## What this project is

`netmon` is a tiny **Linux-only** terminal network monitor written in Rust
(ratatui 0.29 + crossterm 0.28, `libc 0.2`). It shows live download/upload speed
for one network interface plus system-wide CPU, memory, and disk read/write
throughput, each with a hollow braille history waveform. The bottom panel lists
the busiest connections (per-socket detail view) or a per-process aggregate view.

It depends on `/proc` and `ss` (iproute2). There is no config file, no logging,
no export.

## Commands

Build (normal, glibc-linked):
```sh
cargo build --release        # binary at target/release/netmon
```

Run:
```sh
cargo run --release                      # auto-detect default interface
cargo run --release -- eth0              # explicit interface (note the --)
```

**Portable static build (preferred for distributing to other machines):**
```sh
rustup target add x86_64-unknown-linux-musl
cargo build --release --target x86_64-unknown-linux-musl
# -> target/x86_64-unknown-linux-musl/release/netmon, statically linked
```
The glibc build will not run on machines with an older glibc. Always verify a
release with `ldd target/.../netmon` — it should print `statically linked`.

Tests (the project has ~19 tests, including headless render-dump tests):
```sh
cargo test                       # all tests
cargo test -- --nocapture        # show println! output (used by render-dump tests)
cargo test <name_substring>      # run a single test by substring
cargo test --target x86_64-unknown-linux-musl   # tests on the musl target too
```

Lint/format: there is no clippy/fmt gate enforced; run `cargo clippy` / `cargo fmt`
manually if desired. `cargo build` is the de-facto check.

## Code architecture

Two source files, both modules of the `netmon` crate:

- **`src/main.rs`** (~2027 lines) — the TUI. Owns the `App` state struct, reads
  system counters, runs the event loop, and renders everything via ratatui.
- **`src/conns.rs`** (~958 lines) — the connection/process monitor. Runs `ss`,
  parses it, computes per-connection and per-process throughput, resolves host
  names and service names, and resolves user names.

### Data flow & tick loop

- `main()` (main.rs:1549) detects the interface, builds `App`, seeds the first
  counter reading, sets up the crossterm alternate-screen terminal, then calls
  `run_app()`.
- `run_app()` (main.rs:1611) is the event loop. `TICK_MS = 500` (2 Hz).
  Each iteration: `terminal.draw(|f| ui(f, app))`, then handle input or
  `app.tick()` on timeout. `ui()` (main.rs:887) is the top-level layout split
  that calls `render_top()` (the metric box + waveforms) and
  `render_conn_panel()`.
- `App` (main.rs:53) holds **all** persistent state: eight ring-buffer
  `VecDeque` histories (`MAX_HISTORY = 172800` = 24 h @ 2 Hz), previous raw
  counters for deltas, auto-iface tracking, filter/sort state, and a
  `ConnMonitor` (`app.conns`).
- System metrics are read directly in `main.rs` from `/proc`:
  `read_system_cpu()` (`/proc/stat`), `read_system_mem_pct()` and
  `read_system_swap_pct()` (`/proc/meminfo`), `read_system_disk_space()`
  (`statvfs("/")`, pct used + free bytes),
  `read_sys_disk_sectors()` (`/proc/diskstats`, summed over whole disks in
  `/sys/block`, partitions excluded). Interface speeds come from
  `/proc/net/dev` (`rx_bytes`/`tx_bytes` deltas). Default interface is detected
  from `/proc/net/route`.

### Waveform rendering

`draw_waveform` (u64) and `draw_waveform_f` (f64) (main.rs:617, :759) render the
braille curves by writing directly to `f.buffer_mut()`. Key properties to
preserve when editing:
- The X-axis scale is **dynamic**: until a full day of history exists, samples
  are stretched across the full width; then it scrolls. The time axis (baseline
  + `HH:MM` labels + `now HH:MM:SS`) is drawn at the **bottom** row
  (`vrows[12].y` after the 6-metric layout) — see the earlier fix where it was
  accidentally drawn on a metric row.
- Disk/DL/UL scales = window max + 10 % headroom; CPU/MEM/SWAP/DISK SPACE are
  fixed 0–100 %. MEM (magenta), SWAP (white) and DISK SPACE (gray) each get
  their own metric row. DISK SPACE is the **largest mounted `/dev` filesystem's**
  usage % (`read_system_disk_space()` reads `/proc/mounts` directly — the same
  source `df -h` uses — so unmounted block devices like device-mapper `dm-*`
  that `df` never lists are never picked; it dedupes filesystems by device id
  (`st_dev`) to avoid double-counting bind/overlay mounts, then selects the
  mounted `/dev` filesystem with the biggest `statvfs` total capacity — including
  a whole-disk mount like `/dev/sdd`, not just partitions — and computes used/avail
  with `df`-style math: `used = total − f_bfree`, `avail = f_bavail`, so numbers
  line up with `df`).
  Its label shows total / used / free capacity with dynamic T/G/M units (like
  `df -h`) and the actual device path (e.g. `/dev/sdd`); the waveform shows
  only the usage %.
- All timestamps are rendered in **Asia/Shanghai (UTC+8)** via `shanghai_hms()`,
  regardless of host timezone.

### Connection panel

`ConnMonitor` (conns.rs:80) holds the connection state:
- `last: Vec<ConnStat>` (one row per socket), plus per-pid `procs: HashMap<u32,
  ProcInfo>` (USER/CPU%/MEM%/TIME+/DISK R/W, rebuilt each `sample()`).
- `sample()` runs `ss -tunpi` at ~2 Hz; only `ESTAB` TCP sockets are kept (UDP
  is shown but always `n/a` rate because the kernel tracks no cumulative UDP
  bytes). Reverse-DNS and `/etc/services` lookups are async/cached so the UI
  never blocks.
- Two views, selected by `app.aggregate`:
  - **detail** (`detail_view()`): `PROC(PID)/PRO/SRC/DST/HOST/SVC/RX/TX`.
  - **aggregate** (`aggregate_view()`): one row per process (`AggRow`) with
    summed RX/TX, conn count, and the per-process metrics.
- **Sorting** uses a `SortKey` enum + `sort_val()` which returns a `SortVal`
  that can compare f64/Num/Str values (conns.rs/main.rs around 1197–1329,
  `ConnRow` enum wraps both `ConnStat` and `AggRow`). Cursor `o`/`O` and
  clickable headers drive `sort_key`/`sort_asc`.
- **Filtering** is in `conn_matches()` (conns.rs) — it matches comm, pid,
  local/remote addresses, host, service, **and the owner username**
  (looked up from `procs` by pid). An empty filter matches all.

### User name resolution (important gotcha)

`getpwuid_r` in a **static musl** binary only reads `/etc/passwd` and cannot use
NSS. So `resolve_user()` (conns.rs) first tries `getpwuid_r`, and if it returns
only the numeric uid it falls back to shelling out to `getent passwd <uid>`
(NSS-aware). Results are cached in `uid_cache` per uid. If `getent` also fails,
the raw numeric uid is shown (correct when the uid genuinely has no name).

## Things to know before editing the UI

- **ratatui `Length` constraints shrink unevenly when the available space is
  less than their sum** (first and last keep full height; middle ones get
  crushed). The top metric box must be allocated enough rows: `ui()` caps that
  section at `Constraint::Length(31)` and `render_top()` computes equal metric
  heights dynamically, so do not reintroduce fixed per-metric `Length(3)`
  arrays that can be starved.
- **Column alignment:** numeric cells are **left-aligned** under their left-aligned
  headers (the earlier "not aligned" bug was right-aligned values). `format_speed`
  (main.rs:582) returns left-aligned `number unit` with no internal padding.
  Keep rate columns at width 11–12 so `1023.9 MB/s` never truncates.
- **Self-verify UI changes headlessly:** the tests use `TestBackend::new(W, H)`
  + `terminal.draw(|f| ui(f, &mut app))` and dump `f.buffer()` rows with
  `buf[(x,y)].symbol()`. This is the established way to confirm layout/alignment
  without a real terminal — prefer adding a temporary dump test over assuming a
  fix is correct. The `sample_app()` helper builds a few `ConnStat` rows.

## Tests module notes

- The `#[cfg(test)] mod tests` lives in `main.rs:1771`. It references both crates.
- `App` fields and `ConnMonitor.procs` / `ProcInfo` are **private** — tests in
  `main.rs` cannot construct `ProcInfo` or set `procs` directly; build scenarios
  via `sample_app()` and public methods like `aggregate_view`/`detail_view`.
- `conns.rs` also has its own `#[cfg(test)] mod tests` (conns.rs:805) with
  parser unit tests (`parse_users`, `getent_user`, etc.).

## Project layout

```
src/main.rs    TUI: App state, /proc counter reading, 2 Hz loop, layout,
               waveform rendering, connection-panel rendering, tests
src/conns.rs   ConnMonitor: ss parsing, per-conn/per-proc rates, DNS/service
               resolution, user-name resolution (getpwuid_r + getent), tests
Cargo.toml     ratatui 0.29 (crossterm feature) + crossterm 0.28 + libc 0.2
LICENSE        MIT
```

## Limitations worth remembering

- Linux only; interface list is read once at start (hot-plugged interfaces need
  a restart).
- One interface at a time; no multi-interface aggregation.
- UDP rows have no throughput rate (`n/a`).
- If `ss` is missing, the panel shows `ss unavailable` and the rest keeps running.
