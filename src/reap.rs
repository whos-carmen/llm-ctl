//! Sole-ownership: kill any llama-server running outside llm-ctl.
//! Used at startup so llm-ctl is the single manager of llama-server on the host.

use std::process::Command;
use std::time::Duration;

/// TERM then KILL any llama-server processes; returns the pids force-killed.
pub fn reap_legacy_llama() -> Vec<u32> {
    let _initial: Vec<u32> = Vec::new();
    for pid in llama_server_pids() {
        let _ = Command::new("/bin/kill").arg("-TERM").arg(pid.to_string()).status();
    }
    // Blocking sleep; this is called from async main at startup.
    std::thread::sleep(Duration::from_secs(3)); // grace period
    let mut killed = Vec::new();
    for pid in llama_server_pids() {
        let _ = Command::new("/bin/kill").arg("-KILL").arg(pid.to_string()).status();
        killed.push(pid);
    }
    killed
}

/// Scan /proc for processes whose comm is `llama-server` (the process name),
/// not a cmdline substring — avoids collateral-killing shells/editors/grep
/// whose argv happens to mention "llama-server".
fn llama_server_pids() -> Vec<u32> {
    let mut out = Vec::new();
    let self_pid = std::process::id();
    if let Ok(entries) = std::fs::read_dir("/proc") {
        for entry in entries.flatten() {
            let name = entry.file_name();
            let Ok(pid) = name.to_string_lossy().parse::<u32>() else {
                continue;
            };
            if pid == self_pid {
                continue;
            }
            // comm is limited to 15 chars and is the process name.
            let Ok(comm) = std::fs::read_to_string(format!("/proc/{pid}/comm")) else {
                continue;
            };
            if comm.trim() == "llama-server" {
                out.push(pid);
            }
        }
    }
    out
}
