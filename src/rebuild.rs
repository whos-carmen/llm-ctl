//! Rebuild job: git pull + cmake build of llama.cpp, with a live tail log and
//! SHA before/after. Non-disruptive: the running worker keeps serving; a later
//! stop/start picks up the freshly built binary (same path).

use crate::config::Llama;
use anyhow::Result;
use serde::Serialize;
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};
use tokio::io::{AsyncBufReadExt, BufReader};
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
}

impl RebuildManager {
    pub fn new(llama: Llama) -> Self {
        Self {
            llama,
            state: Arc::new(Mutex::new(Job {
                status: "idle".into(),
                ..Default::default()
            })),
        }
    }

    pub async fn job(&self) -> Job {
        self.state.lock().await.clone()
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
            st.before_sha = before_sha;
        }

        let state = self.state.clone();
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
            st.after_sha = after_sha;
            if ok {
                st.status = "ok".into();
                st.push_line("rebuild ok; relaunch the worker to pick up the new binary".into());
            } else {
                st.status = "failed".into();
                st.error = Some(format!("pull_ok={pull_ok} build_ok={build_ok}"));
            }
        });
        Ok(())
    }
}

async fn run_and_collect(cmd: &mut Command) -> (bool, Vec<String>) {
    cmd.stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped());
    let Ok(mut child) = cmd.spawn() else {
        return (false, vec!["spawn failed".into()]);
    };
    let out = collect_lines(child.stdout.take().expect("stdout piped")).await;
    let err = collect_lines(child.stderr.take().expect("stderr piped")).await;
    let status = child.wait().await;
    let ok = status.map(|s| s.success()).unwrap_or(false);
    let mut lines = out;
    lines.extend(err);
    (ok, lines)
}

async fn collect_lines<R: tokio::io::AsyncRead + Unpin>(r: R) -> Vec<String> {
    let mut lines = Vec::new();
    let mut reader = BufReader::new(r);
    let mut line = String::new();
    loop {
        line.clear();
        match reader.read_line(&mut line).await {
            Ok(0) | Err(_) => break,
            Ok(_) => lines.push(line.trim_end().to_string()),
        }
    }
    lines
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
