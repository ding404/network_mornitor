mod conns;
use conns::{AggRow, ConnMonitor, ConnStat};

use std::collections::{HashMap, HashSet, VecDeque};
use std::fs;
use std::io;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use crossterm::{
    event::{
        self, DisableMouseCapture, EnableMouseCapture, Event, KeyCode, MouseButton,
        MouseEventKind,
    },
    execute,
    terminal::{disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen},
};
use ratatui::{
    backend::{Backend, CrosstermBackend},
    layout::{Constraint, Direction, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Cell, Paragraph, Row, Table},
    Frame, Terminal,
};

/// Asia/Shanghai is UTC+8 with no daylight-saving time, so its offset from UTC
/// is a constant 8 hours. Convert a UNIX timestamp (UTC seconds) into Shanghai
/// wall-clock `(hour, minute, second)`. Using a fixed offset keeps the display
/// independent of whatever timezone the host happens to be configured for.
fn shanghai_hms(utc_secs: u64) -> (u32, u32, u32) {
    const SHANGHAI_OFFSET: u64 = 8 * 3600;
    let local = utc_secs + SHANGHAI_OFFSET;
    let h = (local / 3600) % 24;
    let m = (local / 60) % 60;
    let s = local % 60;
    (h as u32, m as u32, s as u32)
}

// ── Constants ────────────────────────────────────────────────────────────────

/// Maximum number of data points stored in history (1 day at 0.5s interval).
/// 86400 s/day × 2 samples/s = 172800. The full buffer is also the X-axis
/// window shown on the waveform.
const MAX_HISTORY: usize = 172800;
/// Refresh interval in milliseconds. 500 ms → 2 samples/sec, so a 1-second
/// judging window contains 2 samples of accumulated traffic.
const TICK_MS: u64 = 500;
/// Background tint for alternating rows in the connections table (zebra striping).
const ZEBRA: Color = Color::Rgb(34, 34, 44);

// ── Application State ───────────────────────────────────────────────────────

struct App {
    /// Network interface name (e.g. "eth0", "wlp2s0").
    iface: String,
    /// All switchable interfaces found in /proc/net/dev (excludes "lo").
    ifaces: Vec<String>,
    /// Index of the active interface inside `ifaces`.
    iface_idx: usize,
    /// Download speed history (bytes/s), newest at the back.
    rx_history: VecDeque<u64>,
    /// Upload speed history (bytes/s), newest at the back.
    tx_history: VecDeque<u64>,
    /// System-wide CPU utilisation history (0..100 %), newest at the back.
    sys_cpu_history: VecDeque<f64>,
    /// System-wide memory usage history (0..100 % of total RAM), newest at the back.
    sys_mem_history: VecDeque<f64>,
    /// System-wide disk read throughput history (bytes/s), newest at the back.
    sys_disk_read_history: VecDeque<f64>,
    /// System-wide disk write throughput history (bytes/s), newest at the back.
    sys_disk_write_history: VecDeque<f64>,
    /// Previous cumulative disk read sectors (from /proc/diskstats) for the delta.
    prev_disk_read_sectors: u64,
    /// Previous cumulative disk write sectors for the delta.
    prev_disk_write_sectors: u64,
    /// Time of the previous disk-stat sample.
    prev_disk_time: Instant,
    /// Previous aggregate busy-ticks from /proc/stat (for the CPU% delta).
    prev_cpu_busy: u64,
    /// Previous aggregate total-ticks from /proc/stat (for the CPU% delta).
    prev_cpu_total: u64,
    /// Time of the previous /proc/stat sample.
    prev_cpu_time: Instant,
    /// Current download speed (bytes/s).
    current_rx: u64,
    /// Current upload speed (bytes/s).
    current_tx: u64,
    /// Maximum speed seen in the current session (bytes/s), for gauge scaling.
    peak_speed: u64,
    /// Previous raw RX bytes counter from /proc/net/dev.
    prev_rx_bytes: u64,
    /// Previous raw TX bytes counter from /proc/net/dev.
    prev_tx_bytes: u64,
    /// Timestamp of the last sample.
    last_sample: Instant,
    /// Previous raw (rx_bytes, tx_bytes) per interface, for computing each
    /// interface's own throughput without switching the active one.
    prev_counters: HashMap<String, (u64, u64)>,
    /// Accumulated (rx+tx) bytes per interface over the current 1-second judging
    /// window, used to pick the busiest interface without reacting to spikes.
    auto_window_bytes: HashMap<String, u64>,
    /// Start of the current 1-second judging window.
    auto_window_start: Instant,
    /// Whether to automatically switch to the busiest interface.
    auto_iface: bool,
    /// Earliest time the next automatic interface switch may happen (debounce).
    last_auto_switch: Instant,
    /// Last time per-connection throughput was sampled (kept at ~2 Hz).
    last_conns_sample: Instant,
    /// Per-connection / per-process throughput monitor.
    conns: ConnMonitor,
    /// Focus mode: the connections panel fills the whole screen.
    focus_conns: bool,
    /// Aggregate connections by process instead of listing each socket.
    aggregate: bool,
    /// Whether the user is currently typing a filter.
    filter_mode: bool,
    /// Active filter string (case-insensitive substring).
    filter: String,
    /// Column the Top Connections list is sorted by.
    sort_key: SortKey,
    /// Sort direction: `true` = ascending, `false` = descending.
    sort_asc: bool,
    /// Terminal row of the connections-panel header, for click-to-sort hit testing.
    header_y: u16,
    /// Per-column header hitboxes `(x_start, x_end_exclusive, sort_key)` for mouse clicks.
    col_hit: Vec<(u16, u16, SortKey)>,
}

impl App {
    fn new(iface: String, ifaces: Vec<String>, iface_idx: usize) -> Self {
        Self {
            iface,
            ifaces,
            iface_idx,
            rx_history: VecDeque::with_capacity(MAX_HISTORY),
            tx_history: VecDeque::with_capacity(MAX_HISTORY),
            sys_cpu_history: VecDeque::with_capacity(MAX_HISTORY),
            sys_mem_history: VecDeque::with_capacity(MAX_HISTORY),
            sys_disk_read_history: VecDeque::with_capacity(MAX_HISTORY),
            sys_disk_write_history: VecDeque::with_capacity(MAX_HISTORY),
            prev_disk_read_sectors: 0,
            prev_disk_write_sectors: 0,
            prev_disk_time: Instant::now(),
            prev_cpu_busy: 0,
            prev_cpu_total: 0,
            prev_cpu_time: Instant::now(),
            current_rx: 0,
            current_tx: 0,
            peak_speed: 1, // avoid divide-by-zero
            prev_rx_bytes: 0,
            prev_tx_bytes: 0,
            last_sample: Instant::now(),
            prev_counters: HashMap::new(),
            auto_window_bytes: HashMap::new(),
            auto_window_start: Instant::now(),
            auto_iface: true,
            last_auto_switch: Instant::now(),
            last_conns_sample: Instant::now(),
            conns: ConnMonitor::new(),
            focus_conns: false,
            aggregate: false,
            filter_mode: false,
            filter: String::new(),
            sort_key: SortKey::Throughput,
            sort_asc: false,
            header_y: 0,
            col_hit: Vec::new(),
        }
    }

    /// Read raw (rx_bytes, tx_bytes) from /proc/net/dev for the configured interface.
    fn read_counters(&self) -> io::Result<(u64, u64)> {
        let content = fs::read_to_string("/proc/net/dev")?;
        for line in content.lines().skip(2) {
            let line = line.trim();
            if let Some(rest) = line.strip_prefix(&format!("{}:", self.iface)) {
                let fields: Vec<&str> = rest.split_whitespace().collect();
                if fields.len() >= 9 {
                    let rx: u64 = fields[0]
                        .parse()
                        .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
                    let tx: u64 = fields[8]
                        .parse()
                        .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
                    return Ok((rx, tx));
                }
            }
        }
        Err(io::Error::new(
            io::ErrorKind::NotFound,
            format!("interface '{}' not found in /proc/net/dev", self.iface),
        ))
    }

    /// Read raw (rx_bytes, tx_bytes) for *every* interface from /proc/net/dev.
    fn read_all_counters(&self) -> io::Result<HashMap<String, (u64, u64)>> {
        let content = fs::read_to_string("/proc/net/dev")?;
        let mut map = HashMap::new();
        for line in content.lines().skip(2) {
            let line = line.trim();
            if let Some(colon) = line.find(':') {
                let iface = line[..colon].trim().to_string();
                let rest = &line[colon + 1..];
                let fields: Vec<&str> = rest.split_whitespace().collect();
                if fields.len() >= 9 {
                    let rx: u64 = fields[0].parse().unwrap_or(0);
                    let tx: u64 = fields[8].parse().unwrap_or(0);
                    map.insert(iface, (rx, tx));
                }
            }
        }
        Ok(map)
    }

    /// Return the interface (from the known list) with the most traffic accumulated
    /// in the current 1-second window, or None if nothing is accumulated yet. Ties
    /// resolve to the first match, which keeps the current interface when it is
    /// among the joint-busiest.
    fn busiest_iface(&self) -> Option<String> {
        let mut best: Option<&String> = None;
        let mut best_total = 0u64;
        for (iface, &bytes) in &self.auto_window_bytes {
            if bytes > best_total {
                best_total = bytes;
                best = Some(iface);
            }
        }
        best.cloned()
    }

    /// Switch to the busiest interface (by accumulated 1s traffic) if auto mode is
    /// on, the 1s debounce has elapsed, and a different interface is clearly ahead.
    /// The +4096 byte margin (4 KB/s) avoids thrashing between similar loads.
    fn auto_switch_if_needed(&mut self, now: Instant) {
        if !self.auto_iface
            || now.duration_since(self.last_auto_switch) <= Duration::from_secs(1)
        {
            return;
        }
        let best = match self.busiest_iface() {
            Some(b) => b,
            None => return,
        };
        let cur_bytes = self.auto_window_bytes.get(&self.iface).copied().unwrap_or(0);
        let best_bytes = self.auto_window_bytes.get(&best).copied().unwrap_or(0);
        if best != self.iface && best_bytes > cur_bytes + 4096 {
            if let Some(idx) = self.ifaces.iter().position(|n| n == &best) {
                self.switch_iface(idx);
                self.last_auto_switch = now;
            }
        }
    }

    /// Take a sample: read all counters, accumulate per-interface traffic into a
    /// 1-second window, and (optionally) auto-switch to the busiest interface once
    /// that window elapses.
    fn tick(&mut self) -> io::Result<()> {
        let now = Instant::now();
        let counters = self.read_all_counters()?;
        let elapsed = now.duration_since(self.last_sample).as_secs_f64();
        let ifaces = self.ifaces.clone();

        // Compute each interface's byte delta this tick; feed it to the 1-second
        // judging window and derive the active interface's instantaneous rate.
        for iface in &ifaces {
            if let Some(&(rx, tx)) = counters.get(iface) {
                let (prev_rx, prev_tx) = self.prev_counters.get(iface).copied().unwrap_or((0, 0));
                // Guard against the first sample (no baseline yet) to avoid counting
                // all traffic since boot as a single delta.
                let d_rx = if prev_rx > 0 { rx.saturating_sub(prev_rx) } else { 0 };
                let d_tx = if prev_tx > 0 { tx.saturating_sub(prev_tx) } else { 0 };
                if iface == &self.iface && elapsed > 0.0 && prev_rx > 0 {
                    self.current_rx = (d_rx as f64 / elapsed) as u64;
                    self.current_tx = (d_tx as f64 / elapsed) as u64;
                }
                *self.auto_window_bytes.entry(iface.clone()).or_insert(0) += d_rx + d_tx;
                self.prev_counters.insert(iface.clone(), (rx, tx));
            }
        }

        // Once the 1-second window has elapsed, judge the busiest interface from the
        // accumulated traffic and reset the window for the next round.
        if now.duration_since(self.auto_window_start) >= Duration::from_secs(1) {
            self.auto_switch_if_needed(now);
            for v in self.auto_window_bytes.values_mut() {
                *v = 0;
            }
            self.auto_window_start = now;
        }

        // Push to history ring buffers.
        self.rx_history.push_back(self.current_rx);
        self.tx_history.push_back(self.current_tx);
        if self.rx_history.len() > MAX_HISTORY {
            self.rx_history.pop_front();
        }
        if self.tx_history.len() > MAX_HISTORY {
            self.tx_history.pop_front();
        }

        // Update peak (use a rolling decay so the gauge doesn't get stuck).
        self.peak_speed = self
            .peak_speed
            .max(self.current_rx)
            .max(self.current_tx);

        if let Some(&(rx, tx)) = counters.get(&self.iface) {
            self.prev_rx_bytes = rx;
            self.prev_tx_bytes = tx;
        }
        self.last_sample = now;

        // Sample system-wide CPU and memory usage so their histories can be drawn
        // as waveforms below the DL/UL sparklines. Both are sampled every tick
        // (~2 Hz) to stay in sync with the traffic history.
        if let Some((busy, total)) = read_system_cpu() {
            if self.prev_cpu_total > 0 && total > self.prev_cpu_total {
                let dt = now.duration_since(self.prev_cpu_time).as_secs_f64();
                let d_busy = busy.saturating_sub(self.prev_cpu_busy) as f64;
                let d_total = total.saturating_sub(self.prev_cpu_total) as f64;
                if dt > 0.0 && d_total > 0.0 {
                    let pct = (d_busy / d_total * 100.0).clamp(0.0, 100.0);
                    self.sys_cpu_history.push_back(pct);
                } else {
                    self.sys_cpu_history.push_back(0.0);
                }
            } else {
                // First sample on this interface: prime the baseline, no value yet.
                self.sys_cpu_history.push_back(0.0);
            }
            self.prev_cpu_busy = busy;
            self.prev_cpu_total = total;
            self.prev_cpu_time = now;
        }
        if self.sys_cpu_history.len() > MAX_HISTORY {
            self.sys_cpu_history.pop_front();
        }

        if let Some(pct) = read_system_mem_pct() {
            self.sys_mem_history.push_back(pct.clamp(0.0, 100.0));
        } else {
            self.sys_mem_history.push_back(0.0);
        }
        if self.sys_mem_history.len() > MAX_HISTORY {
            self.sys_mem_history.pop_front();
        }

        // System disk read/write throughput (bytes/s). Sectors are 512 bytes; we sum
        // the cumulative counters and divide by the elapsed time between ticks.
        if let Some((rsec, wsec)) = read_sys_disk_sectors() {
            if self.prev_disk_read_sectors > 0
                && rsec >= self.prev_disk_read_sectors
                && wsec >= self.prev_disk_write_sectors
            {
                let dt = now.duration_since(self.prev_disk_time).as_secs_f64();
                if dt > 0.0 {
                    let dr = (rsec - self.prev_disk_read_sectors) as f64 * 512.0 / dt;
                    let dw = (wsec - self.prev_disk_write_sectors) as f64 * 512.0 / dt;
                    self.sys_disk_read_history.push_back(dr);
                    self.sys_disk_write_history.push_back(dw);
                } else {
                    self.sys_disk_read_history.push_back(0.0);
                    self.sys_disk_write_history.push_back(0.0);
                }
            } else {
                // First sample (or counter reset): prime the baseline, no value yet.
                self.sys_disk_read_history.push_back(0.0);
                self.sys_disk_write_history.push_back(0.0);
            }
            self.prev_disk_read_sectors = rsec;
            self.prev_disk_write_sectors = wsec;
            self.prev_disk_time = now;
        }
        if self.sys_disk_read_history.len() > MAX_HISTORY {
            self.sys_disk_read_history.pop_front();
        }
        if self.sys_disk_write_history.len() > MAX_HISTORY {
            self.sys_disk_write_history.pop_front();
        }

        // Per-connection throughput is sampled at its own slower cadence (~2 Hz)
        // so we don't run `ss` on every main tick.
        if now.duration_since(self.last_conns_sample) >= Duration::from_millis(500) {
            let _ = self.conns.sample();
            self.last_conns_sample = now;
        }

        Ok(())
    }

    /// Switch to the interface at `idx` (wrapping). Resets history, peak and the
    /// counter baseline so the first sample on the new interface is not a bogus delta.
    fn switch_iface(&mut self, idx: usize) {
        if self.ifaces.is_empty() {
            return;
        }
        self.iface_idx = idx % self.ifaces.len();
        self.iface = self.ifaces[self.iface_idx].clone();

        // Drop history and peak from the previous interface.
        self.rx_history.clear();
        self.tx_history.clear();
        self.sys_cpu_history.clear();
        self.sys_mem_history.clear();
        self.sys_disk_read_history.clear();
        self.sys_disk_write_history.clear();
        self.prev_disk_read_sectors = 0;
        self.prev_disk_write_sectors = 0;
        self.prev_disk_time = Instant::now();
        self.prev_cpu_busy = 0;
        self.prev_cpu_total = 0;
        self.prev_cpu_time = Instant::now();
        self.current_rx = 0;
        self.current_tx = 0;
        self.peak_speed = 1;

        // Re-baseline counters immediately; skip the delta on the next tick.
        if let Ok((rx, tx)) = self.read_counters() {
            self.prev_rx_bytes = rx;
            self.prev_tx_bytes = tx;
            self.prev_counters.insert(self.iface.clone(), (rx, tx));
        } else {
            self.prev_rx_bytes = 0;
            self.prev_tx_bytes = 0;
            self.prev_counters.insert(self.iface.clone(), (0, 0));
        }
        // The first sample on the new interface must read 0, not a delta spike.
        self.last_sample = Instant::now();
    }

    /// Return the displayed max for sparkline scaling (rolling window max + 10% headroom).
    fn sparkline_max(&self, history: &VecDeque<u64>) -> u64 {
        let window: Vec<&u64> = history
            .iter()
            .rev()
            .take(MAX_HISTORY)
            .collect();
        let max = window.iter().fold(1u64, |acc, &&v| acc.max(v));
        (max as f64 * 1.1) as u64 + 1
    }
}

/// f64 variant of `App::sparkline_max` for the disk-throughput histories.
fn sparkline_max_f(history: &VecDeque<f64>) -> f64 {
    let max = history.iter().cloned().fold(1.0_f64, f64::max);
    max * 1.1 + 1.0
}

// ── Helpers ──────────────────────────────────────────────────────────────────

/// Auto-detect the default network interface by reading /proc/net/route.
fn detect_interface() -> io::Result<String> {
    let content = fs::read_to_string("/proc/net/route")?;
    for line in content.lines().skip(1) {
        let fields: Vec<&str> = line.split_whitespace().collect();
        if fields.len() >= 8 && fields[1] == "00000000" && fields[7] == "00000000" {
            return Ok(fields[0].to_string());
        }
    }
    // Fallback: pick the first non-lo interface from /proc/net/dev.
    let dev = fs::read_to_string("/proc/net/dev")?;
    for line in dev.lines().skip(2) {
        let line = line.trim();
        if let Some(idx) = line.find(':') {
            let iface = line[..idx].trim();
            if iface != "lo" {
                return Ok(iface.to_string());
            }
        }
    }
    Err(io::Error::new(
        io::ErrorKind::NotFound,
        "no non-loopback network interface found",
    ))
}

/// List every non-loopback interface present in /proc/net/dev, in file order.
fn list_interfaces() -> io::Result<Vec<String>> {
    let content = fs::read_to_string("/proc/net/dev")?;
    let mut names = Vec::new();
    for line in content.lines().skip(2) {
        let line = line.trim();
        if let Some(idx) = line.find(':') {
            let name = line[..idx].trim();
            if !name.is_empty() && name != "lo" {
                names.push(name.to_string());
            }
        }
    }
    if names.is_empty() {
        return Err(io::Error::new(
            io::ErrorKind::NotFound,
            "no non-loopback network interface found",
        ));
    }
    Ok(names)
}

/// Read the aggregate `cpu` line from `/proc/stat` and return the cumulative
/// (busy_ticks, total_ticks). `total` is the sum of every field on the line;
/// `busy` is total minus the idle+iowait portion. Comparing two snapshots'
/// deltas gives overall CPU utilisation across all cores as a 0..1 fraction
/// without needing to know the core count.
fn read_system_cpu() -> Option<(u64, u64)> {
    let content = fs::read_to_string("/proc/stat").ok()?;
    let line = content.lines().find(|l| l.starts_with("cpu "))?;
    let fields: Vec<u64> = line
        .split_whitespace()
        .skip(1) // drop the "cpu" token
        .filter_map(|f| f.parse::<u64>().ok())
        .collect();
    if fields.len() < 4 {
        return None;
    }
    let total: u64 = fields.iter().sum();
    // idle (field 3) + iowait (field 4, may be absent on older kernels).
    let idle = fields[3] + fields.get(4).copied().unwrap_or(0);
    let busy = total.saturating_sub(idle);
    Some((busy, total))
}

/// Read system memory usage as a percentage of total RAM from `/proc/meminfo`.
/// Uses `MemAvailable` when present (the kernel's best estimate of reclaimable
/// memory), falling back to `MemFree`. Returns `None` if `MemTotal` is missing.
fn read_system_mem_pct() -> Option<f64> {
    let content = fs::read_to_string("/proc/meminfo").ok()?;
    let mut total: Option<u64> = None;
    let mut avail: Option<u64> = None;
    for line in content.lines() {
        if line.starts_with("MemTotal:") {
            total = line.split_whitespace().nth(1).and_then(|v| v.parse().ok());
        } else if line.starts_with("MemAvailable:") {
            avail = line.split_whitespace().nth(1).and_then(|v| v.parse().ok());
        } else if line.starts_with("MemFree:") && avail.is_none() {
            avail = line.split_whitespace().nth(1).and_then(|v| v.parse().ok());
        }
    }
    match (total, avail) {
        (Some(t), Some(a)) if t > 0 => {
            Some(((t.saturating_sub(a)) as f64 / t as f64) * 100.0)
        }
        _ => None,
    }
}

/// Read cumulative disk sectors read/written across all whole-block devices.
///
/// `/proc/diskstats` counts both whole disks and their partitions, and a
/// partition's counters are a subset of its parent disk's, so summing everything
/// double-counts. To avoid that we only sum device names that appear as entries
/// under `/sys/block` (these are the whole disks / loop / ram devices; partitions
/// live in subdirectories and are excluded). Each sector is 512 bytes. Returns
/// `(read_sectors, write_sectors)`.
fn read_sys_disk_sectors() -> Option<(u64, u64)> {
    let mut devices: HashSet<String> = HashSet::new();
    if let Ok(entries) = fs::read_dir("/sys/block") {
        for entry in entries.flatten() {
            devices.insert(entry.file_name().to_string_lossy().into_owned());
        }
    }
    let content = fs::read_to_string("/proc/diskstats").ok()?;
    let mut read_sectors = 0u64;
    let mut write_sectors = 0u64;
    for line in content.lines() {
        let fields: Vec<&str> = line.split_whitespace().collect();
        if fields.len() < 10 {
            continue;
        }
        // If we resolved a device list, only count whole disks; otherwise sum all.
        if !devices.is_empty() && !devices.contains(fields[2]) {
            continue;
        }
        // fields[5] = sectors read, fields[9] = sectors written (1-based indices).
        read_sectors += fields[5].parse::<u64>().unwrap_or(0);
        write_sectors += fields[9].parse::<u64>().unwrap_or(0);
    }
    Some((read_sectors, write_sectors))
}

/// Format bytes/s into a human-readable string.
fn format_speed(bytes_per_sec: u64) -> String {
    const UNITS: &[&str] = &["B/s", "KB/s", "MB/s", "GB/s", "TB/s"];
    let mut value = bytes_per_sec as f64;
    let mut unit_idx = 0;
    while value >= 1024.0 && unit_idx < UNITS.len() - 1 {
        value /= 1024.0;
        unit_idx += 1;
    }
    if unit_idx == 0 {
        format!("{:>6.0} {}", value, UNITS[unit_idx])
    } else {
        format!("{:>6.1} {}", value, UNITS[unit_idx])
    }
}

/// Format cumulative CPU time the way htop's `TIME+` does: `MM:SS.cc` below an
/// hour, `HH:MM:SS` at or above an hour (centiseconds dropped).
fn fmt_time_plus(secs: f64) -> String {
    let total_cs = (secs * 100.0).round() as u64;
    let cs = total_cs % 100;
    let total_s = total_cs / 100;
    let ss = total_s % 60;
    let mm = (total_s / 60) % 60;
    let hh = total_s / 3600;
    if hh > 0 {
        format!("{:02}:{:02}:{:02}", hh, mm, ss)
    } else {
        format!("{:02}:{:02}.{:02}", mm, ss, cs)
    }
}

/// Draw a smoothed waveform as a continuous braille line. Each character cell
/// carries a 2x4 dot matrix, so consecutive samples connect into one curve
/// instead of the 8-level stepped bars the `Sparkline` widget produces. A
/// 3-point moving average rounds the curve and flattens single-sample spikes.
fn draw_waveform(
    f: &mut Frame,
    area: Rect,
    history: &VecDeque<u64>,
    max: u64,
    color: Color,
) {
    if area.width == 0 || area.height == 0 || history.is_empty() || max == 0 {
        return;
    }
    let width = area.width as usize;
    let height = area.height as usize; // braille character rows
    let xres = width * 2; // 2 horizontal dots per cell
    let yres = height * 4; // 4 vertical dots per cell

    let n = history.len();

    // Normalised series (0..1) over the whole buffer.
    let data: Vec<f64> = history
        .iter()
        .map(|&v| (v as f64 / max as f64).clamp(0.0, 1.0))
        .collect();

    // 3-point moving average for a rounder curve.
    let smoothed: Vec<f64> = (0..n)
        .map(|i| {
            let a = data[if i > 0 { i - 1 } else { i }];
            let b = data[i];
            let c = data[if i + 1 < n { i + 1 } else { i }];
            (a + b + c) / 3.0
        })
        .collect();

    // Dynamic X scale. The currently-available samples are stretched across the
    // full chart width, so the (initially very sparse) waveform is always clearly
    // visible instead of being crammed into a sliver at the left edge of a 24h
    // window. As history accumulates the window grows until it spans the full
    // MAX_HISTORY (1 day) and the chart "zooms out" to show the whole day, after
    // which it scrolls as new samples arrive.
    let shown = (n as f64).min(MAX_HISTORY as f64);
    let span = (shown - 1.0).max(1.0);
    let x_of = |i: usize| -> f64 {
        i as f64 * (xres - 1) as f64 / span
    };
    let y_of = |val: f64| -> i32 {
        ((1.0 - val) * (yres - 1) as f64).round() as i32
    };

    // Dot grid: one bool per (x, y) dot. We render a *hollow* line, so we first
    // reduce the (possibly huge) sample buffer to one representative value per
    // horizontal dot column. This keeps the curve exactly one dot thick no matter
    // how many samples compress into a column — a 24h buffer (172800 samples)
    // would otherwise fill every column solid and read as a block rather than a
    // line. Value 0 -> bottom, value 1 -> top.
    let xr = (xres as f64 - 1.0).max(1.0);
    let mut col_val: Vec<Option<f64>> = vec![None; xres];
    for dx in 0..xres {
        // Which sample index sits closest to this dot column's centre?
        let sample_f = dx as f64 * span / xr;
        let i0 = sample_f.floor() as i32;
        let i1 = sample_f.ceil() as i32;
        let mut best: Option<(f64, f64)> = None; // (distance, value)
        for &i in &[i0, i1] {
            if i >= 0 && (i as usize) < n {
                let d = (x_of(i as usize) - dx as f64).abs();
                let better = match best {
                    None => true,
                    Some((bd, _)) => d < bd,
                };
                if better {
                    best = Some((d, smoothed[i as usize]));
                }
            }
        }
        if let Some((_, v)) = best {
            col_val[dx] = Some(v);
        }
    }

    let mut dots = vec![false; xres * yres];
    let points: Vec<(i32, i32)> = col_val
        .iter()
        .enumerate()
        .filter_map(|(dx, v)| v.map(|val| (dx as i32, y_of(val))))
        .collect();

    if points.len() == 1 {
        let (x, y) = points[0];
        if x >= 0 && x < xres as i32 && y >= 0 && y < yres as i32 {
            dots[y as usize * xres + x as usize] = true;
        }
    } else {
        for k in 0..points.len().saturating_sub(1) {
            let (x0, y0) = points[k];
            let (x1, y1) = points[k + 1];
            let steps = ((x1 - x0).abs()).max(1) as i32;
            for s in 0..=steps {
                let t = s as f64 / steps as f64;
                let x = (x0 as f64 + (x1 - x0) as f64 * t).round() as i32;
                let y = (y0 as f64 + (y1 - y0) as f64 * t).round() as i32;
                if x >= 0 && x < xres as i32 && y >= 0 && y < yres as i32 {
                    dots[y as usize * xres + x as usize] = true;
                }
            }
        }
    }

    // Paint braille characters into the buffer.
    let buf = f.buffer_mut();
    for cy in 0..height {
        for cx in 0..width {
            let mut bits: u32 = 0;
            for dy in 0..4 {
                for dx in 0..2 {
                    if dots[(cy * 4 + dy) * xres + (cx * 2 + dx)] {
                        let bit = match (dx, dy) {
                            (0, 0) => 0,
                            (0, 1) => 1,
                            (0, 2) => 2,
                            (1, 0) => 3,
                            (1, 1) => 4,
                            (1, 2) => 5,
                            (0, 3) => 6,
                            (1, 3) => 7,
                            _ => 0,
                        };
                        bits |= 1 << bit;
                    }
                }
            }
            let ch = char::from_u32(0x2800 + bits).unwrap_or(' ');
            let cell = &mut buf[(area.x + cx as u16, area.y + cy as u16)];
            let s = ch.to_string();
            cell.set_symbol(&s);
            cell.set_style(Style::default().fg(color));
        }
    }
}

/// Float-valued variant of `draw_waveform` for percentage histories (0..100 %).
/// The data is already normalised against a known max (e.g. 100.0), so the only
/// difference from `draw_waveform` is that the series is `f64` rather than `u64`.
fn draw_waveform_f(
    f: &mut Frame,
    area: Rect,
    history: &VecDeque<f64>,
    max: f64,
    color: Color,
) {
    if area.width == 0 || area.height == 0 || history.is_empty() || max <= 0.0 {
        return;
    }
    let width = area.width as usize;
    let height = area.height as usize; // braille character rows
    let xres = width * 2; // 2 horizontal dots per cell
    let yres = height * 4; // 4 vertical dots per cell

    let n = history.len();

    // Normalised series (0..1) over the whole buffer.
    let data: Vec<f64> = history
        .iter()
        .map(|&v| (v as f64 / max as f64).clamp(0.0, 1.0))
        .collect();

    // 3-point moving average for a rounder curve.
    let smoothed: Vec<f64> = (0..n)
        .map(|i| {
            let a = data[if i > 0 { i - 1 } else { i }];
            let b = data[i];
            let c = data[if i + 1 < n { i + 1 } else { i }];
            (a + b + c) / 3.0
        })
        .collect();

    // Dynamic X scale (same scheme as draw_waveform): stretch the available
    // samples across the full width, then "zoom out" once a full day is buffered.
    let shown = (n as f64).min(MAX_HISTORY as f64);
    let span = (shown - 1.0).max(1.0);
    let x_of = |i: usize| -> f64 {
        i as f64 * (xres - 1) as f64 / span
    };
    let y_of = |val: f64| -> i32 {
        ((1.0 - val) * (yres - 1) as f64).round() as i32
    };

    let xr = (xres as f64 - 1.0).max(1.0);
    let mut col_val: Vec<Option<f64>> = vec![None; xres];
    for dx in 0..xres {
        let sample_f = dx as f64 * span / xr;
        let i0 = sample_f.floor() as i32;
        let i1 = sample_f.ceil() as i32;
        let mut best: Option<(f64, f64)> = None; // (distance, value)
        for &i in &[i0, i1] {
            if i >= 0 && (i as usize) < n {
                let d = (x_of(i as usize) - dx as f64).abs();
                let better = match best {
                    None => true,
                    Some((bd, _)) => d < bd,
                };
                if better {
                    best = Some((d, smoothed[i as usize]));
                }
            }
        }
        if let Some((_, v)) = best {
            col_val[dx] = Some(v);
        }
    }

    let mut dots = vec![false; xres * yres];
    let points: Vec<(i32, i32)> = col_val
        .iter()
        .enumerate()
        .filter_map(|(dx, v)| v.map(|val| (dx as i32, y_of(val))))
        .collect();

    if points.len() == 1 {
        let (x, y) = points[0];
        if x >= 0 && x < xres as i32 && y >= 0 && y < yres as i32 {
            dots[y as usize * xres + x as usize] = true;
        }
    } else {
        for k in 0..points.len().saturating_sub(1) {
            let (x0, y0) = points[k];
            let (x1, y1) = points[k + 1];
            let steps = ((x1 - x0).abs()).max(1) as i32;
            for s in 0..=steps {
                let t = s as f64 / steps as f64;
                let x = (x0 as f64 + (x1 - x0) as f64 * t).round() as i32;
                let y = (y0 as f64 + (y1 - y0) as f64 * t).round() as i32;
                if x >= 0 && x < xres as i32 && y >= 0 && y < yres as i32 {
                    dots[y as usize * xres + x as usize] = true;
                }
            }
        }
    }

    let buf = f.buffer_mut();
    for cy in 0..height {
        for cx in 0..width {
            let mut bits: u32 = 0;
            for dy in 0..4 {
                for dx in 0..2 {
                    if dots[(cy * 4 + dy) * xres + (cx * 2 + dx)] {
                        let bit = match (dx, dy) {
                            (0, 0) => 0,
                            (0, 1) => 1,
                            (0, 2) => 2,
                            (1, 0) => 3,
                            (1, 1) => 4,
                            (1, 2) => 5,
                            (0, 3) => 6,
                            (1, 3) => 7,
                            _ => 0,
                        };
                        bits |= 1 << bit;
                    }
                }
            }
            let ch = char::from_u32(0x2800 + bits).unwrap_or(' ');
            let cell = &mut buf[(area.x + cx as u16, area.y + cy as u16)];
            let s = ch.to_string();
            cell.set_symbol(&s);
            cell.set_style(Style::default().fg(color));
        }
    }
}

/// Render UI.
fn ui(f: &mut Frame, app: &mut App) {
    let area = f.area();
    if app.focus_conns {
        // Focus mode: the connections panel takes the whole screen.
        let panel_area = Layout::default()
            .margin(1)
            .constraints([Constraint::Min(0)])
            .split(area)[0];
        render_conn_panel(f, panel_area, app);
    } else {
        let outer = Layout::default()
            .direction(Direction::Vertical)
            .margin(1)
            .constraints([
                Constraint::Length(31), // top: title + stats box (DL/UL/CPU/MEM/DISK R/DISK W) + footer
                Constraint::Min(5),     // bottom: top connections
            ])
            .split(area);
        render_top(f, outer[0], app);
        render_conn_panel(f, outer[1], app);
    }
}

/// Render the top section: interface title, speed gauges and history sparklines.
fn render_top(f: &mut Frame, area: Rect, app: &App) {
    let main_layout = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(3), // title
            Constraint::Length(27), // stats box: DL/UL/CPU/MEM/DISK R/DISK W waveforms + time axis
            Constraint::Length(1), // footer
        ])
        .split(area);

    // ── Title bar (unchanged) ────────────────────────────────────────────
    let title = Paragraph::new(Line::from(vec![
        Span::styled("◉ Network Monitor", Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD)),
        Span::raw("  │  "),
        Span::styled(
            format!("interface: {}", app.iface),
            Style::default().fg(Color::Yellow),
        ),
        Span::raw("  │  "),
        Span::styled(
            format!("iface {}/{}", app.iface_idx + 1, app.ifaces.len()),
            Style::default().fg(Color::DarkGray),
        ),
        Span::raw("  │  "),
        Span::styled("n/p: iface", Style::default().fg(Color::DarkGray)),
        Span::raw("  │  "),
        Span::styled("i: auto-iface", Style::default().fg(Color::DarkGray)),
        Span::raw("  │  "),
        Span::styled("r: reset", Style::default().fg(Color::DarkGray)),
        Span::raw("  │  "),
        Span::styled("q: quit", Style::default().fg(Color::DarkGray)),
    ]))
    .block(Block::default().borders(Borders::ALL));
    f.render_widget(title, main_layout[0]);

    // ── Compact stats: one bordered box holding DL/UL labels + sparklines,
    //    with a horizontal divider between DL/UL and a vertical divider
    //    between each label and its waveform. ──────────────────────────────
    let window_h = MAX_HISTORY as f64 * TICK_MS as f64 / 1000.0 / 3600.0;
    let rx_max = app.sparkline_max(&app.rx_history);
    let tx_max = app.sparkline_max(&app.tx_history);

    let stats_block = Block::default()
        .borders(Borders::ALL)
        .title(Span::styled(
            format!(" ▼ DL / ▲ UL / CPU / MEM / DISK R / DISK W History ({:.0}h) ", window_h),
            Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD),
        ));
    let stats_inner = stats_block.inner(main_layout[1]);
    f.render_widget(stats_block, main_layout[1]);

    // Six metric rows (DL, UL, CPU, MEM, DISK R, DISK W) of *equal* height,
    // plus dividers between them and the time-axis baseline + labels row. The
    // per-metric height is derived from the actual available space so every
    // waveform block stays the same height. (A fixed `Length(3)` per metric
    // would be shrunk unevenly by ratatui when the box is shorter than the
    // sum — it keeps the first and last rows at full height and squeezes the
    // middle ones — which looked inconsistent.)
    let n_metrics = 6;
    let sep_rows = (n_metrics - 1) as i32 + 2; // 5 dividers + axis baseline + labels
    let inner_h = stats_inner.height as i32;
    let metric_h = (((inner_h - sep_rows).max(0)) / n_metrics as i32).max(1) as u16;
    let mut vcons: Vec<Constraint> = Vec::with_capacity(n_metrics * 2 + 1);
    for i in 0..n_metrics {
        vcons.push(Constraint::Length(metric_h));
        if i + 1 < n_metrics {
            vcons.push(Constraint::Length(1));
        }
    }
    vcons.push(Constraint::Length(1)); // axis baseline
    vcons.push(Constraint::Length(1)); // time labels
    let vrows = Layout::default()
        .direction(Direction::Vertical)
        .constraints(vcons)
        .split(stats_inner);

    // Download: [label | vdiv | sparkline].
    let dl = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Length(20), Constraint::Length(1), Constraint::Min(20)])
        .split(vrows[0]);
    let dl_label = Paragraph::new(Line::from(Span::styled(
        format!("▼ DL {}", format_speed(app.current_rx)),
        Style::default().fg(Color::Green).add_modifier(Modifier::BOLD),
    )));
    f.render_widget(dl_label, dl[0]);
    draw_waveform(f, dl[2], &app.rx_history, rx_max, Color::Green);

    // Upload: [label | vdiv | sparkline].
    let ul = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Length(20), Constraint::Length(1), Constraint::Min(20)])
        .split(vrows[2]);
    let ul_label = Paragraph::new(Line::from(Span::styled(
        format!("▲ UL {}", format_speed(app.current_tx)),
        Style::default().fg(Color::Red).add_modifier(Modifier::BOLD),
    )));
    f.render_widget(ul_label, ul[0]);
    draw_waveform(f, ul[2], &app.tx_history, tx_max, Color::Red);

    // System CPU: [label | vdiv | waveform]. Values are 0..100 %, so the fixed
    // max is simply 100.0 (the waveform fills proportionally to total capacity).
    let cpu = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Length(20), Constraint::Length(1), Constraint::Min(20)])
        .split(vrows[4]);
    let cpu_now = app.sys_cpu_history.back().copied().unwrap_or(0.0);
    let cpu_label = Paragraph::new(Line::from(Span::styled(
        format!("▌ CPU {:.1}%", cpu_now),
        Style::default().fg(Color::Yellow).add_modifier(Modifier::BOLD),
    )));
    f.render_widget(cpu_label, cpu[0]);
    draw_waveform_f(f, cpu[2], &app.sys_cpu_history, 100.0, Color::Yellow);

    // System MEM: [label | vdiv | waveform].
    let mem = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Length(20), Constraint::Length(1), Constraint::Min(20)])
        .split(vrows[6]);
    let mem_now = app.sys_mem_history.back().copied().unwrap_or(0.0);
    let mem_label = Paragraph::new(Line::from(Span::styled(
        format!("▌ MEM {:.1}%", mem_now),
        Style::default().fg(Color::Magenta).add_modifier(Modifier::BOLD),
    )));
    f.render_widget(mem_label, mem[0]);
    draw_waveform_f(f, mem[2], &app.sys_mem_history, 100.0, Color::Magenta);

    // System DISK R: [label | vdiv | waveform]. Byte rates, scaled dynamically.
    let dr_max = sparkline_max_f(&app.sys_disk_read_history);
    let diskr = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Length(20), Constraint::Length(1), Constraint::Min(20)])
        .split(vrows[8]);
    let diskr_now = app.sys_disk_read_history.back().copied().unwrap_or(0.0);
    let diskr_label = Paragraph::new(Line::from(Span::styled(
        format!("▌ DISK R {}", format_speed(diskr_now as u64)),
        Style::default().fg(Color::Blue).add_modifier(Modifier::BOLD),
    )));
    f.render_widget(diskr_label, diskr[0]);
    draw_waveform_f(f, diskr[2], &app.sys_disk_read_history, dr_max, Color::Blue);

    // System DISK W: [label | vdiv | waveform].
    let dw_max = sparkline_max_f(&app.sys_disk_write_history);
    let diskw = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Length(20), Constraint::Length(1), Constraint::Min(20)])
        .split(vrows[10]);
    let diskw_now = app.sys_disk_write_history.back().copied().unwrap_or(0.0);
    let diskw_label = Paragraph::new(Line::from(Span::styled(
        format!("▌ DISK W {}", format_speed(diskw_now as u64)),
        Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD),
    )));
    f.render_widget(diskw_label, diskw[0]);
    draw_waveform_f(f, diskw[2], &app.sys_disk_write_history, dw_max, Color::Cyan);

    // Draw the grid dividers directly into the buffer so the vertical and
    // horizontal lines join into clean crosses. There is a horizontal divider
    // between each waveform row, plus the baseline under the DISK W waveform that
    // carries the time axis.
    let buf = f.buffer_mut();
    let vline_x = dl[1].x;
    let grid_style = Style::default().fg(Color::DarkGray);
    // Vertical divider spanning the whole stats box.
    for y in stats_inner.y..stats_inner.y + stats_inner.height {
        let cell = &mut buf[(vline_x, y)];
        cell.set_symbol("│");
        cell.set_style(grid_style);
    }
    // Horizontal dividers + axis baseline (one cross each where they meet the vline).
    let hlines = [
        vrows[1].y,
        vrows[3].y,
        vrows[5].y,
        vrows[7].y,
        vrows[9].y,
        vrows[11].y,
    ];
    for &hy in &hlines {
        for x in stats_inner.x..stats_inner.x + stats_inner.width {
            let cell = &mut buf[(x, hy)];
            cell.set_symbol("─");
            cell.set_style(grid_style);
        }
        let cross = &mut buf[(vline_x, hy)];
        cross.set_symbol("┼");
        cross.set_style(grid_style);
    }

    // ── Time scale on the X axis ─────────────────────────────────────────────
    // A `now HH:MM:SS` label is printed at the right edge (newest sample =
    // current time). Adaptive `HH:MM` labels are placed below the axis; the
    // number shown scales with the available width so they never overlap. All
    // times are rendered in Asia/Shanghai (UTC+8) via `shanghai_hms`.
    let n = app.rx_history.len();
    if n >= 2 {
        let now_secs = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        let label_style = Style::default().fg(Color::Gray);

        // Seconds of history currently shown on the (dynamic) axis.
        let window_seconds = ((n as f64 - 1.0) * TICK_MS as f64 / 1000.0).max(1.0);
        let xres = dl[2].width as usize * 2; // dots, matches draw_waveform
        let xres_f = (xres as f64 - 1.0).max(0.0);
        let mut last_label_x: i32 = i32::MIN / 2;
        let mut last_label_text = String::new();

        // Dynamically choose how many of the 288 tick positions get a text label
        // so they always have room: aim for one label roughly every 8 columns of
        // axis width. Wide terminals show many labels; narrow ones show few.
        let axis_cols = dl[2].width as i32;
        let max_labels = (axis_cols / 8).max(2);
        let step = ((288 + max_labels - 1) / max_labels).max(1); // ceil(288/max_labels)

        for k in 0..=288 {
            let f = k as f64 / 288.0; // 0 = oldest (left), 1 = newest (right)
            let xpix = f * xres_f;
            let gx = dl[2].x + (xpix.round() as usize / 2) as u16;

            // Time label at every `step`-th position; skip the right edge because
            // the "now" label already marks it, and skip a label that repeats the
            // previous one (happens when the time window is still tiny).
            if k % step == 0 && k != 288 {
                let t = now_secs - ((1.0 - f) * window_seconds) as u64;
                let (hh, mm, _) = shanghai_hms(t);
                let label = format!("{:02}:{:02}", hh, mm);
                let lx = gx.saturating_sub(2) as i32;
                if lx >= stats_inner.x as i32
                    && (lx + label.len() as i32) <= stats_inner.x as i32 + stats_inner.width as i32
                    && lx - last_label_x >= label.len() as i32 + 2
                    && label != last_label_text
                {
                    for (i, ch) in label.chars().enumerate() {
                        let c = &mut buf[(lx as u16 + i as u16, vrows[12].y)];
                        c.set_symbol(&ch.to_string());
                        c.set_style(label_style);
                    }
                    last_label_x = lx;
                    last_label_text = label;
                }
            }
        }

        // Current system time (Asia/Shanghai) at the right edge of the axis.
        let (now_h, now_m, now_s) = shanghai_hms(now_secs);
        let now_label = format!("now {:02}:{:02}:{:02}", now_h, now_m, now_s);
        let nlen = now_label.len() as u16;
        let nlx = stats_inner.x + stats_inner.width.saturating_sub(nlen + 1);
        if nlx >= stats_inner.x
            && nlx + nlen <= stats_inner.x + stats_inner.width
        {
            for (i, ch) in now_label.chars().enumerate() {
                let cell = &mut buf[(nlx + i as u16, vrows[12].y)];
                cell.set_symbol(&ch.to_string());
                cell.set_style(label_style);
            }
        }
    }

    // ── Footer ──────────────────────────────────────────────────────────
    let peak_str = format!("Session peak: {}", format_speed(app.peak_speed));
    let footer = Paragraph::new(Line::from(vec![
        Span::styled(peak_str, Style::default().fg(Color::DarkGray)),
        Span::raw("  │  "),
        Span::styled(
            format!("Samples: {}", app.rx_history.len()),
            Style::default().fg(Color::DarkGray),
        ),
        Span::raw("  │  "),
        Span::styled(
            format!("auto: {}", if app.auto_iface { "on" } else { "off" }),
            Style::default().fg(if app.auto_iface {
                Color::Green
            } else {
                Color::DarkGray
            }),
        ),
    ]));
    f.render_widget(footer, main_layout[2]);
}

/// Column that the Top Connections list can be sorted by. `Throughput` is the
/// default ordering (total RX+TX, descending) that the list used before sorting
/// was added; it has no dedicated header so it is never shown as the active column.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum SortKey {
    Throughput,
    User,
    Proc,
    Proto,
    Src,
    Dst,
    Host,
    Svc,
    Cpu,
    Mem,
    Time,
    Conns,
    Rx,
    Tx,
    DiskR,
    DiskW,
}

/// Order in which `o` (rotate sort column) steps through columns, per view.
const AGG_SORT_ORDER: [SortKey; 10] = [
    SortKey::User,
    SortKey::Proc,
    SortKey::Cpu,
    SortKey::Mem,
    SortKey::Time,
    SortKey::Conns,
    SortKey::Rx,
    SortKey::Tx,
    SortKey::DiskR,
    SortKey::DiskW,
];
const DETAIL_SORT_ORDER: [SortKey; 8] = [
    SortKey::Proc,
    SortKey::Proto,
    SortKey::Src,
    SortKey::Dst,
    SortKey::Host,
    SortKey::Svc,
    SortKey::Rx,
    SortKey::Tx,
];

/// A comparable value extracted for sorting. `None` sorts as empty so that a
/// column which does not apply to the current view type keeps a stable order.
#[derive(PartialEq)]
enum SortVal {
    None,
    Text(String),
    Num(f64),
}

impl Eq for SortVal {}

impl Ord for SortVal {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        match (self, other) {
            (SortVal::None, SortVal::None) => std::cmp::Ordering::Equal,
            (SortVal::None, _) => std::cmp::Ordering::Less,
            (_, SortVal::None) => std::cmp::Ordering::Greater,
            (SortVal::Text(a), SortVal::Text(b)) => a.cmp(b),
            (SortVal::Num(a), SortVal::Num(b)) => {
                a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal)
            }
            // Keep a deterministic order between the two present variants.
            (SortVal::Text(_), SortVal::Num(_)) => std::cmp::Ordering::Less,
            (SortVal::Num(_), SortVal::Text(_)) => std::cmp::Ordering::Greater,
        }
    }
}

impl PartialOrd for SortVal {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

/// Label used for sorting by process (comm + pid), matching the displayed text.
fn proc_sort_label(comm: &str, pid: Option<u32>) -> String {
    match pid {
        Some(p) => format!("{}({})", comm, p),
        None => comm.to_string(),
    }
}

/// Extract the sortable value for `row` under `key`.
fn sort_val(row: &ConnRow, key: SortKey) -> SortVal {
    match (row, key) {
        (ConnRow::Agg(a), SortKey::User) => SortVal::Text(a.user.clone()),
        (ConnRow::Agg(a), SortKey::Proc) => SortVal::Text(proc_sort_label(&a.comm, a.pid)),
        (ConnRow::Agg(a), SortKey::Cpu) => SortVal::Num(a.cpu_pct),
        (ConnRow::Agg(a), SortKey::Mem) => SortVal::Num(a.mem_pct),
        (ConnRow::Agg(a), SortKey::Time) => SortVal::Num(a.time_secs),
        (ConnRow::Agg(a), SortKey::Conns) => SortVal::Num(a.count as f64),
        (ConnRow::Agg(a), SortKey::Rx) => SortVal::Num(a.rx_rate),
        (ConnRow::Agg(a), SortKey::Tx) => SortVal::Num(a.tx_rate),
        (ConnRow::Agg(a), SortKey::DiskR) => SortVal::Num(a.disk_read_rate),
        (ConnRow::Agg(a), SortKey::DiskW) => SortVal::Num(a.disk_write_rate),
        (ConnRow::Agg(a), SortKey::Throughput) => SortVal::Num(a.rx_rate + a.tx_rate),
        (ConnRow::Detail(c), SortKey::Proc) => SortVal::Text(proc_sort_label(&c.comm, c.pid)),
        (ConnRow::Detail(c), SortKey::Proto) => SortVal::Text(c.proto.clone()),
        (ConnRow::Detail(c), SortKey::Src) => SortVal::Text(c.local.clone()),
        (ConnRow::Detail(c), SortKey::Dst) => SortVal::Text(c.remote.clone()),
        (ConnRow::Detail(c), SortKey::Host) => {
            SortVal::Text(c.host.clone().unwrap_or_default())
        }
        (ConnRow::Detail(c), SortKey::Svc) => {
            SortVal::Text(c.service.clone().unwrap_or_default())
        }
        (ConnRow::Detail(c), SortKey::Rx) => SortVal::Num(c.rx_rate.unwrap_or(0.0)),
        (ConnRow::Detail(c), SortKey::Tx) => SortVal::Num(c.tx_rate.unwrap_or(0.0)),
        (ConnRow::Detail(c), SortKey::Throughput) => {
            SortVal::Num(c.rx_rate.unwrap_or(0.0) + c.tx_rate.unwrap_or(0.0))
        }
        _ => SortVal::None,
    }
}

/// Sort `view` in place by `key`, in ascending order when `asc` is true.
fn sort_rows(view: &mut [ConnRow], key: SortKey, asc: bool) {
    view.sort_by(|a, b| {
        let ord = sort_val(a, key).cmp(&sort_val(b, key));
        if asc {
            ord
        } else {
            ord.reverse()
        }
    });
}

/// One row in the connections table, in either detail (per-socket) or
/// aggregated (per-process) form.
enum ConnRow {
    Detail(ConnStat),
    Agg(AggRow),
}

/// Render the Top Connections panel (detail or aggregated view) into `area`.
fn render_conn_panel(f: &mut Frame, area: Rect, app: &mut App) {
    let mut view: Vec<ConnRow> = if app.aggregate {
        app.conns
            .aggregate_view(&app.filter)
            .into_iter()
            .map(ConnRow::Agg)
            .collect()
    } else {
        app.conns
            .detail_view(&app.filter)
            .into_iter()
            .map(ConnRow::Detail)
            .collect()
    };

    // Sort the list according to the active column and direction. `Throughput`
    // (the default) sorts by total RX+TX descending, matching the pre-sort order.
    sort_rows(&mut view, app.sort_key, app.sort_asc);

    let view_len = view.len();
    let max_rows = (area.height.saturating_sub(3) as usize).max(1); // borders + header
    app.conns.clamp_scroll(view_len, max_rows);

    let first = if view_len == 0 { 0 } else { app.conns.scroll + 1 };
    let last = (app.conns.scroll + max_rows).min(view_len);
    let mut title = format!(" Top Connections {}-{} ", first, last.max(first));
    title.push_str(&format!("/{} ", view_len));
    if app.aggregate {
        title.push_str("[agg] ");
    }
    if !app.filter.is_empty() || app.filter_mode {
        title.push_str(&format!("filter:\"{}\" ", app.filter));
    }
    title.push_str("↑↓ pg:scroll c:focus a:agg /:filter click hdr/o:sort O:dir");

    let block = Block::default().borders(Borders::ALL).title(title);

    if !app.conns.available {
        let msg = Paragraph::new(" ss unavailable — install iproute2's `ss` ").block(block);
        f.render_widget(msg, area);
        return;
    }
    if view_len == 0 {
        let hint = if app.filter.is_empty() {
            " no active connections "
        } else {
            " no connections match the filter "
        };
        let msg = Paragraph::new(hint).block(block);
        f.render_widget(msg, area);
        return;
    }

    let widths: Vec<Constraint> = if app.aggregate {
        vec![
            Constraint::Length(8),  // USER
            Constraint::Length(24), // PROC(PID)
            Constraint::Length(16), // CPU%
            Constraint::Length(16), // MEM%
            Constraint::Length(10), // TIME+
            Constraint::Length(6),  // CONNS
            Constraint::Length(11), // RX
            Constraint::Length(11), // TX
            Constraint::Length(11), // DISKR
            Constraint::Length(11), // DISKW
        ]
    } else {
        vec![
            Constraint::Min(14),
            Constraint::Length(4),
            Constraint::Min(13),
            Constraint::Min(13),
            Constraint::Min(12),
            Constraint::Length(7),
            Constraint::Length(9),
            Constraint::Length(9),
        ]
    };

    // Header labels + the SortKey each maps to (parallel arrays).
    let (labels, keys): (&[&str], &[Option<SortKey>]) = if app.aggregate {
        (
            &["USER", "PROC(PID)", "CPU%", "MEM%", "TIME+", "CONNS", "RX", "TX", "DISKR", "DISKW"],
            &[
                Some(SortKey::User),
                Some(SortKey::Proc),
                Some(SortKey::Cpu),
                Some(SortKey::Mem),
                Some(SortKey::Time),
                Some(SortKey::Conns),
                Some(SortKey::Rx),
                Some(SortKey::Tx),
                Some(SortKey::DiskR),
                Some(SortKey::DiskW),
            ],
        )
    } else {
        (
            &["PROC(PID)", "PRO", "SRC", "DST", "HOST", "SVC", "RX", "TX"],
            &[
                Some(SortKey::Proc),
                Some(SortKey::Proto),
                Some(SortKey::Src),
                Some(SortKey::Dst),
                Some(SortKey::Host),
                Some(SortKey::Svc),
                Some(SortKey::Rx),
                Some(SortKey::Tx),
            ],
        )
    };
    let marker = if app.sort_asc { " ▲" } else { " ▼" };
    let header_cells: Vec<Cell> = labels
        .iter()
        .enumerate()
        .map(|(i, &lbl)| {
            if keys.get(i).copied().flatten() == Some(app.sort_key) {
                Cell::from(format!("{}{}", lbl, marker))
            } else {
                Cell::from(lbl)
            }
        })
        .collect();
    let header = Row::new(header_cells).style(Style::default().add_modifier(Modifier::BOLD));

    // Record header hitboxes so a mouse click on a column title can re-sort.
    // This mirrors how ratatui's Table lays the columns out inside the block
    // (full inner width; we don't enable a scrollbar, so no column is reserved).
    let inner = block.inner(area);
    let col_rects = Layout::default()
        .direction(Direction::Horizontal)
        .constraints(widths.clone())
        .split(inner);
    app.header_y = inner.y;
    app.col_hit.clear();
    for (i, k) in keys.iter().enumerate() {
        if let Some(key) = k {
            app.col_hit
                .push((col_rects[i].x, col_rects[i].x + col_rects[i].width, *key));
        }
    }

    let rows = view
        .iter()
        .skip(app.conns.scroll)
        .take(max_rows)
        .enumerate()
        .map(|(i, row)| {
            let global_idx = app.conns.scroll + i;
            let style = if global_idx % 2 == 1 {
                Style::default().bg(ZEBRA)
            } else {
                Style::default()
            };
            match row {
                ConnRow::Agg(a) => {
                    let proc = match a.pid {
                        Some(p) => format!("{}({})", a.comm, p),
                        None => a.comm.clone(),
                    };
                    Row::new(vec![
                        Cell::from(a.user.clone()),
                        Cell::from(proc),
                        Cell::from(format!("{:>6.1}%", a.cpu_pct)),
                        Cell::from(format!("{:>6.1}%", a.mem_pct)),
                        Cell::from(fmt_time_plus(a.time_secs)),
                        Cell::from(a.count.to_string()),
                        Cell::from(format_speed(a.rx_rate as u64)),
                        Cell::from(format_speed(a.tx_rate as u64)),
                        Cell::from(format_speed(a.disk_read_rate as u64)),
                        Cell::from(format_speed(a.disk_write_rate as u64)),
                    ])
                    .style(style)
                }
                ConnRow::Detail(c) => {
                    let proc = match c.pid {
                        Some(p) => format!("{}({})", c.comm, p),
                        None => c.comm.clone(),
                    };
                    let rx = match c.rx_rate {
                        Some(r) => format_speed(r as u64),
                        None => "n/a".to_string(),
                    };
                    let tx = match c.tx_rate {
                        Some(t) => format_speed(t as u64),
                        None => "n/a".to_string(),
                    };
                    let host = c.host.clone().unwrap_or_else(|| "-".to_string());
                    let svc = c.service.clone().unwrap_or_else(|| "-".to_string());
                    Row::new(vec![
                        Cell::from(proc),
                        Cell::from(c.proto.clone()),
                        Cell::from(c.local.clone()),
                        Cell::from(c.remote.clone()),
                        Cell::from(host),
                        Cell::from(svc),
                        Cell::from(rx),
                        Cell::from(tx),
                    ])
                    .style(style)
                }
            }
        });

    let table = Table::default()
        .header(header)
        .block(block)
        .widths(&widths)
        .rows(rows);
    f.render_widget(table, area);
}

// ── Main ────────────────────────────────────────────────────────────────────

fn main() -> io::Result<()> {
    // Detect the default network interface.
    let iface = detect_interface().unwrap_or_else(|e| {
        eprintln!("Error detecting network interface: {}", e);
        eprintln!("Hint: pass an interface name as argument, e.g.: netmon eth0");
        std::process::exit(1);
    });

    // Allow CLI override: `netmon eth0`
    let iface = std::env::args().nth(1).unwrap_or(iface);

    // Build the switchable interface list. The requested interface stays first-class
    // even if it is not in the list (e.g. an explicit "lo").
    let mut ifaces = list_interfaces().unwrap_or_else(|_| vec![iface.clone()]);
    let iface_idx = match ifaces.iter().position(|n| n == &iface) {
        Some(i) => i,
        None => {
            ifaces.insert(0, iface.clone());
            0
        }
    };

    let mut app = App::new(iface, ifaces, iface_idx);

    // Initialise counters with the first read so tick() can compute a delta.
    match app.read_counters() {
        Ok((rx, tx)) => {
            app.prev_rx_bytes = rx;
            app.prev_tx_bytes = tx;
            app.last_sample = Instant::now();
        }
        Err(e) => {
            eprintln!("Failed to read network counters: {}", e);
            std::process::exit(1);
        }
    }

    // Set up terminal.
    enable_raw_mode()?;
    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen, EnableMouseCapture)?;
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend)?;

    // Run the event loop.
    let tick_duration = Duration::from_millis(TICK_MS);
    let res = run_app(&mut terminal, &mut app, tick_duration);

    // Restore terminal.
    disable_raw_mode()?;
    execute!(
        terminal.backend_mut(),
        DisableMouseCapture,
        LeaveAlternateScreen
    )?;

    if let Err(err) = res {
        eprintln!("Error: {err:?}");
    }
    Ok(())
}

fn run_app<B: Backend>(
    terminal: &mut Terminal<B>,
    app: &mut App,
    tick_duration: Duration,
) -> io::Result<()> {
    let mut last_tick = Instant::now();

    loop {
        // Draw.
        terminal.draw(|f| ui(f, app))?;

        // Wait for input or timeout.
        let timeout = tick_duration.saturating_sub(last_tick.elapsed());
        if crossterm::event::poll(timeout)? {
            match event::read()? {
                Event::Key(key) => {
                    // While typing a filter, route keys to the filter buffer.
                    if app.filter_mode {
                        match key.code {
                            KeyCode::Char(c) => app.filter.push(c),
                            KeyCode::Backspace => {
                                app.filter.pop();
                            }
                            KeyCode::Enter | KeyCode::Esc => app.filter_mode = false,
                            _ => {}
                        }
                        continue;
                    }
                    match key.code {
                    KeyCode::Char('q') | KeyCode::Char('Q') | KeyCode::Esc => {
                        return Ok(());
                    }
                    KeyCode::Char('r') | KeyCode::Char('R') => {
                        // Reset history, peak, connection monitor and filter.
                        app.rx_history.clear();
                        app.tx_history.clear();
                        app.sys_cpu_history.clear();
                        app.sys_mem_history.clear();
                        app.sys_disk_read_history.clear();
                        app.sys_disk_write_history.clear();
                        app.prev_disk_read_sectors = 0;
                        app.prev_disk_write_sectors = 0;
                        app.prev_disk_time = Instant::now();
                        app.prev_cpu_busy = 0;
                        app.prev_cpu_total = 0;
                        app.prev_cpu_time = Instant::now();
                        app.peak_speed = 1;
                        app.conns.reset();
                        app.filter.clear();
                        app.filter_mode = false;
                    }
                    KeyCode::Char('/') => {
                        // Enter filter input mode.
                        app.filter_mode = true;
                    }
                    KeyCode::Char('c') | KeyCode::Char('C') => {
                        // Toggle focus mode (connections fill the screen).
                        app.focus_conns = !app.focus_conns;
                        app.conns.scroll_top();
                    }
                    KeyCode::Char('a') | KeyCode::Char('A') => {
                        // Toggle aggregate-by-process view.
                        app.aggregate = !app.aggregate;
                        // Columns differ between views, so fall back to the default order.
                        app.sort_key = SortKey::Throughput;
                        app.sort_asc = false;
                        app.conns.scroll_top();
                    }
                    KeyCode::Char('n') | KeyCode::Char('N') | KeyCode::Tab => {
                        // Next interface (wrap around). Manual control disables auto.
                        app.auto_iface = false;
                        app.last_auto_switch = Instant::now();
                        app.switch_iface(app.iface_idx + 1);
                    }
                    KeyCode::Char('p') | KeyCode::Char('P') | KeyCode::BackTab => {
                        // Previous interface (wrap around). Manual control disables auto.
                        app.auto_iface = false;
                        app.last_auto_switch = Instant::now();
                        let len = app.ifaces.len();
                        if len > 0 {
                            app.switch_iface(app.iface_idx + len - 1);
                        }
                    }
                    KeyCode::Char('i') | KeyCode::Char('I') => {
                        // Toggle automatic switching to the busiest interface.
                        app.auto_iface = !app.auto_iface;
                        app.last_auto_switch = Instant::now();
                    }
                    KeyCode::PageUp => app.conns.scroll_page_up(),
                    KeyCode::PageDown => app.conns.scroll_page_down(),
                    KeyCode::Home => app.conns.scroll_top(),
                    KeyCode::End => app.conns.scroll_bottom(),
                    KeyCode::Up | KeyCode::Char('k') | KeyCode::Char('K') => {
                        app.conns.scroll_up();
                    }
                    KeyCode::Down | KeyCode::Char('j') | KeyCode::Char('J') => {
                        app.conns.scroll_down();
                    }
                    KeyCode::Char('o') => {
                        // Rotate the sort column to the next one available in this view.
                        let order: &[SortKey] = if app.aggregate {
                            &AGG_SORT_ORDER
                        } else {
                            &DETAIL_SORT_ORDER
                        };
                        app.sort_key = match order.iter().position(|k| *k == app.sort_key) {
                            Some(pos) => order[(pos + 1) % order.len()],
                            None => order[0],
                        };
                        app.sort_asc = false; // newest column starts descending
                        app.conns.scroll_top();
                    }
                    KeyCode::Char('O') => {
                        // Toggle sort direction on the current column.
                        app.sort_asc = !app.sort_asc;
                        app.conns.scroll_top();
                    }
                    _ => {}
                }
            }
            Event::Mouse(me) => {
                // Left-click on a column title re-sorts by that column; clicking the
                // active column again flips the direction.
                if !app.filter_mode {
                    if let MouseEventKind::Down(MouseButton::Left) = me.kind {
                        if me.row == app.header_y {
                            for &(xs, xe, key) in &app.col_hit {
                                if me.column >= xs && me.column < xe {
                                    if app.sort_key == key {
                                        app.sort_asc = !app.sort_asc;
                                    } else {
                                        app.sort_key = key;
                                        app.sort_asc = false;
                                    }
                                    app.conns.scroll_top();
                                    break;
                                }
                            }
                        }
                    }
                }
            }
            Event::Resize(_, _) => {}
            // Focus / paste events are irrelevant to the UI; ignore them.
            Event::FocusGained | Event::FocusLost | Event::Paste(_) => {}
        }
        }

        // Tick if enough time has passed.
        if last_tick.elapsed() >= tick_duration {
            if let Err(e) = app.tick() {
                // Non-fatal: keep running but don't crash on transient errors.
                eprintln!("Warning: tick failed: {e}");
            }
            last_tick = Instant::now();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::backend::TestBackend;

    fn sample_app() -> App {
        let mut app = App::new("dummy0".to_string(), vec!["dummy0".to_string()], 0);
        app.conns.last = vec![
            ConnStat {
                proto: "tcp".into(),
                local: "10.0.0.1:1".into(),
                remote: "10.0.0.2:2".into(),
                comm: "procA".into(),
                pid: Some(11),
                rx_rate: Some(1000.0),
                tx_rate: Some(500.0),
                host: Some("hosta".into()),
                service: Some("https".into()),
            },
            ConnStat {
                proto: "tcp".into(),
                local: "10.0.0.1:3".into(),
                remote: "10.0.0.3:4".into(),
                comm: "procB".into(),
                pid: Some(12),
                rx_rate: Some(20.0),
                tx_rate: None,
                host: None,
                service: None,
            },
            ConnStat {
                proto: "udp".into(),
                local: "10.0.0.1:5".into(),
                remote: "10.0.0.4:6".into(),
                comm: "procA".into(),
                pid: Some(11),
                rx_rate: None,
                tx_rate: None,
                host: None,
                service: Some("domain".into()),
            },
        ];
        app
    }

    #[test]
    fn renders_all_connection_modes_without_panic() {
        let mut app = sample_app();
        let backend = TestBackend::new(120, 40);
        let mut term = Terminal::new(backend).unwrap();

        // Detail view (default).
        term.draw(|f| ui(f, &mut app)).unwrap();
        // Focus mode.
        app.focus_conns = true;
        term.draw(|f| ui(f, &mut app)).unwrap();
        // Aggregate view.
        app.focus_conns = false;
        app.aggregate = true;
        term.draw(|f| ui(f, &mut app)).unwrap();
        // Filter that matches some rows.
        app.aggregate = false;
        app.filter = "procA".into();
        term.draw(|f| ui(f, &mut app)).unwrap();
        // Filter that matches nothing.
        app.filter = "zzz".into();
        term.draw(|f| ui(f, &mut app)).unwrap();
    }

    #[test]
    fn aggregate_header_shows_disk_columns() {
        let mut app = sample_app();
        app.aggregate = true;
        // Wide enough for all ten aggregate columns.
        let backend = TestBackend::new(200, 40);
        let mut term = Terminal::new(backend).unwrap();
        term.draw(|f| ui(f, &mut app)).unwrap();
        let content = format!("{:?}", term.backend().buffer());
        assert!(content.contains("DISKR"), "aggregate header missing DISKR");
        assert!(content.contains("DISKW"), "aggregate header missing DISKW");
    }

    #[test]
    fn sorts_aggregate_rows_by_column() {
        fn agg(cpu: f64, mem: f64, user: &str) -> ConnRow {
            ConnRow::Agg(AggRow {
                comm: "x".into(),
                pid: Some(1),
                count: 1,
                rx_rate: 0.0,
                tx_rate: 0.0,
                user: user.to_string(),
                time_secs: 0.0,
                cpu_pct: cpu,
                mem_pct: mem,
                disk_read_rate: 0.0,
                disk_write_rate: 0.0,
            })
        }
        let mut rows = vec![agg(10.0, 1.0, "bob"), agg(50.0, 5.0, "amy"), agg(30.0, 3.0, "cara")];

        // CPU% descending: 50, 30, 10.
        sort_rows(&mut rows, SortKey::Cpu, false);
        let cpus: Vec<f64> = rows
            .iter()
            .map(|r| match r {
                ConnRow::Agg(a) => a.cpu_pct,
                _ => 0.0,
            })
            .collect();
        assert_eq!(cpus, vec![50.0, 30.0, 10.0]);

        // CPU% ascending: 10, 30, 50.
        sort_rows(&mut rows, SortKey::Cpu, true);
        let cpus: Vec<f64> = rows
            .iter()
            .map(|r| match r {
                ConnRow::Agg(a) => a.cpu_pct,
                _ => 0.0,
            })
            .collect();
        assert_eq!(cpus, vec![10.0, 30.0, 50.0]);

        // USER ascending alphabetically: amy, bob, cara.
        sort_rows(&mut rows, SortKey::User, true);
        let users: Vec<String> = rows
            .iter()
            .map(|r| match r {
                ConnRow::Agg(a) => a.user.clone(),
                _ => String::new(),
            })
            .collect();
        assert_eq!(users, vec!["amy", "bob", "cara"]);
    }

    #[test]
    fn sorts_aggregate_rows_by_disk_columns() {
        let a = ConnRow::Agg(AggRow {
            comm: "a".into(),
            pid: Some(1),
            count: 1,
            rx_rate: 0.0,
            tx_rate: 0.0,
            user: "a".into(),
            time_secs: 0.0,
            cpu_pct: 0.0,
            mem_pct: 0.0,
            disk_read_rate: 100.0,
            disk_write_rate: 5.0,
        });
        let b = ConnRow::Agg(AggRow {
            comm: "b".into(),
            pid: Some(2),
            count: 1,
            rx_rate: 0.0,
            tx_rate: 0.0,
            user: "b".into(),
            time_secs: 0.0,
            cpu_pct: 0.0,
            mem_pct: 0.0,
            disk_read_rate: 300.0,
            disk_write_rate: 50.0,
        });
        let c = ConnRow::Agg(AggRow {
            comm: "c".into(),
            pid: Some(3),
            count: 1,
            rx_rate: 0.0,
            tx_rate: 0.0,
            user: "c".into(),
            time_secs: 0.0,
            cpu_pct: 0.0,
            mem_pct: 0.0,
            disk_read_rate: 200.0,
            disk_write_rate: 20.0,
        });
        let mut rows = vec![a, b, c];

        // DISKR descending: 300 (b), 200 (c), 100 (a).
        sort_rows(&mut rows, SortKey::DiskR, false);
        let dr: Vec<f64> = rows
            .iter()
            .map(|r| match r {
                ConnRow::Agg(x) => x.disk_read_rate,
                _ => 0.0,
            })
            .collect();
        assert_eq!(dr, vec![300.0, 200.0, 100.0]);

        // DISKW ascending: 5 (a), 20 (c), 50 (b).
        sort_rows(&mut rows, SortKey::DiskW, true);
        let dw: Vec<f64> = rows
            .iter()
            .map(|r| match r {
                ConnRow::Agg(x) => x.disk_write_rate,
                _ => 0.0,
            })
            .collect();
        assert_eq!(dw, vec![5.0, 20.0, 50.0]);
    }

    #[test]
    fn auto_switches_to_busiest_interface() {
        let mut app = sample_app();
        app.ifaces = vec!["eth0".to_string(), "eth1".to_string()];
        app.iface = "eth0".to_string();
        app.iface_idx = 0;
        app.auto_iface = true;
        // Allow an immediate switch.
        app.last_auto_switch = Instant::now() - Duration::from_secs(10);

        // eth1 accumulated far more traffic over the 1s window.
        app.auto_window_bytes.insert("eth0".to_string(), 0);
        app.auto_window_bytes.insert("eth1".to_string(), 100_000);
        app.current_rx = 0;
        app.current_tx = 0;

        app.auto_switch_if_needed(Instant::now());
        assert_eq!(app.iface, "eth1");
        assert_eq!(app.iface_idx, 1);

        // A tiny margin (1000 bytes in 1s) must NOT trigger a switch -> no thrashing.
        let mut app2 = sample_app();
        app2.ifaces = vec!["eth0".to_string(), "eth1".to_string()];
        app2.iface = "eth0".to_string();
        app2.iface_idx = 0;
        app2.auto_iface = true;
        app2.last_auto_switch = Instant::now() - Duration::from_secs(10);
        app2.auto_window_bytes.insert("eth0".to_string(), 0);
        app2.auto_window_bytes.insert("eth1".to_string(), 1000);
        app2.current_rx = 0;
        app2.current_tx = 0;
        app2.auto_switch_if_needed(Instant::now());
        assert_eq!(app2.iface, "eth0", "should not thrash on a tiny margin");

        // Auto off -> never switches.
        let mut app3 = app2;
        app3.auto_iface = false;
        app3.auto_window_bytes.insert("eth1".to_string(), 100_000);
        app3.auto_switch_if_needed(Instant::now());
        assert_eq!(app3.iface, "eth0");
    }

    #[test]
    fn renders_unavailable_and_empty_states() {
        let mut app = sample_app();
        app.conns.available = false;
        let backend = TestBackend::new(80, 20);
        let mut term = Terminal::new(backend).unwrap();
        term.draw(|f| ui(f, &mut app)).unwrap();

        // Empty list (ss available but no connections).
        let mut app2 = sample_app();
        app2.conns.last.clear();
        app2.conns.available = true;
        term.draw(|f| ui(f, &mut app2)).unwrap();
    }
}
