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
    pub pid: Option<u32>,
    pub status: WorkerStatus,
    pub last_slots: Option<serde_json::Value>,
    pub last_metrics: Option<serde_json::Value>,
}

impl WorkerState {
    pub fn new() -> Self {
        Self {
            model_id: None,
            pid: None,
            status: WorkerStatus::Stopped,
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
}

impl Supervisor {
    pub fn new(worker: Worker, binary: String, state: Arc<Mutex<WorkerState>>) -> Self {
        Self {
            worker,
            binary,
            state,
            client: reqwest::Client::new(),
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
        }

        // Keep worker output for debugging.
        let worker_log = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open("/tmp/llmctl-worker.log")
            .map_err(anyhow::Error::msg)?;
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
        let state = self.state.clone();
        tokio::spawn(async move {
            let _ = child.wait().await;
            let mut st = state.lock().await;
            if st.pid == pid {
                st.pid = None;
                st.status = WorkerStatus::Stopped;
            }
        });
        Ok(())
    }

    /// Switch to `model`: stop the current worker if a different one is active.
    pub async fn start_model(&self, model: &Model, host: &str) -> Result<()> {
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
            let _ = Command::new("/bin/kill").arg("-TERM").arg(pid.to_string()).status();
            for _ in 0..10 {
                if !pid_alive(pid) {
                    break;
                }
                std::thread::sleep(Duration::from_millis(500));
            }
            if pid_alive(pid) {
                let _ = Command::new("/bin/kill").arg("-KILL").arg(pid.to_string()).status();
            }
        }
        let mut st = self.state.lock().await;
        st.pid = None;
        st.status = WorkerStatus::Stopped;
        Ok(())
    }

    /// Health poll: flip Starting -> Ready, or mark Crashed.
    pub async fn poll(&self) {
        let has_pid = self.state.lock().await.pid.is_some();
        if !has_pid {
            return;
        }
        let ok = self
            .client
            .get(&self.health_url())
            .timeout(Duration::from_secs(2))
            .send()
            .await
            .map(|r| r.status().is_success())
            .unwrap_or(false);
        let mut st = self.state.lock().await;
        if ok {
            st.status = WorkerStatus::Ready;
        } else {
            st.status = WorkerStatus::Crashed("worker health check failed".into());
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

fn pid_alive(pid: u32) -> bool {
    std::path::Path::new(&format!("/proc/{pid}")).exists()
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
