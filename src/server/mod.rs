//! The HTTP server (architecture §3 and §7): precomputed and on-demand preview subsets, the file
//! and licence mirror, and the probes. It only reads `FONTS_DIR`.

mod cache;
mod config;
mod generation;
mod limit;
mod metrics;
mod routes;

pub use config::Config;
pub use generation::Generation;
pub use limit::{Gate, Permit};

use crate::store::{Kind, Store};
use axum::Router;
use axum::body::Bytes;
use axum::http::HeaderValue;
use cache::Lru;
use limit::RateLimiter;
use metrics::{Metrics, Summary, inc};
use std::net::SocketAddr;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, RwLock};
use std::time::{Duration, Instant};
use tokio::sync::{Semaphore, watch};

/// Font files larger than this are read for each use instead of kept in the face cache.
const MAX_CACHED_FACE: u64 = 64 << 20;
const POLL: Duration = Duration::from_secs(30);
const SUMMARY: Duration = Duration::from_secs(300);
const DRAIN: Duration = Duration::from_secs(10);
/// How long a failed subset is answered from the cache before it is tried again.
const FAILED_TTL: Duration = Duration::from_secs(300);

/// An on-demand result or compressed download in the response cache.
pub(crate) enum Cached {
    Ok {
        body: Bytes,
        etag: HeaderValue,
        content_type: &'static str,
    },
    /// The subset failed at `at`; kept for [`FAILED_TTL`] so the same request is not computed
    /// again at once.
    Failed { at: Instant },
}

impl Cached {
    fn weight(&self, key: &str) -> u64 {
        let body = match self {
            Cached::Ok { body, .. } => body.len(),
            Cached::Failed { .. } => 0,
        };
        (body + key.len() + 128) as u64
    }

    fn fresh(&self) -> bool {
        match self {
            Cached::Ok { .. } => true,
            Cached::Failed { at } => at.elapsed() < FAILED_TTL,
        }
    }
}

type BrJob = Box<dyn FnOnce() + Send>;

/// The one long-lived thread that brotli-compresses `/v1/files` downloads. A compression holds
/// the whole file, its output and the encoder's tables. On the blocking pool each one could run
/// on another thread, and glibc kept every thread's malloc arena at that peak: anonymous memory
/// grew past 600 MiB within 15 s of 1,000 concurrent downloads. One thread keeps one arena.
struct BrThread(std::sync::mpsc::Sender<BrJob>);

impl BrThread {
    fn spawn() -> std::io::Result<Self> {
        let (jobs, rx) = std::sync::mpsc::channel::<BrJob>();
        std::thread::Builder::new()
            .name("files-br".into())
            .spawn(move || {
                for job in rx {
                    // A panic drops the job's reply; that download goes out uncompressed.
                    let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(job));
                }
            })?;
        Ok(Self(jobs))
    }

    async fn run<T: Send + 'static>(
        &self,
        job: impl FnOnce() -> T + Send + 'static,
    ) -> std::io::Result<T> {
        let (reply, result) = tokio::sync::oneshot::channel();
        self.0
            .send(Box::new(move || {
                let _ = reply.send(job());
            }))
            .map_err(|_| std::io::Error::other("the compression thread has stopped"))?;
        result
            .await
            .map_err(|_| std::io::Error::other("compression panicked"))
    }
}

pub struct App {
    config: Config,
    store: Store,
    current: RwLock<Option<Arc<Generation>>>,
    last_error: Mutex<Option<String>>,
    /// Serialises reloads so an older load finishing last cannot replace a newer generation.
    reload_lock: Mutex<()>,
    /// Font files for the subsetter.
    faces: Lru<Bytes>,
    responses: Lru<Arc<Cached>>,
    /// Brotli-compressed `/v1/files` downloads, the one slot that compresses them, and the
    /// thread that does.
    files_br: Lru<Arc<Cached>>,
    br_slot: Arc<Semaphore>,
    br_thread: BrThread,
    limiter: RateLimiter,
    gate: Gate,
    /// `/v1/files` downloads in flight.
    downloads: Gate,
    metrics: Metrics,
    stopping: AtomicBool,
}

impl App {
    /// Fails only when the compression thread cannot start.
    pub fn new(config: Config) -> std::io::Result<Arc<Self>> {
        Ok(Arc::new(Self {
            store: Store::new(&config.fonts_dir),
            current: RwLock::new(None),
            last_error: Mutex::new(None),
            reload_lock: Mutex::new(()),
            faces: Lru::new(config.face_cache, config.face_cache_bytes),
            responses: Lru::new(usize::MAX, config.response_cache_bytes),
            files_br: Lru::new(usize::MAX, config.files_br_cache_bytes),
            br_slot: Arc::new(Semaphore::new(1)),
            br_thread: BrThread::spawn()?,
            limiter: RateLimiter::new(config.rate_per_s, config.burst),
            gate: Gate::new(config.concurrency, config.queue),
            downloads: Gate::new(config.files_concurrency, config.files_queue),
            metrics: Metrics::default(),
            stopping: AtomicBool::new(false),
            config,
        }))
    }

    /// The generation requests are served from.
    pub fn generation(&self) -> Option<Arc<Generation>> {
        self.current
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }

    /// The subsetting slots of the on-demand routes.
    pub fn gate(&self) -> &Gate {
        &self.gate
    }

    /// Marks the server as shutting down: `/readyz` answers 503 from now on.
    pub fn begin_shutdown(&self) {
        self.stopping.store(true, Ordering::Release);
    }

    fn shutting_down(&self) -> bool {
        self.stopping.load(Ordering::Acquire)
    }

    /// A response-cache entry, unless it is a failure older than [`FAILED_TTL`].
    fn cached(&self, key: &str) -> Option<Arc<Cached>> {
        self.responses.get(key).filter(|c| c.fresh())
    }

    /// Resolves `current` once and loads that generation if it is not the one being served.
    /// Returns whether it switched. On failure the loaded generation stays; the error is logged
    /// when it differs from the previous one.
    pub fn reload(&self) -> Result<bool, String> {
        let result = self.try_reload();
        let mut last = self
            .last_error
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        match &result {
            Err(e) => {
                inc(&self.metrics.generation_load_failures);
                if last.as_ref() != Some(e) {
                    eprintln!("generation not loaded: {e}");
                    *last = Some(e.clone());
                }
            }
            Ok(_) => *last = None,
        }
        result
    }

    fn try_reload(&self) -> Result<bool, String> {
        let _serial = self
            .reload_lock
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let name = self.store.current_generation().ok_or_else(|| {
            format!(
                "{} is not a symlink to a generation",
                self.store.current_link().display()
            )
        })?;
        if self.generation().is_some_and(|g| g.name == name) {
            return Ok(false);
        }
        let limits = generation::Limits {
            max_codepoints: crate::subset::codepoints::MAX_CODEPOINTS,
            rate_per_s: self.config.rate_per_s,
            burst: self.config.burst,
        };
        let loaded = generation::load(&self.store, &name, &limits)?;
        println!(
            "loaded generation {name}: {} faces, {} licences",
            loaded.faces.len(),
            loaded.licences.len()
        );
        self.metrics.generation(&name, loaded.faces.len());
        *self
            .current
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(Arc::new(loaded));
        Ok(true)
    }

    /// The API routes.
    pub fn router(self: &Arc<Self>) -> Router {
        routes::router(self.clone())
    }

    /// `/metrics`, for the metrics port only.
    pub fn metrics_router(self: &Arc<Self>) -> Router {
        let app = self.clone();
        Router::new().route(
            "/metrics",
            axum::routing::get(move || {
                let app = app.clone();
                async move {
                    let (fe, fb) = app.faces.size();
                    let (re, rb) = app.responses.size();
                    let (be, bb) = app.files_br.size();
                    (
                        [("content-type", "text/plain; version=0.0.4")],
                        app.metrics.render(&[
                            ("face", fe, fb),
                            ("response", re, rb),
                            ("files_br", be, bb),
                        ]),
                    )
                }
            }),
        )
    }

    /// The font file `sha256` for the subsetter, from the face cache or disk. Blocking; callers
    /// hold a gate slot, so at most `ONDEMAND_CONCURRENCY` reads run at once.
    fn face_bytes(&self, sha256: &str, size: u64) -> std::io::Result<Bytes> {
        if let Some(bytes) = self.faces.get(sha256) {
            inc(&self.metrics.face_hits);
            return Ok(bytes);
        }
        inc(&self.metrics.face_misses);
        let bytes = Bytes::from(std::fs::read(self.store.path(Kind::File, sha256))?);
        if size <= MAX_CACHED_FACE {
            self.faces
                .put(sha256.to_owned(), bytes.clone(), bytes.len() as u64);
        }
        Ok(bytes)
    }

    fn log_summary(&self, previous: &mut Summary) {
        let now = self.metrics.totals();
        let delta = Summary {
            total: now.total - previous.total,
            limited: now.limited - previous.limited,
            busy: now.busy - previous.busy,
            server_errors: now.server_errors - previous.server_errors,
        };
        if delta.total > 0 {
            let (re, rb) = self.responses.size();
            let (fe, _) = self.faces.size();
            println!(
                "last {} s: {} requests, {} rate-limited (429), {} busy (503), {} server errors; \
                 response cache {re} entries, {:.1} MB; face cache {fe} faces",
                SUMMARY.as_secs(),
                delta.total,
                delta.limited,
                delta.busy,
                delta.server_errors,
                rb as f64 / f64::from(1 << 20),
            );
        }
        *previous = now;
        self.limiter.prune(Instant::now());
    }
}

async fn reload_in_background(app: &Arc<App>) {
    let app = app.clone();
    let _ = tokio::task::spawn_blocking(move || app.reload()).await;
}

async fn stopped(mut rx: watch::Receiver<bool>) {
    let _ = rx.wait_for(|stop| *stop).await;
}

/// SIGTERM and SIGINT stop the server; SIGHUP reloads at once.
async fn signals(app: Arc<App>, stop: watch::Sender<bool>) {
    use tokio::signal::unix::{SignalKind, signal};
    let (Ok(mut term), Ok(mut int), Ok(mut hup)) = (
        signal(SignalKind::terminate()),
        signal(SignalKind::interrupt()),
        signal(SignalKind::hangup()),
    ) else {
        // Dropping `stop` would start the shutdown, so the server runs on without signals.
        eprintln!("cannot install signal handlers; SIGTERM, SIGINT and SIGHUP are not handled");
        std::future::pending::<()>().await;
        drop(stop);
        return;
    };
    loop {
        tokio::select! {
            _ = term.recv() => break,
            _ = int.recv() => break,
            _ = hup.recv() => reload_in_background(&app).await,
        }
    }
    println!(
        "stopping: draining open requests for up to {} s",
        DRAIN.as_secs()
    );
    app.begin_shutdown();
    let _ = stop.send(true);
}

async fn housekeeping(app: Arc<App>) {
    let mut poll = tokio::time::interval(POLL);
    let mut summary = tokio::time::interval(SUMMARY);
    poll.tick().await;
    summary.tick().await;
    let mut previous = Summary::default();
    loop {
        tokio::select! {
            _ = poll.tick() => reload_in_background(&app).await,
            _ = summary.tick() => app.log_summary(&mut previous),
        }
    }
}

/// How long a client may take to send its request headers.
const HEADER_READ_TIMEOUT: Duration = Duration::from_secs(10);

/// Serves `router` until `stop` fires, then stops accepting and waits for open connections.
/// `axum::serve` sets no connection timers, so a client that sends its headers slowly would
/// otherwise hold a connection forever.
async fn serve(
    listener: tokio::net::TcpListener,
    router: Router,
    stop: watch::Receiver<bool>,
) -> std::io::Result<()> {
    use hyper_util::rt::{TokioExecutor, TokioIo, TokioTimer};
    use hyper_util::server::conn::auto::Builder;
    use hyper_util::server::graceful::GracefulShutdown;
    use hyper_util::service::TowerToHyperService;

    let mut builder = Builder::new(TokioExecutor::new());
    builder
        .http1()
        .timer(TokioTimer::new())
        .header_read_timeout(HEADER_READ_TIMEOUT);
    builder
        .http2()
        .timer(TokioTimer::new())
        .keep_alive_interval(Duration::from_secs(30))
        .keep_alive_timeout(Duration::from_secs(20));
    let graceful = GracefulShutdown::new();
    let stopping = stopped(stop);
    tokio::pin!(stopping);
    loop {
        let (stream, peer) = tokio::select! {
            () = &mut stopping => break,
            accepted = listener.accept() => match accepted {
                Ok(conn) => conn,
                Err(e) => {
                    // Typically file-descriptor exhaustion; back off instead of spinning.
                    eprintln!("accept failed: {e}");
                    tokio::time::sleep(Duration::from_millis(100)).await;
                    continue;
                }
            },
        };
        let service = TowerToHyperService::new(
            router
                .clone()
                .layer(axum::Extension(axum::extract::ConnectInfo(peer))),
        );
        let conn = builder.serve_connection_with_upgrades(TokioIo::new(stream), service);
        let conn = graceful.watch(conn.into_owned());
        tokio::spawn(async move {
            let _ = conn.await;
        });
    }
    drop(listener);
    graceful.shutdown().await;
    Ok(())
}

async fn run(config: Config) -> i32 {
    let app = match App::new(config.clone()) {
        Ok(app) => app,
        Err(e) => {
            eprintln!("cannot start the compression thread: {e}");
            return 1;
        }
    };
    reload_in_background(&app).await;
    let addr = SocketAddr::from(([0, 0, 0, 0], config.port));
    let metrics_addr = SocketAddr::from(([0, 0, 0, 0], config.metrics_port));
    let (listener, metrics_listener) = match (
        tokio::net::TcpListener::bind(addr).await,
        tokio::net::TcpListener::bind(metrics_addr).await,
    ) {
        (Ok(a), Ok(m)) => (a, m),
        (Err(e), _) => {
            eprintln!("cannot listen on {addr}: {e}");
            return 1;
        }
        (_, Err(e)) => {
            eprintln!("cannot listen on {metrics_addr}: {e}");
            return 1;
        }
    };
    println!("font-preview-service listening on {addr}, metrics on {metrics_addr}");
    let (stop_tx, stop_rx) = watch::channel(false);
    tokio::spawn(signals(app.clone(), stop_tx));
    tokio::spawn(housekeeping(app.clone()));
    let metrics = serve(metrics_listener, app.metrics_router(), stop_rx.clone());
    tokio::spawn(metrics);
    let server = serve(listener, app.router(), stop_rx.clone());
    let mut task = tokio::spawn(server);
    tokio::select! {
        result = &mut task => {
            if let Ok(Err(e)) | Err(e) = result.map_err(std::io::Error::other) {
                eprintln!("server error: {e}");
                return 1;
            }
        }
        () = stopped(stop_rx) => {
            if tokio::time::timeout(DRAIN, &mut task).await.is_err() {
                eprintln!("open requests still running after {} s; exiting", DRAIN.as_secs());
            }
        }
    }
    println!("stopped");
    0
}

/// Runs the server until SIGTERM or SIGINT. Returns the process exit code.
pub fn main() -> i32 {
    let config = match Config::from_env() {
        Ok(cfg) => cfg,
        Err(e) => {
            eprintln!("font-preview-service: {e}");
            return 2;
        }
    };
    let runtime = match tokio::runtime::Runtime::new() {
        Ok(rt) => rt,
        Err(e) => {
            eprintln!("cannot start the runtime: {e}");
            return 1;
        }
    };
    let code = runtime.block_on(run(config));
    // Subsets still running after the drain are abandoned.
    runtime.shutdown_timeout(Duration::from_secs(1));
    code
}
