use anyhow::Result;
use serde::Deserialize;
use std::collections::{BTreeMap, HashMap};

#[derive(Deserialize, Debug, Clone)]
pub struct Config {
    pub listen: Listen,
    pub llama: Llama,
    pub worker: Worker,
    pub hf: Hf,
    pub proxmox: Proxmox,
    pub db: Db,
    #[serde(default)]
    pub models: Vec<Model>,
}

#[derive(Deserialize, Debug, Clone)]
pub struct Listen {
    pub host: String,
    pub port: u16,
}

#[derive(Deserialize, Debug, Clone)]
pub struct Llama {
    pub repo: String,
    pub build_dir: String,
    pub binary: String,
    pub git_remote: String,
    pub git_branch: String,
}

#[derive(Deserialize, Debug, Clone)]
pub struct Worker {
    pub port: u16,
    #[serde(default)]
    pub default_args: Vec<String>,
    #[serde(default)]
    pub gpu_env: HashMap<String, String>,
    #[serde(default = "default_true")]
    pub restart_on_crash: bool,
    #[serde(default)]
    pub max_restarts: u32,
}

fn default_true() -> bool {
    true
}

#[derive(Deserialize, Debug, Clone)]
pub struct Hf {
    #[serde(default = "default_hf_cli")]
    pub cli: Vec<String>,
    pub cache: String,
    #[serde(default)]
    pub autoregister: bool,
}

fn default_hf_cli() -> Vec<String> {
    vec!["uvx".into(), "hf".into()]
}

#[derive(Deserialize, Debug, Clone)]
pub struct Proxmox {
    pub api: String,
    pub node: String,
    pub token_id: String,
    pub ct_storage: String,
    pub ct_tag: String,
    #[serde(default)]
    pub ct_vmid_start: u16,
    /// Optional PEM cert to pin (path). When set, TLS is verified against it.
    #[serde(default)]
    pub cert_file: Option<String>,
    #[serde(default)]
    pub cts: BTreeMap<String, String>,
}

#[derive(Deserialize, Debug, Clone)]
pub struct Db {
    pub pgurl: String,
    #[serde(default)]
    pub schema_migrate: bool,
}

#[derive(Deserialize, Debug, Clone)]
pub struct Model {
    pub id: String,
    pub model: String,
    #[serde(default)]
    pub autostart: bool,
    #[serde(default)]
    pub description: String,
}

impl Config {
    pub fn load(path: &str) -> Result<Self> {
        let text = std::fs::read_to_string(path)?;
        let mut cfg: Config = toml::from_str(&text)?;
        // Expand ~ in the paths we later touch directly.
        cfg.llama.repo = expand_tilde(&cfg.llama.repo);
        cfg.llama.build_dir = expand_tilde(&cfg.llama.build_dir);
        cfg.llama.binary = expand_tilde(&cfg.llama.binary);
        cfg.hf.cache = expand_tilde(&cfg.hf.cache);
        Ok(cfg)
    }
}

fn expand_tilde(p: &str) -> String {
    if let Some(rest) = p.strip_prefix("~/") {
        if let Ok(home) = std::env::var("HOME") {
            return format!("{home}/{rest}");
        }
    }
    p.to_string()
}
