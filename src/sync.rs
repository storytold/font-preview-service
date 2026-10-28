//! `font-preview-service sync`: mirrors every font and licence file the pinned index lists into the
//! content-addressed store, then builds a generation (manifest and static subsets) and switches
//! `current` to it.

use crate::index12;
use crate::precompute::{self, Face};
use crate::store::{Kind, MANIFEST, Store};
use craft_font_store::{
    Cancel, Client, FontFile, Index, IndexPin, SourceEntry, SourcesFile, sha256_hex, verify_font,
};
use serde::Serialize;
use std::collections::{HashMap, HashSet};
use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::sync::atomic::{AtomicUsize, Ordering};

pub const DEFAULT_INDEX_REPO: &str = "aiongg/craft-font-index";
const WORKERS: usize = 4;
const LOG_EVERY: usize = 100;
const MAX_SOURCES_BYTES: u64 = 1 << 20;
const MAX_INDEX_BYTES: u64 = 32 << 20;
const MAX_LICENCE_BYTES: u64 = 1 << 20;

/// Where bytes come from. Production uses [`HttpFetch`]; tests substitute a local server.
pub trait Fetch: Sync {
    fn get(&self, url: &str, max: u64) -> Result<Vec<u8>, String>;
}

pub struct HttpFetch(Client);

impl HttpFetch {
    pub fn new() -> Self {
        Self(Client::default())
    }
}

impl Default for HttpFetch {
    fn default() -> Self {
        Self::new()
    }
}

impl Fetch for HttpFetch {
    fn get(&self, url: &str, max: u64) -> Result<Vec<u8>, String> {
        self.0
            .get(url, max, &Cancel::new(), &mut |_| {})
            .map_err(|e| e.to_string())
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct Config {
    pub fonts_dir: PathBuf,
    pub repo: String,
    pub commit: String,
    pub sources_sha256: String,
    pub tuning: Tuning,
}

/// How the precompute step runs, from the `SYNC_*` variables.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Tuning {
    /// `SYNC_THREADS`; default the available parallelism, at most 4.
    pub threads: usize,
    /// `SYNC_MEMORY_MB`, default 512: the summed size of font files precomputed at once.
    pub memory_bytes: u64,
    /// `SYNC_MAX_FAILED_PERCENT`, default 1: above this share of faces with a failed subset, the
    /// run keeps `current` and fails.
    pub max_failed_percent: f64,
    /// `SYNC_FORCE=1`: build a generation even when nothing changed.
    pub force: bool,
}

impl Default for Tuning {
    fn default() -> Self {
        Self {
            threads: std::thread::available_parallelism()
                .map_or(1, std::num::NonZero::get)
                .min(4),
            memory_bytes: 512 << 20,
            max_failed_percent: 1.0,
            force: false,
        }
    }
}

impl Tuning {
    fn from_lookup(get: &impl Fn(&str) -> Option<String>) -> Result<Self, String> {
        fn parse<T: std::str::FromStr>(
            get: &impl Fn(&str) -> Option<String>,
            key: &str,
        ) -> Result<Option<T>, String> {
            match get(key).filter(|v| !v.is_empty()) {
                None => Ok(None),
                Some(v) => v
                    .parse()
                    .map(Some)
                    .map_err(|_| format!("{key} `{v}` is not a valid value")),
            }
        }
        let default = Self::default();
        let threads = parse::<usize>(get, "SYNC_THREADS")?.unwrap_or(default.threads);
        let memory_mb = parse::<u64>(get, "SYNC_MEMORY_MB")?;
        let max_failed_percent =
            parse::<f64>(get, "SYNC_MAX_FAILED_PERCENT")?.unwrap_or(default.max_failed_percent);
        if threads == 0 || memory_mb == Some(0) {
            return Err("SYNC_THREADS and SYNC_MEMORY_MB must be above 0".into());
        }
        if !(0.0..=100.0).contains(&max_failed_percent) {
            return Err("SYNC_MAX_FAILED_PERCENT must be between 0 and 100".into());
        }
        Ok(Self {
            threads,
            memory_bytes: memory_mb.map_or(default.memory_bytes, |mb| mb << 20),
            max_failed_percent,
            force: get("SYNC_FORCE").as_deref() == Some("1"),
        })
    }

    fn limits(&self) -> precompute::Limits {
        precompute::Limits {
            threads: self.threads,
            memory_bytes: self.memory_bytes,
        }
    }
}

fn is_hex(s: &str, len: usize) -> bool {
    s.len() == len
        && s.bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

fn is_repo(s: &str) -> bool {
    let part = |p: &str| {
        !p.is_empty()
            && !p.starts_with('.')
            && p.bytes()
                .all(|b| b.is_ascii_alphanumeric() || b"._-".contains(&b))
    };
    s.split_once('/').is_some_and(|(o, n)| part(o) && part(n))
}

impl Config {
    /// Reads `FONTS_DIR`, `INDEX_REPO`, `INDEX_COMMIT`, `INDEX_SHA256` and the `SYNC_*` tuning
    /// through `get`.
    pub fn from_lookup(get: impl Fn(&str) -> Option<String>) -> Result<Self, String> {
        let need = |key: &str| {
            get(key)
                .filter(|v| !v.is_empty())
                .ok_or_else(|| format!("{key} is not set"))
        };
        let fonts_dir = PathBuf::from(need("FONTS_DIR")?);
        let repo = get("INDEX_REPO")
            .filter(|v| !v.is_empty())
            .unwrap_or_else(|| DEFAULT_INDEX_REPO.to_string());
        let commit = need("INDEX_COMMIT")?;
        let sources_sha256 = need("INDEX_SHA256")?;
        if !is_repo(&repo) {
            return Err(format!("INDEX_REPO `{repo}` is not owner/name"));
        }
        if !is_hex(&commit, 40) {
            return Err("INDEX_COMMIT must be 40 lowercase hex digits".into());
        }
        if !is_hex(&sources_sha256, 64) {
            return Err("INDEX_SHA256 must be 64 lowercase hex digits".into());
        }
        let tuning = Tuning::from_lookup(&get)?;
        Ok(Self {
            fonts_dir,
            repo,
            commit,
            sources_sha256,
            tuning,
        })
    }

    pub fn from_env() -> Result<Self, String> {
        Self::from_lookup(|k| std::env::var(k).ok())
    }

    /// The pinned index repository, for building (percent-encoded) download URLs. The strings are
    /// leaked because `IndexPin` holds `&'static str`; a process builds one per run.
    fn pin(&self) -> IndexPin {
        IndexPin {
            repo: Box::leak(self.repo.clone().into_boxed_str()),
            commit: Box::leak(self.commit.clone().into_boxed_str()),
            sha256: Box::leak(self.sources_sha256.clone().into_boxed_str()),
        }
    }
}

#[derive(Debug, Default, Serialize)]
pub struct SourceReport {
    pub source: String,
    pub families: usize,
    pub files: usize,
    pub licences: usize,
    /// Families with a licence path but no hashed `license_file`; their licence is not mirrored.
    pub licences_unhashed: usize,
}

#[derive(Debug, Serialize)]
pub struct Failure {
    pub family: String,
    pub file: String,
    pub reason: String,
}

#[derive(Debug, Default, Serialize)]
pub struct Totals {
    pub fetched: usize,
    pub skipped: usize,
    pub failed: usize,
}

#[derive(Debug, Serialize)]
pub struct Manifest {
    pub index_repo: String,
    pub index_commit: String,
    pub sources_sha256: String,
    pub sources: Vec<SourceReport>,
    pub totals: Totals,
    pub failures: Vec<Failure>,
    /// The generation directory a clean run built.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub generation: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub precompute: Option<precompute::Report>,
    /// Why a generation was built but `current` not switched to it.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub refused_switch: Option<String>,
    /// Nothing changed since the generation `current` points to, which stays.
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub unchanged: bool,
}

impl Manifest {
    /// Whether `current` now holds this run's index: the exit status of `sync`.
    pub fn succeeded(&self) -> bool {
        self.failures.is_empty() && self.refused_switch.is_none()
    }
}

/// Tries each URL in turn and returns the first body `accept` takes. A refusal (403 from jsDelivr
/// for large repositories) or a bad body moves on to the next URL.
fn fetch_first<T>(
    fetch: &dyn Fetch,
    urls: &[String],
    max: u64,
    accept: &dyn Fn(&[u8]) -> Result<T, String>,
) -> Result<T, String> {
    let mut last = "no URL to try".to_string();
    for url in urls {
        match fetch.get(url, max).and_then(|bytes| accept(&bytes)) {
            Ok(v) => return Ok(v),
            Err(e) => last = format!("{url}: {e}"),
        }
    }
    Err(last)
}

fn read_sources(cfg: &Config, pin: &IndexPin, fetch: &dyn Fetch) -> Result<SourcesFile, String> {
    fetch_first(fetch, &pin.urls(), MAX_SOURCES_BYTES, &|bytes| {
        if sha256_hex(bytes) != cfg.sources_sha256 {
            return Err("sources.json does not match INDEX_SHA256".into());
        }
        SourcesFile::from_json(bytes).map_err(|e| e.to_string())
    })
}

/// Reads an index, preferring its gzip form and falling back to the plain file when no mirror
/// serves a usable gzip. The second value reads the same verified JSON for the 1.2 fields.
fn read_index(
    entry: &SourceEntry,
    pin: &IndexPin,
    fetch: &dyn Fetch,
) -> Result<(Index, index12::Index), String> {
    let attempt = |path: &str, gz: bool| {
        fetch_first(fetch, &pin.file_urls(path), MAX_INDEX_BYTES, &|bytes| {
            let index = entry.read(bytes, gz).map_err(|e| e.to_string())?;
            let json = if gz {
                let mut json = Vec::new();
                flate2::read::GzDecoder::new(bytes)
                    .take(MAX_INDEX_BYTES)
                    .read_to_end(&mut json)
                    .map_err(|e| e.to_string())?;
                json
            } else {
                bytes.to_vec()
            };
            let view = index12::Index::from_json(&json).map_err(|e| e.to_string())?;
            Ok((index, view))
        })
    };
    match entry.gz() {
        Some((gz_path, _, _)) => attempt(gz_path, true).or_else(|gz_err| {
            attempt(&entry.path, false)
                .map_err(|plain_err| format!("{gz_err}; plain fallback: {plain_err}"))
        }),
        None => attempt(&entry.path, false),
    }
}

struct Job {
    family: String,
    label: String,
    kind: Kind,
    sha256: String,
    size: u64,
    urls: Vec<String>,
    font: Option<FontFile>,
}

/// Adds one job per distinct blob of the index; `seen` spans all sources so shared blobs are fetched
/// once. A blob already seen from another source gains this source's URLs as extra mirrors.
fn collect_jobs(
    source: &str,
    index: &Index,
    seen: &mut HashMap<(Kind, String), usize>,
    jobs: &mut Vec<Job>,
) -> SourceReport {
    let mut report = SourceReport {
        source: source.to_string(),
        ..SourceReport::default()
    };
    let mut add = |job: Job| match seen.get(&(job.kind, job.sha256.clone())) {
        Some(&i) => {
            for url in job.urls {
                if !jobs[i].urls.contains(&url) {
                    jobs[i].urls.push(url);
                }
            }
        }
        None => {
            seen.insert((job.kind, job.sha256.clone()), jobs.len());
            jobs.push(job);
        }
    };
    for family in index.families() {
        report.families += 1;
        for file in &family.files {
            report.files += 1;
            add(Job {
                family: family.family.clone(),
                label: file.path.clone(),
                kind: Kind::File,
                sha256: file.sha256.clone(),
                size: file.size,
                urls: index.file_urls(family, file),
                font: Some(file.clone()),
            });
        }
        if let Some(lic) = &family.license_file {
            report.licences += 1;
            add(Job {
                family: family.family.clone(),
                label: lic.path.clone(),
                kind: Kind::Licence,
                sha256: lic.sha256.clone(),
                size: lic.size,
                urls: index.license_urls(family),
                font: None,
            });
        } else if family.license_path.is_some() {
            report.licences_unhashed += 1;
        }
    }
    report
}

enum Outcome {
    Skipped,
    Fetched,
    Failed(String),
}

fn check_body(job: &Job, bytes: &[u8]) -> Result<(), String> {
    if let Some(font) = &job.font {
        return verify_font(font, bytes).map_err(|e| e.to_string());
    }
    if bytes.len() as u64 != job.size {
        return Err(format!("size {} instead of {}", bytes.len(), job.size));
    }
    if sha256_hex(bytes) != job.sha256 {
        return Err("sha256 mismatch".into());
    }
    Ok(())
}

fn process(job: &Job, store: &Store, fetch: &dyn Fetch) -> Outcome {
    if store.has(job.kind, &job.sha256, job.size) {
        return Outcome::Skipped;
    }
    let max = if job.kind == Kind::File {
        job.size
    } else {
        job.size.min(MAX_LICENCE_BYTES)
    };
    let body = fetch_first(fetch, &job.urls, max, &|bytes| {
        check_body(job, bytes).map(|()| bytes.to_vec())
    });
    match body {
        Ok(bytes) => match store.put(job.kind, &job.sha256, &bytes) {
            Ok(()) => Outcome::Fetched,
            Err(e) => Outcome::Failed(format!("writing failed: {e}")),
        },
        Err(reason) => Outcome::Failed(reason),
    }
}

fn run_jobs(
    jobs: &[Job],
    store: &Store,
    fetch: &dyn Fetch,
    log: &(dyn Fn(&str) + Sync),
) -> (Totals, Vec<Failure>) {
    let next = AtomicUsize::new(0);
    let done = AtomicUsize::new(0);
    let results = Mutex::new((Totals::default(), Vec::new()));
    std::thread::scope(|scope| {
        for _ in 0..WORKERS {
            scope.spawn(|| {
                while let Some(job) = jobs.get(next.fetch_add(1, Ordering::Relaxed)) {
                    let outcome = process(job, store, fetch);
                    let n = done.fetch_add(1, Ordering::Relaxed) + 1;
                    if let Outcome::Failed(reason) = &outcome {
                        log(&format!("FAILED {} / {}: {reason}", job.family, job.label));
                    }
                    if n.is_multiple_of(LOG_EVERY) {
                        log(&format!("{n}/{} files handled", jobs.len()));
                    }
                    let Ok(mut guard) = results.lock() else {
                        return;
                    };
                    let (totals, failures) = &mut *guard;
                    match outcome {
                        Outcome::Skipped => totals.skipped += 1,
                        Outcome::Fetched => totals.fetched += 1,
                        Outcome::Failed(reason) => {
                            totals.failed += 1;
                            failures.push(Failure {
                                family: job.family.clone(),
                                file: job.label.clone(),
                                reason,
                            });
                        }
                    }
                }
            });
        }
    });
    results
        .into_inner()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// Runs the sync. `Err` means the index could not be read at all, or the generation could not be
/// written; per-file problems are in the returned manifest's `failures`, per-subset ones in its
/// `precompute` report.
pub fn run(
    cfg: &Config,
    fetch: &dyn Fetch,
    log: &(dyn Fn(&str) + Sync),
) -> Result<Manifest, String> {
    let store = Store::new(&cfg.fonts_dir);
    let _lock = lock(&store)?;
    store.remove_stale_tmp();
    for problem in store.remove_incomplete_generations() {
        log(&format!("WARNING {problem}"));
    }
    let pin = cfg.pin();
    let sources =
        read_sources(cfg, &pin, fetch).map_err(|e| format!("reading sources.json: {e}"))?;
    log(&format!(
        "sources.json lists {} indexes",
        sources.indexes.len()
    ));
    let mut seen = HashMap::new();
    let (mut reports, mut jobs, mut failures) = (Vec::new(), Vec::new(), Vec::new());
    let (mut seen_faces, mut faces) = (HashSet::new(), Vec::new());
    for entry in &sources.indexes {
        match read_index(entry, &pin, fetch) {
            Ok((index, view)) => {
                let report = collect_jobs(&entry.source, &index, &mut seen, &mut jobs);
                precompute::collect_faces(&view, &mut seen_faces, &mut faces);
                log(&format!(
                    "{}: {} families, {} files",
                    entry.source, report.families, report.files
                ));
                reports.push(report);
            }
            Err(reason) => {
                log(&format!("FAILED index {}: {reason}", entry.source));
                failures.push(Failure {
                    family: entry.source.clone(),
                    file: entry.path.clone(),
                    reason,
                });
            }
        }
    }
    let (totals, file_failures) = run_jobs(&jobs, &store, fetch, log);
    failures.extend(file_failures);
    let totals = Totals {
        failed: failures.len(),
        ..totals
    };
    let mut manifest = Manifest {
        index_repo: cfg.repo.clone(),
        index_commit: cfg.commit.clone(),
        sources_sha256: cfg.sources_sha256.clone(),
        sources: reports,
        totals,
        failures,
        generation: None,
        precompute: None,
        refused_switch: None,
        unchanged: false,
    };
    log(&format!(
        "fetch: {} fetched, {} already present, {} failed",
        manifest.totals.fetched, manifest.totals.skipped, manifest.totals.failed
    ));
    if !manifest.failures.is_empty() {
        write_manifest(&store.failed_manifest_path(), &manifest)?;
        log("fetch failures: no new generation; `current` is unchanged");
    } else if !cfg.tuning.force
        && let Some(name) = unchanged_generation(&store, &manifest)
    {
        log(&format!("unchanged: current -> {name} stays"));
        manifest.generation = Some(name);
        manifest.unchanged = true;
        store.remove_failed_manifest();
    } else {
        build_generation(&store, &faces, &mut manifest, &cfg.tuning, log)?;
    }
    Ok(manifest)
}

/// The generation `current` points to, when it exists, holds this index commit and sources, and
/// this run fetched nothing new.
fn unchanged_generation(store: &Store, manifest: &Manifest) -> Option<String> {
    if manifest.totals.fetched != 0 {
        return None;
    }
    let name = store.current_generation()?;
    if !store.generation_dir(&name).is_dir() {
        return None;
    }
    let bytes = std::fs::read(store.current_manifest_path()).ok()?;
    let current: serde_json::Value = serde_json::from_slice(&bytes).ok()?;
    (current["index_commit"] == manifest.index_commit.as_str()
        && current["sources_sha256"] == manifest.sources_sha256.as_str())
    .then_some(name)
}

fn log_report(report: &precompute::Report, log: &(dyn Fn(&str) + Sync)) {
    for f in &report.failures {
        log(&format!(
            "FAILED subset {} / {} {}: {}",
            f.family, f.sha256, f.inst, f.reason
        ));
    }
    for w in &report.warnings {
        log(&format!(
            "WARNING {} / {}: {}",
            w.family, w.sha256, w.reason
        ));
    }
    log(&format!(
        "precompute: {} faces, {} sample, {} ascii, {} var written, {} faces failed",
        report.faces,
        report.written.sample,
        report.written.ascii,
        report.written.var,
        report.failed_faces()
    ));
}

/// Why precompute failed too often to switch to its generation, if it did.
fn too_many_failures(report: &precompute::Report, max_percent: f64) -> Option<String> {
    let failed = report.failed_faces();
    let allowed = max_percent / 100.0 * report.faces as f64;
    (failed as f64 > allowed).then(|| {
        format!(
            "{failed} of {} faces failed precompute, more than {max_percent}%",
            report.faces
        )
    })
}

/// Precomputes the static subsets into a new generation, writes its manifest last, syncs it to
/// disk, switches `current` to it and removes old generations. With too many failed faces, it
/// removes the new generation instead and reports in `manifest.failed.json`.
fn build_generation(
    store: &Store,
    faces: &[Face],
    manifest: &mut Manifest,
    tuning: &Tuning,
    log: &(dyn Fn(&str) + Sync),
) -> Result<(), String> {
    let (name, dir) = store
        .new_generation(&manifest.index_commit)
        .map_err(|e| format!("creating a generation in {}: {e}", store.root().display()))?;
    let report = precompute::run(faces, store, &dir, tuning.limits());
    log_report(&report, log);
    let refused = too_many_failures(&report, tuning.max_failed_percent);
    manifest.precompute = Some(report);
    if let Some(reason) = refused {
        log(&format!("{reason}: `current` is unchanged"));
        if let Err(e) = std::fs::remove_dir_all(&dir) {
            log(&format!("WARNING removing {}: {e}", dir.display()));
        }
        manifest.refused_switch = Some(reason);
        return write_manifest(&store.failed_manifest_path(), manifest);
    }
    manifest.generation = Some(name.clone());
    write_manifest(&dir.join(MANIFEST), manifest)?;
    store
        .sync_tree(&dir)
        .map_err(|e| format!("syncing {}: {e}", dir.display()))?;
    let previous = store.current_generation();
    store
        .switch_current(&name)
        .map_err(|e| format!("switching current to {name}: {e}"))?;
    log(&format!("current -> {name}"));
    for warning in store.after_switch(&name, previous.as_deref(), KEPT_GENERATIONS) {
        log(&format!("WARNING {warning}"));
    }
    Ok(())
}

/// Generations kept besides the new one: servers may still read the previous one until they
/// reload, and the one before stays for rolling back by hand.
const KEPT_GENERATIONS: usize = 2;

/// Takes an exclusive lock on `FONTS_DIR/.sync.lock`, held until the returned file is dropped.
fn lock(store: &Store) -> Result<std::fs::File, String> {
    std::fs::create_dir_all(store.root())
        .map_err(|e| format!("creating {}: {e}", store.root().display()))?;
    let path = store.root().join(".sync.lock");
    let file = std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(&path)
        .map_err(|e| format!("opening {}: {e}", path.display()))?;
    match file.try_lock() {
        Ok(()) => Ok(file),
        Err(std::fs::TryLockError::WouldBlock) => Err("another sync is running".into()),
        Err(std::fs::TryLockError::Error(e)) => Err(format!("locking {}: {e}", path.display())),
    }
}

/// Writes `manifest` to `path` through a synced temporary file.
fn write_manifest(path: &Path, manifest: &Manifest) -> Result<(), String> {
    let json = serde_json::to_vec_pretty(manifest).map_err(|e| e.to_string())?;
    let tmp = path.with_extension(format!("json.{}.tmp", std::process::id()));
    let write = || -> std::io::Result<()> {
        let mut f = std::fs::File::create(&tmp)?;
        std::io::Write::write_all(&mut f, &json)?;
        f.sync_all()?;
        std::fs::rename(&tmp, path)
    };
    write().map_err(|e| {
        let _ = std::fs::remove_file(&tmp);
        format!("writing {}: {e}", path.display())
    })
}

/// The `sync` subcommand. Returns the process exit code.
pub fn main() -> i32 {
    let cfg = match Config::from_env() {
        Ok(cfg) => cfg,
        Err(e) => {
            eprintln!("sync: {e}");
            return 2;
        }
    };
    match run(&cfg, &HttpFetch::new(), &|line| println!("{line}")) {
        Ok(m) if m.succeeded() => 0,
        Ok(_) => 1,
        Err(e) => {
            eprintln!("sync: {e}");
            1
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    const COMMIT: &str = "0123456789abcdef0123456789abcdef01234567";

    fn env(pairs: &[(&str, &str)]) -> impl Fn(&str) -> Option<String> {
        let map: HashMap<String, String> = pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect();
        move |k| map.get(k).cloned()
    }

    fn good() -> Vec<(&'static str, String)> {
        vec![
            ("FONTS_DIR", "/data".into()),
            ("INDEX_COMMIT", COMMIT.into()),
            ("INDEX_SHA256", "a".repeat(64)),
        ]
    }

    fn lookup(pairs: &[(&str, String)]) -> Result<Config, String> {
        let owned: Vec<(&str, &str)> = pairs.iter().map(|(k, v)| (*k, v.as_str())).collect();
        Config::from_lookup(env(&owned))
    }

    #[test]
    fn config_defaults_the_repo() {
        let cfg = lookup(&good()).unwrap();
        assert_eq!(cfg.repo, DEFAULT_INDEX_REPO);
        assert_eq!(
            cfg.pin().file_urls("ofl/a b/X.ttf"),
            [
                format!(
                    "https://cdn.jsdelivr.net/gh/aiongg/craft-font-index@{COMMIT}/ofl/a%20b/X.ttf"
                ),
                format!(
                    "https://raw.githubusercontent.com/aiongg/craft-font-index/{COMMIT}/ofl/a%20b/X.ttf"
                ),
            ]
        );
    }

    #[test]
    fn config_refuses_missing_or_malformed_pins() {
        let without = |key: &str| {
            good()
                .into_iter()
                .filter(|(k, _)| *k != key)
                .collect::<Vec<_>>()
        };
        assert!(lookup(&without("INDEX_COMMIT")).is_err());
        assert!(lookup(&without("INDEX_SHA256")).is_err());
        assert!(lookup(&without("FONTS_DIR")).is_err());
        for (key, bad) in [
            ("INDEX_COMMIT", "main"),
            ("INDEX_COMMIT", "ABCDEF0123456789abcdef0123456789abcdef01"),
            ("INDEX_SHA256", "abc"),
            ("INDEX_REPO", "no-slash"),
            ("INDEX_REPO", "a/../b"),
            ("SYNC_THREADS", "0"),
            ("SYNC_MEMORY_MB", "lots"),
            ("SYNC_MAX_FAILED_PERCENT", "101"),
        ] {
            let mut pairs = without(key);
            pairs.push((key, bad.to_string()));
            assert!(lookup(&pairs).is_err(), "{key}={bad}");
        }
    }

    struct Failing;
    impl Fetch for Failing {
        fn get(&self, url: &str, _: u64) -> Result<Vec<u8>, String> {
            Err(format!("no route to {url}"))
        }
    }

    #[test]
    fn present_blobs_are_skipped_without_fetching() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::new(dir.path());
        let bytes = b"licence text";
        let sha = sha256_hex(bytes);
        store.put(Kind::Licence, &sha, bytes).unwrap();
        let job = Job {
            family: "f".into(),
            label: "OFL.txt".into(),
            kind: Kind::Licence,
            sha256: sha,
            size: bytes.len() as u64,
            urls: vec!["https://example.invalid/x".into()],
            font: None,
        };
        assert!(matches!(process(&job, &store, &Failing), Outcome::Skipped));
        let missing = Job {
            sha256: sha256_hex(b"other"),
            size: 5,
            ..job
        };
        assert!(matches!(
            process(&missing, &store, &Failing),
            Outcome::Failed(_)
        ));
    }
}
