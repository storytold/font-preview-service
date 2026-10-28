//! Route handlers and response headers.

use super::generation::{Face, Generation};
use super::metrics::inc;
use super::{App, Cached, Permit};
use crate::precompute::{self, ASCII, SAMPLE, VAR, static_path};
use crate::store::Kind;
use crate::subset::{self, Instance, Quality, closure, codepoints};
use axum::Router;
use axum::body::{Body, Bytes, HttpBody};
use axum::extract::{ConnectInfo, MatchedPath, Path, Request, State};
use axum::http::request::Parts;
use axum::http::{HeaderMap, HeaderValue, StatusCode, header};
use axum::middleware::{Next, from_fn_with_state};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use craft_font_store::sha256_hex;
use http_body::{Frame, SizeHint};
use skrifa::{FontRef, MetadataProvider};
use std::collections::BTreeSet;
use std::io::SeekFrom;
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};
use std::time::{Duration, Instant};
use tokio::io::{AsyncReadExt, AsyncSeekExt};
use tokio_util::io::ReaderStream;

const IMMUTABLE: &str = "public, max-age=31536000, immutable";
const WOFF2: &str = "font/woff2";
/// Downloads up to this size are brotli-compressed for clients that accept it.
const MAX_BR_FILE: u64 = 16 << 20;
/// Read size of streamed downloads.
const STREAM_CHUNK: usize = 64 << 10;

type AppState = State<Arc<App>>;

pub fn router(app: Arc<App>) -> Router {
    Router::new()
        .route("/healthz", get(|| async { "ok" }))
        .route("/readyz", get(readyz))
        .route("/v1/manifest.json", get(manifest))
        .route("/v1/files/{sha256}", get(file))
        .route("/v1/licences/{sha256}", get(licence))
        .route("/v1/faces/{sha256}/{inst}/sample-{file}", get(sample))
        .route("/v1/faces/{sha256}/{inst}/ascii.woff2", get(ascii))
        .route("/v1/faces/{sha256}/{inst}/u/{codepoints}", get(text))
        .fallback(|| async { not_found() })
        .layer(from_fn_with_state(app.clone(), track))
        .with_state(app)
}

/// Counts every request by route and status, and lets web builds read `/v1` responses.
async fn track(State(app): AppState, req: Request, next: Next) -> Response {
    let route = req
        .extensions()
        .get::<MatchedPath>()
        .map_or("other", MatchedPath::as_str)
        .to_owned();
    let path = req.uri().path();
    let v1 = path.starts_with("/v1/");
    // Axum decodes path segments, so an encoded form would get past the canonical-form checks;
    // canonical face URLs never contain `%`.
    let mut response = if path.starts_with("/v1/faces/") && path.contains('%') {
        error(
            StatusCode::BAD_REQUEST,
            "percent-encoding is not allowed in face URLs\n",
        )
    } else {
        next.run(req).await
    };
    if v1 {
        response.headers_mut().insert(
            header::ACCESS_CONTROL_ALLOW_ORIGIN,
            HeaderValue::from_static("*"),
        );
    }
    app.metrics.request(&route, response.status().as_u16());
    response
}

fn error(status: StatusCode, message: &'static str) -> Response {
    (
        status,
        [
            (header::CACHE_CONTROL, "no-store"),
            (header::CONTENT_TYPE, "text/plain; charset=utf-8"),
        ],
        message,
    )
        .into_response()
}

fn not_found() -> Response {
    error(StatusCode::NOT_FOUND, "not found\n")
}

fn busy() -> Response {
    let mut r = error(StatusCode::SERVICE_UNAVAILABLE, "busy\n");
    r.headers_mut()
        .insert(header::RETRY_AFTER, HeaderValue::from_static("1"));
    r
}

fn not_ready() -> Response {
    let mut r = error(StatusCode::SERVICE_UNAVAILABLE, "no generation loaded\n");
    r.headers_mut()
        .insert(header::RETRY_AFTER, HeaderValue::from_static("5"));
    r
}

async fn readyz(State(app): AppState) -> Response {
    if app.shutting_down() {
        return error(StatusCode::SERVICE_UNAVAILABLE, "shutting down\n");
    }
    match app.generation() {
        // Sync prunes old generations; if loading new ones keeps failing it can remove the one
        // being served, and its precomputed files are gone.
        Some(g) if !g.dir.is_dir() => error(
            StatusCode::SERVICE_UNAVAILABLE,
            "generation directory is gone\n",
        ),
        Some(_) => "ready".into_response(),
        None => not_ready(),
    }
}

async fn manifest(State(app): AppState) -> Response {
    let Some(generation) = app.generation() else {
        return not_ready();
    };
    (
        [
            (header::CONTENT_TYPE, "application/json"),
            (header::CACHE_CONTROL, "max-age=60"),
        ],
        generation.manifest.clone(),
    )
        .into_response()
}

/// Whether `If-None-Match` names `etag` (weak comparison, as RFC 9110 asks for this header).
fn matches(headers: &HeaderMap, etag: &str) -> bool {
    headers
        .get_all(header::IF_NONE_MATCH)
        .iter()
        .filter_map(|v| v.to_str().ok())
        .flat_map(|v| v.split(','))
        .map(|tag| tag.trim().trim_start_matches("W/"))
        .any(|tag| tag == "*" || tag == etag)
}

/// An immutable response, or 304 when the client already has it.
fn immutable(
    headers: &HeaderMap,
    body: impl Into<Body>,
    etag: &str,
    content_type: &'static str,
    extra: &[(header::HeaderName, &'static str)],
) -> Response {
    let not_modified = matches(headers, etag);
    let mut response = if not_modified {
        StatusCode::NOT_MODIFIED.into_response()
    } else {
        Response::new(body.into())
    };
    let h = response.headers_mut();
    h.insert(header::CACHE_CONTROL, HeaderValue::from_static(IMMUTABLE));
    if let Ok(v) = HeaderValue::from_str(etag) {
        h.insert(header::ETAG, v);
    }
    if !not_modified {
        h.insert(header::CONTENT_TYPE, HeaderValue::from_static(content_type));
    }
    for (name, value) in extra {
        // A 304 describes the representation without sending it.
        if not_modified && *name == header::CONTENT_ENCODING {
            continue;
        }
        h.insert(name.clone(), HeaderValue::from_static(value));
    }
    response
}

fn content_etag(bytes: &[u8]) -> String {
    format!("\"{}\"", &sha256_hex(bytes)[..32])
}

fn respond_cached(
    headers: &HeaderMap,
    cached: &Cached,
    extra: &[(header::HeaderName, &'static str)],
) -> Response {
    match cached {
        Cached::Ok {
            body,
            etag,
            content_type,
        } => immutable(
            headers,
            body.clone(),
            etag.to_str().unwrap_or_default(),
            content_type,
            extra,
        ),
        Cached::Failed { .. } => error(StatusCode::INTERNAL_SERVER_ERROR, "subsetting failed\n"),
    }
}

/// The client address: the rightmost entry of `CLIENT_IP_HEADER` when configured and parseable,
/// otherwise the peer.
fn client_ip(app: &App, req: &Parts) -> IpAddr {
    let from_header = app.config.client_ip_header.as_ref().and_then(|name| {
        let value = req.headers.get_all(name).iter().next_back()?;
        value.to_str().ok()?.rsplit(',').next()?.trim().parse().ok()
    });
    from_header
        .or_else(|| {
            req.extensions
                .get::<ConnectInfo<SocketAddr>>()
                .map(|c| c.0.ip())
        })
        .unwrap_or(IpAddr::V4(Ipv4Addr::UNSPECIFIED))
}

enum Want {
    /// The sample text, whose hash the URL names.
    Sample(String),
    Ascii,
    /// Canonical codepoint string and its codepoints.
    Text(String, Vec<u32>),
}

enum Which {
    List,
    Var,
    Named(Instance),
}

/// The `{inst}` of `face`, with the name it is cached under. Every value is the face itself for a
/// static face, so `var` is `list` there; named instances must match the derived string exactly.
fn resolve(face: &Face, inst: &str) -> Option<(Which, String)> {
    match inst {
        "list" => Some((Which::List, "list".into())),
        "var" if !face.variable => Some((Which::List, "list".into())),
        "var" => Some((Which::Var, "var".into())),
        name => face
            .named
            .get(name)
            .map(|i| (Which::Named(i.clone()), name.to_owned())),
    }
}

async fn sample(
    State(app): AppState,
    Path((sha, inst, file)): Path<(String, String, String)>,
    req: Parts,
) -> Response {
    let Some(hash) = file.strip_suffix(".woff2") else {
        return not_found();
    };
    face(app, sha, inst, Want::Sample(hash.to_owned()), req).await
}

async fn ascii(
    State(app): AppState,
    Path((sha, inst)): Path<(String, String)>,
    req: Parts,
) -> Response {
    face(app, sha, inst, Want::Ascii, req).await
}

async fn text(
    State(app): AppState,
    Path((sha, inst, file)): Path<(String, String, String)>,
    req: Parts,
) -> Response {
    let Some(list) = file.strip_suffix(".woff2") else {
        return not_found();
    };
    match codepoints::parse(list) {
        Ok(cps) => face(app, sha, inst, Want::Text(list.to_owned(), cps), req).await,
        Err(codepoints::ParseError::TooMany) => {
            error(StatusCode::BAD_REQUEST, "more than 256 codepoints\n")
        }
        Err(codepoints::ParseError::NotCanonical) => error(
            StatusCode::BAD_REQUEST,
            "codepoints are not in canonical form (sorted lowercase hex ranges, e.g. 20,41-5a,e9)\n",
        ),
    }
}

async fn face(app: Arc<App>, sha: String, inst: String, want: Want, req: Parts) -> Response {
    let Some(generation) = app.generation() else {
        return not_ready();
    };
    let Some(face) = generation.faces.get(&sha) else {
        return not_found();
    };
    let Some((which, inst_key)) = resolve(face, &inst) else {
        return not_found();
    };
    // A URL for another sample text, from before an index change, falls back to `u/`.
    if let Want::Sample(hash) = &want
        && face.sample_hash.as_ref() != Some(hash)
    {
        return not_found();
    }
    let precomputed = match (&want, &which) {
        (Want::Sample(_), Which::List) => Some((SAMPLE, face.has_sample)),
        (Want::Sample(_), Which::Var) => Some((VAR, face.has_var)),
        (Want::Ascii, Which::List) => Some((ASCII, face.has_ascii)),
        _ => None,
    };
    if let Some((name, present)) = precomputed {
        if !present {
            return not_found();
        }
        return match tokio::fs::read(static_path(&generation.dir, &sha, name)).await {
            Ok(bytes) => {
                let etag = content_etag(&bytes);
                immutable(&req.headers, Bytes::from(bytes), &etag, WOFF2, &[])
            }
            Err(e) => {
                eprintln!("reading {name} of {sha} in {}: {e}", generation.name);
                error(StatusCode::INTERNAL_SERVER_ERROR, "read failed\n")
            }
        };
    }
    let (cps, kind, intersect) = match want {
        Want::Ascii if !face.has_ascii => return not_found(),
        Want::Ascii => (
            codepoints::of_text(&precompute::ascii()),
            "ascii".to_owned(),
            false,
        ),
        Want::Sample(hash) => {
            let Some(text) = &face.sample else {
                return not_found();
            };
            let mut cps = codepoints::of_text(text);
            cps.extend(codepoints::of_text(&face.family));
            (cps, format!("sample-{hash}"), false)
        }
        Want::Text(canonical, cps) => (cps.into_iter().collect(), format!("u:{canonical}"), true),
    };
    let instance = match which {
        Which::List => face.list.clone(),
        Which::Var => Instance::Var,
        Which::Named(i) => i,
    };
    // Entries are derived from one generation's index, so a new generation never reads them.
    let job = Job {
        key: format!("{}/{sha}/{inst_key}/{kind}", generation.name),
        sha,
        instance,
        cps,
        intersect,
    };
    on_demand(app, generation, job, req).await
}

struct Job {
    key: String,
    sha: String,
    instance: Instance,
    cps: BTreeSet<u32>,
    /// Drop what the face's cmap does not map (after the closure).
    intersect: bool,
}

async fn on_demand(app: Arc<App>, generation: Arc<Generation>, job: Job, req: Parts) -> Response {
    if let Err(wait) = app.limiter.check(client_ip(&app, &req), Instant::now()) {
        let mut r = error(StatusCode::TOO_MANY_REQUESTS, "too many requests\n");
        let secs = wait.as_secs_f64().ceil().max(1.0) as u64;
        r.headers_mut().insert(header::RETRY_AFTER, secs.into());
        return r;
    }
    if let Some(cached) = app.cached(&job.key) {
        inc(&app.metrics.response_hits);
        return respond_cached(&req.headers, &cached, &[]);
    }
    inc(&app.metrics.response_misses);
    let Some(permit) = app.gate.acquire().await else {
        return busy();
    };
    let started = Instant::now();
    let worker = app.clone();
    let key = job.key.clone();
    let result = tokio::task::spawn_blocking(move || {
        let _permit = permit;
        compute(&worker, &generation, &job)
    })
    .await
    .unwrap_or_else(|e| Err((format!("subsetting panicked: {e}"), true)));
    app.metrics.on_demand(started.elapsed().as_secs_f64());
    let cached = match result {
        Ok(woff2) => Cached::Ok {
            etag: HeaderValue::from_str(&content_etag(&woff2))
                .unwrap_or(HeaderValue::from_static("\"\"")),
            body: Bytes::from(woff2),
            content_type: WOFF2,
        },
        Err((reason, keep)) => {
            inc(&app.metrics.subset_failures);
            eprintln!("on-demand {key}: {reason}");
            if !keep {
                return error(StatusCode::INTERNAL_SERVER_ERROR, "subsetting failed\n");
            }
            Cached::Failed { at: Instant::now() }
        }
    };
    let cached = Arc::new(cached);
    app.responses
        .put(key.clone(), cached.clone(), cached.weight(&key));
    respond_cached(&req.headers, &cached, &[])
}

/// Subsets one on-demand request. Blocking. The error says whether to cache it: subsetting errors
/// repeat, read errors may not.
fn compute(app: &App, generation: &Generation, job: &Job) -> Result<Vec<u8>, (String, bool)> {
    let face = generation
        .faces
        .get(&job.sha)
        .ok_or_else(|| ("face left the generation".to_owned(), false))?;
    let naming = face.naming.as_ref().map_err(|e| (e.clone(), true))?;
    let font = app
        .face_bytes(&job.sha, face.size)
        .map_err(|e| (format!("reading the font: {e}"), false))?;
    let cps: Vec<u32> = job.cps.iter().copied().collect();
    let mut set = closure(&cps);
    if job.intersect {
        let full = FontRef::new(&font).map_err(|e| (format!("cannot read font: {e}"), true))?;
        let charmap = full.charmap();
        set.retain(|&cp| charmap.map(cp).is_some());
    }
    subset::subset(&font, &set, &job.instance, naming, Quality::OnDemand)
        .map_err(|e| (e.to_string(), true))
}

async fn licence(State(app): AppState, Path(sha): Path<String>, req: Parts) -> Response {
    let Some(generation) = app.generation() else {
        return not_ready();
    };
    if !generation.licences.contains(&sha) {
        return not_found();
    }
    let etag = format!("\"{sha}\"");
    if matches(&req.headers, &etag) {
        return immutable(&req.headers, Bytes::new(), &etag, "", &[]);
    }
    match tokio::fs::read(app.store.path(Kind::Licence, &sha)).await {
        Ok(bytes) => immutable(
            &req.headers,
            Bytes::from(bytes),
            &etag,
            "text/plain; charset=utf-8",
            &[],
        ),
        Err(e) => {
            eprintln!("reading licence {sha}: {e}");
            error(StatusCode::INTERNAL_SERVER_ERROR, "read failed\n")
        }
    }
}

fn accepts_br(headers: &HeaderMap) -> bool {
    headers
        .get_all(header::ACCEPT_ENCODING)
        .iter()
        .filter_map(|v| v.to_str().ok())
        .flat_map(|v| v.split(','))
        .any(|item| {
            let mut parts = item.split(';').map(str::trim);
            parts.next().is_some_and(|c| c.eq_ignore_ascii_case("br"))
                && parts.all(|p| {
                    p.strip_prefix("q=")
                        .is_none_or(|q| q.parse::<f32>().is_ok_and(|q| q > 0.0))
                })
        })
}

fn font_type(bytes: &[u8]) -> &'static str {
    match bytes.get(..4) {
        Some(b"\0\x01\0\0" | b"true") => "font/ttf",
        Some(b"OTTO") => "font/otf",
        Some(b"ttcf") => "font/collection",
        Some(b"wOF2") => "font/woff2",
        Some(b"wOFF") => "font/woff",
        _ => "application/octet-stream",
    }
}

const VARY: (header::HeaderName, &str) = (header::VARY, "Accept-Encoding");
const BR_HEADERS: [(header::HeaderName, &str); 2] = [VARY, (header::CONTENT_ENCODING, "br")];

async fn file(State(app): AppState, Path(sha): Path<String>, req: Parts) -> Response {
    let Some(generation) = app.generation() else {
        return not_ready();
    };
    let Some(size) = generation.faces.get(&sha).map(|face| face.size) else {
        return not_found();
    };
    let headers = &req.headers;
    let br = accepts_br(headers) && size <= MAX_BR_FILE;
    let br_etag = format!("\"{sha}-br\"");
    if br && matches(headers, &br_etag) {
        return immutable(headers, Bytes::new(), &br_etag, "", &BR_HEADERS);
    }
    let etag = format!("\"{sha}\"");
    if matches(headers, &etag) {
        return immutable(headers, Bytes::new(), &etag, "", &[VARY]);
    }
    let Some(permit) = app.downloads.acquire().await else {
        return busy();
    };
    if br && let Some(response) = compressed_file(&app, &sha, br_etag, &req).await {
        return response.map(|body| hold(body, permit));
    }
    send_file(&app, &sha, etag, headers)
        .await
        .map(|body| hold(body, permit))
}

/// How long a download keeps its slot. hyper stops polling a body it cannot write, so a client
/// that stops reading would otherwise keep the slot as long as its connection stays open. After
/// this the download goes on without a slot; it then holds no more than its buffered chunks.
const DOWNLOAD_SLOT_LIMIT: Duration = Duration::from_secs(30);

/// `body` with its download slot, which goes back when the body is sent or dropped, or after
/// [`DOWNLOAD_SLOT_LIMIT`]. Each download in flight holds up to a read chunk plus hyper's write
/// buffer, so the slots bound their memory.
fn hold(body: Body, permit: Permit) -> Body {
    let (done, gone) = tokio::sync::oneshot::channel::<()>();
    tokio::spawn(async move {
        // `gone` resolves when `Held` drops its sender.
        let _ = tokio::time::timeout(DOWNLOAD_SLOT_LIMIT, gone).await;
        drop(permit);
    });
    Body::new(Held { body, _done: done })
}

struct Held {
    body: Body,
    _done: tokio::sync::oneshot::Sender<()>,
}

impl HttpBody for Held {
    type Data = Bytes;
    type Error = axum::Error;

    fn poll_frame(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Option<Result<Frame<Bytes>, axum::Error>>> {
        Pin::new(&mut self.body).poll_frame(cx)
    }

    fn is_end_stream(&self) -> bool {
        self.body.is_end_stream()
    }

    fn size_hint(&self) -> SizeHint {
        self.body.size_hint()
    }
}

async fn send_file(app: &App, sha: &str, etag: String, headers: &HeaderMap) -> Response {
    match open_file(app, sha).await {
        Ok((file, len, content_type)) => {
            let body = Body::from_stream(ReaderStream::with_capacity(file, STREAM_CHUNK));
            let mut response = immutable(headers, body, &etag, content_type, &[VARY]);
            response
                .headers_mut()
                .insert(header::CONTENT_LENGTH, len.into());
            response
        }
        Err(e) => {
            eprintln!("reading file {sha}: {e}");
            error(StatusCode::INTERNAL_SERVER_ERROR, "read failed\n")
        }
    }
}

/// The font file at its start, with its length and media type.
async fn open_file(app: &App, sha: &str) -> std::io::Result<(tokio::fs::File, u64, &'static str)> {
    let mut file = tokio::fs::File::open(app.store.path(Kind::File, sha)).await?;
    let len = file.metadata().await?.len();
    let mut head = [0u8; 4];
    let mut n = 0;
    while n < head.len() {
        match file.read(&mut head[n..]).await? {
            0 => break,
            read => n += read,
        }
    }
    file.seek(SeekFrom::Start(0)).await?;
    Ok((file, len, font_type(&head[..n])))
}

/// The brotli download, or `None` to send the file as it is. Compression waits for nobody and
/// spends the client's on-demand tokens: without the free compression slot or a token, or when
/// compressing fails, the download goes out uncompressed.
async fn compressed_file(app: &Arc<App>, sha: &str, etag: String, req: &Parts) -> Option<Response> {
    let headers = &req.headers;
    if let Some(cached) = app.files_br.get(sha) {
        inc(&app.metrics.files_br_hits);
        return Some(respond_cached(headers, &cached, &BR_HEADERS));
    }
    let permit = app.br_slot.clone().try_acquire_owned().ok()?;
    app.limiter
        .check(client_ip(app, req), Instant::now())
        .ok()?;
    inc(&app.metrics.files_br_misses);
    let path = app.store.path(Kind::File, sha);
    let worker = app.clone();
    let key = sha.to_owned();
    // Cached on the compression thread, so the work counts even when the client has gone.
    let compressed = app
        .br_thread
        .run(move || {
            let _permit = permit;
            let bytes = std::fs::read(path)?;
            let mut out = Vec::new();
            let params = brotli::enc::BrotliEncoderParams {
                quality: 5,
                size_hint: bytes.len(),
                ..Default::default()
            };
            brotli::BrotliCompress(&mut &bytes[..], &mut out, &params)?;
            let cached = Arc::new(Cached::Ok {
                body: Bytes::from(out),
                etag: HeaderValue::from_str(&etag).unwrap_or(HeaderValue::from_static("\"\"")),
                content_type: font_type(&bytes),
            });
            worker
                .files_br
                .put(key.clone(), cached.clone(), cached.weight(&key));
            Ok::<_, std::io::Error>(cached)
        })
        .await
        .and_then(|r| r);
    let cached = match compressed {
        Ok(cached) => cached,
        Err(e) => {
            eprintln!("compressing file {sha}: {e}");
            return None;
        }
    };
    Some(respond_cached(headers, &cached, &BR_HEADERS))
}
