//! Terminal rendering and table interaction.

use std::collections::VecDeque;
use std::time::{SystemTime, UNIX_EPOCH};

use crate::conns::{AggRow, ConnStat};
use crate::{
    fmt_time_plus, format_bytes_short, format_speed, shanghai_hms, sparkline_max_f, App,
    MAX_HISTORY, TICK_MS, ZEBRA,
};
use ratatui::{
    layout::{Constraint, Direction, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Cell, Paragraph, Row, Table},
    Frame,
};

/// Draw a smoothed waveform as a continuous braille line.
///
/// Only the columns visible on screen are sampled. The previous implementation
/// first normalised and smoothed the complete 24-hour buffer on every frame,
/// even though the terminal can display only a few hundred columns.
fn draw_waveform<T, F>(
    f: &mut Frame,
    area: Rect,
    history: &VecDeque<T>,
    max: f64,
    color: Color,
    value: F,
) where
    F: Fn(&T) -> f64,
{
    if area.width == 0 || area.height == 0 || history.is_empty() || max <= 0.0 {
        return;
    }

    let width = area.width as usize;
    let height = area.height as usize;
    let xres = width * 2;
    let yres = height * 4;
    let n = history.len();
    let shown = n.min(MAX_HISTORY);
    let span = (shown.saturating_sub(1) as f64).max(1.0);
    let xres_f = (xres.saturating_sub(1) as f64).max(1.0);

    let y_of = |normalised: f64| -> i32 {
        ((1.0 - normalised.clamp(0.0, 1.0)) * (yres.saturating_sub(1) as f64)).round() as i32
    };

    let mut points = Vec::with_capacity(xres);
    for dx in 0..xres {
        let sample_f = dx as f64 * span / xres_f;
        let i0 = sample_f.floor() as usize;
        let i1 = sample_f.ceil() as usize;
        let i = if i1 < shown
            && (i1 as f64 * xres_f / span - dx as f64) < (dx as f64 - i0 as f64 * xres_f / span)
        {
            i1
        } else {
            i0.min(shown.saturating_sub(1))
        };

        let prev = i.saturating_sub(1);
        let next = (i + 1).min(shown.saturating_sub(1));
        let smoothed =
            (value(&history[prev]) + value(&history[i]) + value(&history[next])) / 3.0 / max;
        points.push((dx as i32, y_of(smoothed)));
    }

    let mut dots = vec![false; xres * yres];
    for pair in points.windows(2) {
        let [(x0, y0), (x1, y1)] = pair else {
            continue;
        };
        let steps = (x1 - x0).abs().max(1);
        for step in 0..=steps {
            let t = step as f64 / steps as f64;
            let x = (*x0 as f64 + (*x1 - *x0) as f64 * t).round() as i32;
            let y = (*y0 as f64 + (*y1 - *y0) as f64 * t).round() as i32;
            if x >= 0 && x < xres as i32 && y >= 0 && y < yres as i32 {
                dots[y as usize * xres + x as usize] = true;
            }
        }
    }

    if points.len() == 1 {
        let (x, y) = points[0];
        if x >= 0 && x < xres as i32 && y >= 0 && y < yres as i32 {
            dots[y as usize * xres + x as usize] = true;
        }
    }

    let buf = f.buffer_mut();
    for cy in 0..height {
        for cx in 0..width {
            let mut bits = 0u32;
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
                            _ => unreachable!(),
                        };
                        bits |= 1 << bit;
                    }
                }
            }
            let cell = &mut buf[(area.x + cx as u16, area.y + cy as u16)];
            let symbol = char::from_u32(0x2800 + bits).unwrap_or(' ').to_string();
            cell.set_symbol(&symbol);
            cell.set_style(Style::default().fg(color));
        }
    }
}
/// Render UI.
pub(crate) fn ui(f: &mut Frame, app: &mut App) {
    let area = f.area();
    // The full metric panel needs 39 rows plus a five-row connection panel and
    // the outer margin. Prefer a useful connection view over allowing ratatui
    // to crush the fixed metric layout on short terminals.
    let compact = area.height.saturating_sub(2) < 44;
    if app.focus_conns || compact {
        // Focus mode, or compact-terminal fallback: the connections panel takes
        // the whole screen.
        let panel_area = Layout::default()
            .margin(1)
            .constraints([Constraint::Min(0)])
            .split(area)[0];
        if app.show_users {
            render_user_panel(f, panel_area, app);
        } else {
            render_conn_panel(f, panel_area, app);
        }
    } else {
        let outer = Layout::default()
            .direction(Direction::Vertical)
            .margin(1)
            .constraints([
                Constraint::Length(39), // top: title + stats box (DL/UL/CPU/MEM/SWAP/DISK R/DISK W/DISK SPACE) + footer
                Constraint::Min(5),     // bottom: top connections
            ])
            .split(area);
        render_top(f, outer[0], app);
        if app.show_users {
            render_user_panel(f, outer[1], app);
        } else {
            render_conn_panel(f, outer[1], app);
        }
    }
}

/// Render the top section: interface title, speed gauges and history sparklines.
fn render_top(f: &mut Frame, area: Rect, app: &App) {
    let main_layout = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(3),  // title
            Constraint::Length(35), // stats box: DL/UL/CPU/MEM/SWAP/DISK R/DISK W/DISK SPACE waveforms + time axis
            Constraint::Length(1),  // footer
        ])
        .split(area);

    // ── Title bar (unchanged) ────────────────────────────────────────────
    let title = Paragraph::new(Line::from(vec![
        Span::styled(
            "◉ Network Monitor",
            Style::default()
                .fg(Color::Cyan)
                .add_modifier(Modifier::BOLD),
        ),
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
        Span::styled(
            format!("uptime: {}", crate::format_uptime(app.host_uptime)),
            Style::default().fg(Color::Green),
        ),
        Span::raw("  │  "),
        Span::styled("u: users", Style::default().fg(Color::DarkGray)),
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

    let stats_block = Block::default().borders(Borders::ALL).title(Span::styled(
        format!(
            " ▼ DL / ▲ UL / CPU / MEM / SWAP / DISK R / DISK W / DISK SPACE History ({:.0}h) ",
            window_h
        ),
        Style::default()
            .fg(Color::Cyan)
            .add_modifier(Modifier::BOLD),
    ));
    let stats_inner = stats_block.inner(main_layout[1]);
    f.render_widget(stats_block, main_layout[1]);

    // Eight metric rows (DL, UL, CPU, MEM, SWAP, DISK R, DISK W, DISK SPACE) of
    // *equal* height, plus dividers between them and the time-axis baseline +
    // labels row. The per-metric height is derived from the actual available
    // space so every waveform block stays the same height. (A fixed `Length(3)`
    // per metric would be shrunk unevenly by ratatui when the box is shorter than
    // the sum — it keeps the first and last rows at full height and squeezes the
    // middle ones — which looked inconsistent.)
    let n_metrics = 8;
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
        .constraints([
            Constraint::Length(26),
            Constraint::Length(1),
            Constraint::Min(20),
        ])
        .split(vrows[0]);
    let dl_label = Paragraph::new(Line::from(Span::styled(
        format!("▼ DL {}", format_speed(app.current_rx)),
        Style::default()
            .fg(Color::Green)
            .add_modifier(Modifier::BOLD),
    )));
    f.render_widget(dl_label, dl[0]);
    draw_waveform(
        f,
        dl[2],
        &app.rx_history,
        rx_max as f64,
        Color::Green,
        |value| *value as f64,
    );

    // Upload: [label | vdiv | sparkline].
    let ul = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([
            Constraint::Length(26),
            Constraint::Length(1),
            Constraint::Min(20),
        ])
        .split(vrows[2]);
    let ul_label = Paragraph::new(Line::from(Span::styled(
        format!("▲ UL {}", format_speed(app.current_tx)),
        Style::default().fg(Color::Red).add_modifier(Modifier::BOLD),
    )));
    f.render_widget(ul_label, ul[0]);
    draw_waveform(
        f,
        ul[2],
        &app.tx_history,
        tx_max as f64,
        Color::Red,
        |value| *value as f64,
    );

    // System CPU: [label | vdiv | waveform]. Values are 0..100 %, so the fixed
    // max is simply 100.0 (the waveform fills proportionally to total capacity).
    let cpu = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([
            Constraint::Length(26),
            Constraint::Length(1),
            Constraint::Min(20),
        ])
        .split(vrows[4]);
    let cpu_now = app.sys_cpu_history.back().copied().unwrap_or(0.0);
    let cpu_label = Paragraph::new(Line::from(Span::styled(
        format!("▌ CPU {:.1}%", cpu_now),
        Style::default()
            .fg(Color::Yellow)
            .add_modifier(Modifier::BOLD),
    )));
    f.render_widget(cpu_label, cpu[0]);
    draw_waveform(
        f,
        cpu[2],
        &app.sys_cpu_history,
        100.0,
        Color::Yellow,
        |value| *value,
    );

    // System MEM: [label | vdiv | waveform].
    let mem = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([
            Constraint::Length(26),
            Constraint::Length(1),
            Constraint::Min(20),
        ])
        .split(vrows[6]);
    let mem_now = app.sys_mem_history.back().copied().unwrap_or(0.0);
    let mem_label = Paragraph::new(Line::from(Span::styled(
        format!("▌ MEM {:.1}%", mem_now),
        Style::default()
            .fg(Color::Magenta)
            .add_modifier(Modifier::BOLD),
    )));
    f.render_widget(mem_label, mem[0]);
    draw_waveform(
        f,
        mem[2],
        &app.sys_mem_history,
        100.0,
        Color::Magenta,
        |value| *value,
    );

    // System SWAP: [label | vdiv | waveform]. Values are 0..100 % of total swap.
    let swap = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([
            Constraint::Length(26),
            Constraint::Length(1),
            Constraint::Min(20),
        ])
        .split(vrows[8]);
    let swap_now = app.sys_swap_history.back().copied().unwrap_or(0.0);
    let swap_label = Paragraph::new(Line::from(Span::styled(
        format!("▌ SWAP {:.1}%", swap_now),
        Style::default()
            .fg(Color::White)
            .add_modifier(Modifier::BOLD),
    )));
    f.render_widget(swap_label, swap[0]);
    draw_waveform(
        f,
        swap[2],
        &app.sys_swap_history,
        100.0,
        Color::White,
        |value| *value,
    );

    // System DISK R: [label | vdiv | waveform]. Byte rates, scaled dynamically.
    let dr_max = sparkline_max_f(&app.sys_disk_read_history);
    let diskr = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([
            Constraint::Length(26),
            Constraint::Length(1),
            Constraint::Min(20),
        ])
        .split(vrows[10]);
    let diskr_now = app.sys_disk_read_history.back().copied().unwrap_or(0.0);
    let diskr_label = Paragraph::new(Line::from(Span::styled(
        format!("▌ DISK R {}", format_speed(diskr_now as u64)),
        Style::default()
            .fg(Color::Blue)
            .add_modifier(Modifier::BOLD),
    )));
    f.render_widget(diskr_label, diskr[0]);
    draw_waveform(
        f,
        diskr[2],
        &app.sys_disk_read_history,
        dr_max,
        Color::Blue,
        |value| *value,
    );

    // System DISK W: [label | vdiv | waveform].
    let dw_max = sparkline_max_f(&app.sys_disk_write_history);
    let diskw = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([
            Constraint::Length(26),
            Constraint::Length(1),
            Constraint::Min(20),
        ])
        .split(vrows[12]);
    let diskw_now = app.sys_disk_write_history.back().copied().unwrap_or(0.0);
    let diskw_label = Paragraph::new(Line::from(Span::styled(
        format!("▌ DISK W {}", format_speed(diskw_now as u64)),
        Style::default()
            .fg(Color::Cyan)
            .add_modifier(Modifier::BOLD),
    )));
    f.render_widget(diskw_label, diskw[0]);
    draw_waveform(
        f,
        diskw[2],
        &app.sys_disk_write_history,
        dw_max,
        Color::Cyan,
        |value| *value,
    );

    // System DISK SPACE: [label | vdiv | waveform]. Usage % of the largest
    // `/dev` disk, fixed 0..100 % scale like MEM. The label also shows total /
    // used / free capacity (dynamic T/G/M units, like `df -h`).
    let dspace = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([
            Constraint::Length(26),
            Constraint::Length(1),
            Constraint::Min(20),
        ])
        .split(vrows[14]);
    let dspace_now = app.sys_disk_space_history.back().copied().unwrap_or(0.0);
    let total_str = format_bytes_short(app.sys_disk_space_total);
    let used_str = format_bytes_short(app.sys_disk_space_used);
    let free_str = format_bytes_short(app.sys_disk_space_avail);
    // Line 1: percent + the actual /dev device path. Line 2: total/used/free
    // (T/U/F) with dynamic units. Kept to two lines so it fits the label cell
    // even when the metric row is only 2 rows tall on a short terminal.
    let dspace_label = Paragraph::new(vec![
        Line::from(Span::styled(
            format!("▌ DISK {:.1}% {}", dspace_now, app.sys_disk_space_dev),
            Style::default()
                .fg(Color::Gray)
                .add_modifier(Modifier::BOLD),
        )),
        Line::from(Span::styled(
            format!("T:{} U:{} F:{}", total_str, used_str, free_str),
            Style::default().fg(Color::DarkGray),
        )),
    ]);
    f.render_widget(dspace_label, dspace[0]);
    draw_waveform(
        f,
        dspace[2],
        &app.sys_disk_space_history,
        100.0,
        Color::Gray,
        |value| *value,
    );

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
    // `base_idx` is the row that carries the time axis (just below the last metric);
    // every odd row up to it is a divider between metrics, and `base_idx` itself is
    // the baseline under the last waveform.
    let base_idx = 2 * n_metrics - 1;
    let labels_idx = 2 * n_metrics;
    let hlines: Vec<u16> = (1..=base_idx).step_by(2).map(|i| vrows[i].y).collect();
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
                        let c = &mut buf[(lx as u16 + i as u16, vrows[labels_idx].y)];
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
        if nlx >= stats_inner.x && nlx + nlen <= stats_inner.x + stats_inner.width {
            for (i, ch) in now_label.chars().enumerate() {
                let cell = &mut buf[(nlx + i as u16, vrows[labels_idx].y)];
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

fn format_epoch_clock(timestamp: u64) -> String {
    if timestamp == 0 {
        return "n/a".to_string();
    }
    crate::format_epoch_datetime(timestamp)
}

#[derive(Clone)]
struct UserTableRow {
    user: String,
    tty: String,
    online: bool,
    sessions: usize,
    last_login: u64,
    last_process: Option<crate::conns::SessionProcess>,
}

/// Render users aggregated by username, or all sessions for the selected user.
fn render_user_panel(f: &mut Frame, area: Rect, app: &mut App) {
    let users = app.conns.users.clone();
    if let Some(selected) = app.selected_user.clone() {
        if let Some(user) = users.iter().find(|user| user.user == selected) {
            let rows = user
                .session_rows
                .iter()
                .map(|session| UserTableRow {
                    user: session.user.clone(),
                    tty: session.tty.clone(),
                    online: session.online,
                    sessions: usize::from(session.online),
                    last_login: session.last_login,
                    last_process: session.last_process.clone(),
                })
                .collect();
            render_user_table(
                f,
                area,
                app,
                format!("Sessions for {}", user.user),
                rows,
                " no session records ",
            );
            return;
        }
        app.selected_user = None;
    }

    let rows = users
        .iter()
        .map(|user| UserTableRow {
            user: user.user.clone(),
            tty: String::new(),
            online: user.online,
            sessions: user.sessions,
            last_login: user.last_login,
            last_process: user.last_process.clone(),
        })
        .collect();
    render_user_table(f, area, app, "Users".to_string(), rows, " no user records ");
}

fn render_user_table(
    f: &mut Frame,
    area: Rect,
    app: &mut App,
    title_prefix: String,
    rows: Vec<UserTableRow>,
    empty_message: &str,
) {
    let max_rows = area.height.saturating_sub(3) as usize;
    app.conns.clamp_scroll(rows.len(), max_rows.max(1));
    let first = if rows.is_empty() {
        0
    } else {
        app.conns.scroll + 1
    };
    let last = (app.conns.scroll + max_rows).min(rows.len());
    let title = format!(
        " {} {}-{}/{}  ↑↓:scroll {} ",
        title_prefix,
        first,
        last.max(first),
        rows.len(),
        if title_prefix == "Users" {
            "Enter: sessions  u:users c:connections"
        } else {
            "Esc: back  u:users c:connections"
        }
    );
    let block = Block::default().borders(Borders::ALL).title(title);
    app.header_y = 0;
    app.col_hit.clear();

    if rows.is_empty() {
        f.render_widget(Paragraph::new(empty_message).block(block), area);
        return;
    }

    let widths = [
        Constraint::Length(7),  // STATUS
        Constraint::Length(6),  // USER
        Constraint::Length(8),  // SESSIONS
        Constraint::Length(19), // LAST LOGIN
        Constraint::Length(12), // LAST PROCESS
        Constraint::Length(19), // STARTED
    ];
    let header = Row::new(vec![
        Cell::from("STATUS"),
        Cell::from("USER"),
        Cell::from("SESSIONS"),
        Cell::from("LAST LOGIN"),
        Cell::from("LAST PROCESS"),
        Cell::from("STARTED"),
    ])
    .style(Style::default().add_modifier(Modifier::BOLD));
    let rows = rows
        .iter()
        .skip(app.conns.scroll)
        .take(max_rows.max(1))
        .enumerate()
        .map(|(index, user)| {
            let style = if (app.conns.scroll + index) % 2 == 1 {
                Style::default().bg(ZEBRA)
            } else {
                Style::default()
            };
            let (process, started) = match user.last_process.as_ref() {
                Some(process) => (
                    if user.tty.is_empty() {
                        process.comm.clone()
                    } else {
                        format!("{} {}", user.tty, process.comm)
                    },
                    format_epoch_clock(process.started_at),
                ),
                None => (
                    if user.tty.is_empty() {
                        "n/a".to_string()
                    } else {
                        format!("{} n/a", user.tty)
                    },
                    "n/a".to_string(),
                ),
            };
            Row::new(vec![
                Cell::from(if user.online { "online" } else { "offline" }),
                Cell::from(user.user.clone()),
                Cell::from(user.sessions.to_string()),
                Cell::from(format_epoch_clock(user.last_login)),
                Cell::from(process),
                Cell::from(started),
            ])
            .style(style)
        });
    let table = Table::default()
        .header(header)
        .block(block)
        .column_spacing(1)
        .widths(widths)
        .rows(rows);
    f.render_widget(table, area);
}

/// Column that the Top Connections list can be sorted by. `Throughput` is the
/// default ordering (total RX+TX, descending) that the list used before sorting
/// was added; it has no dedicated header so it is never shown as the active column.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum SortKey {
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
pub(crate) const AGG_SORT_ORDER: [SortKey; 10] = [
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
pub(crate) const DETAIL_SORT_ORDER: [SortKey; 8] = [
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
        (ConnRow::Detail(c), SortKey::Host) => SortVal::Text(c.host.clone().unwrap_or_default()),
        (ConnRow::Detail(c), SortKey::Svc) => SortVal::Text(c.service.clone().unwrap_or_default()),
        (ConnRow::Detail(c), SortKey::Rx) => SortVal::Num(c.rx_rate.unwrap_or(0.0)),
        (ConnRow::Detail(c), SortKey::Tx) => SortVal::Num(c.tx_rate.unwrap_or(0.0)),
        (ConnRow::Detail(c), SortKey::Throughput) => {
            SortVal::Num(c.rx_rate.unwrap_or(0.0) + c.tx_rate.unwrap_or(0.0))
        }
        _ => SortVal::None,
    }
}

/// Sort `view` in place by `key`, in ascending order when `asc` is true.
pub(crate) fn sort_rows(view: &mut [ConnRow], key: SortKey, asc: bool) {
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
pub(crate) enum ConnRow {
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

    let first = if view_len == 0 {
        0
    } else {
        app.conns.scroll + 1
    };
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
            Constraint::Length(20), // PROC(PID)
            Constraint::Length(8),  // CPU%
            Constraint::Length(8),  // MEM%
            Constraint::Length(10), // TIME+
            Constraint::Length(6),  // CONNS
            Constraint::Length(12), // RX
            Constraint::Length(12), // TX
            Constraint::Length(12), // DISKR
            Constraint::Length(12), // DISKW
        ]
    } else {
        vec![
            Constraint::Length(20), // PROC(PID)
            Constraint::Length(4),  // PRO
            Constraint::Length(19), // SRC
            Constraint::Length(19), // DST
            Constraint::Length(14), // HOST
            Constraint::Length(7),  // SVC
            Constraint::Length(12), // RX
            Constraint::Length(12), // TX
        ]
    };

    // Header labels + the SortKey each maps to (parallel arrays).
    let (labels, keys): (&[&str], &[Option<SortKey>]) = if app.aggregate {
        (
            &[
                "USER",
                "PROC(PID)",
                "CPU%",
                "MEM%",
                "TIME+",
                "CONNS",
                "RX",
                "TX",
                "DISKR",
                "DISKW",
            ],
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
                        Cell::from(format!("{:.1}%", a.cpu_pct)),
                        Cell::from(format!("{:.1}%", a.mem_pct)),
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
