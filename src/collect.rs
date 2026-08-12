//! Shared bounded line collector for child process stdout/stderr.
//!
//! Jobs read stdout and stderr CONCURRENTLY into one capped sink (a child that
//! fills a full stderr pipe while stdout is still open would otherwise
//! deadlock inside a sequential read). The sink is capped so verbose output
//! can't balloon memory.

use std::sync::{Arc, Mutex};
use tokio::io::{AsyncBufReadExt, BufReader};

/// A capped, cloneable line sink. Push keeps only the most recent `cap` lines.
#[derive(Clone, Default)]
pub struct LineSink {
    inner: Arc<Mutex<Vec<String>>>,
    cap: usize,
}

impl LineSink {
    pub fn new(cap: usize) -> Self {
        Self { inner: Arc::new(Mutex::new(Vec::new())), cap }
    }
    pub fn push(&self, line: String) {
        let mut v = self.inner.lock().unwrap();
        if v.len() >= self.cap {
            let drop = v.len() - self.cap + 1;
            v.drain(0..drop);
        }
        v.push(line);
    }
    /// Take a snapshot of the current lines.
    pub fn snapshot(&self) -> Vec<String> {
        self.inner.lock().unwrap().clone()
    }
}

/// Read `r` line by line, pushing each (trimmed) line into `sink`. Stops at EOF
/// or error. Safe to call for stdout and stderr in parallel via `tokio::join!`.
pub async fn collect_capped<R: tokio::io::AsyncRead + Unpin>(r: R, sink: LineSink) {
    let mut reader = BufReader::new(r);
    let mut line = String::new();
    loop {
        line.clear();
        match reader.read_line(&mut line).await {
            Ok(0) | Err(_) => break,
            Ok(_) => sink.push(line.trim_end().to_string()),
        }
    }
}