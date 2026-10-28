//! Counters for the metrics port, in the Prometheus text format.

use std::collections::BTreeMap;
use std::fmt::Write;
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};

/// Upper bounds of the on-demand latency buckets, in seconds.
const BUCKETS: [f64; 11] = [
    0.001, 0.0025, 0.005, 0.01, 0.025, 0.05, 0.1, 0.25, 0.5, 1.0, 2.5,
];

#[derive(Default)]
pub struct Metrics {
    requests: Mutex<BTreeMap<(String, u16), u64>>,
    latency_buckets: [AtomicU64; BUCKETS.len()],
    latency_count: AtomicU64,
    latency_micros: AtomicU64,
    pub face_hits: AtomicU64,
    pub face_misses: AtomicU64,
    pub response_hits: AtomicU64,
    pub response_misses: AtomicU64,
    pub files_br_hits: AtomicU64,
    pub files_br_misses: AtomicU64,
    pub subset_failures: AtomicU64,
    pub generation_load_failures: AtomicU64,
    generation: Mutex<Option<(String, usize)>>,
}

/// Request counts by status class.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Summary {
    pub total: u64,
    pub limited: u64,
    pub busy: u64,
    pub server_errors: u64,
}

pub fn inc(counter: &AtomicU64) {
    counter.fetch_add(1, Ordering::Relaxed);
}

impl Metrics {
    pub fn request(&self, route: &str, status: u16) {
        let mut map = self
            .requests
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        match map.get_mut(&(route.to_owned(), status)) {
            Some(n) => *n += 1,
            None => {
                map.insert((route.to_owned(), status), 1);
            }
        }
    }

    pub fn on_demand(&self, seconds: f64) {
        for (bound, bucket) in BUCKETS.iter().zip(&self.latency_buckets) {
            if seconds <= *bound {
                inc(bucket);
            }
        }
        inc(&self.latency_count);
        self.latency_micros
            .fetch_add((seconds * 1e6) as u64, Ordering::Relaxed);
    }

    pub fn generation(&self, name: &str, faces: usize) {
        *self
            .generation
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some((name.to_owned(), faces));
    }

    /// Totals per status class over all routes.
    pub fn totals(&self) -> Summary {
        let map = self
            .requests
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let mut s = Summary::default();
        for (&(_, status), &n) in map.iter() {
            s.total += n;
            match status {
                429 => s.limited += n,
                503 => s.busy += n,
                500..=599 => s.server_errors += n,
                _ => {}
            }
        }
        s
    }

    /// The metrics page. `caches` are (name, entries, bytes).
    pub fn render(&self, caches: &[(&str, usize, u64)]) -> String {
        let mut out = String::new();
        let load = |c: &AtomicU64| c.load(Ordering::Relaxed);
        out.push_str("# TYPE fps_requests_total counter\n");
        for ((route, status), n) in self
            .requests
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .iter()
        {
            let _ = writeln!(
                out,
                "fps_requests_total{{route=\"{route}\",status=\"{status}\"}} {n}"
            );
        }
        out.push_str("# TYPE fps_ondemand_seconds histogram\n");
        for (bound, bucket) in BUCKETS.iter().zip(&self.latency_buckets) {
            let _ = writeln!(
                out,
                "fps_ondemand_seconds_bucket{{le=\"{bound}\"}} {}",
                load(bucket)
            );
        }
        let count = load(&self.latency_count);
        let _ = writeln!(out, "fps_ondemand_seconds_bucket{{le=\"+Inf\"}} {count}");
        let _ = writeln!(
            out,
            "fps_ondemand_seconds_sum {}",
            load(&self.latency_micros) as f64 / 1e6
        );
        let _ = writeln!(out, "fps_ondemand_seconds_count {count}");
        out.push_str("# TYPE fps_cache_hits_total counter\n");
        let _ = writeln!(
            out,
            "fps_cache_hits_total{{cache=\"face\"}} {}",
            load(&self.face_hits)
        );
        let _ = writeln!(
            out,
            "fps_cache_hits_total{{cache=\"response\"}} {}",
            load(&self.response_hits)
        );
        let _ = writeln!(
            out,
            "fps_cache_hits_total{{cache=\"files_br\"}} {}",
            load(&self.files_br_hits)
        );
        out.push_str("# TYPE fps_cache_misses_total counter\n");
        let _ = writeln!(
            out,
            "fps_cache_misses_total{{cache=\"face\"}} {}",
            load(&self.face_misses)
        );
        let _ = writeln!(
            out,
            "fps_cache_misses_total{{cache=\"response\"}} {}",
            load(&self.response_misses)
        );
        let _ = writeln!(
            out,
            "fps_cache_misses_total{{cache=\"files_br\"}} {}",
            load(&self.files_br_misses)
        );
        out.push_str("# TYPE fps_cache_entries gauge\n");
        for (name, entries, _) in caches {
            let _ = writeln!(out, "fps_cache_entries{{cache=\"{name}\"}} {entries}");
        }
        out.push_str("# TYPE fps_cache_bytes gauge\n");
        for (name, _, bytes) in caches {
            let _ = writeln!(out, "fps_cache_bytes{{cache=\"{name}\"}} {bytes}");
        }
        out.push_str("# TYPE fps_subset_failures_total counter\n");
        let _ = writeln!(
            out,
            "fps_subset_failures_total {}",
            load(&self.subset_failures)
        );
        out.push_str("# TYPE fps_generation_load_failures_total counter\n");
        let _ = writeln!(
            out,
            "fps_generation_load_failures_total {}",
            load(&self.generation_load_failures)
        );
        out.push_str("# TYPE fps_generation_loaded gauge\n");
        match &*self
            .generation
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
        {
            Some((name, faces)) => {
                let _ = writeln!(out, "fps_generation_loaded{{generation=\"{name}\"}} 1");
                let _ = writeln!(
                    out,
                    "# TYPE fps_generation_faces gauge\nfps_generation_faces {faces}"
                );
            }
            None => out.push_str("fps_generation_loaded 0\n"),
        }
        out
    }
}
