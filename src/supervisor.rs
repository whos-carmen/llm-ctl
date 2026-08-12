//! Supervisor: lifecycle of the single llama-server worker child.

use crate::config::{Model, Worker};
use anyhow::Result;
use std::process::Command;
use std::sync::Arc;
use std::time::Duration;
use tokio::process::Command as TokioCommand;
use tokio::sync::Mutex;

#[derive(Clone, Debug)]
pub enum WorkerStatus {
    Stopped,
    Starting,
    Ready,
    Stopping,
    Crashed(String),
}

/// Shared, async-safe view of the worker.
pub struct WorkerState {
    pub model_id: Option<String>,
    pub model: Option<Model>,
    pub pid: Option<u32>,
    pub status: WorkerStatus,
    pub stopping: bool,
    pub started_at: Option<std::time::Instant>,
    pub restarts: u8,
    pub last_restart: Option<std::time::Instant>,
    /// Epoch seconds of the last accepted completion (for idle gating in UIs).
    pub last_request_at: Option<f64>,
    /// Cache stats of the last recorded turn (cache_n, prompt_n, predicted_n).
    pub last_turn: Option<serde_json::Value>,
    pub last_slots: Option<serde_json::Value>,
    pub last_metrics: Option<serde_json::Value>,
}

impl WorkerState {
    pub fn new() -> Self {
        Self {
            model_id: None,
            model: None,
            pid: None,
            status: WorkerStatus::Stopped,
            stopping: false,
            started_at: None,
            restarts: 0,
            last_restart: None,
            last_request_at: None,
            last_turn: None,
            last_slots: None,
            last_metrics: None,
        }
    }
}

pub struct Supervisor {
    pub worker: Worker,
    pub binary: String,
    pub state: Arc<Mutex<WorkerState>>,
    client: reqwest::Client,
    /// Serializes the spawn/stop/switch lifecycle (TOCTOU guard).
    lifecycle: Mutex<()>,
}

impl Supervisor {
    pub fn new(worker: Worker, binary: String, state: Arc<Mutex<WorkerState>>) -> Self {
        Self {
            worker,
            binary,
            state,
            client: reqwest::Client::new(),
            lifecycle: Mutex::new(()),
        }
    }

    fn health_url(&self) -> String {
        format!("http://127.0.0.1:{}/health", self.worker.port)
    }
    fn slots_url(&self) -> String {
        format!("http://127.0.0.1:{}/slots", self.worker.port)
    }
    fn metrics_url(&self) -> String {
        format!("http://127.0.0.1:{}/metrics", self.worker.port)
    }

    /// Spawn the worker for `model` (no-op if that model is already running).
    pub async fn spawn(&self, model: &Model, host: &str) -> Result<()> {
        {
            let st = self.state.lock().await;
            if st.model_id.as_deref() == Some(model.id.as_str()) && st.pid.is_some() {
                return Ok(());
            }
        }
        {
            let mut st = self.state.lock().await;
            st.status = WorkerStatus::Starting;
            st.model_id = Some(model.id.clone());
            st.model = Some(model.clone());
            st.started_at = Some(std::time::Instant::now());
            st.stopping = false;
            st.restarts = 0;
        }

        // Keep worker output for debugging (user state dir, not /tmp - symlink safe).
        let worker_log = worker_log_file()?;
        let mut cmd = TokioCommand::new(&self.binary);
        cmd.args(&self.worker.default_args)
            .arg("-m")
            .arg(&model.model)
            .arg("--host")
            .arg(host)
            .arg("--port")
            .arg(self.worker.port.to_string())
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::from(worker_log.try_clone()?))
            .stderr(std::process::Stdio::from(worker_log));
        for (k, v) in &self.worker.gpu_env {
            cmd.env(k, v);
        }

        let mut child = cmd.spawn()?;
        let pid = child.id();
        tracing::info!(pid, model = %model.id, port = self.worker.port, "worker spawned");
        {
            let mut st = self.state.lock().await;
            st.pid = pid;
        }

        // Reap the child's exit; only clear state if it is still our pid.
        // Unexpected exit (not a deliberate stop) => Crashed so the poller can
        // honor restart_on_crash.
        let state = self.state.clone();
        tokio::spawn(async move {
            let _ = child.wait().await;
            let mut st = state.lock().await;
            if st.pid == pid {
                st.pid = None;
                st.status = if st.stopping {
                    WorkerStatus::Stopped
                } else {
                    WorkerStatus::Crashed("worker exited unexpectedly".into())
                };
            }
        });
        Ok(())
    }

    /// Switch to `model`: stop the current worker if a different one is active.
    /// Serialized via `lifecycle` to avoid TOCTOU between concurrent /start calls.
    pub async fn start_model(&self, model: &Model, host: &str) -> Result<()> {
        let _guard = self.lifecycle.lock().await;
        {
            let st = self.state.lock().await;
            if st.model_id.as_deref() == Some(model.id.as_str()) && st.pid.is_some() {
                return Ok(()); // already this model
            }
        }
        if self.state.lock().await.pid.is_some() {
            self.stop().await?;
        }
        self.spawn(model, host).await
    }

    /// Stop the current worker: SIGTERM, grace, SIGKILL.
    pub async fn stop(&self) -> Result<()> {
        let pid = self.state.lock().await.pid;
        if let Some(pid) = pid {
            {
                let mut st = self.state.lock().await;
                st.stopping = true;
            }
            let _ = Command::new("/bin/kill").arg("-TERM").arg(pid.to_string()).status();
            for _ in 0..10 {
                if !pid_alive(pid) {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(500)).await;
            }
            if pid_alive(pid) {
                let _ = Command::new("/bin/kill").arg("-KILL").arg(pid.to_string()).status();
            }
        }
        let mut st = self.state.lock().await;
        st.pid = None;
        st.status = WorkerStatus::Stopped;
        st.stopping = false;
        Ok(())
    }

    /// Health poll: Starting -> Ready (with a 60s grace so slow model loads
    /// aren't misreported as crashes); Ready -> Crashed on failure.
    pub async fn poll(&self) {
        let has_pid = self.state.lock().await.pid.is_some();
        if !has_pid {
            return; // wait-task owns Stopped/Crashed transitions when pid is gone
        }
        let ok = self
            .client
            .get(&self.health_url())
            .timeout(Duration::from_secs(2))
            .send()
            .await
            .map(|r| r.status().is_success())
            .unwrap_or(false);
        let (status, started_at) = {
            let st = self.state.lock().await;
            (st.status.clone(), st.started_at)
        };
        let mut st = self.state.lock().await;
        if ok {
            st.status = WorkerStatus::Ready;
            return;
        }
        match status {
            WorkerStatus::Starting => {
                if started_at
                    .map(|s| s.elapsed() > Duration::from_secs(60))
                    .unwrap_or(false)
                {
                    st.status = WorkerStatus::Crashed("model load timed out".into());
                }
                // else: still loading; keep Starting
            }
            WorkerStatus::Ready | WorkerStatus::Crashed(_) => {
                st.status = WorkerStatus::Crashed("worker health check failed".into());
            }
            _ => {}
        }
    }

    /// Restart a Crashed worker when restart_on_crash allows (5s cooldown,
    /// max_restarts cap). Returns true if a restart was triggered.
    pub async fn maybe_restart(&self) -> bool {
        let (want, model) = {
            let st = self.state.lock().await;
            let want = self.worker.restart_on_crash
                && u32::from(st.restarts) < self.worker.max_restarts
                && matches!(st.status, WorkerStatus::Crashed(_))
                && st
                    .last_restart
                    .map(|l| l.elapsed() > Duration::from_secs(5))
                    .unwrap_or(true);
            (want, st.model.clone())
        };
        if !want {
            return false;
        }
        let Some(model) = model else { return false };
        {
            let mut st = self.state.lock().await;
            st.restarts += 1;
            st.last_restart = Some(std::time::Instant::now());
        }
        tracing::warn!(model = %model.id, "restarting crashed worker");
        let _ = self.stop().await; // clear stale pid/port (no-op if already gone)
        match self.spawn(&model, "127.0.0.1").await {
            Ok(()) => true,
            Err(e) => {
                tracing::error!(%e, "worker restart spawn failed");
                false
            }
        }
    }

    /// Snapshot /slots + /metrics for the API/UI.
    pub async fn poll_slots(&self) {
        let has_pid = self.state.lock().await.pid.is_some();
        if !has_pid {
            return;
        }
        let slots = match self
            .client
            .get(&self.slots_url())
            .timeout(Duration::from_secs(2))
            .send()
            .await
        {
            Ok(r) => r.json::<serde_json::Value>().await.ok(),
            Err(_) => None,
        };
        let metrics = match self
            .client
            .get(&self.metrics_url())
            .timeout(Duration::from_secs(2))
            .send()
            .await
        {
            Ok(r) => r.text().await.ok(),
            Err(_) => None,
        };
        let mut st = self.state.lock().await;
        if let Some(s) = slots {
            st.last_slots = Some(s);
        }
        if let Some(m) = metrics {
            st.last_metrics = Some(parse_metrics(&m));
        }
    }

    /// Candidate models with current status (for /api/models).
    pub async fn list(&self, models: &[Model]) -> Vec<serde_json::Value> {
        let st = self.state.lock().await;
        let active = st.model_id.clone();
        let status = format!("{:?}", st.status);
        models
            .iter()
            .map(|m| {
                serde_json::json!({
                    "id": m.id,
                    "model": m.model,
                    "autostart": m.autostart,
                    "description": m.description,
                    "active": active.as_deref() == Some(m.id.as_str()),
                    "status": status,
                })
            })
            .collect()
    }
}

/// Open the worker log under the user's XDG state dir.
fn worker_log_file() -> anyhow::Result<std::fs::File> {
    let base = std::env::var("XDG_STATE_HOME")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|_| {
            std::path::PathBuf::from(std::env::var("HOME").unwrap_or_else(|_| ".".into()))
                .join(".local/state")
        });
    let dir = base.join("llm-ctl");
    std::fs::create_dir_all(&dir).map_err(anyhow::Error::msg)?;
    std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(dir.join("worker.log"))
        .map_err(anyhow::Error::msg)
}

/// True if `pid` exists AND its argv still mentions llama-server (guards
/// against killing an unrelated process after a PID reuse).
fn pid_alive(pid: u32) -> bool {
    let cmdline = match std::fs::read(format!("/proc/{pid}/cmdline")) {
        Ok(c) => c,
        Err(_) => return false,
    };
    String::from_utf8_lossy(&cmdline).contains("llama-server")
}

/// Parse the Prometheus text format into {name: value} for easy display.
fn parse_metrics(text: &str) -> serde_json::Value {
    let mut map = serde_json::Map::new();
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        if let Some((name, val)) = line.split_once(char::is_whitespace) {
            if let Ok(f) = val.parse::<f64>() {
                map.insert(name.to_string(), serde_json::json!(f));
            }
        }
    }
    serde_json::Value::Object(map)
}
