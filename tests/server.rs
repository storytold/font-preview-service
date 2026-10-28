mod common;

use axum::body::{Body, Bytes};
use axum::http::{HeaderMap, Request, StatusCode};
use common::{COMMIT, Local, config, file_entry, fixture_from, serve};
use craft_font_store::sha256_hex;
use font_preview_service::server::{App, Config};
use font_preview_service::subset::codepoints;
use font_preview_service::sync::run;
use serde_json::{Value, json};
use skrifa::{FontRef, MetadataProvider};
use std::path::{Path, PathBuf};
use std::sync::{Arc, OnceLock};
use std::time::Duration;
use tower::ServiceExt;

const ABEEZEE: &[u8] = include_bytes!("data/abeezee/ABeeZee-Regular.ttf");
const SARABUN: &[u8] = include_bytes!("data/sarabun/Sarabun-Regular.ttf");
const ORBITRON: &[u8] = include_bytes!("data/orbitron/Orbitron[wght].ttf");
const OFL: &[u8] = include_bytes!("data/abeezee/OFL.txt");
const NOTICE: &[u8] = b"Notice for the Sarabun fixture.\n";
const THAI: &str = "โดยที่การไม่นำพาและการหมิ่นในคุณค่าของสิทธิมนุษยชน";
const LATIN: &str = "Whereas recognition of the inherent dignity";

fn samples() -> Value {
    json!({
        "Latn": {"text": LATIN, "method": "udhr", "language": "en_Latn"},
        "Thai": {"text": THAI, "method": "udhr", "language": "th_Thai"},
    })
}

fn sha(bytes: &[u8]) -> String {
    sha256_hex(bytes)
}

/// ABeeZee (static, with a licence file), Orbitron (variable, Regular and Bold) and Sarabun (Thai,
/// with a notice file).
fn fixture() -> common::Fixture {
    let mut orbitron = file_entry("ofl/orbitron/Orbitron[wght].ttf", ORBITRON);
    orbitron["axes"] = json!([{"tag": "wght", "min": 400.0, "max": 900.0}]);
    orbitron["instances"] = json!([{"name": "Regular", "coordinates": {"wght": 400.0}},
                                   {"name": "Bold", "coordinates": {"wght": 700.0}}]);
    orbitron["list_instance"] = json!(0);
    let families = json!([
        {"family": "ABeeZee", "license": "OFL", "reserved_names": [], "sample_script": "Latn",
         "license_file": {"path": "ofl/abeezee/OFL.txt", "size": OFL.len(), "sha256": sha(OFL)},
         "files": [file_entry("ofl/abeezee/ABeeZee-Regular.ttf", ABEEZEE)]},
        {"family": "Orbitron", "license": "OFL", "reserved_names": [], "sample_script": "Latn",
         "files": [orbitron]},
        {"family": "Sarabun", "license": "OFL", "reserved_names": [], "sample_script": "Thai",
         "notice_file": {"path": "ofl/sarabun/NOTICE", "size": NOTICE.len(), "sha256": sha(NOTICE)},
         "files": [file_entry("ofl/sarabun/Sarabun-Regular.ttf", SARABUN)]},
    ]);
    fixture_from(
        families,
        vec![
            ("ofl/abeezee/ABeeZee-Regular.ttf", ABEEZEE.to_vec()),
            ("ofl/abeezee/OFL.txt", OFL.to_vec()),
            ("ofl/orbitron/Orbitron[wght].ttf", ORBITRON.to_vec()),
            ("ofl/sarabun/Sarabun-Regular.ttf", SARABUN.to_vec()),
            ("ofl/sarabun/NOTICE", NOTICE.to_vec()),
        ],
        samples(),
    )
}

/// One synced volume for the tests that only read it.
fn volume() -> &'static Path {
    static VOLUME: OnceLock<(tempfile::TempDir, PathBuf)> = OnceLock::new();
    &VOLUME
        .get_or_init(|| {
            let fx = fixture();
            let port = serve(fx.server.clone());
            let dir = tempfile::tempdir().unwrap();
            let m = run(
                &config(dir.path(), &fx.sources_sha256),
                &Local(port),
                &|_| {},
            )
            .unwrap();
            assert!(m.succeeded(), "{:?} {:?}", m.failures, m.precompute);
            let path = dir.path().to_path_buf();
            (dir, path)
        })
        .1
}

fn app_with(cfg: Config) -> Arc<App> {
    let app = App::new(cfg).unwrap();
    assert!(app.reload().unwrap());
    app
}

fn app() -> Arc<App> {
    app_with(Config::new(volume()))
}

async fn get(
    app: &Arc<App>,
    uri: &str,
    headers: &[(&str, &str)],
) -> (StatusCode, HeaderMap, Bytes) {
    let mut req = Request::builder().uri(uri);
    for (k, v) in headers {
        req = req.header(*k, *v);
    }
    let response = app
        .router()
        .oneshot(req.body(Body::empty()).unwrap())
        .await
        .unwrap();
    let status = response.status();
    let headers = response.headers().clone();
    let body = axum::body::to_bytes(response.into_body(), 64 << 20)
        .await
        .unwrap();
    (status, headers, body)
}

fn face_url(font: &[u8], inst: &str, file: &str) -> String {
    format!("/v1/faces/{}/{inst}/{file}", sha(font))
}

/// The sample file name for a face whose sample text is `text`.
fn sample(text: &str) -> String {
    format!("sample-{}.woff2", &sha(text.as_bytes())[..12])
}

#[tokio::test]
async fn healthz_returns_ok() {
    let app = App::new(Config::new("/nonexistent")).unwrap();
    let (status, _, body) = get(&app, "/healthz", &[]).await;
    assert_eq!(status, 200);
    assert_eq!(&body[..], b"ok");
}

#[tokio::test]
async fn sample_is_immutable_with_an_etag() {
    let app = app();
    let url = face_url(ABEEZEE, "list", &sample(LATIN));
    let (status, headers, body) = get(&app, &url, &[]).await;
    assert_eq!(status, 200);
    assert_eq!(headers["content-type"], "font/woff2");
    assert_eq!(
        headers["cache-control"],
        "public, max-age=31536000, immutable"
    );
    assert_eq!(headers["access-control-allow-origin"], "*");
    assert!(wuff::decompress_woff2(&body).is_ok());
    let etag = headers["etag"].to_str().unwrap().to_owned();
    assert!(etag.starts_with('"'), "strong: {etag}");

    let (status, headers, body) = get(&app, &url, &[("if-none-match", &etag)]).await;
    assert_eq!(status, 304);
    assert_eq!(headers["etag"], etag.as_str());
    assert!(body.is_empty());

    // For a static face every instance is the face itself.
    let (status, headers, _) = get(&app, &face_url(ABEEZEE, "var", &sample(LATIN)), &[]).await;
    assert_eq!(
        (status, headers["etag"].to_str().unwrap()),
        (StatusCode::OK, etag.as_str())
    );
    let (status, ..) = get(&app, &face_url(ABEEZEE, "wght400", &sample(LATIN)), &[]).await;
    assert_eq!(status, 404);
}

#[tokio::test]
async fn unknown_faces_and_bad_codepoints_are_refused() {
    let app = app();
    let unknown = "0".repeat(64);
    for uri in [
        format!("/v1/faces/{unknown}/list/{}", sample(LATIN)),
        face_url(ABEEZEE, "list", "sample.woff2"),
        face_url(ABEEZEE, "list", &sample(THAI)),
        face_url(SARABUN, "list", &sample(LATIN)),
        face_url(ABEEZEE, "list", "sample-.woff2"),
        format!("/v1/files/{unknown}"),
        format!("/v1/licences/{}", sha(ABEEZEE)),
        face_url(ABEEZEE, "list", "u/41.ttf"),
        "/v1/nothing".to_owned(),
    ] {
        let (status, headers, _) = get(&app, &uri, &[]).await;
        assert_eq!(status, 404, "{uri}");
        assert_eq!(headers["cache-control"], "no-store");
    }
    for cps in ["41,20", "0041", "4A", "41-42,43", "100-200"] {
        let (status, ..) = get(
            &app,
            &face_url(ABEEZEE, "list", &format!("u/{cps}.woff2")),
            &[],
        )
        .await;
        assert_eq!(status, 400, "{cps}");
    }
    let (status, headers, _) = get(&app, &face_url(SARABUN, "list", &sample(THAI)), &[]).await;
    assert_eq!(status, 200);
    assert_eq!(
        headers["cache-control"],
        "public, max-age=31536000, immutable"
    );
    for file in [
        &sample(LATIN),
        "u/41.woff2",
        "u/%34%31.woff2",
        "ascii%2ewoff2",
    ] {
        let encoded = face_url(ABEEZEE, "list", file).replace("/list/", "/l%69st/");
        let (status, headers, _) = get(&app, &encoded, &[]).await;
        assert_eq!(status, 400, "{encoded}");
        assert_eq!(headers["access-control-allow-origin"], "*");
    }
}

#[tokio::test]
async fn named_instances_are_accepted_in_their_exact_form_only() {
    let app = app();
    let precomputed = get(&app, &face_url(ORBITRON, "var", &sample(LATIN)), &[]).await;
    assert_eq!(precomputed.0, 200);
    let ttf = wuff::decompress_woff2(&precomputed.2).unwrap();
    assert!(
        !FontRef::new(&ttf).unwrap().axes().is_empty(),
        "var keeps fvar"
    );

    let (status, _, body) = get(&app, &face_url(ORBITRON, "wght700", &sample(LATIN)), &[]).await;
    assert_eq!(status, 200);
    let ttf = wuff::decompress_woff2(&body).unwrap();
    assert!(
        FontRef::new(&ttf).unwrap().axes().is_empty(),
        "pinned to Bold"
    );
    for inst in ["wght700.0", "wght600", "WGHT700"] {
        let (status, ..) = get(&app, &face_url(ORBITRON, inst, &sample(LATIN)), &[]).await;
        assert_eq!(status, 404, "{inst}");
    }
    let (status, ..) = get(
        &app,
        &face_url(ORBITRON, "wght%37%30%30", &sample(LATIN)),
        &[],
    )
    .await;
    assert_eq!(status, 400, "an encoded form of wght700");
    let (status, ..) = get(&app, &face_url(ORBITRON, "wght400", "ascii.woff2"), &[]).await;
    assert_eq!(status, 200);
}

#[tokio::test]
async fn on_demand_subsets_map_the_requested_codepoints() {
    let app = app();
    // U+4E00 is not in Sarabun; the server drops it.
    let cps = codepoints::canonical(&format!("{THAI}一"));
    let url = face_url(SARABUN, "list", &format!("u/{cps}.woff2"));
    let (status, headers, body) = get(&app, &url, &[]).await;
    assert_eq!(status, 200);
    assert_eq!(
        headers["cache-control"],
        "public, max-age=31536000, immutable"
    );
    let ttf = wuff::decompress_woff2(&body).unwrap();
    let font = FontRef::new(&ttf).unwrap();
    let charmap = font.charmap();
    for c in THAI.chars() {
        assert!(charmap.map(c).is_some(), "{c:?}");
    }
    assert!(charmap.map('一').is_none());
    assert!(charmap.map('A').is_none(), "only what was asked for");
    // The second request comes from the response cache, with the same ETag.
    let (status, again, _) = get(&app, &url, &[]).await;
    assert_eq!((status, &again["etag"]), (StatusCode::OK, &headers["etag"]));
}

#[tokio::test]
async fn on_demand_limits_answer_with_retry_after() {
    let mut cfg = Config::new(volume());
    cfg.concurrency = 1;
    cfg.queue = 0;
    let app = app_with(cfg.clone());
    let held = app.gate().try_acquire().unwrap();
    let url = face_url(ABEEZEE, "list", "u/41-43.woff2");
    let (status, headers, _) = get(&app, &url, &[]).await;
    assert_eq!(status, 503);
    assert_eq!(headers["retry-after"], "1");
    // Precomputed files do not need a slot.
    let (status, ..) = get(&app, &face_url(ABEEZEE, "list", "ascii.woff2"), &[]).await;
    assert_eq!(status, 200);
    drop(held);
    assert_eq!(get(&app, &url, &[]).await.0, 200);

    cfg.burst = 2.0;
    cfg.rate_per_s = 0.5;
    cfg.client_ip_header = Some("x-forwarded-for".parse().unwrap());
    let app = app_with(cfg);
    let client = |ip: &'static str| [("x-forwarded-for", ip)];
    for _ in 0..2 {
        assert_eq!(get(&app, &url, &client("10.0.0.1, 192.0.2.1")).await.0, 200);
    }
    let (status, headers, _) = get(&app, &url, &client("10.0.0.2, 192.0.2.1")).await;
    assert_eq!(status, 429, "the rightmost entry is the client");
    assert_eq!(headers["retry-after"], "2");
    assert_eq!(get(&app, &url, &client("192.0.2.2")).await.0, 200);
}

#[tokio::test]
async fn files_and_licences_are_mirrored() {
    let app = app();
    let url = format!("/v1/files/{}", sha(ABEEZEE));
    let (status, headers, body) = get(&app, &url, &[]).await;
    assert_eq!(status, 200);
    assert_eq!(&body[..], ABEEZEE);
    assert_eq!(headers["content-type"], "font/ttf");
    assert_eq!(
        headers["content-length"],
        ABEEZEE.len().to_string().as_str()
    );
    assert_eq!(headers["vary"], "Accept-Encoding");
    assert!(headers.get("content-encoding").is_none());

    let (status, headers, body) = get(&app, &url, &[("accept-encoding", "gzip, br")]).await;
    assert_eq!(status, 200);
    assert_eq!(headers["content-encoding"], "br");
    let mut plain = Vec::new();
    brotli::BrotliDecompress(&mut &body[..], &mut plain).unwrap();
    assert_eq!(plain, ABEEZEE);
    let (status, ..) = get(&app, &url, &[("accept-encoding", "br;q=0")]).await;
    assert_eq!(status, 200);
    let etag = headers["etag"].to_str().unwrap();
    let (status, headers, _) = get(
        &app,
        &url,
        &[("accept-encoding", "br"), ("if-none-match", etag)],
    )
    .await;
    assert_eq!(status, 304);
    assert!(headers.get("content-encoding").is_none());
    assert_eq!(headers["vary"], "Accept-Encoding");

    for (text, file) in [(OFL, sha(OFL)), (NOTICE, sha(NOTICE))] {
        let (status, headers, body) = get(&app, &format!("/v1/licences/{file}"), &[]).await;
        assert_eq!(status, 200);
        assert_eq!(headers["content-type"], "text/plain; charset=utf-8");
        assert_eq!(&body[..], text);
    }

    let (status, headers, body) = get(&app, "/v1/manifest.json", &[]).await;
    assert_eq!(status, 200);
    assert_eq!(headers["cache-control"], "max-age=60");
    let manifest: Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(manifest["index"]["commit"], COMMIT);
    assert_eq!(manifest["faces"], 3);
    assert_eq!(manifest["limits"]["max_codepoints"], 256);
}

#[tokio::test]
async fn readiness_follows_the_loaded_generation() {
    let fx = fixture_from(
        json!([{"family": "ABeeZee", "license": "OFL", "sample_script": "Latn",
                "files": [file_entry("ofl/abeezee/ABeeZee-Regular.ttf", ABEEZEE)]}]),
        vec![("ofl/abeezee/ABeeZee-Regular.ttf", ABEEZEE.to_vec())],
        samples(),
    );
    let port = serve(fx.server.clone());
    let dir = tempfile::tempdir().unwrap();
    let app = App::new(Config::new(dir.path())).unwrap();
    assert_eq!(get(&app, "/readyz", &[]).await.0, 503);
    assert!(app.reload().is_err());

    let mut cfg = config(dir.path(), &fx.sources_sha256);
    cfg.tuning.force = true;
    let generation = |n: u32| format!("gen-{COMMIT}-{n}");
    let manifest_generation = |body: Bytes| {
        let v: Value = serde_json::from_slice(&body).unwrap();
        v["generation"].as_str().unwrap().to_owned()
    };
    run(&cfg, &Local(port), &|_| {}).unwrap();
    assert!(app.reload().unwrap());
    assert_eq!(get(&app, "/readyz", &[]).await.0, 200);
    let body = get(&app, "/v1/manifest.json", &[]).await.2;
    assert_eq!(manifest_generation(body), generation(1));

    run(&cfg, &Local(port), &|_| {}).unwrap();
    assert!(app.reload().unwrap());
    assert!(!app.reload().unwrap(), "unchanged");
    let body = get(&app, "/v1/manifest.json", &[]).await.2;
    assert_eq!(manifest_generation(body), generation(2));
    let (status, ..) = get(&app, &face_url(ABEEZEE, "list", &sample(LATIN)), &[]).await;
    assert_eq!(status, 200);

    std::fs::remove_dir_all(&app.generation().unwrap().dir).unwrap();
    assert_eq!(get(&app, "/readyz", &[]).await.0, 503, "pruned generation");

    app.begin_shutdown();
    assert_eq!(get(&app, "/readyz", &[]).await.0, 503);
    assert_eq!(get(&app, "/healthz", &[]).await.0, 200);
}

#[tokio::test]
async fn downloads_keep_their_slot_until_the_body_is_gone() {
    let mut cfg = Config::new(volume());
    cfg.files_concurrency = 1;
    cfg.files_queue = 4;
    let app = app_with(cfg);
    let url = format!("/v1/files/{}", sha(ABEEZEE));
    let request = || Request::builder().uri(&url).body(Body::empty()).unwrap();
    let mut sending = Vec::new();
    // One sending, four waiting: the fifth and sixth requests find the queue full.
    for _ in 0..5 {
        let app = app.clone();
        let req = request();
        sending.push(tokio::spawn(async move { app.router().oneshot(req).await }));
    }
    let first = sending.remove(0).await.unwrap().unwrap();
    assert_eq!(first.status(), 200);
    let (status, headers, _) = get(&app, &url, &[]).await;
    assert_eq!(status, 503, "one download and four waiting fill the slots");
    assert_eq!(headers["retry-after"], "1");
    drop(first);
    for waiting in sending {
        let response = waiting.await.unwrap().unwrap();
        assert_eq!(response.status(), 200);
        let body = axum::body::to_bytes(response.into_body(), 64 << 20)
            .await
            .unwrap();
        assert_eq!(&body[..], ABEEZEE);
    }
}

#[tokio::test(start_paused = true)]
async fn a_stalled_download_gives_up_its_slot() {
    let mut cfg = Config::new(volume());
    cfg.files_concurrency = 1;
    cfg.files_queue = 0;
    let app = app_with(cfg);
    let url = format!("/v1/files/{}", sha(ABEEZEE));
    let req = Request::builder().uri(&url).body(Body::empty()).unwrap();
    let stalled = app.router().oneshot(req).await.unwrap();
    assert_eq!(stalled.status(), 200);
    assert_eq!(get(&app, &url, &[]).await.0, 503);
    tokio::time::sleep(Duration::from_secs(31)).await;
    assert_eq!(get(&app, &url, &[]).await.0, 200, "the slot went back");
    drop(stalled);
}

#[tokio::test]
async fn compression_spends_on_demand_tokens() {
    let mut cfg = Config::new(volume());
    cfg.burst = 1.0;
    cfg.rate_per_s = 0.001;
    let app = app_with(cfg);
    let br = [("accept-encoding", "br")];
    let abeezee = format!("/v1/files/{}", sha(ABEEZEE));
    let (status, headers, _) = get(&app, &abeezee, &br).await;
    assert_eq!(status, 200);
    assert_eq!(headers["content-encoding"], "br");
    let (status, headers, body) = get(&app, &format!("/v1/files/{}", sha(SARABUN)), &br).await;
    assert_eq!(status, 200, "out of tokens: sent as it is");
    assert!(headers.get("content-encoding").is_none());
    assert_eq!(&body[..], SARABUN);
    let (_, headers, _) = get(&app, &abeezee, &br).await;
    assert_eq!(
        headers["content-encoding"], "br",
        "cached results need no token"
    );
}
