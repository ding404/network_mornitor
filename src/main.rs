mod conns;
mod metrics;
mod ui;
use conns::ConnMonitor;
#[cfg(test)]
use conns::{AggRow, ConnStat, SessionProcess, UserRow, UserSession};
use metrics::{
    read_host_uptime, read_sys_disk_sectors, read_system_cpu, read_system_disk_space,
    read_system_mem_pct, read_system_swap_pct,
};
#[cfg(test)]
use ui::{sort_rows, ConnRow};
use ui::{ui, SortKey, AGG_SORT_ORDER, DETAIL_SORT_ORDER};

use std::collections::{HashMap, VecDeque};
use std::fs;
use std::io;
use std::time::{Duration, Instant};

use crossterm::{
    event::{
        self, DisableMouseCapture, EnableMouseCapture, Event, KeyCode, MouseButton, MouseEventKind,
    },
    execute,
    terminal::{disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen},
};
use ratatui::{
    backend::{Backend, CrosstermBackend},
    style::Color,
    Terminal,
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

/// Convert a UNIX timestamp to a Gregorian date and time in Asia/Shanghai.
fn shanghai_datetime(utc_secs: u64) -> (i64, u32, u32, u32, u32, u32) {
    const SHANGHAI_OFFSET: i64 = 8 * 3600;
    let max_secs = (i64::MAX - SHANGHAI_OFFSET) as u64;
    let local = utc_secs.min(max_secs) as i64 + SHANGHAI_OFFSET;
    let days = local.div_euclid(86_400);
    let day_seconds = local.rem_euclid(86_400);

    // Civil date conversion from days since 1970-01-01, Gregorian calendar.
    let z = days + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
    let mut year = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let month_part = (5 * doy + 2) / 153;
    let day = doy - (153 * month_part + 2) / 5 + 1;
    let month = month_part + if month_part < 10 { 3 } else { -9 };
    year += if month <= 2 { 1 } else { 0 };

    let hour = (day_seconds / 3_600) as u32;
    let minute = ((day_seconds / 60) % 60) as u32;
    let second = (day_seconds % 60) as u32;
    (year, month as u32, day as u32, hour, minute, second)
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
    /// System-wide swap usage history (0..100 % of total swap), newest at the back.
    sys_swap_history: VecDeque<f64>,
    /// System-wide disk read throughput history (bytes/s), newest at the back.
    sys_disk_read_history: VecDeque<f64>,
    /// System-wide disk write throughput history (bytes/s), newest at the back.
    sys_disk_write_history: VecDeque<f64>,
    /// System-wide disk space usage history (0..100 % used of the largest disk), newest at the back.
    sys_disk_space_history: VecDeque<f64>,
    /// Total capacity of the largest `/dev` disk (bytes), for the live label.
    sys_disk_space_total: u64,
    /// Bytes currently used on that disk's mounted filesystems, for the live label.
    sys_disk_space_used: u64,
    /// Bytes currently available (excluding reserved blocks) on those filesystems, for the live label.
    sys_disk_space_avail: u64,
    /// Device path of the largest `/dev` disk (e.g. `/dev/sdd`), for the live label.
    sys_disk_space_dev: String,
    /// Host uptime in seconds, read from `/proc/uptime`.
    host_uptime: f64,
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
    /// Show the aggregated online-user panel instead of connections.
    show_users: bool,
    /// Selected username whose individual sessions are shown in the user panel.
    selected_user: Option<String>,
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
            sys_swap_history: VecDeque::with_capacity(MAX_HISTORY),
            sys_disk_read_history: VecDeque::with_capacity(MAX_HISTORY),
            sys_disk_write_history: VecDeque::with_capacity(MAX_HISTORY),
            sys_disk_space_history: VecDeque::with_capacity(MAX_HISTORY),
            sys_disk_space_total: 0,
            sys_disk_space_used: 0,
            sys_disk_space_avail: 0,
            sys_disk_space_dev: String::new(),
            host_uptime: 0.0,
            prev_disk_read_sectors: 0,
            prev_disk_write_sectors: 0,
            prev_disk_time: Instant::now(),
            prev_cpu_busy: 0,
            prev_cpu_total: 0,
            prev_cpu_time: Instant::now(),
            current_rx: 0,
            current_tx: 0,
            peak_speed: 1, // avoid divide-by-zero
            last_sample: Instant::now(),
            prev_counters: HashMap::new(),
            auto_window_bytes: HashMap::new(),
            auto_window_start: Instant::now(),
            auto_iface: true,
            last_auto_switch: Instant::now(),
            last_conns_sample: Instant::now(),
            conns: ConnMonitor::new(),
            focus_conns: false,
            show_users: false,
            selected_user: None,
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
                    let rx: u64 = match fields[0].parse() {
                        Ok(value) => value,
                        Err(_) => continue,
                    };
                    let tx: u64 = match fields[8].parse() {
                        Ok(value) => value,
                        Err(_) => continue,
                    };
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
        if !self.auto_iface || now.duration_since(self.last_auto_switch) <= Duration::from_secs(1) {
            return;
        }
        let best = match self.busiest_iface() {
            Some(b) => b,
            None => return,
        };
        let cur_bytes = self
            .auto_window_bytes
            .get(&self.iface)
            .copied()
            .unwrap_or(0);
        let best_bytes = self.auto_window_bytes.get(&best).copied().unwrap_or(0);
        if best != self.iface && best_bytes.saturating_sub(cur_bytes) > 4096 {
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
                let previous = self.prev_counters.get(iface).copied();
                // Guard against the first sample (no baseline yet) to avoid counting
                // all traffic since boot as a single delta.
                let (d_rx, d_tx) = previous
                    .map(|(prev_rx, prev_tx)| {
                        (rx.saturating_sub(prev_rx), tx.saturating_sub(prev_tx))
                    })
                    .unwrap_or((0, 0));
                if iface == &self.iface && elapsed > 0.0 && previous.is_some() {
                    self.current_rx = (d_rx as f64 / elapsed) as u64;
                    self.current_tx = (d_tx as f64 / elapsed) as u64;
                }
                let total_delta = d_rx.saturating_add(d_tx);
                let window = self.auto_window_bytes.entry(iface.clone()).or_insert(0);
                *window = window.saturating_add(total_delta);
                self.prev_counters.insert(iface.clone(), (rx, tx));
            } else {
                // Avoid treating a reappearing interface's long outage as one
                // enormous burst, and keep automatic selection based on live data.
                self.prev_counters.remove(iface);
                self.auto_window_bytes.remove(iface);
                if iface == &self.iface {
                    self.current_rx = 0;
                    self.current_tx = 0;
                }
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
        self.peak_speed = self.peak_speed.max(self.current_rx).max(self.current_tx);

        self.last_sample = now;

        if let Some(uptime) = read_host_uptime() {
            self.host_uptime = uptime.max(0.0);
        }

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

        if let Some(pct) = read_system_swap_pct() {
            self.sys_swap_history.push_back(pct.clamp(0.0, 100.0));
        } else {
            self.sys_swap_history.push_back(0.0);
        }
        if self.sys_swap_history.len() > MAX_HISTORY {
            self.sys_swap_history.pop_front();
        }

        // System disk space usage: largest `/dev` disk. Percentage used goes to
        // the history; total/used/avail bytes and the device path are kept for
        // the live label.
        if let Some((pct, total, used, avail, dev)) = read_system_disk_space() {
            self.sys_disk_space_history.push_back(pct.clamp(0.0, 100.0));
            self.sys_disk_space_total = total;
            self.sys_disk_space_used = used;
            self.sys_disk_space_avail = avail;
            self.sys_disk_space_dev = dev;
        } else {
            self.sys_disk_space_history.push_back(0.0);
        }
        if self.sys_disk_space_history.len() > MAX_HISTORY {
            self.sys_disk_space_history.pop_front();
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

        self.reset_metric_state();

        // Re-baseline counters immediately; skip the delta on the next tick.
        if let Ok((rx, tx)) = self.read_counters() {
            self.prev_counters.insert(self.iface.clone(), (rx, tx));
        } else {
            self.prev_counters.insert(self.iface.clone(), (0, 0));
        }
        // The first sample on the new interface must read 0, not a delta spike.
        self.last_sample = Instant::now();
    }

    /// Clear measurements and baselines that cannot be carried across a reset
    /// or an interface switch.
    fn reset_metric_state(&mut self) {
        self.rx_history.clear();
        self.tx_history.clear();
        self.sys_cpu_history.clear();
        self.sys_mem_history.clear();
        self.sys_swap_history.clear();
        self.sys_disk_read_history.clear();
        self.sys_disk_write_history.clear();
        self.sys_disk_space_history.clear();
        self.prev_disk_read_sectors = 0;
        self.prev_disk_write_sectors = 0;
        self.prev_disk_time = Instant::now();
        self.prev_cpu_busy = 0;
        self.prev_cpu_total = 0;
        self.prev_cpu_time = Instant::now();
        self.current_rx = 0;
        self.current_tx = 0;
        self.peak_speed = 1;
        self.sys_disk_space_total = 0;
        self.sys_disk_space_used = 0;
        self.sys_disk_space_avail = 0;
        self.sys_disk_space_dev.clear();
        self.auto_window_bytes.clear();
        self.auto_window_start = Instant::now();
        self.last_sample = Instant::now();
    }

    /// Return the displayed max for sparkline scaling (rolling window max + 10% headroom).
    fn sparkline_max(&self, history: &VecDeque<u64>) -> u64 {
        let max = history.iter().copied().max().unwrap_or(1);
        (max as f64 * 1.1) as u64 + 1
    }
}

/// f64 variant of `App::sparkline_max` for the disk-throughput histories.
fn sparkline_max_f(history: &VecDeque<f64>) -> f64 {
    let max = history.iter().copied().fold(1.0_f64, f64::max);
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
        format!("{} {}", value as u64, UNITS[unit_idx])
    } else {
        format!("{:.1} {}", value, UNITS[unit_idx])
    }
}

/// Format a byte count into a short human-readable size with dynamic units,
/// e.g. `12.3G`, `1.2T` or `512.0M` (binary 1024-based, like `df -h`).
fn format_bytes_short(bytes: u64) -> String {
    const KIB: f64 = 1024.0;
    const MIB: f64 = KIB * 1024.0;
    const GIB: f64 = MIB * 1024.0;
    const TIB: f64 = GIB * 1024.0;
    let v = bytes as f64;
    if v >= TIB {
        format!("{:.1}T", v / TIB)
    } else if v >= GIB {
        format!("{:.1}G", v / GIB)
    } else if v >= MIB {
        format!("{:.1}M", v / MIB)
    } else if v >= KIB {
        format!("{:.1}K", v / KIB)
    } else {
        format!("{}B", bytes)
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

/// Format host uptime as days plus a 24-hour clock.
fn format_uptime(secs: f64) -> String {
    let total = secs.max(0.0).floor() as u64;
    let days = total / 86_400;
    let hours = (total / 3_600) % 24;
    let minutes = (total / 60) % 60;
    let seconds = total % 60;
    if days > 0 {
        format!("{}d {:02}:{:02}:{:02}", days, hours, minutes, seconds)
    } else {
        format!("{:02}:{:02}:{:02}", hours, minutes, seconds)
    }
}

fn format_epoch_datetime(timestamp: u64) -> String {
    let (year, month, day, hour, minute, second) = shanghai_datetime(timestamp);
    format!(
        "{:04}-{:02}-{:02} {:02}:{:02}:{:02}",
        year, month, day, hour, minute, second
    )
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
            app.prev_counters.insert(app.iface.clone(), (rx, tx));
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
                        KeyCode::Char('q') | KeyCode::Char('Q') => {
                            return Ok(());
                        }
                        KeyCode::Esc => {
                            if app.selected_user.take().is_some() {
                                app.conns.scroll_top();
                            } else {
                                return Ok(());
                            }
                        }
                        KeyCode::Char('r') | KeyCode::Char('R') => {
                            // Reset history, peak, connection monitor and filter.
                            app.reset_metric_state();
                            app.conns.reset();
                            app.filter.clear();
                            app.filter_mode = false;
                            app.selected_user = None;
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
                        KeyCode::Char('u') | KeyCode::Char('U') => {
                            app.show_users = !app.show_users;
                            app.selected_user = None;
                            app.conns.scroll_top();
                        }
                        KeyCode::Enter if app.show_users && app.selected_user.is_none() => {
                            if let Some(user) = app.conns.users.get(app.conns.scroll) {
                                app.selected_user = Some(user.user.clone());
                                app.conns.scroll_top();
                            }
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
        let mut rows = vec![
            agg(10.0, 1.0, "bob"),
            agg(50.0, 5.0, "amy"),
            agg(30.0, 3.0, "cara"),
        ];

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

    #[test]
    fn reset_clears_histories_and_disk_label() {
        let mut app = sample_app();
        app.rx_history.push_back(123);
        app.sys_disk_space_history.push_back(75.0);
        app.sys_disk_space_total = 10;
        app.sys_disk_space_used = 8;
        app.sys_disk_space_avail = 2;
        app.sys_disk_space_dev = "/dev/test".into();
        app.auto_window_bytes.insert("dummy0".into(), 100);

        app.reset_metric_state();

        assert!(app.rx_history.is_empty());
        assert!(app.sys_disk_space_history.is_empty());
        assert_eq!(app.sys_disk_space_total, 0);
        assert_eq!(app.sys_disk_space_used, 0);
        assert_eq!(app.sys_disk_space_avail, 0);
        assert!(app.sys_disk_space_dev.is_empty());
        assert!(app.auto_window_bytes.is_empty());
    }

    #[test]
    fn formats_small_disk_sizes_without_losing_units() {
        assert_eq!(format_bytes_short(512), "512B");
        assert_eq!(format_bytes_short(1024), "1.0K");
        assert_eq!(format_bytes_short(1024 * 1024), "1.0M");
    }

    #[test]
    fn formats_host_uptime_as_days_and_clock() {
        assert_eq!(format_uptime(90061.7), "1d 01:01:01");
    }

    #[test]
    fn formats_login_times_with_shanghai_date() {
        assert_eq!(format_epoch_datetime(1_609_459_200), "2021-01-01 08:00:00");
    }

    #[test]
    fn user_table_keeps_full_login_and_process_timestamps() {
        let mut app = sample_app();
        app.show_users = true;
        app.conns.users = vec![UserRow {
            user: "dj".into(),
            online: true,
            sessions: 1,
            last_login: 1_609_459_200,
            last_process: Some(SessionProcess {
                user: "dj".into(),
                tty: "pts/0".into(),
                comm: "bash".into(),
                started_at: 1_609_459_200,
                start_ticks: 1,
            }),
            session_rows: Vec::new(),
        }];
        let backend = TestBackend::new(80, 20);
        let mut term = Terminal::new(backend).unwrap();

        term.draw(|f| ui(f, &mut app)).unwrap();

        let rendered: String = term
            .backend()
            .buffer()
            .content
            .iter()
            .map(|cell| cell.symbol())
            .collect();
        assert!(rendered.contains("SESSIONS LAST LOGIN"));
        assert!(rendered.contains("LAST PROCESS STARTED"));
        assert_eq!(rendered.matches("2021-01-01 08:00:00").count(), 2);
    }

    #[test]
    fn user_list_highlights_current_selection() {
        let mut app = sample_app();
        app.show_users = true;
        app.conns.users = vec![
            UserRow {
                user: "alice".into(),
                online: true,
                sessions: 1,
                last_login: 2,
                last_process: None,
                session_rows: Vec::new(),
            },
            UserRow {
                user: "bob".into(),
                online: false,
                sessions: 0,
                last_login: 1,
                last_process: None,
                session_rows: Vec::new(),
            },
        ];
        let backend = TestBackend::new(80, 20);
        let mut term = Terminal::new(backend).unwrap();

        term.draw(|f| ui(f, &mut app)).unwrap();

        let first_status = term
            .backend()
            .buffer()
            .content
            .iter()
            .find(|cell| cell.symbol() == "o" && cell.bg == ratatui::style::Color::Blue)
            .unwrap();
        assert_eq!(first_status.bg, ratatui::style::Color::Blue);
        assert!(first_status
            .modifier
            .contains(ratatui::style::Modifier::BOLD));
    }

    #[test]
    fn selected_user_table_shows_all_sessions() {
        let mut app = sample_app();
        app.show_users = true;
        app.selected_user = Some("dj".into());
        app.conns.users = vec![UserRow {
            user: "dj".into(),
            online: true,
            sessions: 2,
            last_login: 1_672_531_200,
            last_process: None,
            session_rows: vec![
                UserSession {
                    user: "dj".into(),
                    tty: "pts/1".into(),
                    online: true,
                    last_login: 1_672_531_200,
                    last_process: Some(SessionProcess {
                        user: "dj".into(),
                        tty: "pts/1".into(),
                        comm: "bash".into(),
                        started_at: 1_600_000_000,
                        start_ticks: 1,
                    }),
                },
                UserSession {
                    user: "dj".into(),
                    tty: "pts/2".into(),
                    online: true,
                    last_login: 1_609_459_200,
                    last_process: Some(SessionProcess {
                        user: "dj".into(),
                        tty: "pts/2".into(),
                        comm: "vim".into(),
                        started_at: 1_700_000_000,
                        start_ticks: 2,
                    }),
                },
            ],
        }];
        let backend = TestBackend::new(100, 20);
        let mut term = Terminal::new(backend).unwrap();

        term.draw(|f| ui(f, &mut app)).unwrap();

        let rendered: String = term
            .backend()
            .buffer()
            .content
            .iter()
            .map(|cell| cell.symbol())
            .collect();
        assert!(rendered.contains("Sessions for dj"));
        assert!(rendered.contains("pts/2"));
        assert!(rendered.contains("pts/1"));
        assert!(rendered.find("pts/2").unwrap() < rendered.find("pts/1").unwrap());
        assert_eq!(rendered.matches("2021-01-01 08:00:00").count(), 1);
        assert!(rendered.contains("2023-01-01 08:00:00"));
    }
}
