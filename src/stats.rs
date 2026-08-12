//! Lightweight host metrics: CPU utilization (from /proc/stat deltas) and GPU
//! (AMD amdgpu sysfs). Polled by a task and exposed on the panel.

use std::path::Path;

#[derive(Clone, Debug, Default, serde::Serialize)]
pub struct HostStats {
    pub cpu_pct: f64,
    pub gpu_pct: Option<f64>,
    pub vram_used_mb: Option<f64>,
    pub vram_total_mb: Option<f64>,
    pub gpu_temp_c: Option<f64>,
}

/// CPU cumulative counters: (total, idle).
type CpuCounters = (u64, u64);

fn read_cpu() -> Option<CpuCounters> {
    let line = std::fs::read_to_string("/proc/stat").ok()?.lines().next()?.to_string();
    let mut it = line.split_whitespace().skip(1);
    let mut vals = Vec::new();
    for _ in 0..8 {
        vals.push(it.next()?.parse::<u64>().ok()?);
    }
    let idle = vals[3] + vals.get(4).copied().unwrap_or(0); // idle + iowait
    let total: u64 = vals.iter().sum();
    Some((total, idle))
}

/// Find the DRM card directory with the most VRAM (the real GPU).
fn gpu_device() -> Option<(String, u64)> {
    let mut best: Option<(String, u64)> = None;
    if let Ok(entries) = std::fs::read_dir("/sys/class/drm") {
        for e in entries.flatten() {
            let name = e.file_name().to_string_lossy().into_owned();
            if !name.starts_with("card") {
                continue;
            }
            let dev = format!("/sys/class/drm/{name}/device");
            if !Path::new(&format!("{dev}/gpu_busy_percent")).exists() {
                continue; // not a real GPU device node
            }
            let total = std::fs::read_to_string(format!("{dev}/mem_info_vram_total"))
                .ok()
                .and_then(|s| s.trim().parse::<u64>().ok());
            if let Some(t) = total {
                if best.as_ref().map(|(_, bt)| t > *bt).unwrap_or(true) {
                    best = Some((dev, t));
                }
            }
        }
    }
    best
}

fn read_u64(path: &str) -> Option<u64> {
    std::fs::read_to_string(path)
        .ok()
        .and_then(|s| s.trim().parse().ok())
}

/// Compute CPU utilization since `prev` and return the new counters.
pub fn sample(prev: CpuCounters) -> (HostStats, CpuCounters) {
    let cur = read_cpu();
    let cpu_pct = match (prev, cur) {
        (p, Some(c)) if c.0 > p.0 => {
            let d_total = c.0 - p.0;
            let d_idle = c.1 - p.1;
            if d_total > 0 {
                ((d_total - d_idle) as f64 / d_total as f64) * 100.0
            } else {
                0.0
            }
        }
        _ => 0.0,
    };

    let gpu = gpu_device().map(|(dev, _)| {
        let gpu_pct = read_u64(&format!("{dev}/gpu_busy_percent")).map(|v| v as f64);
        let vram_used = read_u64(&format!("{dev}/mem_info_vram_used")).map(|v| v as f64 / 1048576.0);
        let vram_total = read_u64(&format!("{dev}/mem_info_vram_total")).map(|v| v as f64 / 1048576.0);
        let temp = std::fs::read_dir(format!("{dev}/hwmon"))
            .ok()
            .and_then(|mut d| d.next())
            .and_then(|e| e.ok())
            .and_then(|e| read_u64(&e.path().join("temp1_input").to_string_lossy()))
            .map(|v| v as f64 / 1000.0);
        HostStats {
            cpu_pct,
            gpu_pct,
            vram_used_mb: vram_used,
            vram_total_mb: vram_total,
            gpu_temp_c: temp,
        }
    });

    // Even without a GPU, still return CPU.
    let mut stats = HostStats { cpu_pct, ..Default::default() };
    if let Some(g) = gpu {
        stats.gpu_pct = g.gpu_pct;
        stats.vram_used_mb = g.vram_used_mb;
        stats.vram_total_mb = g.vram_total_mb;
        stats.gpu_temp_c = g.gpu_temp_c;
    }
    (stats, cur.unwrap_or(prev))
}

/// Initial CPU counters for the first sample delta.
pub fn init_cpu() -> CpuCounters {
    read_cpu().unwrap_or((0, 0))
}