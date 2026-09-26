//! Per-connection / per-process network throughput via `ss`.
//!
//! We sample the output of `ss -tunpi` once per tick. For TCP sockets the
//! detail line carries `bytes_received:` / `bytes_sent:` (cumulative counters
//! from the kernel's `tcp_info`), so we can compute a per-second rate from the
//! delta between two samples. UDP sockets are connectionless and the kernel
//! does not maintain cumulative per-socket byte counters, so their rate is
//! always `None` (rendered as `n/a`).

use std::collections::{HashMap, HashSet};
use std::ffi::CStr;
use std::io;
use std::net::IpAddr;
use std::process::Command;
use std::sync::mpsc;
use std::thread;
use std::time::Instant;

use libc;

/// One sampled connection (or UDP socket).
#[derive(Clone)]
pub struct ConnStat {
    pub proto: String,
    pub local: String,
    pub remote: String,
    pub comm: String,
    pub pid: Option<u32>,
    /// Received bytes/s. `None` for UDP (kernel does not track it).
    pub rx_rate: Option<f64>,
    /// Sent bytes/s. `None` for UDP.
    pub tx_rate: Option<f64>,
    /// Reverse-DNS host name for the remote IP. `None` while unresolved or
    /// when the IP has no PTR record.
    pub host: Option<String>,
    /// Service name for the remote port (from `/etc/services`), e.g. "https".
    pub service: Option<String>,
}

/// A process aggregated over all of its connections (used by the "aggregate"
/// view so a process with many sockets shows as a single row).
#[derive(Clone)]
pub struct AggRow {
    pub comm: String,
    pub pid: Option<u32>,
    pub count: usize,
    pub rx_rate: f64,
    pub tx_rate: f64,
    /// Owner username of the process (from `/proc/<pid>/status` Uid → passwd).
    pub user: String,
    /// Cumulative CPU time (utime + stime) in seconds, htop's `TIME+`.
    pub time_secs: f64,
    /// Recent average CPU usage as a percentage of one core (htop's `CPU%`).
    pub cpu_pct: f64,
    /// Resident memory usage as a percentage of total RAM (htop's `MEM%`).
    pub mem_pct: f64,
    /// Per-process disk read throughput in bytes/s, from `/proc/<pid>/io`.
    pub disk_read_rate: f64,
    /// Per-process disk write throughput in bytes/s, from `/proc/<pid>/io`.
    pub disk_write_rate: f64,
}

/// htop-style per-process resource info, read from `/proc/<pid>`.
#[derive(Clone)]
pub struct ProcInfo {
    pub user: String,
    pub cpu_pct: f64,
    pub mem_pct: f64,
    pub time_secs: f64,
    /// Disk read throughput (bytes/s) from `/proc/<pid>/io` cumulative counters.
    pub disk_read_rate: f64,
    /// Disk write throughput (bytes/s) from `/proc/<pid>/io` cumulative counters.
    pub disk_write_rate: f64,
}

type Key = (String, String, String);

/// Tracks the previous sample of every connection and produces a sorted list
/// of the current top connections on each `sample()` call.
pub struct ConnMonitor {
    prev: HashMap<Key, (u64, u64, Instant)>,
    pub scroll: usize,
    pub last: Vec<ConnStat>,
    /// Whether the last `ss` invocation succeeded. `false` when `ss` is
    /// missing or fails, in which case `last` is empty.
    pub available: bool,
    /// Sender for async reverse-DNS results (cloned into worker threads).
    tx: mpsc::Sender<(IpAddr, Option<String>)>,
    /// Receiver for async reverse-DNS results, drained at the start of each sample.
    rx: mpsc::Receiver<(IpAddr, Option<String>)>,
    /// Resolved host names keyed by remote IP (`None` = no PTR / failed).
    host_cache: HashMap<IpAddr, Option<String>>,
    /// IPs with an in-flight reverse-DNS lookup (avoids spawning duplicates).
    pending: HashSet<IpAddr>,
    /// Port → service-name map loaded from `/etc/services`.
    services: HashMap<(u16, String), String>,
    /// Last known visible row count, used to compute page-scroll steps.
    last_max_rows: usize,
    /// Per-pid resource info (USER/CPU%/MEM%/TIME+) for processes with at least
    /// one visible connection. Rebuilt each `sample()`.
    procs: HashMap<u32, ProcInfo>,
    /// Previous (cpu ticks, instant) per pid, for computing CPU% deltas.
    proc_prev: HashMap<u32, (u64, Instant)>,
    /// Previous (read_bytes, write_bytes, instant) per pid, for computing
    /// per-process disk read/write throughput deltas from `/proc/<pid>/io`.
    proc_io_prev: HashMap<u32, (u64, u64, Instant)>,
    /// Total system RAM in KiB, from `/proc/meminfo` (read once).
    mem_total_kb: u64,
    /// Cache of uid → username. Populated lazily: an in-process `getpwuid_r`
    /// lookup first, falling back to `getent passwd <uid>` (which honours NSS
    /// modules such as ldap/sssd that a static musl build cannot use directly).
    /// Misses are cached too, so we never re-spawn `getent` for the same uid.
    uid_cache: HashMap<u32, String>,
}

impl ConnMonitor {
    pub fn new() -> Self {
        let (tx, rx) = mpsc::channel();
        Self {
            prev: HashMap::new(),
            scroll: 0,
            last: Vec::new(),
            available: true,
            tx,
            rx,
            host_cache: HashMap::new(),
            pending: HashSet::new(),
            services: load_services(),
            last_max_rows: 10,
            procs: HashMap::new(),
            proc_prev: HashMap::new(),
            proc_io_prev: HashMap::new(),
            mem_total_kb: total_mem_kb(),
            uid_cache: HashMap::new(),
        }
    }

    /// Clear history, peak baseline and scroll position.
    pub fn reset(&mut self) {
        self.prev.clear();
        self.procs.clear();
        self.proc_prev.clear();
        self.proc_io_prev.clear();
        self.scroll = 0;
        self.last.clear();
    }

    pub fn scroll_up(&mut self) {
        if self.scroll > 0 {
            self.scroll -= 1;
        }
    }

    pub fn scroll_down(&mut self) {
        self.scroll += 1;
    }

    /// Scroll up/down by roughly one screen.
    pub fn scroll_page_up(&mut self) {
        let step = self.last_max_rows.max(1);
        self.scroll = self.scroll.saturating_sub(step);
    }

    pub fn scroll_page_down(&mut self) {
        let step = self.last_max_rows.max(1);
        self.scroll = self.scroll.saturating_add(step);
    }

    /// Jump to the first / last row (last is applied on the next clamp).
    pub fn scroll_top(&mut self) {
        self.scroll = 0;
    }

    pub fn scroll_bottom(&mut self) {
        self.scroll = usize::MAX;
    }

    /// Keep `scroll` within `[0, view_len - max_rows]`. `view_len` is the length
    /// of the currently displayed view (detail or aggregated), which may differ
    /// from `self.last.len()`. Called from the renderer each frame.
    pub fn clamp_scroll(&mut self, view_len: usize, max_rows: usize) {
        self.last_max_rows = max_rows;
        let max_scroll = view_len.saturating_sub(max_rows);
        if self.scroll > max_scroll {
            self.scroll = max_scroll;
        }
    }

    /// Recompute `self.procs` for every pid present in `conns`, and update
    /// `self.proc_prev` so the next call can derive a CPU% delta. Reads
    /// `/proc/<pid>/stat` and `/proc/<pid>/status`; any unreadable process is
    /// recorded with `"-"` / zeroed values rather than dropping the row.
    fn refresh_procs(&mut self, conns: &[ConnStat], now: Instant) {
        let clk = clk_tck();
        let mut pids: Vec<u32> = conns.iter().filter_map(|c| c.pid).collect();
        pids.sort_unstable();
        pids.dedup();

        let mut new_procs: HashMap<u32, ProcInfo> = HashMap::with_capacity(pids.len());
        for pid in pids {
            let mut info = ProcInfo {
                user: "-".to_string(),
                cpu_pct: 0.0,
                mem_pct: 0.0,
                time_secs: 0.0,
                disk_read_rate: 0.0,
                disk_write_rate: 0.0,
            };
            if let Some((utime, stime)) = read_proc_stat(pid) {
                let total_ticks = utime + stime;
                info.time_secs = total_ticks as f64 / clk as f64;
                if let Some(&(prev_ticks, prev_inst)) = self.proc_prev.get(&pid) {
                    let dt = now.duration_since(prev_inst).as_secs_f64();
                    if dt > 0.0 {
                        let dcpu = total_ticks.saturating_sub(prev_ticks) as f64;
                        info.cpu_pct = (dcpu / (clk as f64 * dt)) * 100.0;
                    }
                }
                self.proc_prev.insert(pid, (total_ticks, now));
            }
            if let Some((uid, vmrss)) = read_proc_status(pid) {
                if self.mem_total_kb > 0 {
                    info.mem_pct = vmrss as f64 / self.mem_total_kb as f64 * 100.0;
                }
                info.user = self.resolve_user(uid);
            }
            // Per-process disk I/O from `/proc/<pid>/io` (cumulative counters).
            // Unreadable for processes owned by other users; treated as 0.0.
            if let Some((rb, wb)) = read_proc_io(pid) {
                if let Some(&(prev_rb, prev_wb, prev_inst)) = self.proc_io_prev.get(&pid) {
                    let dt = now.duration_since(prev_inst).as_secs_f64();
                    if dt > 0.0 {
                        info.disk_read_rate = (rb.saturating_sub(prev_rb)) as f64 / dt;
                        info.disk_write_rate = (wb.saturating_sub(prev_wb)) as f64 / dt;
                    }
                }
                self.proc_io_prev.insert(pid, (rb, wb, now));
            } else {
                // Cannot read I/O for this process; drop any stale baseline.
                self.proc_io_prev.remove(&pid);
            }
            new_procs.insert(pid, info);
        }
        self.proc_prev.retain(|pid, _| new_procs.contains_key(pid));
        self.procs = new_procs;
    }

    /// Run `ss`, parse it, compute per-connection rates and store the sorted
    /// result in `self.last`. Errors are swallowed: on failure `available` is
    /// set to `false` and the list is cleared so the UI can show a message,
    /// but the rest of the program keeps running.
    pub fn sample(&mut self) -> io::Result<()> {
        let output = match Command::new("ss").args(["-tunpi"]).output() {
            Ok(o) => o,
            Err(e) => {
                self.available = false;
                self.last.clear();
                return Err(e);
            }
        };
        if !output.status.success() {
            self.available = false;
            self.last.clear();
            return Ok(());
        }
        self.available = true;

        // Collect any completed reverse-DNS lookups from worker threads.
        while let Ok((ip, host)) = self.rx.try_recv() {
            self.host_cache.insert(ip, host);
            self.pending.remove(&ip);
        }

        let text = String::from_utf8_lossy(&output.stdout);
        let now = Instant::now();
        let lines: Vec<&str> = text.lines().collect();
        let mut conns: Vec<ConnStat> = Vec::new();
        let mut seen: HashSet<Key> = HashSet::new();

        let mut i = 0;
        while i < lines.len() {
            let line = lines[i];
            if line.trim().is_empty() {
                i += 1;
                continue;
            }
            let trimmed = line.trim_start();
            // A connection "main" line starts with the protocol token.
            let first = trimmed.split_whitespace().next().unwrap_or("");
            if first != "tcp" && first != "udp" {
                i += 1;
                continue;
            }

            let mut parts = trimmed.split_whitespace();
            let proto = parts.next().unwrap().to_string();
            let state = parts.next().unwrap_or("").to_string();
            let _recv_q = parts.next();
            let _send_q = parts.next();
            let local = parts.next().unwrap_or("").to_string();
            let remote = parts.next().unwrap_or("").to_string();
            let rest: Vec<&str> = parts.collect();
            let (comm, pid) = parse_users(&rest.join(" "));

            // Skip TCP sockets that are not actively transferring data
            // (LISTEN, TIME-WAIT, ...). Keep UDP sockets and ESTAB TCP.
            if proto == "tcp" && state != "ESTAB" {
                i += 1;
                continue;
            }

            let key = (proto.clone(), local.clone(), remote.clone());
            seen.insert(key.clone());

            // Look ahead: the detail line is indented and mentions `bytes_`.
            let mut rx_bytes: Option<u64> = None;
            let mut tx_bytes: Option<u64> = None;
            if i + 1 < lines.len() {
                let next = lines[i + 1];
                let nt = next.trim_start();
                if (next.starts_with('\t') || next.starts_with("    ")) && nt.contains("bytes_") {
                    rx_bytes = find_bytes(nt, "bytes_received:");
                    tx_bytes = find_bytes(nt, "bytes_sent:");
                    i += 1; // consume the detail line
                }
            }

            let (rx_rate, tx_rate) = match (rx_bytes, tx_bytes) {
                (Some(r), Some(t)) => match self.prev.get(&key) {
                    Some(&(pr, pt, pt0)) => {
                        let dt = now.duration_since(pt0).as_secs_f64();
                        if dt > 0.0 {
                            let rr = r.saturating_sub(pr) as f64 / dt;
                            let tr = t.saturating_sub(pt) as f64 / dt;
                            (Some(rr), Some(tr))
                        } else {
                            (None, None)
                        }
                    }
                    None => (None, None), // first sighting: no rate yet
                },
                _ => (None, None),
            };

            if let (Some(r), Some(t)) = (rx_bytes, tx_bytes) {
                self.prev.insert(key, (r, t, now));
            }

            // Resolve the remote endpoint to a service name and (async) host name.
            let remote_ep = parse_endpoint(&remote);
            let service = remote_ep.and_then(|(_, port)| self.lookup_service(port, &proto));
            let host = if let Some(ip) = remote_ep.map(|(i, _)| i) {
                if let Some(h) = self.host_cache.get(&ip) {
                    h.clone()
                } else {
                    // Spawn a worker thread on first sighting; UI shows "-" until it returns.
                    if self.pending.insert(ip) {
                        let tx = self.tx.clone();
                        thread::spawn(move || {
                            let resolved = reverse_lookup(ip);
                            let _ = tx.send((ip, resolved));
                        });
                    }
                    None
                }
            } else {
                None
            };

            conns.push(ConnStat {
                proto,
                local,
                remote,
                comm,
                pid,
                rx_rate,
                tx_rate,
                host,
                service,
            });

            i += 1;
        }

        // Drop prev entries for connections that disappeared.
        self.prev.retain(|k, _| seen.contains(k));

        // Refresh per-process resource info (USER / CPU% / MEM% / TIME+) for
        // every pid that currently has a visible connection.
        self.refresh_procs(&conns, now);

        // Sort by throughput (rx+tx) descending; connections with no rate
        // (UDP / first sample) sink to the bottom.
        conns.sort_by(|a, b| {
            let a_na = a.rx_rate.is_none() && a.tx_rate.is_none();
            let b_na = b.rx_rate.is_none() && b.tx_rate.is_none();
            match (a_na, b_na) {
                (true, false) => std::cmp::Ordering::Greater,
                (false, true) => std::cmp::Ordering::Less,
                _ => {
                    let ra = a.rx_rate.unwrap_or(0.0) + a.tx_rate.unwrap_or(0.0);
                    let rb = b.rx_rate.unwrap_or(0.0) + b.tx_rate.unwrap_or(0.0);
                    rb.partial_cmp(&ra).unwrap_or(std::cmp::Ordering::Equal)
                }
            }
        });

        self.last = conns;
        Ok(())
    }

    /// Resolve a uid to a username, caching the outcome in `self.uid_cache`.
    ///
    /// Tries the in-process `getpwuid_r` first (fast, covers `/etc/passwd`); when
    /// that yields only the raw numeric uid, falls back to `getent passwd <uid>`
    /// (NSS-aware, so it resolves ldap/sssd/systemd accounts that a static musl
    /// build cannot). Both outcomes are cached, so `getent` is spawned at most once
    /// per distinct uid for the life of the process.
    fn resolve_user(&mut self, uid: u32) -> String {
        if let Some(name) = self.uid_cache.get(&uid) {
            return name.clone();
        }
        let mut name = uid_to_user(uid);
        // `uid_to_user` echoes the numeric uid when no name is found; only then do we
        // spend a `getent` call to consult NSS.
        if name.parse::<u32>().is_ok() {
            if let Some(resolved) = getent_user(uid) {
                name = resolved;
            }
        }
        self.uid_cache.insert(uid, name.clone());
        name
    }
}

/// Parse `users:(("name",pid=N,fd=M), ...)` and return the first owner's
/// comm and pid. Returns `("-", None)` when no owner is visible (e.g. owned by
/// another user, shown as `users:(())`).
fn parse_users(s: &str) -> (String, Option<u32>) {
    let start = match s.find("users:(") {
        Some(p) => p + 7,
        None => return ("-".to_string(), None),
    };
    let rest = &s[start..];
    let after_open = match rest.find('(') {
        Some(p) => &rest[p + 1..],
        None => return ("-".to_string(), None),
    };
    let comma = match after_open.find(',') {
        Some(p) => p,
        None => return ("-".to_string(), None),
    };
    let name = after_open[..comma].trim_matches('"').to_string();
    let pid = after_open[comma..]
        .find("pid=")
        .and_then(|p| {
            let digs: String = after_open[comma + p + 4..]
                .chars()
                .take_while(|c| c.is_ascii_digit())
                .collect();
            digs.parse::<u32>().ok()
        });
    (name, pid)
}

/// Extract a `key:NNN` integer from a line (used for `bytes_received:`/`bytes_sent:`).
fn find_bytes(s: &str, key: &str) -> Option<u64> {
    let pos = s.find(key)?;
    let after = &s[pos + key.len()..];
    let val: String = after.chars().take_while(|c| c.is_ascii_digit()).collect();
    val.parse::<u64>().ok()
}

/// Clock ticks per second (`sysconf(_SC_CLK_TCK)`), at least 1.
fn clk_tck() -> u64 {
    unsafe { libc::sysconf(libc::_SC_CLK_TCK) as u64 }.max(1)
}

/// Total system RAM in KiB, from `/proc/meminfo` `MemTotal` (0 on failure).
fn total_mem_kb() -> u64 {
    if let Ok(s) = std::fs::read_to_string("/proc/meminfo") {
        for line in s.lines() {
            if line.starts_with("MemTotal:") {
                if let Some(v) = line.split_whitespace().nth(1).and_then(|x| x.parse::<u64>().ok()) {
                    return v;
                }
            }
        }
    }
    0
}

/// Read `(utime, stime)` in clock ticks from `/proc/<pid>/stat`. The `comm`
/// field may contain spaces and parentheses, so we split after the *last* `)`.
fn read_proc_stat(pid: u32) -> Option<(u64, u64)> {
    let s = std::fs::read_to_string(format!("/proc/{}/stat", pid)).ok()?;
    let rp = s.rfind(')')?;
    let rest = &s[rp + 1..];
    let fields: Vec<&str> = rest.split_whitespace().collect();
    if fields.len() < 13 {
        return None;
    }
    let utime = fields[11].parse::<u64>().ok()?; // 12th field after ')'
    let stime = fields[12].parse::<u64>().ok()?; // 13th field after ')'
    Some((utime, stime))
}

/// Read `(real_uid, VmRSS_kb)` from `/proc/<pid>/status`.
fn read_proc_status(pid: u32) -> Option<(u32, u64)> {
    let s = std::fs::read_to_string(format!("/proc/{}/status", pid)).ok()?;
    let mut uid: Option<u32> = None;
    let mut vmrss: Option<u64> = None;
    for line in s.lines() {
        if uid.is_none() && line.starts_with("Uid:") {
            uid = line.split_whitespace().nth(1).and_then(|v| v.parse::<u32>().ok());
        } else if line.starts_with("VmRSS:") {
            vmrss = line.split_whitespace().nth(1).and_then(|v| v.parse::<u64>().ok());
        }
        if uid.is_some() && vmrss.is_some() {
            break;
        }
    }
    Some((uid?, vmrss?))
}

/// Read `(read_bytes, write_bytes)` from `/proc/<pid>/io`. These are cumulative
/// counters since process start. Returns `None` when the file is unreadable
/// (e.g. process owned by another user, or kernel `hidepid` mount option).
fn read_proc_io(pid: u32) -> Option<(u64, u64)> {
    let s = std::fs::read_to_string(format!("/proc/{}/io", pid)).ok()?;
    let mut rb: Option<u64> = None;
    let mut wb: Option<u64> = None;
    for line in s.lines() {
        if rb.is_none() && line.starts_with("read_bytes:") {
            rb = line.split_whitespace().nth(1).and_then(|v| v.parse::<u64>().ok());
        } else if wb.is_none() && line.starts_with("write_bytes:") {
            wb = line.split_whitespace().nth(1).and_then(|v| v.parse::<u64>().ok());
        }
        if rb.is_some() && wb.is_some() {
            break;
        }
    }
    Some((rb?, wb?))
}

/// Map a numeric uid to a username via `getpwuid_r`, falling back to the raw
/// uid string when the account is not in the password database.
fn uid_to_user(uid: u32) -> String {
    unsafe {
        let mut buf = vec![0u8; 1024];
        let mut pwd: libc::passwd = std::mem::zeroed();
        let mut result: *mut libc::passwd = std::ptr::null_mut();
        let rc = libc::getpwuid_r(
            uid,
            &mut pwd,
            buf.as_mut_ptr() as *mut libc::c_char,
            buf.len(),
            &mut result,
        );
        if rc == 0 && !result.is_null() && !pwd.pw_name.is_null() {
            let name = CStr::from_ptr(pwd.pw_name);
            return name.to_string_lossy().into_owned();
        }
    }
    uid.to_string()
}

/// Resolve a uid to a username, consulting an in-process `getpwuid_r` first and
/// falling back to `getent passwd <uid>` when that returns only the raw number.
///
/// The fallback matters for static musl binaries: musl's `getpwuid_r` reads
/// `/etc/passwd` directly but cannot use NSS modules (ldap/sssd/systemd), so
/// accounts provided by those sources would otherwise show as a bare uid. `getent`
/// is itself dynamically linked against the system libc and honours NSS, so it
/// resolves names a static build cannot. Results are cached so `getent` is only
/// spawned once per distinct uid.
fn getent_user(uid: u32) -> Option<String> {
    let out = Command::new("getent")
        .arg("passwd")
        .arg(uid.to_string())
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    // `getent passwd <uid>` prints "name:x:uid:gid:gecos:home:shell".
    let line = String::from_utf8_lossy(&out.stdout);
    let name = line.split('\n').next()?.split(':').next()?;
    let name = name.trim();
    if name.is_empty() {
        None
    } else {
        Some(name.to_string())
    }
}

/// Split an endpoint string (`IP:port`, `[IPv6]:port`, or `*`) into its IP and
/// port. Returns `None` for wildcards or unparsable values.
fn parse_endpoint(s: &str) -> Option<(IpAddr, u16)> {
    if let Some(rest) = s.strip_prefix('[') {
        // IPv6 form: [addr]:port
        let (ip_s, port_s) = rest.rsplit_once("]:")?;
        let ip = ip_s.parse::<IpAddr>().ok()?;
        let port = port_s.parse::<u16>().ok()?;
        return Some((ip, port));
    }
    if s.starts_with('*') {
        return None;
    }
    let (ip_s, port_s) = s.rsplit_once(':')?;
    let ip = ip_s.parse::<IpAddr>().ok()?;
    let port = port_s.parse::<u16>().ok()?;
    Some((ip, port))
}

/// Reverse-DNS lookup of an IP via libc `getnameinfo` (NI_NAMEREQD so a missing
/// PTR record yields `None`). Runs in a worker thread so it never blocks the UI.
fn reverse_lookup(ip: IpAddr) -> Option<String> {
    use std::net::SocketAddr;
    let sa: SocketAddr = SocketAddr::new(ip, 0);

    let (ss, len) = unsafe {
        let mut ss: libc::sockaddr_storage = std::mem::zeroed();
        match sa {
            SocketAddr::V4(v4) => {
                let sin = libc::sockaddr_in {
                    sin_family: libc::AF_INET as u16,
                    sin_port: 0,
                    sin_addr: libc::in_addr {
                        s_addr: u32::from(*v4.ip()).to_be(),
                    },
                    sin_zero: [0; 8],
                };
                let p = &mut ss as *mut _ as *mut libc::sockaddr_in;
                *p = sin;
                (ss, std::mem::size_of::<libc::sockaddr_in>() as libc::socklen_t)
            }
            SocketAddr::V6(v6) => {
                let sin6 = libc::sockaddr_in6 {
                    sin6_family: libc::AF_INET6 as u16,
                    sin6_port: 0,
                    sin6_flowinfo: 0,
                    sin6_addr: libc::in6_addr {
                        s6_addr: v6.ip().octets(),
                    },
                    sin6_scope_id: 0,
                };
                let p = &mut ss as *mut _ as *mut libc::sockaddr_in6;
                *p = sin6;
                (ss, std::mem::size_of::<libc::sockaddr_in6>() as libc::socklen_t)
            }
        }
    };

    let mut host: [libc::c_char; 1025] = [0; 1025];
    let ret = unsafe {
        libc::getnameinfo(
            &ss as *const _ as *const libc::sockaddr,
            len,
            host.as_mut_ptr(),
            host.len() as libc::socklen_t,
            std::ptr::null_mut(),
            0,
            libc::NI_NAMEREQD,
        )
    };
    if ret != 0 {
        return None;
    }
    let bytes: Vec<u8> = host
        .iter()
        .take_while(|&&c| c != 0)
        .map(|&c| c as u8)
        .collect();
    String::from_utf8(bytes).ok()
}

impl ConnMonitor {
    /// Look up the service name for a `(port, proto)` pair from `/etc/services`.
    fn lookup_service(&self, port: u16, proto: &str) -> Option<String> {
        self.services
            .get(&(port, proto.to_ascii_lowercase()))
            .cloned()
    }
}

impl ConnMonitor {
    /// The connection list with an optional case-insensitive substring filter
    /// applied to process name, pid, addresses, host and service.
    pub fn detail_view(&self, filter: &str) -> Vec<ConnStat> {
        self.last
            .iter()
            .filter(|c| conn_matches(c, filter))
            .cloned()
            .collect()
    }

    /// Connections grouped by `(comm, pid)`, with summed RX/TX rates and a
    /// connection count, sorted by total throughput (descending).
    pub fn aggregate_view(&self, filter: &str) -> Vec<AggRow> {
        use std::collections::HashMap;
        let mut map: HashMap<(String, Option<u32>), AggRow> = HashMap::new();
        for c in self.last.iter().filter(|c| conn_matches(c, filter)) {
            let entry = map
                .entry((c.comm.clone(), c.pid))
                .or_insert_with(|| {
                    let p = c.pid.and_then(|p| self.procs.get(&p));
                    AggRow {
                        comm: c.comm.clone(),
                        pid: c.pid,
                        count: 0,
                        rx_rate: 0.0,
                        tx_rate: 0.0,
                        user: p.map(|p| p.user.clone()).unwrap_or_else(|| "-".to_string()),
                        time_secs: p.map(|p| p.time_secs).unwrap_or(0.0),
                        cpu_pct: p.map(|p| p.cpu_pct).unwrap_or(0.0),
                        mem_pct: p.map(|p| p.mem_pct).unwrap_or(0.0),
                        disk_read_rate: p.map(|p| p.disk_read_rate).unwrap_or(0.0),
                        disk_write_rate: p.map(|p| p.disk_write_rate).unwrap_or(0.0),
                    }
                });
            entry.count += 1;
            entry.rx_rate += c.rx_rate.unwrap_or(0.0);
            entry.tx_rate += c.tx_rate.unwrap_or(0.0);
        }
        let mut rows: Vec<AggRow> = map.into_values().collect();
        rows.sort_by(|a, b| {
            let ra = a.rx_rate + a.tx_rate;
            let rb = b.rx_rate + b.tx_rate;
            let a_na = a.rx_rate == 0.0 && a.tx_rate == 0.0;
            let b_na = b.rx_rate == 0.0 && b.tx_rate == 0.0;
            match (a_na, b_na) {
                (true, false) => std::cmp::Ordering::Greater,
                (false, true) => std::cmp::Ordering::Less,
                _ => rb.partial_cmp(&ra).unwrap_or(std::cmp::Ordering::Equal),
            }
        });
        rows
    }
}

/// Whether a connection matches a case-insensitive substring `filter`. An empty
/// filter matches everything.
fn conn_matches(c: &ConnStat, filter: &str) -> bool {
    if filter.is_empty() {
        return true;
    }
    let f = filter.to_ascii_lowercase();
    let pid_s = c.pid.map(|p| p.to_string()).unwrap_or_default();
    [
        c.comm.as_str(),
        pid_s.as_str(),
        c.local.as_str(),
        c.remote.as_str(),
        c.host.as_deref().unwrap_or(""),
        c.service.as_deref().unwrap_or(""),
    ]
    .iter()
    .any(|s| s.to_ascii_lowercase().contains(&f))
}

/// Load the port → service-name map from `/etc/services`. Missing file or
/// parse errors simply yield an empty map (the UI then shows `-` for service).
fn load_services() -> HashMap<(u16, String), String> {
    let mut map = HashMap::new();
    let content = match std::fs::read_to_string("/etc/services") {
        Ok(c) => c,
        Err(_) => return map,
    };
    for line in content.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let mut parts = line.split_whitespace();
        let name = match parts.next() {
            Some(n) => n,
            None => continue,
        };
        let portproto = match parts.next() {
            Some(p) => p,
            None => continue,
        };
        if let Some((port_s, proto)) = portproto.split_once('/') {
            if let Ok(port) = port_s.parse::<u16>() {
                let proto = proto.to_ascii_lowercase();
                if proto == "tcp" || proto == "udp" {
                    map.insert((port, proto), name.to_string());
                }
            }
        }
    }
    map
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};

    #[test]
    fn parse_endpoint_ipv4() {
        assert_eq!(
            parse_endpoint("127.0.0.1:10808"),
            Some((IpAddr::V4(Ipv4Addr::new(127, 0, 0, 1)), 10808))
        );
    }

    #[test]
    fn parse_endpoint_ipv6() {
        assert_eq!(
            parse_endpoint("[::1]:123"),
            Some((IpAddr::V6(Ipv6Addr::LOCALHOST), 123))
        );
    }

    #[test]
    fn parse_endpoint_wildcard_is_none() {
        assert_eq!(parse_endpoint("*:53"), None);
    }

    #[test]
    fn services_map_loads_common_ports() {
        let m = load_services();
        assert_eq!(m.get(&(443, "tcp".to_string())).map(|s| s.as_str()), Some("https"));
        assert_eq!(m.get(&(53, "udp".to_string())).map(|s| s.as_str()), Some("domain"));
    }

    #[test]
    fn parse_users_finds_first_owner() {
        let (comm, pid) =
            parse_users(r#"users:(("node-MainThread",pid=26951,fd=24))"#);
        assert_eq!(comm, "node-MainThread");
        assert_eq!(pid, Some(26951));
    }

    #[test]
    fn getent_user_parses_name() {
        // uid 1000 resolves to "dj" on this dev box via getent passwd; if that
        // ever changes the test still validates the parsing shape (name:x:uid:...).
        if let Some(name) = getent_user(1000) {
            assert!(!name.is_empty());
            assert!(!name.contains(':'));
        }
    }

    #[test]
    fn getent_user_unknown_uid_is_none() {
        // A uid that cannot exist resolves to nothing (no panic, no fake name).
        assert!(getent_user(u32::MAX).is_none());
    }

    #[test]
    fn parse_users_handles_no_owner() {
        let (comm, pid) = parse_users("users:(())");
        assert_eq!(comm, "-");
        assert_eq!(pid, None);
    }

    #[test]
    fn find_bytes_extracts_values() {
        let s = "cubic wscale:8 rto:204 bytes_acked:7984 bytes_sent:7983 bytes_received:6313 segs_out:30";
        assert_eq!(find_bytes(s, "bytes_received:"), Some(6313));
        assert_eq!(find_bytes(s, "bytes_sent:"), Some(7983));
        assert_eq!(find_bytes(s, "bytes_acked:"), Some(7984));
    }

    #[test]
    fn sample_runs_and_parses_real_ss() {
        let mut m = ConnMonitor::new();
        // Skip the test if `ss` is unavailable in this environment.
        if m.sample().is_err() {
            return;
        }
        assert!(m.available);
        // A second sample lets rates stabilise; ensure no panic and list is built.
        std::thread::sleep(std::time::Duration::from_millis(50));
        let _ = m.sample();
        let _ = m.last.len();
    }

    #[test]
    fn aggregate_view_groups_by_process() {
        let mut m = ConnMonitor::new();
        m.last = vec![
            ConnStat { proto: "tcp".into(), local: "1.1.1.1:1".into(), remote: "2.2.2.2:2".into(), comm: "app".into(), pid: Some(1), rx_rate: Some(100.0), tx_rate: Some(50.0), host: None, service: None },
            ConnStat { proto: "tcp".into(), local: "1.1.1.1:3".into(), remote: "2.2.2.2:4".into(), comm: "app".into(), pid: Some(1), rx_rate: Some(200.0), tx_rate: Some(0.0), host: None, service: None },
            ConnStat { proto: "udp".into(), local: "1.1.1.1:5".into(), remote: "3.3.3.3:6".into(), comm: "other".into(), pid: Some(2), rx_rate: None, tx_rate: None, host: None, service: None },
        ];
        let agg = m.aggregate_view("");
        assert_eq!(agg.len(), 2);
        let app_row = agg.iter().find(|r| r.comm == "app").unwrap();
        assert_eq!(app_row.count, 2);
        assert!((app_row.rx_rate - 300.0).abs() < 1e-9);
        assert!((app_row.tx_rate - 50.0).abs() < 1e-9);
        let other = agg.iter().find(|r| r.comm == "other").unwrap();
        assert_eq!(other.count, 1);
    }

    #[test]
    fn detail_view_filters_by_substring() {
        let mut m = ConnMonitor::new();
        m.last = vec![
            ConnStat { proto: "tcp".into(), local: "1.1.1.1:1".into(), remote: "2.2.2.2:2".into(), comm: "chrome".into(), pid: Some(1), rx_rate: None, tx_rate: None, host: None, service: None },
            ConnStat { proto: "tcp".into(), local: "1.1.1.1:3".into(), remote: "2.2.2.2:4".into(), comm: "ssh".into(), pid: Some(2), rx_rate: None, tx_rate: None, host: None, service: None },
        ];
        let f = m.detail_view("ssh");
        assert_eq!(f.len(), 1);
        assert_eq!(f[0].comm, "ssh");
        assert_eq!(m.detail_view("nomatch").len(), 0);
    }
}

