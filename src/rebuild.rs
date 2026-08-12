//! Rebuild job: git pull + cmake build of llama.cpp, with a live tail log and
//! SHA before/after. Non-disruptive: the running worker keeps serving; a later
//! stop/start picks up the freshly built binary (same path).

use crate::collect::{collect_capped, LineSink};
use crate::config::Llama;
use anyhow::Result;
use serde::Serialize;
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};
use tokio::process::Command;
use tokio::sync::Mutex;

const LOG_CAP: usize = 800;

#[derive(Clone, Debug, Default, Serialize)]
pub struct Job {
    pub status: String, // idle | running | ok | failed
    pub started_at: Option<f64>,
    pub finished_at: Option<f64>,
    pub before_sha: Option<String>,
    pub after_sha: Option<String>,
    pub error: Option<String>,
    pub log: Vec<String>,
}

impl Job {
    fn push_line(&mut self, line: String) {
        if self.log.len() >= LOG_CAP {
            self.log.drain(0..self.log.len() - LOG_CAP + 1);
        }
        self.log.push(line);
    }
}

pub struct RebuildManager {
    llama: Llama,
    state: Arc<Mutex<Job>>,
    /// Persistent (in-memory) record of past build attempts, newest first.
    history: Arc<Mutex<Vec<BuildRecord>>>,
}

/// One completed build attempt.
#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub struct BuildRecord {
    pub at: f64,
    pub before_sha: Option<String>,
    pub after_sha: Option<String>,
    pub ok: bool,
}

/// Path to the persisted build history (XDG state dir: ~/.local/state/llm-ctl/builds.json).
fn history_path() -> std::path::PathBuf {
    let base = std::env::var("XDG_STATE_HOME")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|_| {
            std::path::PathBuf::from(std::env::var("HOME").unwrap_or_else(|_| ".".into()))
                .join(".local/state")
        });
    base.join("llm-ctl").join("builds.json")
}

fn load_history() -> Vec<BuildRecord> {
    let p = history_path();
    std::fs::read_to_string(&p)
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default()
}

fn save_history(h: &[BuildRecord]) {
    let p = history_path();
    if let Some(dir) = p.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    if let Ok(json) = serde_json::to_string_pretty(h) {
        let _ = std::fs::write(&p, json);
    }
}

impl RebuildManager {
    pub fn new(llama: Llama) -> Self {
        Self {
            llama,
            state: Arc::new(Mutex::new(Job {
                status: "idle".into(),
                ..Default::default()
            })),
            history: Arc::new(Mutex::new(load_history())),
        }
    }

    pub async fn job(&self) -> Job {
        self.state.lock().await.clone()
    }

    /// Past build attempts, newest first.
    pub async fn history(&self) -> Vec<BuildRecord> {
        self.history.lock().await.clone()
    }

    pub async fn start(&self) -> Result<(), String> {
        // Claim the job under the lock BEFORE any work so concurrent
        // /api/rebuild calls cannot both start a job.
        {
            let mut st = self.state.lock().await;
            if st.status == "running" {
                return Err("a rebuild is already running".into());
            }
            *st = Job {
                status: "running".into(),
                started_at: Some(now()),
                ..Default::default()
            };
        }
        let before_sha = git_sha(&self.llama.repo).await;
        {
            let mut st = self.state.lock().await;
            st.before_sha = before_sha.clone();
        }

        let state = self.state.clone();
        let history = self.history.clone();
        let repo = self.llama.repo.clone();
        let remote = self.llama.git_remote.clone();
        let branch = self.llama.git_branch.clone();
        let build_dir = self.llama.build_dir.clone();

        tokio::spawn(async move {
            let mut log: Vec<String> = Vec::new();

            // 1. git pull --ff-only <remote> <branch>
            let (pull_ok, pull_lines) = run_and_collect(
                Command::new("git").args(["-C", &repo, "pull", "--ff-only", &remote, &branch]),
            )
            .await;
            log.extend(pull_lines);

            // 2. cmake --build <build_dir> -j N
            let nproc = nproc();
            let (build_ok, build_lines) = run_and_collect(
                Command::new("cmake").args(["--build", &build_dir, "-j", &nproc.to_string()]),
            )
            .await;
            log.extend(build_lines);

            let ok = pull_ok && build_ok;
            let after_sha = git_sha(&repo).await;

            let mut st = state.lock().await;
            for l in log {
                st.push_line(l);
            }
            st.finished_at = Some(now());
            st.after_sha = after_sha.clone();
            if ok {
                st.status = "ok".into();
                st.push_line("rebuild ok; relaunch the worker to pick up the new binary".into());
            } else {
                st.status = "failed".into();
                st.error = Some(format!("pull_ok={pull_ok} build_ok={build_ok}"));
            }
            // Record into the persistent history (newest first, capped), then
            // flush to disk so it survives a daemon restart.
            {
                let mut h = history.lock().await;
                h.insert(0, BuildRecord {
                    at: now(),
                    before_sha: before_sha,
                    after_sha,
                    ok,
                });
                if h.len() > 20 {
                    h.truncate(20);
                }
                save_history(&h);
            }
        });
        Ok(())
    }
}

/// Run a command once, reading stdout+stderr concurrently into a capped sink,
/// with a watchdog. Returns (success, collected lines).
async fn run_and_collect(cmd: &mut Command) -> (bool, Vec<String>) {
    cmd.stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped());
    let Ok(mut child) = cmd.spawn() else {
        return (false, vec!["spawn failed".into()]);
    };
    let sink = LineSink::new(LOG_CAP);
    let out = child.stdout.take().expect("stdout piped");
    let err = child.stderr.take().expect("stderr piped");
    let c_out = tokio::spawn(collect_capped(out, sink.clone()));
    let c_err = tokio::spawn(collect_capped(err, sink.clone()));
    let status: Option<std::process::ExitStatus> =
            match tokio::time::timeout(
            std::time::Duration::from_secs(30 * 60),
            child.wait(),
        )
        .await
        {
            Ok(s) => s.ok(),
            Err(_) => {
                tracing::warn!("rebuild watchdog: killing hung child");
                let _ = child.start_kill();
                let _ = child.wait().await;
                None
            }
        };
    let _ = c_out.await;
    let _ = c_err.await;
    let ok = status.map(|s| s.success()).unwrap_or(false);
    (ok, sink.snapshot())
}

async fn git_sha(repo: &str) -> Option<String> {
    let out = Command::new("git")
        .args(["-C", repo, "rev-parse", "HEAD"])
        .output()
        .await
        .ok()?;
    if !out.status.success() {
        return None;
    }
    Some(String::from_utf8_lossy(&out.stdout).trim().to_string())
}

fn nproc() -> usize {
    std::fs::read_to_string("/proc/cpuinfo")
        .map(|s| s.lines().filter(|l| l.starts_with("processor")).count())
        .unwrap_or(8)
        .max(1)
}

fn now() -> f64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs_f64()
}
