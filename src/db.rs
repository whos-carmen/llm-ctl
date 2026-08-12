//! Session store connectivity. Postgres is installed on the ops tier in M3;
//! until then this only proves the TCP channel (expected to be refused).

pub async fn probe(pgurl: &str) -> Result<(), String> {
    let url = reqwest::Url::parse(pgurl).map_err(|e| e.to_string())?;
    let host = url.host_str().ok_or("pgurl missing host")?.to_string();
    let port = url.port().unwrap_or(5432);
    let addr = format!("{host}:{port}")
        .parse::<std::net::SocketAddr>()
        .map_err(|e| format!("invalid host:port: {e}"))?;
    tokio::time::timeout(
        std::time::Duration::from_secs(3),
        tokio::net::TcpStream::connect(addr),
    )
    .await
    .map_err(|_| "connect timeout".to_string())?
    .map(|_| ())
    .map_err(|e| e.to_string())
}
