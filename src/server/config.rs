//! Server configuration, from the environment only.

use axum::http::HeaderName;
use std::path::PathBuf;

/// | Variable | Default | |
/// |---|---|---|
/// | `PORT` | 8000 | API port; an invalid value falls back to the default |
/// | `METRICS_PORT` | 9100 | Prometheus metrics, never on the API port |
/// | `FONTS_DIR` | required | the volume `sync` fills, read only |
/// | `ONDEMAND_RATE` | 50 | per-IP token refill per second on on-demand routes; 0 turns the limit off |
/// | `ONDEMAND_BURST` | 200 | per-IP bucket size |
/// | `ONDEMAND_CONCURRENCY` | available parallelism | subsets computed at once |
/// | `ONDEMAND_QUEUE` | 4 × concurrency | requests that may wait for a slot; beyond it, 503 |
/// | `FACE_CACHE` | 500 | font files the subsetter keeps in memory (each at most 64 MB) |
/// | `FACE_CACHE_MB` | 256 | summed size of those font files |
/// | `RESPONSE_CACHE_MB` | 256 | on-demand results |
/// | `FILES_BR_CACHE_MB` | 64 | brotli-compressed file downloads |
/// | `FILES_CONCURRENCY` | 64 | file downloads sent at once |
/// | `FILES_QUEUE` | 16 × concurrency | downloads that may wait for a slot; beyond it, 503 |
/// | `CLIENT_IP_HEADER` | unset | header with the client address, set by the proxy in front |
///
/// Without `CLIENT_IP_HEADER` the client is the peer address. With it, the server takes the
/// header's rightmost entry, the one the nearest proxy wrote; a client can only prepend to it.
/// The header must be one that proxy always sets: behind Caddy and Cloudflare that is
/// `CF-Connecting-IP` (a single address that Cloudflare overwrites), since the rightmost
/// `X-Forwarded-For` entry Caddy appends is Cloudflare's edge address.
#[derive(Clone, Debug)]
pub struct Config {
    pub port: u16,
    pub metrics_port: u16,
    pub fonts_dir: PathBuf,
    pub rate_per_s: f64,
    pub burst: f64,
    pub concurrency: usize,
    pub queue: usize,
    pub face_cache: usize,
    pub face_cache_bytes: u64,
    pub response_cache_bytes: u64,
    pub files_br_cache_bytes: u64,
    pub files_concurrency: usize,
    pub files_queue: usize,
    pub client_ip_header: Option<HeaderName>,
}

impl Config {
    /// The defaults, serving `fonts_dir`.
    pub fn new(fonts_dir: impl Into<PathBuf>) -> Self {
        let concurrency = std::thread::available_parallelism().map_or(1, std::num::NonZero::get);
        Self {
            port: 8000,
            metrics_port: 9100,
            fonts_dir: fonts_dir.into(),
            rate_per_s: 50.0,
            burst: 200.0,
            concurrency,
            queue: 4 * concurrency,
            face_cache: 500,
            face_cache_bytes: 256 << 20,
            response_cache_bytes: 256 << 20,
            files_br_cache_bytes: 64 << 20,
            files_concurrency: 64,
            files_queue: 16 * 64,
            client_ip_header: None,
        }
    }

    pub fn from_env() -> Result<Self, String> {
        Self::from_lookup(|k| std::env::var(k).ok())
    }

    pub fn from_lookup(get: impl Fn(&str) -> Option<String>) -> Result<Self, String> {
        let get = |key: &str| get(key).filter(|v| !v.is_empty());
        fn parse<T: std::str::FromStr>(
            key: &str,
            value: Option<String>,
        ) -> Result<Option<T>, String> {
            value
                .map(|v| {
                    v.parse()
                        .map_err(|_| format!("{key} `{v}` is not a valid value"))
                })
                .transpose()
        }
        let fonts_dir = get("FONTS_DIR").ok_or("FONTS_DIR is not set")?;
        let mut cfg = Self::new(fonts_dir);
        cfg.port = port_from_env(get("PORT").as_deref());
        if let Some(p) = parse("METRICS_PORT", get("METRICS_PORT"))? {
            cfg.metrics_port = p;
        }
        if let Some(r) = parse::<f64>("ONDEMAND_RATE", get("ONDEMAND_RATE"))? {
            cfg.rate_per_s = r;
        }
        if let Some(b) = parse::<f64>("ONDEMAND_BURST", get("ONDEMAND_BURST"))? {
            cfg.burst = b;
        }
        if let Some(c) = parse("ONDEMAND_CONCURRENCY", get("ONDEMAND_CONCURRENCY"))? {
            cfg.concurrency = c;
            cfg.queue = 4 * c;
        }
        if let Some(q) = parse("ONDEMAND_QUEUE", get("ONDEMAND_QUEUE"))? {
            cfg.queue = q;
        }
        if let Some(n) = parse("FACE_CACHE", get("FACE_CACHE"))? {
            cfg.face_cache = n;
        }
        let mb = |key: &str| parse::<u64>(key, get(key)).map(|v| v.map(|mb| mb << 20));
        if let Some(b) = mb("FACE_CACHE_MB")? {
            cfg.face_cache_bytes = b;
        }
        if let Some(b) = mb("RESPONSE_CACHE_MB")? {
            cfg.response_cache_bytes = b;
        }
        if let Some(b) = mb("FILES_BR_CACHE_MB")? {
            cfg.files_br_cache_bytes = b;
        }
        if let Some(n) = parse("FILES_CONCURRENCY", get("FILES_CONCURRENCY"))? {
            cfg.files_concurrency = n;
            cfg.files_queue = 16 * n;
        }
        if let Some(q) = parse("FILES_QUEUE", get("FILES_QUEUE"))? {
            cfg.files_queue = q;
        }
        if let Some(h) = get("CLIENT_IP_HEADER") {
            cfg.client_ip_header = Some(
                HeaderName::try_from(h.as_str())
                    .map_err(|_| format!("CLIENT_IP_HEADER `{h}` is not a header name"))?,
            );
        }
        let finite = |v: f64| v.is_finite() && v >= 0.0;
        if !finite(cfg.rate_per_s)
            || !finite(cfg.burst)
            || (cfg.rate_per_s > 0.0 && cfg.burst < 1.0)
        {
            return Err("ONDEMAND_RATE must be at least 0 and ONDEMAND_BURST at least 1".into());
        }
        if cfg.concurrency == 0 {
            return Err("ONDEMAND_CONCURRENCY must be above 0".into());
        }
        if cfg.files_concurrency == 0 {
            return Err("FILES_CONCURRENCY must be above 0".into());
        }
        Ok(cfg)
    }
}

/// Reads `PORT`, defaulting to 8000. DHI runtime images run as nonroot,
/// so the default must stay above 1024.
fn port_from_env(value: Option<&str>) -> u16 {
    value.and_then(|p| p.parse().ok()).unwrap_or(8000)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn port_defaults_to_8000() {
        assert_eq!(port_from_env(None), 8000);
    }

    #[test]
    fn port_reads_valid_value() {
        assert_eq!(port_from_env(Some("9090")), 9090);
    }

    #[test]
    fn port_falls_back_on_invalid_value() {
        assert_eq!(port_from_env(Some("not-a-port")), 8000);
        assert_eq!(port_from_env(Some("70000")), 8000);
    }

    fn env(pairs: &[(&str, &str)]) -> impl Fn(&str) -> Option<String> {
        let map: std::collections::HashMap<String, String> = pairs
            .iter()
            .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
            .collect();
        move |k| map.get(k).cloned()
    }

    #[test]
    fn config_reads_the_environment() {
        assert!(
            Config::from_lookup(env(&[])).is_err(),
            "FONTS_DIR is required"
        );
        let cfg = Config::from_lookup(env(&[
            ("FONTS_DIR", "/fonts"),
            ("ONDEMAND_CONCURRENCY", "3"),
            ("FILES_CONCURRENCY", "8"),
            ("RESPONSE_CACHE_MB", "8"),
            ("FACE_CACHE_MB", "32"),
            ("CLIENT_IP_HEADER", "CF-Connecting-IP"),
        ]))
        .unwrap();
        assert_eq!((cfg.concurrency, cfg.queue), (3, 12));
        assert_eq!((cfg.files_concurrency, cfg.files_queue), (8, 128));
        assert_eq!(cfg.response_cache_bytes, 8 << 20);
        assert_eq!(cfg.face_cache_bytes, 32 << 20);
        assert_eq!(cfg.files_br_cache_bytes, 64 << 20);
        assert_eq!(cfg.client_ip_header.unwrap().as_str(), "cf-connecting-ip");
        for (key, bad) in [
            ("ONDEMAND_CONCURRENCY", "0"),
            ("FILES_CONCURRENCY", "0"),
            ("ONDEMAND_RATE", "-1"),
            ("FACE_CACHE", "many"),
            ("CLIENT_IP_HEADER", "bad header"),
        ] {
            let cfg = Config::from_lookup(env(&[("FONTS_DIR", "/f"), (key, bad)]));
            assert!(cfg.is_err(), "{key}={bad}");
        }
    }
}
