//! HuggingFace download job: `uvx hf download` with a live tail log, then
//! auto-register the finished GGUF into the runtime model list.

use crate::config::{Hf, Model};
use anyhow::Result;
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::process::Command;
use tokio::sync::Mutex;

const LOG_CAP: usize = 500;

#[derive(Clone, Debug, Default, Serialize)]
pub struct Job {
    pub status: String, // idle | running | ok | failed
    pub repo: Option<String>,
    pub include: Option<String>,
    pub started_at: Option<f64>,
    pub finished_at: Option<f64>,
    pub error: Option<String>,
    pub registered: Option<String>,
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

#[derive(Deserialize)]
pub struct HfDownloadReq {
    pub repo: String,
    #[serde(default)]
    pub include: Option<String>,
}

pub struct DownloadManager {
    cfg: Hf,
    state: Arc<Mutex<Job>>,
    models: Arc<Mutex<Vec<Model>>>,
}

impl DownloadManager {
    pub fn new(cfg: Hf, models: Arc<Mutex<Vec<Model>>>) -> Self {
        Self {
            cfg,
            state: Arc::new(Mutex::new(Job::default())),
            models,
        }
    }

    pub async fn job(&self) -> Job {
        self.state.lock().await.clone()
    }

    pub async fn start(&self, req: &HfDownloadReq) -> Result<(), String> {
        {
            let st = self.state.lock().await;
            if st.status == "running" {
                return Err("a download is already running".into());
            }
        }
        let include = req.include.clone().unwrap_or_else(|| "*.gguf".to_string());

        let mut cmd = Command::new(&self.cfg.cli[0]);
        cmd.args(&self.cfg.cli[1..])
            .arg("download")
            .arg(&req.repo)
            .args(["--include", &include])
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped());
        let mut child = cmd.spawn().map_err(|e| format!("spawn failed: {e}"))?;

        {
            let mut st = self.state.lock().await;
            *st = Job {
                status: "running".into(),
                repo: Some(req.repo.clone()),
                include: Some(include.clone()),
                started_at: Some(now()),
                ..Default::default()
            };
        }

        let stdout = child.stdout.take().expect("stdout piped");
        let stderr = child.stderr.take().expect("stderr piped");
        let state = self.state.clone();
        let models = self.models.clone();
        let cache = self.cfg.cache.clone();
        let repo = req.repo.clone();

        tokio::spawn(async move {
            let out = collect_lines(stdout).await;
            let err = collect_lines(stderr).await;
            let exit = child.wait().await;
            let ok = exit.as_ref().map(|s| s.success()).unwrap_or(false);

            let mut st = state.lock().await;
            for l in out.iter().chain(err.iter()) {
                st.push_line(l.clone());
            }
            st.finished_at = Some(now());
            if ok {
                match resolve_gguf(&cache, &repo) {
                    Some(path) => {
                        let id = derive_id(&path);
                        let mut ms = models.lock().await;
                        if !ms.iter().any(|m| m.id == id) {
                            ms.push(Model {
                                id: id.clone(),
                                model: path,
                                autostart: false,
                                description: format!("hf: {repo}"),
                            });
                            st.push_line(format!("registered model '{id}'"));
                        }
                        st.status = "ok".into();
                        st.registered = Some(id);
                    }
                    None => {
                        st.status = "failed".into();
                        st.error = Some("resolved GGUF not found in HF cache".into());
                    }
                }
            } else {
                st.status = "failed".into();
                st.error = Some(format!("hf download exit {:?}", exit.map(|s| s.code())));
            }
        });
        Ok(())
    }
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

fn now() -> f64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs_f64()
}

/// Find the newest *.gguf under the repo's HF cache dir (snapshots tree).
fn resolve_gguf(cache: &str, repo: &str) -> Option<String> {
    let dir_name = repo.replace('/', "--");
    let base = Path::new(cache).join("hub").join(format!("models--{dir_name}"));
    let mut best: Option<(PathBuf, u64)> = None;
    let mut stack = vec![base];
    while let Some(dir) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&dir) else { continue };
        for e in entries.flatten() {
            let p = e.path();
            if p.is_dir() {
                stack.push(p);
            } else if p.extension().map(|x| x == "gguf").unwrap_or(false) {
                let mtime = e.metadata().map(|m| m.modified().ok().and_then(|t| t.duration_since(UNIX_EPOCH).ok()).map(|d| d.as_secs()).unwrap_or(0)).unwrap_or(0);
                if best.as_ref().map(|(_, m)| mtime > *m).unwrap_or(true) {
                    best = Some((p, mtime));
                }
            }
        }
    }
    best.map(|(p, _)| p.to_string_lossy().into_owned())
}

/// "LFM2.5-8B-A1B-UD-Q5_K_XL" -> "lfm2.5-8b-a1b-ud-q5-k-xl"
fn derive_id(path: &str) -> String {
    let stem = Path::new(path)
        .file_stem()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_default();
    let mut id = String::new();
    let mut dash = false;
    for c in stem.chars() {
        if c.is_alphanumeric() {
            id.push(c.to_ascii_lowercase());
            dash = false;
        } else if !dash {
            id.push('-');
            dash = true;
        }
    }
    let id = id.trim_matches('-').to_string();
    if id.is_empty() {
        "hf-model".into()
    } else {
        id
    }
}
