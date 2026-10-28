//! The local HTTP server and fixture builder the integration tests share.

#![allow(dead_code)]

use axum::Router;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::routing::get;
use craft_font_store::sha256_hex;
use font_preview_service::sync::{Config, Fetch, Tuning};
use serde_json::{Value, json};
use std::collections::{HashMap, HashSet};
use std::io::Write;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

pub const COMMIT: &str = "45e3d5269e4a17d823f6d84470bc084dc5ae46f6";

pub struct Server {
    pub files: HashMap<String, Vec<u8>>,
    /// Paths jsDelivr refuses with 403, as it does for large repositories.
    pub refused_on_jsdelivr: HashSet<String>,
    pub font_hits: AtomicUsize,
}

/// Starts a server on 127.0.0.1 in its own thread and returns its port.
pub fn serve(server: Arc<Server>) -> u16 {
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let rt = tokio::runtime::Runtime::new().unwrap();
        rt.block_on(async move {
            let app = Router::new()
                .route("/{origin}/{*path}", get(handle))
                .with_state(server);
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            tx.send(listener.local_addr().unwrap().port()).unwrap();
            axum::serve(listener, app).await.unwrap();
        });
    });
    rx.recv().unwrap()
}

async fn handle(
    State(s): State<Arc<Server>>,
    Path((origin, path)): Path<(String, String)>,
) -> Result<Vec<u8>, StatusCode> {
    if path.ends_with(".ttf") || path.ends_with(".otf") || path.ends_with("OFL.txt") {
        s.font_hits.fetch_add(1, Ordering::Relaxed);
    }
    if origin == "jsd" && s.refused_on_jsdelivr.contains(&path) {
        return Err(StatusCode::FORBIDDEN);
    }
    s.files.get(&path).cloned().ok_or(StatusCode::NOT_FOUND)
}

/// Plain HTTP to the local server; the production client is https only. The mirror hosts map to
/// `/jsd/<path in repo>` and `/raw/<path in repo>`.
pub struct Local(pub u16);

impl Fetch for Local {
    fn get(&self, url: &str, max: u64) -> Result<Vec<u8>, String> {
        let local = if let Some(rest) = url.strip_prefix("https://cdn.jsdelivr.net/gh/") {
            let after_at = rest.split_once('@').ok_or("bad jsdelivr url")?.1;
            format!(
                "jsd/{}",
                after_at.split_once('/').ok_or("bad jsdelivr url")?.1
            )
        } else if let Some(rest) = url.strip_prefix("https://raw.githubusercontent.com/") {
            let mut parts = rest.splitn(4, '/');
            format!("raw/{}", parts.nth(3).ok_or("bad raw url")?)
        } else {
            return Err(format!("unexpected host in {url}"));
        };
        let mut resp = ureq::get(&format!("http://127.0.0.1:{}/{local}", self.0))
            .call()
            .map_err(|e| e.to_string())?;
        resp.body_mut()
            .with_config()
            .limit(max + 1)
            .read_to_vec()
            .map_err(|e| e.to_string())
    }
}

pub fn gzip(bytes: &[u8]) -> Vec<u8> {
    let mut enc = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
    enc.write_all(bytes).unwrap();
    enc.finish().unwrap()
}

pub struct Fixture {
    pub server: Arc<Server>,
    pub sources_sha256: String,
}

/// One Google Fonts index with `families`, served with `files` (paths in the repository), plus
/// `sources.json` and the gzipped index.
pub fn fixture_from(families: Value, files: Vec<(&str, Vec<u8>)>, samples: Value) -> Fixture {
    let index = json!({
        "schema": 1, "version": "1.2", "source": "google/fonts", "commit": COMMIT,
        "base_url": format!("https://cdn.jsdelivr.net/gh/google/fonts@{COMMIT}/"),
        "fallback_base_url": format!("https://raw.githubusercontent.com/google/fonts/{COMMIT}/"),
        "samples": samples,
        "families": families,
    })
    .to_string()
    .into_bytes();
    let gz = gzip(&index);
    let sources = json!({"version": "1.0", "indexes": [{
        "source": "google/fonts", "name": "Google Fonts", "curated": true,
        "path": "indexes/google-fonts.json", "size": index.len(), "sha256": sha256_hex(&index),
        "gz_path": "indexes/google-fonts.json.gz", "gz_size": gz.len(), "gz_sha256": sha256_hex(&gz),
        "commit": COMMIT,
    }]})
    .to_string()
    .into_bytes();
    let mut all = vec![
        ("sources.json".to_string(), sources.clone()),
        ("indexes/google-fonts.json.gz".to_string(), gz),
    ];
    all.extend(files.into_iter().map(|(k, v)| (k.to_string(), v)));
    let server = Arc::new(Server {
        files: all.into_iter().collect(),
        refused_on_jsdelivr: HashSet::new(),
        font_hits: AtomicUsize::new(0),
    });
    Fixture {
        server,
        sources_sha256: sha256_hex(&sources),
    }
}

/// A file entry of the index for `bytes` at `path`.
pub fn file_entry(path: &str, bytes: &[u8]) -> Value {
    json!({"path": path, "size": bytes.len(), "sha256": sha256_hex(bytes), "style": "normal",
           "weight": 400, "axes": [], "postscript": []})
}

pub fn config(dir: &std::path::Path, sha: &str) -> Config {
    Config {
        fonts_dir: dir.to_path_buf(),
        repo: "aiongg/craft-font-index".into(),
        commit: COMMIT.into(),
        sources_sha256: sha.into(),
        tuning: Tuning::default(),
    }
}
