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
use std::io::{self, Read, Seek, SeekFrom};
use std::net::IpAddr;
use std::os::unix::fs::MetadataExt;
use std::process::Command;
use std::sync::mpsc;
use std::thread;
use std::time::Instant;

/// One currently logged-in user session read from utmp.
#[derive(Clone)]
pub struct LoginSession {
    pub user: String,
    pub tty: String,
    pub login_time: u64,
}

/// One currently running process associated with a user's terminal.
#[derive(Clone)]
pub struct SessionProcess {
    pub user: String,
    pub tty: String,
    pub comm: String,
    pub started_at: u64,
    pub(crate) start_ticks: u64,
}

#[derive(Clone)]
struct LastLogin {
    uid: u32,
    login_time: u64,
}

/// One row in the online-user view. Same-name sessions are intentionally
/// collapsed; timestamps are the newest values across those sessions.
#[derive(Clone)]
pub struct UserRow {
    pub user: String,
    pub online: bool,
    pub sessions: usize,
    pub last_login: u64,
    pub last_process: Option<SessionProcess>,
}

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

/// Worker-owned sampler that runs `ss` and `/proc` reads without touching UI
/// interaction state.
struct ConnSampler {
    prev: HashMap<Key, (u64, u64, Instant)>,
    pub last: Vec<ConnStat>,
    pub users: Vec<UserRow>,
    lastlog_mtime: Option<std::time::SystemTime>,
    lastlog_cache: Vec<LastLogin>,
    last_users_refresh: Option<Instant>,
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

impl ConnSampler {
    fn new() -> Self {
        let (tx, rx) = mpsc::channel();
        Self {
            prev: HashMap::new(),
            last: Vec::new(),
            users: Vec::new(),
            lastlog_mtime: None,
            lastlog_cache: Vec::new(),
            last_users_refresh: None,
            available: true,
            tx,
            rx,
            host_cache: HashMap::new(),
            pending: HashSet::new(),
            services: load_services(),
            procs: HashMap::new(),
            proc_prev: HashMap::new(),
            proc_io_prev: HashMap::new(),
            mem_total_kb: total_mem_kb(),
            uid_cache: HashMap::new(),
        }
    }

    /// Clear rate baselines and process state.
    pub fn reset(&mut self) {
        self.prev.clear();
        self.procs.clear();
        self.proc_prev.clear();
        self.proc_io_prev.clear();
        self.last.clear();
        self.users.clear();
        self.lastlog_mtime = None;
        self.lastlog_cache.clear();
        self.last_users_refresh = None;
        self.available = true;
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

    /// Refresh online users from utmp and associate each row with the newest
    /// currently running process on one of that user's active terminals.
    fn refresh_users(&mut self) {
        let processes = self.read_all_terminal_processes();
        let sessions =
            merge_login_sessions(read_login_sessions(), infer_login_sessions(&processes));
        let mut rows = aggregate_user_rows(&sessions, &processes);
        for login in self.read_last_login_cached() {
            let user = self.resolve_user(login.uid);
            if let Some(row) = rows.iter_mut().find(|row| row.user == user) {
                row.last_login = row.last_login.max(login.login_time);
            } else {
                rows.push(UserRow {
                    user,
                    online: false,
                    sessions: 0,
                    last_login: login.login_time,
                    last_process: None,
                });
            }
        }
        sort_user_rows(&mut rows);
        self.users = rows;
    }

    /// Scan process stat files and retain processes with a known `/dev/pts/N`
    /// controlling terminal. This also supplies an online-session fallback when
    /// the host does not maintain a readable utmp file.
    fn read_all_terminal_processes(&mut self) -> Vec<SessionProcess> {
        let tty_names = terminal_device_names();
        let boot_time = match read_boot_time() {
            Some(value) => value,
            None => return Vec::new(),
        };
        let clk = clk_tck();
        let mut processes = Vec::new();
        let entries = match std::fs::read_dir("/proc") {
            Ok(entries) => entries,
            Err(_) => return processes,
        };

        for entry in entries.flatten() {
            let name = entry.file_name();
            let pid = match name.to_string_lossy().parse::<u32>() {
                Ok(pid) => pid,
                Err(_) => continue,
            };
            let uid = match read_proc_uid(pid) {
                Some(uid) => uid,
                None => continue,
            };
            let user = self.resolve_user(uid);
            let info = match read_proc_session_info(pid) {
                Some(info) => info,
                None => continue,
            };
            let tty = match tty_names.get(&info.tty_nr) {
                Some(tty) => tty,
                None => continue,
            };
            processes.push(SessionProcess {
                user,
                tty: tty.clone(),
                comm: info.comm,
                started_at: boot_time.saturating_add(info.start_ticks / clk),
                start_ticks: info.start_ticks,
            });
        }
        processes
    }

    fn read_last_login_cached(&mut self) -> Vec<LastLogin> {
        let metadata = match std::fs::metadata("/var/log/lastlog") {
            Ok(metadata) => metadata,
            Err(_) => {
                self.lastlog_mtime = None;
                self.lastlog_cache.clear();
                return Vec::new();
            }
        };
        let modified = metadata.modified().ok();
        if modified == self.lastlog_mtime {
            return self.lastlog_cache.clone();
        }
        self.lastlog_cache = read_lastlog_file(metadata.len());
        self.lastlog_mtime = modified;
        self.lastlog_cache.clone()
    }

    /// Run `ss`, parse it, compute per-connection rates and store the sorted
    /// result in `self.last`. Errors are swallowed at the worker boundary: on
    /// failure `available` is set to `false` and the list is cleared.
    fn sample(&mut self) -> io::Result<()> {
        let now = Instant::now();
        if self
            .last_users_refresh
            .is_none_or(|last| now.duration_since(last) >= std::time::Duration::from_secs(2))
        {
            self.refresh_users();
            self.last_users_refresh = Some(now);
        }
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

enum WorkerCommand {
    Sample(u64),
    Reset,
    Shutdown,
}

struct WorkerSnapshot {
    generation: u64,
    available: bool,
    last: Vec<ConnStat>,
    procs: HashMap<u32, ProcInfo>,
    users: Vec<UserRow>,
}

/// UI-facing connection state. Expensive `ss` and `/proc/<pid>` reads happen in
/// a dedicated worker; this object only owns the latest immutable snapshot and
/// the interaction state used by the renderer.
pub struct ConnMonitor {
    pub scroll: usize,
    pub last: Vec<ConnStat>,
    pub users: Vec<UserRow>,
    pub available: bool,
    procs: HashMap<u32, ProcInfo>,
    last_max_rows: usize,
    command_tx: mpsc::Sender<WorkerCommand>,
    snapshot_rx: mpsc::Receiver<WorkerSnapshot>,
    generation: u64,
    sample_pending: bool,
}

impl ConnMonitor {
    pub fn new() -> Self {
        let (command_tx, command_rx) = mpsc::channel();
        let (snapshot_tx, snapshot_rx) = mpsc::channel();
        thread::spawn(move || {
            let mut sampler = ConnSampler::new();
            while let Ok(command) = command_rx.recv() {
                match command {
                    WorkerCommand::Sample(generation) => {
                        let result = sampler.sample();
                        let (available, last, procs, users) = match result {
                            Ok(()) => (
                                sampler.available,
                                sampler.last.clone(),
                                sampler.procs.clone(),
                                sampler.users.clone(),
                            ),
                            Err(_) => (false, Vec::new(), HashMap::new(), sampler.users.clone()),
                        };
                        if snapshot_tx
                            .send(WorkerSnapshot {
                                generation,
                                available,
                                last,
                                procs,
                                users,
                            })
                            .is_err()
                        {
                            break;
                        }
                    }
                    WorkerCommand::Reset => {
                        sampler.reset();
                    }
                    WorkerCommand::Shutdown => break,
                }
            }
        });
        Self {
            scroll: 0,
            last: Vec::new(),
            users: Vec::new(),
            available: true,
            procs: HashMap::new(),
            last_max_rows: 10,
            command_tx,
            snapshot_rx,
            generation: 0,
            sample_pending: false,
        }
    }

    /// Submit a sampling request without waiting for `ss` or `/proc` reads.
    /// Completed snapshots are applied before and after submission.
    pub fn sample(&mut self) -> io::Result<()> {
        self.poll_worker();
        if !self.sample_pending {
            self.command_tx
                .send(WorkerCommand::Sample(self.generation))
                .map_err(|_| {
                    io::Error::new(io::ErrorKind::BrokenPipe, "connection worker stopped")
                })?;
            self.sample_pending = true;
        }
        self.poll_worker();
        Ok(())
    }

    fn poll_worker(&mut self) {
        while let Ok(snapshot) = self.snapshot_rx.try_recv() {
            if snapshot.generation != self.generation {
                continue;
            }
            self.sample_pending = false;
            self.available = snapshot.available;
            self.last = snapshot.last;
            self.procs = snapshot.procs;
            self.users = snapshot.users;
        }
    }

    /// Clear history, rate baselines, and scroll position in both UI and worker
    /// state. Generation IDs discard any snapshot produced before the reset.
    pub fn reset(&mut self) {
        self.generation = self.generation.wrapping_add(1);
        let _ = self.command_tx.send(WorkerCommand::Reset);
        self.sample_pending = false;
        self.procs.clear();
        self.users.clear();
        self.scroll = 0;
        self.last.clear();
        self.available = true;
    }

    pub fn scroll_up(&mut self) {
        if self.scroll > 0 {
            self.scroll -= 1;
        }
    }

    pub fn scroll_down(&mut self) {
        self.scroll += 1;
    }

    pub fn scroll_page_up(&mut self) {
        let step = self.last_max_rows.max(1);
        self.scroll = self.scroll.saturating_sub(step);
    }

    pub fn scroll_page_down(&mut self) {
        let step = self.last_max_rows.max(1);
        self.scroll = self.scroll.saturating_add(step);
    }

    pub fn scroll_top(&mut self) {
        self.scroll = 0;
    }

    pub fn scroll_bottom(&mut self) {
        self.scroll = usize::MAX;
    }

    pub fn clamp_scroll(&mut self, view_len: usize, max_rows: usize) {
        self.last_max_rows = max_rows;
        let max_scroll = view_len.saturating_sub(max_rows);
        if self.scroll > max_scroll {
            self.scroll = max_scroll;
        }
    }
}

impl Drop for ConnMonitor {
    fn drop(&mut self) {
        let _ = self.command_tx.send(WorkerCommand::Shutdown);
    }
}

/// Collapse utmp sessions by username and retain the newest login and process
/// timestamps. Processes from terminals that are not in `sessions` are ignored.
fn aggregate_user_rows(sessions: &[LoginSession], processes: &[SessionProcess]) -> Vec<UserRow> {
    let mut rows: HashMap<String, UserRow> = HashMap::new();
    let active: HashSet<(String, String)> = sessions
        .iter()
        .map(|s| (s.user.clone(), s.tty.clone()))
        .collect();

    for session in sessions {
        let row = rows.entry(session.user.clone()).or_insert_with(|| UserRow {
            user: session.user.clone(),
            online: true,
            sessions: 0,
            last_login: 0,
            last_process: None,
        });
        row.sessions += 1;
        row.last_login = row.last_login.max(session.login_time);
    }
    for process in processes {
        if !active.contains(&(process.user.clone(), process.tty.clone())) {
            continue;
        }
        if let Some(row) = rows.get_mut(&process.user) {
            let replace = row
                .last_process
                .as_ref()
                .is_none_or(|old| process.start_ticks > old.start_ticks);
            if replace {
                row.last_process = Some(process.clone());
            }
        }
    }

    let mut rows: Vec<UserRow> = rows.into_values().collect();
    sort_user_rows(&mut rows);
    rows
}

fn sort_user_rows(rows: &mut [UserRow]) {
    rows.sort_by(|a, b| {
        b.online
            .cmp(&a.online)
            .then_with(|| b.last_login.cmp(&a.last_login))
    });
}

/// Infer one session per `(user, pts/N)` from currently running terminal
/// processes. The earliest process on a terminal approximates the session's
/// login time when utmp is unavailable.
fn infer_login_sessions(processes: &[SessionProcess]) -> Vec<LoginSession> {
    let mut first_start: HashMap<(String, String), u64> = HashMap::new();
    for process in processes {
        let key = (process.user.clone(), process.tty.clone());
        let entry = first_start.entry(key).or_insert(process.started_at);
        *entry = (*entry).min(process.started_at);
    }
    let mut sessions: Vec<LoginSession> = first_start
        .into_iter()
        .map(|((user, tty), login_time)| LoginSession {
            user,
            tty,
            login_time,
        })
        .collect();
    sessions.sort_by(|a, b| a.user.cmp(&b.user).then(a.tty.cmp(&b.tty)));
    sessions
}

fn merge_login_sessions(
    mut sessions: Vec<LoginSession>,
    inferred: Vec<LoginSession>,
) -> Vec<LoginSession> {
    for candidate in inferred {
        if !sessions
            .iter()
            .any(|session| session.user == candidate.user && session.tty == candidate.tty)
        {
            sessions.push(candidate);
        }
    }
    sessions.sort_by(|a, b| a.user.cmp(&b.user).then(a.tty.cmp(&b.tty)));
    sessions
}

/// Read active USER_PROCESS records from the system utmp database.
fn read_login_sessions() -> Vec<LoginSession> {
    let bytes = std::fs::read("/run/utmp")
        .or_else(|_| std::fs::read("/var/run/utmp"))
        .unwrap_or_default();
    parse_utmp_bytes(&bytes)
}

const UTMP_GLIBC_RECORD_SIZE: usize = 384;
const UTMP_MUSL_RECORD_SIZE: usize = 400;
const UTMP_TYPE_OFFSET: usize = 0;
const UTMP_LINE_OFFSET: usize = 8;
const UTMP_USER_OFFSET: usize = 44;
const UTMP_LINE_SIZE: usize = 32;
const UTMP_USER_SIZE: usize = 32;
const UTMP_GLIBC_TIME_OFFSET: usize = 340;
const UTMP_MUSL_TIME_OFFSET: usize = 344;

/// Parse Linux utmp records without calling libc's utmp functions. The latter
/// are stubs in musl and are deprecated by the libc crate for the musl target.
/// The file ABI is parsed explicitly because musl's in-memory `utmpx` layout
/// is not identical to the glibc layout used by many login programs.
fn parse_utmp_bytes(bytes: &[u8]) -> Vec<LoginSession> {
    let record_size = match detect_utmp_record_size(bytes) {
        Some(size) => size,
        None => return Vec::new(),
    };
    bytes
        .chunks_exact(record_size)
        .filter_map(|chunk| {
            let ut_type = u16::from_ne_bytes(
                chunk[UTMP_TYPE_OFFSET..UTMP_TYPE_OFFSET + 2]
                    .try_into()
                    .ok()?,
            );
            if ut_type != 7 {
                return None;
            }
            let user = bytes_to_string(&chunk[UTMP_USER_OFFSET..UTMP_USER_OFFSET + UTMP_USER_SIZE]);
            let tty = bytes_to_string(&chunk[UTMP_LINE_OFFSET..UTMP_LINE_OFFSET + UTMP_LINE_SIZE]);
            if user.is_empty() || tty.is_empty() {
                return None;
            }
            let time_offset = if record_size == UTMP_MUSL_RECORD_SIZE {
                UTMP_MUSL_TIME_OFFSET
            } else {
                UTMP_GLIBC_TIME_OFFSET
            };
            let login_time = if record_size == UTMP_MUSL_RECORD_SIZE {
                u64::from_ne_bytes(chunk[time_offset..time_offset + 8].try_into().ok()?)
            } else {
                u32::from_ne_bytes(chunk[time_offset..time_offset + 4].try_into().ok()?) as u64
            };
            Some(LoginSession {
                user,
                tty,
                login_time,
            })
        })
        .collect()
}

fn detect_utmp_record_size(bytes: &[u8]) -> Option<usize> {
    let mut best: Option<(usize, u32)> = None;
    for record_size in [UTMP_GLIBC_RECORD_SIZE, UTMP_MUSL_RECORD_SIZE] {
        if !bytes.len().is_multiple_of(record_size) {
            continue;
        }
        let score = bytes
            .chunks_exact(record_size)
            .map(|record| {
                if u16::from_ne_bytes(record[..2].try_into().unwrap()) != 7 {
                    return 0;
                }
                let user =
                    bytes_to_string(&record[UTMP_USER_OFFSET..UTMP_USER_OFFSET + UTMP_USER_SIZE]);
                let tty =
                    bytes_to_string(&record[UTMP_LINE_OFFSET..UTMP_LINE_OFFSET + UTMP_LINE_SIZE]);
                if user.is_empty() || tty.is_empty() {
                    return 0;
                }
                let time_offset = if record_size == UTMP_MUSL_RECORD_SIZE {
                    UTMP_MUSL_TIME_OFFSET
                } else {
                    UTMP_GLIBC_TIME_OFFSET
                };
                let timestamp = if record_size == UTMP_MUSL_RECORD_SIZE {
                    u64::from_ne_bytes(record[time_offset..time_offset + 8].try_into().unwrap())
                } else {
                    u32::from_ne_bytes(record[time_offset..time_offset + 4].try_into().unwrap())
                        as u64
                };
                1 + plausible_lastlog_time(timestamp) as u32
            })
            .sum();
        if best.is_none_or(|(_, best_score)| score > best_score) {
            best = Some((record_size, score));
        }
    }
    best.map(|(record_size, _)| record_size)
}

fn read_lastlog_file(file_len: u64) -> Vec<LastLogin> {
    let mut file = match std::fs::File::open("/var/log/lastlog") {
        Ok(file) => file,
        Err(_) => return Vec::new(),
    };
    let record_size = match detect_lastlog_record_size(&mut file, file_len) {
        Some(size) => size,
        None => return Vec::new(),
    };
    if file.seek(SeekFrom::Start(0)).is_err() {
        return Vec::new();
    }

    let mut record = vec![0u8; record_size];
    let mut entries = Vec::new();
    for uid in 0..(file_len / record_size as u64) {
        if file.read_exact(&mut record).is_err() {
            break;
        }
        let login_time = lastlog_time(&record, record_size);
        if login_time > 0 {
            entries.push(LastLogin {
                uid: uid as u32,
                login_time,
            });
        }
    }
    entries
}

/// Parse Linux lastlog records. glibc commonly uses a 4-byte time field
/// (292-byte records on x86_64), while musl uses an 8-byte time field
/// (296-byte records). The sparse file size identifies the active layout.
#[cfg(test)]
fn parse_lastlog_bytes(bytes: &[u8]) -> Vec<LastLogin> {
    let record_size = match detect_lastlog_record_size_from_bytes(bytes) {
        Some(size) => size,
        None => return Vec::new(),
    };
    let mut entries = Vec::new();
    for (uid, record) in bytes.chunks_exact(record_size).enumerate() {
        let login_time = lastlog_time(record, record_size);
        if login_time > 0 {
            entries.push(LastLogin {
                uid: uid as u32,
                login_time,
            });
        }
    }
    entries
}

fn lastlog_time(record: &[u8], record_size: usize) -> u64 {
    if record_size == 296 {
        u64::from_ne_bytes(record[..8].try_into().unwrap())
    } else {
        u32::from_ne_bytes(record[..4].try_into().unwrap()) as u64
    }
}

fn plausible_lastlog_time(timestamp: u64) -> bool {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_secs())
        .unwrap_or(u64::MAX);
    timestamp > 0 && timestamp <= now.saturating_add(86_400)
}

#[cfg(test)]
fn detect_lastlog_record_size_from_bytes(bytes: &[u8]) -> Option<usize> {
    let candidates: Vec<usize> = [292, 296]
        .into_iter()
        .filter(|size| bytes.len().is_multiple_of(*size))
        .collect();
    let mut best: Option<(usize, u8)> = None;
    for size in candidates {
        let score = bytes
            .chunks_exact(size)
            .take(4096)
            .map(|record| plausible_lastlog_time(lastlog_time(record, size)) as u8)
            .sum::<u8>();
        // Preserve the candidate order on ties: 292 is the common glibc file
        // layout, and an empty/ambiguous file has no timestamp to recover.
        if best.is_none_or(|(_, best_score)| score > best_score) {
            best = Some((size, score));
        }
    }
    best.map(|(size, _)| size)
}

fn detect_lastlog_record_size(file: &mut std::fs::File, file_len: u64) -> Option<usize> {
    let candidates: Vec<usize> = [292, 296]
        .into_iter()
        .filter(|size| file_len.is_multiple_of(*size as u64))
        .collect();
    let mut best: Option<(usize, u32)> = None;
    for size in candidates {
        let mut score = 0u32;
        let sample_count = (file_len / size as u64).min(4096);
        for uid in 0..sample_count {
            if file
                .seek(SeekFrom::Start(uid * size as u64))
                .and_then(|_| {
                    let mut head = [0u8; 8];
                    file.read_exact(&mut head).map(|_| head)
                })
                .map(|head| {
                    let timestamp = lastlog_time(&head, size);
                    if plausible_lastlog_time(timestamp) {
                        score += 1;
                    }
                })
                .is_err()
            {
                break;
            }
        }
        // Preserve the candidate order on ties; see the byte-slice detector.
        if best.is_none_or(|(_, best_score)| score > best_score) {
            best = Some((size, score));
        }
    }
    best.map(|(size, _)| size)
}

fn bytes_to_string(value: &[u8]) -> String {
    let bytes: Vec<u8> = value.iter().copied().take_while(|&c| c != 0).collect();
    String::from_utf8_lossy(&bytes).into_owned()
}

fn terminal_device_names() -> HashMap<u64, String> {
    let mut names = HashMap::new();
    let entries = match std::fs::read_dir("/dev/pts") {
        Ok(entries) => entries,
        Err(_) => return names,
    };
    for entry in entries.flatten() {
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if name.parse::<u32>().is_err() {
            continue;
        }
        if let Ok(metadata) = entry.metadata() {
            names.insert(metadata.rdev(), format!("pts/{name}"));
        }
    }
    names
}

struct ProcSessionInfo {
    comm: String,
    tty_nr: u64,
    start_ticks: u64,
}

/// Read the controlling terminal and process start time from `/proc/<pid>/stat`.
fn read_proc_session_info(pid: u32) -> Option<ProcSessionInfo> {
    let content = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    let left = content.find('(')?;
    let right = content.rfind(')')?;
    if right <= left {
        return None;
    }
    let comm = content[left + 1..right].to_string();
    let fields: Vec<&str> = content[right + 1..].split_whitespace().collect();
    if fields.len() <= 19 {
        return None;
    }
    let tty_nr = fields[4].parse::<i64>().ok()?.max(0) as u64;
    let start_ticks = fields[19].parse::<u64>().ok()?;
    Some(ProcSessionInfo {
        comm,
        tty_nr,
        start_ticks,
    })
}

fn read_proc_uid(pid: u32) -> Option<u32> {
    let content = std::fs::read_to_string(format!("/proc/{pid}/status")).ok()?;
    content
        .lines()
        .find(|line| line.starts_with("Uid:"))
        .and_then(|line| line.split_whitespace().nth(1))
        .and_then(|value| value.parse::<u32>().ok())
}

fn read_boot_time() -> Option<u64> {
    let content = std::fs::read_to_string("/proc/stat").ok()?;
    content
        .lines()
        .find(|line| line.starts_with("btime "))
        .and_then(|line| line.split_whitespace().nth(1))
        .and_then(|value| value.parse::<u64>().ok())
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
    let pid = after_open[comma..].find("pid=").and_then(|p| {
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
                if let Some(v) = line
                    .split_whitespace()
                    .nth(1)
                    .and_then(|x| x.parse::<u64>().ok())
                {
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
            uid = line
                .split_whitespace()
                .nth(1)
                .and_then(|v| v.parse::<u32>().ok());
        } else if line.starts_with("VmRSS:") {
            vmrss = line
                .split_whitespace()
                .nth(1)
                .and_then(|v| v.parse::<u64>().ok());
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
            rb = line
                .split_whitespace()
                .nth(1)
                .and_then(|v| v.parse::<u64>().ok());
        } else if wb.is_none() && line.starts_with("write_bytes:") {
            wb = line
                .split_whitespace()
                .nth(1)
                .and_then(|v| v.parse::<u64>().ok());
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
                (
                    ss,
                    std::mem::size_of::<libc::sockaddr_in>() as libc::socklen_t,
                )
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
                (
                    ss,
                    std::mem::size_of::<libc::sockaddr_in6>() as libc::socklen_t,
                )
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

impl ConnSampler {
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
            .filter(|c| self.conn_matches(c, filter))
            .cloned()
            .collect()
    }

    /// Connections grouped by `(comm, pid)`, with summed RX/TX rates and a
    /// connection count, sorted by total throughput (descending).
    pub fn aggregate_view(&self, filter: &str) -> Vec<AggRow> {
        use std::collections::HashMap;
        let mut map: HashMap<(String, Option<u32>), AggRow> = HashMap::new();
        for c in self.last.iter().filter(|c| self.conn_matches(c, filter)) {
            let entry = map.entry((c.comm.clone(), c.pid)).or_insert_with(|| {
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
/// filter matches everything. Matches against the process name, pid, local/remote
/// addresses, resolved host/service names, and the connection owner's username
/// (resolved from `self.procs`, which honours NSS via `getent`).
impl ConnMonitor {
    fn conn_matches(&self, c: &ConnStat, filter: &str) -> bool {
        if filter.is_empty() {
            return true;
        }
        let f = filter.to_ascii_lowercase();
        let pid_s = c.pid.map(|p| p.to_string()).unwrap_or_default();
        let mut fields: Vec<&str> = vec![
            c.comm.as_str(),
            pid_s.as_str(),
            c.local.as_str(),
            c.remote.as_str(),
            c.host.as_deref().unwrap_or(""),
            c.service.as_deref().unwrap_or(""),
        ];
        // Include the owner's username (if known) so filtering by e.g. "root" or any
        // NSS-resolved account name works in both the aggregate and detail views.
        if let Some(user) = c
            .pid
            .and_then(|p| self.procs.get(&p))
            .map(|p| p.user.as_str())
        {
            fields.push(user);
        }
        fields.iter().any(|s| s.to_ascii_lowercase().contains(&f))
    }
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
        assert_eq!(
            m.get(&(443, "tcp".to_string())).map(|s| s.as_str()),
            Some("https")
        );
        assert_eq!(
            m.get(&(53, "udp".to_string())).map(|s| s.as_str()),
            Some("domain")
        );
    }

    #[test]
    fn parse_users_finds_first_owner() {
        let (comm, pid) = parse_users(r#"users:(("node-MainThread",pid=26951,fd=24))"#);
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
    fn filter_matches_username_in_aggregate_and_detail() {
        let mut cm = ConnMonitor::new();
        // A process owned by "root" (simulating an NSS/getent-resolved name).
        cm.procs.insert(
            1234,
            ProcInfo {
                user: "root".into(),
                cpu_pct: 0.0,
                mem_pct: 0.0,
                time_secs: 0.0,
                disk_read_rate: 0.0,
                disk_write_rate: 0.0,
            },
        );
        cm.last.push(ConnStat {
            proto: "tcp".into(),
            local: "1.2.3.4:5".into(),
            remote: "6.7.8.9:0".into(),
            comm: "sshd".into(),
            pid: Some(1234),
            rx_rate: Some(1.0),
            tx_rate: Some(1.0),
            host: None,
            service: None,
        });

        // Filtering by the owner's username must surface the connection.
        let agg = cm.aggregate_view("root");
        assert_eq!(agg.len(), 1);
        assert_eq!(agg[0].user, "root");
        assert_eq!(cm.detail_view("root").len(), 1);

        // A non-matching username yields nothing.
        assert!(cm.aggregate_view("nobody").is_empty());
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
            ConnStat {
                proto: "tcp".into(),
                local: "1.1.1.1:1".into(),
                remote: "2.2.2.2:2".into(),
                comm: "app".into(),
                pid: Some(1),
                rx_rate: Some(100.0),
                tx_rate: Some(50.0),
                host: None,
                service: None,
            },
            ConnStat {
                proto: "tcp".into(),
                local: "1.1.1.1:3".into(),
                remote: "2.2.2.2:4".into(),
                comm: "app".into(),
                pid: Some(1),
                rx_rate: Some(200.0),
                tx_rate: Some(0.0),
                host: None,
                service: None,
            },
            ConnStat {
                proto: "udp".into(),
                local: "1.1.1.1:5".into(),
                remote: "3.3.3.3:6".into(),
                comm: "other".into(),
                pid: Some(2),
                rx_rate: None,
                tx_rate: None,
                host: None,
                service: None,
            },
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
    fn aggregates_same_user_by_latest_login_and_process() {
        let sessions = vec![
            LoginSession {
                user: "dj".into(),
                tty: "pts/0".into(),
                login_time: 100,
            },
            LoginSession {
                user: "dj".into(),
                tty: "pts/2".into(),
                login_time: 200,
            },
        ];
        let processes = vec![
            SessionProcess {
                user: "dj".into(),
                tty: "pts/0".into(),
                comm: "bash".into(),
                started_at: 150,
                start_ticks: 10,
            },
            SessionProcess {
                user: "dj".into(),
                tty: "pts/2".into(),
                comm: "vim".into(),
                started_at: 250,
                start_ticks: 20,
            },
        ];

        let rows = aggregate_user_rows(&sessions, &processes);

        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].user, "dj");
        assert_eq!(rows[0].sessions, 2);
        assert_eq!(rows[0].last_login, 200);
        assert_eq!(rows[0].last_process.as_ref().unwrap().comm, "vim");
        assert_eq!(rows[0].last_process.as_ref().unwrap().started_at, 250);
    }

    #[test]
    fn sorts_online_users_before_offline_then_by_latest_login() {
        let mut rows = vec![
            UserRow {
                user: "bob".into(),
                online: false,
                sessions: 0,
                last_login: 100,
                last_process: None,
            },
            UserRow {
                user: "alice".into(),
                online: true,
                sessions: 1,
                last_login: 200,
                last_process: None,
            },
            UserRow {
                user: "zoe".into(),
                online: true,
                sessions: 1,
                last_login: 300,
                last_process: None,
            },
            UserRow {
                user: "aaron".into(),
                online: false,
                sessions: 0,
                last_login: 400,
                last_process: None,
            },
        ];

        sort_user_rows(&mut rows);

        assert_eq!(
            rows.iter().map(|row| row.user.as_str()).collect::<Vec<_>>(),
            ["zoe", "alice", "aaron", "bob"]
        );
    }

    fn utmp_fixture(record_size: usize, time_offset: usize, timestamp: u64) -> Vec<u8> {
        let mut record = vec![0u8; record_size];
        record[..2].copy_from_slice(&7u16.to_ne_bytes());
        record[UTMP_LINE_OFFSET..UTMP_LINE_OFFSET + 5].copy_from_slice(b"pts/7");
        record[UTMP_USER_OFFSET..UTMP_USER_OFFSET + 2].copy_from_slice(b"dj");
        if record_size == UTMP_MUSL_RECORD_SIZE {
            record[time_offset..time_offset + 8].copy_from_slice(&timestamp.to_ne_bytes());
        } else {
            record[time_offset..time_offset + 4].copy_from_slice(&(timestamp as u32).to_ne_bytes());
        }
        record
    }

    #[test]
    fn parses_glibc_utmp_file_layout() {
        let bytes = utmp_fixture(UTMP_GLIBC_RECORD_SIZE, UTMP_GLIBC_TIME_OFFSET, 1234);
        let sessions = parse_utmp_bytes(&bytes);

        assert_eq!(sessions.len(), 1);
        assert_eq!(sessions[0].user, "dj");
        assert_eq!(sessions[0].tty, "pts/7");
        assert_eq!(sessions[0].login_time, 1234);
    }

    #[test]
    fn parses_musl_utmp_file_layout() {
        let bytes = utmp_fixture(UTMP_MUSL_RECORD_SIZE, UTMP_MUSL_TIME_OFFSET, 1234);
        let sessions = parse_utmp_bytes(&bytes);

        assert_eq!(sessions.len(), 1);
        assert_eq!(sessions[0].user, "dj");
        assert_eq!(sessions[0].tty, "pts/7");
        assert_eq!(sessions[0].login_time, 1234);
    }

    #[test]
    fn parses_lastlog_records_with_32_bit_time() {
        let record_size = 292;
        let mut bytes = vec![0u8; record_size * 2];
        bytes[record_size..record_size + 4].copy_from_slice(&1234u32.to_ne_bytes());

        let entries = parse_lastlog_bytes(&bytes);

        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].uid, 1);
        assert_eq!(entries[0].login_time, 1234);
    }

    #[test]
    fn detects_ambiguous_lastlog_size_from_record_content() {
        let record_size = 292;
        let mut bytes = vec![0u8; record_size * 74];
        bytes[record_size..record_size + 4].copy_from_slice(&1_609_459_200u32.to_ne_bytes());

        let entries = parse_lastlog_bytes(&bytes);

        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].uid, 1);
        assert_eq!(entries[0].login_time, 1_609_459_200);
    }

    #[test]
    fn parses_lastlog_records_with_64_bit_time() {
        let mut bytes = vec![0u8; 296];
        bytes[..8].copy_from_slice(&u64::MAX.to_ne_bytes());

        let entries = parse_lastlog_bytes(&bytes);

        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].uid, 0);
        assert_eq!(entries[0].login_time, u64::MAX);
    }

    #[test]
    fn infers_online_sessions_from_terminal_processes() {
        let processes = vec![
            SessionProcess {
                user: "dj".into(),
                tty: "pts/1".into(),
                comm: "bash".into(),
                started_at: 100,
                start_ticks: 10,
            },
            SessionProcess {
                user: "dj".into(),
                tty: "pts/1".into(),
                comm: "vim".into(),
                started_at: 200,
                start_ticks: 20,
            },
        ];

        let sessions = infer_login_sessions(&processes);

        assert_eq!(sessions.len(), 1);
        assert_eq!(sessions[0].user, "dj");
        assert_eq!(sessions[0].tty, "pts/1");
        assert_eq!(sessions[0].login_time, 100);
    }

    #[test]
    fn merges_inferred_sessions_with_partial_utmp() {
        let utmp = vec![LoginSession {
            user: "dj".into(),
            tty: "pts/1".into(),
            login_time: 100,
        }];
        let inferred = vec![
            LoginSession {
                user: "dj".into(),
                tty: "pts/1".into(),
                login_time: 100,
            },
            LoginSession {
                user: "dj".into(),
                tty: "pts/2".into(),
                login_time: 200,
            },
        ];

        let merged = merge_login_sessions(utmp, inferred);

        assert_eq!(merged.len(), 2);
        assert!(merged.iter().any(|s| s.tty == "pts/2"));
    }

    #[test]
    fn chooses_latest_process_by_ticks_when_wall_seconds_match() {
        let sessions = vec![LoginSession {
            user: "dj".into(),
            tty: "pts/1".into(),
            login_time: 100,
        }];
        let processes = vec![
            SessionProcess {
                user: "dj".into(),
                tty: "pts/1".into(),
                comm: "old".into(),
                started_at: 300,
                start_ticks: 10,
            },
            SessionProcess {
                user: "dj".into(),
                tty: "pts/1".into(),
                comm: "new".into(),
                started_at: 300,
                start_ticks: 20,
            },
        ];

        let rows = aggregate_user_rows(&sessions, &processes);

        assert_eq!(rows[0].last_process.as_ref().unwrap().comm, "new");
    }

    #[test]
    fn detail_view_filters_by_substring() {
        let mut m = ConnMonitor::new();
        m.last = vec![
            ConnStat {
                proto: "tcp".into(),
                local: "1.1.1.1:1".into(),
                remote: "2.2.2.2:2".into(),
                comm: "chrome".into(),
                pid: Some(1),
                rx_rate: None,
                tx_rate: None,
                host: None,
                service: None,
            },
            ConnStat {
                proto: "tcp".into(),
                local: "1.1.1.1:3".into(),
                remote: "2.2.2.2:4".into(),
                comm: "ssh".into(),
                pid: Some(2),
                rx_rate: None,
                tx_rate: None,
                host: None,
                service: None,
            },
        ];
        let f = m.detail_view("ssh");
        assert_eq!(f.len(), 1);
        assert_eq!(f[0].comm, "ssh");
        assert_eq!(m.detail_view("nomatch").len(), 0);
    }

    #[test]
    fn reset_allows_connection_monitor_recovery() {
        let mut m = ConnMonitor::new();
        m.available = false;
        m.reset();
        assert!(m.available);
    }
}
