mod conns;
use conns::{AggRow, ConnMonitor, ConnStat};

use std::collections::VecDeque;
use std::fs;
use std::io;
use std::time::{Duration, Instant};

use crossterm::{
    event::{self, Event, KeyCode},
    execute,
    terminal::{disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen},
};
use ratatui::{
    backend::{Backend, CrosstermBackend},
    layout::{Constraint, Direction, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Cell, Gauge, Paragraph, Row, Sparkline, Table},
    Frame, Terminal,
};

// ── Constants ────────────────────────────────────────────────────────────────

/// Maximum number of data points stored in history (5 minutes at 1s interval).
const MAX_HISTORY: usize = 300;
/// How many data points to display in the sparkline waveform.
const SPARKLINE_POINTS: usize = 120;
/// Refresh interval in milliseconds.
const TICK_MS: u64 = 1000;
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
}

impl App {
    fn new(iface: String, ifaces: Vec<String>, iface_idx: usize) -> Self {
        Self {
            iface,
            ifaces,
            iface_idx,
            rx_history: VecDeque::with_capacity(MAX_HISTORY),
            tx_history: VecDeque::with_capacity(MAX_HISTORY),
            current_rx: 0,
            current_tx: 0,
            peak_speed: 1, // avoid divide-by-zero
            prev_rx_bytes: 0,
            prev_tx_bytes: 0,
            last_sample: Instant::now(),
            conns: ConnMonitor::new(),
            focus_conns: false,
            aggregate: false,
            filter_mode: false,
            filter: String::new(),
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

    /// Take a sample: read counters, compute delta, push to history.
    fn tick(&mut self) -> io::Result<()> {
        let (rx_bytes, tx_bytes) = self.read_counters()?;
        let now = Instant::now();
        let elapsed = now.duration_since(self.last_sample).as_secs_f64();

        if elapsed > 0.0 && self.prev_rx_bytes > 0 {
            // Compute bytes/s (handle counter wrap with saturating sub).
            let rx_delta = rx_bytes.saturating_sub(self.prev_rx_bytes);
            let tx_delta = tx_bytes.saturating_sub(self.prev_tx_bytes);

            self.current_rx = (rx_delta as f64 / elapsed) as u64;
            self.current_tx = (tx_delta as f64 / elapsed) as u64;
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

        self.prev_rx_bytes = rx_bytes;
        self.prev_tx_bytes = tx_bytes;
        self.last_sample = now;

        // Sample per-connection throughput (non-fatal on failure).
        let _ = self.conns.sample();

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
        self.current_rx = 0;
        self.current_tx = 0;
        self.peak_speed = 1;

        // Re-baseline counters immediately; skip the delta on the next tick.
        if let Ok((rx, tx)) = self.read_counters() {
            self.prev_rx_bytes = rx;
            self.prev_tx_bytes = tx;
        } else {
            self.prev_rx_bytes = 0;
            self.prev_tx_bytes = 0;
        }
        self.last_sample = Instant::now();
    }

    /// Return the displayed max for sparkline scaling (rolling window max + 10% headroom).
    fn sparkline_max(&self, history: &VecDeque<u64>) -> u64 {
        let window: Vec<&u64> = history
            .iter()
            .rev()
            .take(SPARKLINE_POINTS)
            .collect();
        let max = window.iter().fold(1u64, |acc, &&v| acc.max(v));
        (max as f64 * 1.1) as u64 + 1
    }
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
        format!("{:>6.0} {}", value, UNITS[unit_idx])
    } else {
        format!("{:>6.1} {}", value, UNITS[unit_idx])
    }
}

/// Render a titled sparkline block for a history buffer.
fn render_sparkline(
    f: &mut Frame,
    area: Rect,
    title: &str,
    history: &VecDeque<u64>,
    max: u64,
    color: Color,
) {
    let data: Vec<u64> = history
        .iter()
        .rev()
        .take(SPARKLINE_POINTS)
        .rev()
        .copied()
        .collect();

    let sparkline = Sparkline::default()
        .block(
            Block::default()
                .borders(Borders::ALL)
                .title(Span::styled(title, Style::default().fg(color).add_modifier(Modifier::BOLD))),
        )
        .data(&data)
        .max(max)
        .style(Style::default().fg(color));

    f.render_widget(sparkline, area);
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
                Constraint::Percentage(60), // top: gauges + waveforms
                Constraint::Percentage(40), // bottom: top connections
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
            Constraint::Length(3),  // title
            Constraint::Length(6),  // gauges
            Constraint::Min(3),     // download waveform
            Constraint::Min(3),     // upload waveform
            Constraint::Length(1),  // footer
        ])
        .split(area);

    // ── Title bar ────────────────────────────────────────────────────────
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
        Span::styled("r: reset", Style::default().fg(Color::DarkGray)),
        Span::raw("  │  "),
        Span::styled("q: quit", Style::default().fg(Color::DarkGray)),
    ]))
    .block(Block::default().borders(Borders::ALL));
    f.render_widget(title, main_layout[0]);

    // ── Speed gauges ─────────────────────────────────────────────────────
    let gauge_area = main_layout[1];
    let gauge_layout = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Percentage(50), Constraint::Percentage(50)])
        .split(gauge_area);

    // Download gauge.
    let rx_ratio = if app.peak_speed > 0 {
        app.current_rx as f64 / app.peak_speed as f64
    } else {
        0.0
    };
    let rx_gauge = Gauge::default()
        .block(
            Block::default()
                .borders(Borders::ALL)
                .title(Span::styled(
                    " ▼ DOWNLOAD ",
                    Style::default().fg(Color::Green).add_modifier(Modifier::BOLD),
                )),
        )
        .gauge_style(Style::default().fg(Color::Green).bg(Color::DarkGray))
        .ratio(rx_ratio.clamp(0.0, 1.0))
        .label(format!(" {} ", format_speed(app.current_rx)));
    f.render_widget(rx_gauge, gauge_layout[0]);

    // Upload gauge.
    let tx_ratio = if app.peak_speed > 0 {
        app.current_tx as f64 / app.peak_speed as f64
    } else {
        0.0
    };
    let tx_gauge = Gauge::default()
        .block(
            Block::default()
                .borders(Borders::ALL)
                .title(Span::styled(
                    " ▲ UPLOAD ",
                    Style::default().fg(Color::Red).add_modifier(Modifier::BOLD),
                )),
        )
        .gauge_style(Style::default().fg(Color::Red).bg(Color::DarkGray))
        .ratio(tx_ratio.clamp(0.0, 1.0))
        .label(format!(" {} ", format_speed(app.current_tx)));
    f.render_widget(tx_gauge, gauge_layout[1]);

    // ── Waveforms ────────────────────────────────────────────────────────
    let rx_max = app.sparkline_max(&app.rx_history);
    let tx_max = app.sparkline_max(&app.tx_history);

    render_sparkline(
        f,
        main_layout[2],
        &format!(" ▼ Download History ({:.0}s) ", SPARKLINE_POINTS),
        &app.rx_history,
        rx_max,
        Color::Green,
    );

    render_sparkline(
        f,
        main_layout[3],
        &format!(" ▲ Upload History ({:.0}s) ", SPARKLINE_POINTS),
        &app.tx_history,
        tx_max,
        Color::Red,
    );

    // ── Footer ───────────────────────────────────────────────────────────
    let peak_str = format!("Session peak: {}", format_speed(app.peak_speed));
    let footer = Paragraph::new(Line::from(vec![
        Span::styled(peak_str, Style::default().fg(Color::DarkGray)),
        Span::raw("  │  "),
        Span::styled(
            format!("Samples: {}", app.rx_history.len()),
            Style::default().fg(Color::DarkGray),
        ),
    ]));
    f.render_widget(footer, main_layout[4]);
}

/// One row in the connections table, in either detail (per-socket) or
/// aggregated (per-process) form.
enum ConnRow {
    Detail(ConnStat),
    Agg(AggRow),
}

/// Render the Top Connections panel (detail or aggregated view) into `area`.
fn render_conn_panel(f: &mut Frame, area: Rect, app: &mut App) {
    let view: Vec<ConnRow> = if app.aggregate {
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
    title.push_str("↑↓ pg:scroll c:focus a:agg /:filter");

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

    let header = if app.aggregate {
        Row::new(vec!["PROC(PID)", "CONNS", "RX", "TX"])
            .style(Style::default().add_modifier(Modifier::BOLD))
    } else {
        Row::new(vec!["PROC(PID)", "PRO", "SRC", "DST", "HOST", "SVC", "RX", "TX"])
            .style(Style::default().add_modifier(Modifier::BOLD))
    };

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
                        Cell::from(proc),
                        Cell::from(a.count.to_string()),
                        Cell::from(format_speed(a.rx_rate as u64)),
                        Cell::from(format_speed(a.tx_rate as u64)),
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

    let widths: Vec<Constraint> = if app.aggregate {
        vec![
            Constraint::Min(20),
            Constraint::Length(7),
            Constraint::Length(11),
            Constraint::Length(11),
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
    execute!(stdout, EnterAlternateScreen)?;
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend)?;

    // Run the event loop.
    let tick_duration = Duration::from_millis(TICK_MS);
    let res = run_app(&mut terminal, &mut app, tick_duration);

    // Restore terminal.
    disable_raw_mode()?;
    execute!(terminal.backend_mut(), LeaveAlternateScreen)?;

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
            if let Event::Key(key) = event::read()? {
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
                        app.conns.scroll_top();
                    }
                    KeyCode::Char('n') | KeyCode::Char('N') | KeyCode::Tab => {
                        // Next interface (wrap around).
                        app.switch_iface(app.iface_idx + 1);
                    }
                    KeyCode::Char('p') | KeyCode::Char('P') | KeyCode::BackTab => {
                        // Previous interface (wrap around).
                        let len = app.ifaces.len();
                        if len > 0 {
                            app.switch_iface(app.iface_idx + len - 1);
                        }
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
                    _ => {}
                }
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
