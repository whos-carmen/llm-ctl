//! HuggingFace download job: `uvx hf download` with a live tail log, then
//! auto-register the finished GGUF into the runtime model list.

use crate::collect::{collect_capped, LineSink};
use crate::config::{Hf, Model};
use anyhow::Result;
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};
use tokio::process::Command;
use tokio::sync::Mutex;

const LOG_CAP: usize = 500;
const HF_API_BASE: &str = "https://huggingface.co";

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
            state: Arc::new(Mutex::new(Job {
                status: "idle".into(),
                ..Default::default()
            })),
            models,
        }
    }

    pub async fn job(&self) -> Job {
        self.state.lock().await.clone()
    }

    /// List a repo's GGUF files via the HF tree API, sorted by size ascending.
    /// Returns `(path, size_bytes)` pairs. Threat: repo not found / not a model
    /// / network failure surface as `Err` with a client-friendly message.
    pub async fn list_files(
        &self,
        client: &reqwest::Client,
        repo: &str,
    ) -> Result<Vec<(String, u64)>, String> {
        let url = format!("{HF_API_BASE}/api/models/{repo}/tree/main?recursive=true");
        let resp = client
            .get(&url)
            .send()
            .await
            .map_err(|e| format!("HF API unreachable: {e}"))?;
        if !resp.status().is_success() {
            return Err(format!(
                "HF API {} for repo '{repo}' (not found / not a model?)",
                resp.status()
            ));
        }
        let body: serde_json::Value = resp
            .json()
            .await
            .map_err(|e| format!("bad HF API response: {e}"))?;
        let arr = body
            .as_array()
            .ok_or_else(|| "unexpected HF API payload".to_string())?;
        let mut files: Vec<(String, u64)> = arr
            .iter()
            .filter_map(|e| {
                let p = e.get("path").and_then(|x| x.as_str())?;
                if !p.ends_with(".gguf") {
                    return None;
                }
                let size = e.get("size").and_then(|x| x.as_u64()).unwrap_or(0);
                Some((p.to_string(), size))
            })
            .collect();
        files.sort_by_key(|(_, s)| *s);
        Ok(files)
    }

    pub async fn start(&self, req: &HfDownloadReq) -> Result<(), String> {
        let include = req.include.clone().unwrap_or_else(|| "*.gguf".to_string());
        if self.cfg.cli.is_empty() {
            return Err("hf.cli is empty in config".into());
        }
        // Claim the job under the lock BEFORE spawning so two concurrent
        // /api/hf/download calls cannot both start a job.
        {
            let mut st = self.state.lock().await;
            if st.status == "running" {
                return Err("a download is already running".into());
            }
            *st = Job {
                status: "running".into(),
                repo: Some(req.repo.clone()),
                include: Some(include.clone()),
                started_at: Some(now()),
                ..Default::default()
            };
        }

        let mut cmd = Command::new(&self.cfg.cli[0]);
        cmd.args(&self.cfg.cli[1..])
            .arg("download")
            .arg(&req.repo)
            .args(["--include", &include])
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped());
        let mut child = match cmd.spawn() {
            Ok(c) => c,
            Err(e) => {
                let mut st = self.state.lock().await;
                st.status = "failed".into();
                st.error = Some(format!("spawn failed: {e}"));
                return Err(format!("spawn failed: {e}"));
            }
        };

        let stdout = child.stdout.take().expect("stdout piped");
        let stderr = child.stderr.take().expect("stderr piped");
        let state = self.state.clone();
        let models = self.models.clone();
        let cache = self.cfg.cache.clone();
        let repo = req.repo.clone();

        tokio::spawn(async move {
            // Read stdout+stderr CONCURRENTLY into one capped sink (a child that
            // fills its stderr pipe while stdout is still open would deadlock a
            // sequential read). Watch the child with a timeout so a hung `hf`
            // can't leave the job "running" forever.
            let sink = LineSink::new(LOG_CAP);
            let c_out = tokio::spawn(collect_capped(stdout, sink.clone()));
            let c_err = tokio::spawn(collect_capped(stderr, sink.clone()));
            let exit: Option<std::process::ExitStatus> =
                match tokio::time::timeout(
                std::time::Duration::from_secs(30 * 60),
                child.wait(),
            )
            .await
            {
                Ok(r) => r.ok(),
                Err(_) => {
                    tracing::warn!("download watchdog: killing hung hf child");
                    let _ = child.start_kill();
                    let _ = child.wait().await;
                    None
                }
            };
            let _ = c_out.await;
            let _ = c_err.await;

            let mut st = state.lock().await;
            for l in sink.snapshot() {
                st.push_line(l);
            }
            st.finished_at = Some(now());
            let ok = exit.as_ref().map(|s| s.success()).unwrap_or(false);
            if ok {
                // Directory walk is blocking I/O: run off the async runtime.
                let resolved = tokio::task::spawn_blocking({
                    let cache = cache.clone();
                    let repo = repo.clone();
                    move || resolve_gguf(&cache, &repo)
                })
                .await
                .ok()
                .flatten();
                match resolved {
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
                st.error = Some(format!("hf download exit {:?}", exit.as_ref().map(|s| s.code())));
            }
        });
        Ok(())
    }
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
