//! Proxmox VE client (ops tier). Cross-tier channel from the LLM host.

use crate::config::Config;
use anyhow::{bail, Result};
use serde::Deserialize;

#[derive(Debug, serde::Serialize, Deserialize)]
pub struct CtInfo {
    pub vmid: u64,
    pub name: String,
    pub status: String,
    pub tags: String,
}

#[derive(Deserialize)]
struct LxcResponse {
    data: Vec<Lxc>,
}

#[derive(Deserialize)]
struct Lxc {
    vmid: u64,
    #[serde(default)]
    name: String,
    #[serde(default)]
    status: String,
    #[serde(default)]
    tags: String,
}

/// Expand a leading "~/" to $HOME.
fn expand_tilde(p: &str) -> String {
    if let Some(rest) = p.strip_prefix("~/") {
        if let Ok(home) = std::env::var("HOME") {
            return format!("{home}/{rest}");
        }
    }
    p.to_string()
}

/// The token SECRET: env first, else ~/.keys (PROXMOX_AGENT_ADMIN).
fn read_secret() -> Result<String> {
    if let Ok(v) = std::env::var("PROXMOX_AGENT_ADMIN") {
        if !v.trim().is_empty() {
            return Ok(v.trim().to_string());
        }
    }
    let home = std::env::var("HOME").unwrap_or_else(|_| ".".into());
    let text = std::fs::read_to_string(format!("{home}/.keys"))?;
    for line in text.lines() {
        if let Some((k, v)) = line.split_once('=') {
            if k.trim() == "PROXMOX_AGENT_ADMIN" {
                return Ok(v.trim().to_string());
            }
        }
    }
    bail!("PROXMOX_AGENT_ADMIN not found in ~/.keys")
}

async fn client(cfg: &Config) -> Result<reqwest::Client> {
    let builder = reqwest::Client::builder().timeout(std::time::Duration::from_secs(8));
    let builder = match &cfg.proxmox.cert_file {
        Some(path) => {
            // Pin the PVE cert chain: trust ONLY the certs in this file.
            let path = expand_tilde(path);
            let pem = std::fs::read_to_string(&path)?;
            let mut builder = builder.tls_built_in_root_certs(false);
            let mut found = 0usize;
            for block in pem.split("-----BEGIN CERTIFICATE-----").skip(1) {
                let block = format!("-----BEGIN CERTIFICATE-----{block}");
                match reqwest::Certificate::from_pem(block.as_bytes()) {
                    Ok(c) => {
                        builder = builder.add_root_certificate(c);
                        found += 1;
                    }
                    Err(e) => tracing::warn!(%e, "skip bad cert block in {path}"),
                }
            }
            if found == 0 {
                anyhow::bail!("no valid certificates in pinned file {path}");
            }
            builder
        }
        None => {
            tracing::warn!("proxmox.cert_file unset; TLS verification disabled for PVE");
            builder.danger_accept_invalid_certs(true)
        }
    };
    Ok(builder.build()?)
}

/// List LXC containers on the configured node, filtered to the project tag.
pub async fn list_containers(cfg: &Config) -> Result<Vec<CtInfo>> {
    let secret = read_secret()?;
    let auth = format!("PVEAPIToken={}={}", cfg.proxmox.token_id, secret);
    let url = format!("{}/api2/json/nodes/{}/lxc", cfg.proxmox.api, cfg.proxmox.node);
    let resp = client(cfg)
        .await?
        .get(&url)
        .header("Authorization", auth)
        .send()
        .await?;
    let text = resp.text().await?;
    let parsed: LxcResponse = serde_json::from_str(&text)?;
    Ok(parsed
        .data
        .into_iter()
        .filter(|c| c.tags == cfg.proxmox.ct_tag)
        .map(|c| CtInfo {
            vmid: c.vmid,
            name: c.name,
            status: c.status,
            tags: c.tags,
        })
        .collect())
}
