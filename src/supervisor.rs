//! Supervisor: spawn/hold the single llama-server worker child.

use crate::config::{Model, Worker};
use anyhow::Result;
use std::sync::Arc;
use tokio::process::Command;
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
}

impl WorkerState {
    pub fn new() -> Self {
        Self {
            model_id: None,
            pid: None,
            status: WorkerStatus::Stopped,
        }
    }
}

pub struct Supervisor {
    pub worker: Worker,
    pub binary: String,
    pub state: Arc<Mutex<WorkerState>>,
}

impl Supervisor {
    pub fn new(worker: Worker, binary: String, state: Arc<Mutex<WorkerState>>) -> Self {
        Self {
            worker,
            binary,
            state,
        }
    }

    /// Spawn the worker for `model` (no-op if one is already running).
    pub async fn spawn(&self, model: &Model, host: &str) -> Result<()> {
        {
            let st = self.state.lock().await;
            if st.pid.is_some() {
                return Ok(()); // already running
            }
        }
        {
            let mut st = self.state.lock().await;
            st.status = WorkerStatus::Starting;
            st.model_id = Some(model.id.clone());
        }

        let mut cmd = Command::new(&self.binary);
        // Keep worker output (debuggable): stdin null, stdout/stderr to a log.
        let worker_log = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open("/tmp/llmctl-worker.log")
            .map_err(anyhow::Error::msg)?;
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

        // Reap the child's exit and record it.
        let state = self.state.clone();
        tokio::spawn(async move {
            let _ = child.wait().await;
            let mut st = state.lock().await;
            st.status = WorkerStatus::Stopped;
            st.pid = None;
        });
        Ok(())
    }
}
