//! Linux system-metric readers used by the monitor.
//!
//! This module contains only counter reads and conversions. It does not own
//! application history or rendering state, which keeps sampling policy in
//! `App` and makes the platform-facing code easier to test and replace.

use std::collections::HashSet;
use std::ffi::CString;
use std::fs;

/// Read the aggregate `cpu` line from `/proc/stat` and return cumulative
/// `(busy_ticks, total_ticks)`. Guest ticks are excluded because Linux already
/// includes them in user/nice.
pub(crate) fn read_system_cpu() -> Option<(u64, u64)> {
    let content = fs::read_to_string("/proc/stat").ok()?;
    let line = content.lines().find(|l| l.starts_with("cpu "))?;
    let fields: Vec<u64> = line
        .split_whitespace()
        .skip(1)
        .filter_map(|f| f.parse::<u64>().ok())
        .collect();
    if fields.len() < 4 {
        return None;
    }

    // Linux fields 8 and 9 are guest and guest_nice. They are already included
    // in fields 0 and 1, so summing them again inflates utilisation.
    let total: u64 = fields.iter().take(8).sum();
    let idle = fields[3] + fields.get(4).copied().unwrap_or(0);
    let busy = total.saturating_sub(idle);
    Some((busy, total))
}

/// Read system memory usage as a percentage of total RAM from `/proc/meminfo`.
pub(crate) fn read_system_mem_pct() -> Option<f64> {
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
        (Some(t), Some(a)) if t > 0 => Some((t.saturating_sub(a) as f64 / t as f64) * 100.0),
        _ => None,
    }
}

/// Read system swap usage as a percentage of total swap from `/proc/meminfo`.
pub(crate) fn read_system_swap_pct() -> Option<f64> {
    let content = fs::read_to_string("/proc/meminfo").ok()?;
    let mut total: Option<u64> = None;
    let mut free: Option<u64> = None;
    for line in content.lines() {
        if line.starts_with("SwapTotal:") {
            total = line.split_whitespace().nth(1).and_then(|v| v.parse().ok());
        } else if line.starts_with("SwapFree:") {
            free = line.split_whitespace().nth(1).and_then(|v| v.parse().ok());
        }
    }
    match (total, free) {
        (Some(t), Some(f)) if t > 0 => Some((t.saturating_sub(f) as f64 / t as f64) * 100.0),
        (Some(0), _) => Some(0.0),
        _ => None,
    }
}

/// Read cumulative disk sectors across whole devices listed in `/sys/block`.
/// Partitions are excluded to avoid double-counting their parent device.
pub(crate) fn read_sys_disk_sectors() -> Option<(u64, u64)> {
    let mut devices: HashSet<String> = HashSet::new();
    let entries = fs::read_dir("/sys/block").ok()?;
    for entry in entries.flatten() {
        devices.insert(entry.file_name().to_string_lossy().into_owned());
    }
    if devices.is_empty() {
        return None;
    }

    let content = fs::read_to_string("/proc/diskstats").ok()?;
    let mut read_sectors = 0u64;
    let mut write_sectors = 0u64;
    for line in content.lines() {
        let fields: Vec<&str> = line.split_whitespace().collect();
        if fields.len() < 10 || !devices.contains(fields[2]) {
            continue;
        }
        read_sectors = read_sectors.saturating_add(fields[5].parse::<u64>().unwrap_or(0));
        write_sectors = write_sectors.saturating_add(fields[9].parse::<u64>().unwrap_or(0));
    }
    Some((read_sectors, write_sectors))
}

/// Read disk-space usage for the largest mounted `/dev` filesystem.
///
/// The return value is `(used_percent, total_bytes, used_bytes, available_bytes,
/// device_label)`. Overlay/container environments often have no `/dev` source
/// in `/proc/mounts`; in that case the root filesystem is used as `rootfs`.
pub(crate) fn read_system_disk_space() -> Option<(f64, u64, u64, u64, String)> {
    let mounts = fs::read_to_string("/proc/mounts").ok()?;
    let mut seen_fs: HashSet<u64> = HashSet::new();
    let mut best: Option<(u64, u64, u64, String)> = None;

    for line in mounts.lines() {
        let fields: Vec<&str> = line.split_whitespace().collect();
        if fields.len() < 2 {
            continue;
        }
        let source = fields[0];
        if !source.starts_with("/dev/") {
            continue;
        }

        let mountpoint = match CString::new(fields[1]) {
            Ok(path) => path,
            Err(_) => continue,
        };
        let mut stat: libc::stat = unsafe { std::mem::zeroed() };
        if unsafe { libc::stat(mountpoint.as_ptr(), &mut stat) } != 0 {
            continue;
        }
        if !seen_fs.insert(stat.st_dev) {
            continue;
        }

        let mut statvfs: libc::statvfs = unsafe { std::mem::zeroed() };
        if unsafe { libc::statvfs(mountpoint.as_ptr(), &mut statvfs) } != 0 {
            continue;
        }
        let fragment = if statvfs.f_frsize > 0 {
            statvfs.f_frsize
        } else {
            statvfs.f_bsize
        } as u64;
        let total = (statvfs.f_blocks as u64).saturating_mul(fragment);
        if total == 0 {
            continue;
        }
        let free_total = (statvfs.f_bfree as u64).saturating_mul(fragment);
        let available = (statvfs.f_bavail as u64).saturating_mul(fragment);
        let used = total.saturating_sub(free_total);
        let is_larger = best
            .as_ref()
            .is_none_or(|(best_total, _, _, _)| total > *best_total);
        if is_larger {
            best = Some((total, used, available, source.to_string()));
        }
    }

    let (total, used, available, device) = match best {
        Some(value) => value,
        None => root_filesystem_usage()?,
    };
    let denominator = used.saturating_add(available);
    let percent = if denominator > 0 {
        used as f64 / denominator as f64 * 100.0
    } else {
        0.0
    };
    Some((percent, total, used, available, device))
}

fn root_filesystem_usage() -> Option<(u64, u64, u64, String)> {
    let path = CString::new("/").ok()?;
    let mut statvfs: libc::statvfs = unsafe { std::mem::zeroed() };
    if unsafe { libc::statvfs(path.as_ptr(), &mut statvfs) } != 0 {
        return None;
    }
    let fragment = if statvfs.f_frsize > 0 {
        statvfs.f_frsize
    } else {
        statvfs.f_bsize
    } as u64;
    let total = (statvfs.f_blocks as u64).saturating_mul(fragment);
    if total == 0 {
        return None;
    }
    let used = total.saturating_sub((statvfs.f_bfree as u64).saturating_mul(fragment));
    let available = (statvfs.f_bavail as u64).saturating_mul(fragment);
    Some((total, used, available, "rootfs".to_string()))
}
