mod common;

use common::{COMMIT, Local, config, file_entry, fixture_from, serve};
use craft_font_store::sha256_hex;
use font_preview_service::store::{Kind, Store};
use font_preview_service::sync::run;
use serde_json::{Value, json};
use std::sync::Arc;
use std::sync::atomic::Ordering;

const ABEEZEE: &[u8] = include_bytes!("data/ABeeZee-Regular.ttf");
const BARTLE: &[u8] = include_bytes!("data/BBHBartle-Regular.ttf");
const OFL: &[u8] = include_bytes!("data/OFL.txt");

/// Two families: ABeeZee with a licence file, BBH Bartle with none and refused by jsDelivr.
fn fixture() -> common::Fixture {
    let families = json!([
        {"family": "ABeeZee", "license": "OFL", "license_path": "ofl/abeezee/OFL.txt",
         "license_file": {"path": "ofl/abeezee/OFL.txt", "size": OFL.len(), "sha256": sha256_hex(OFL)},
         "reserved_names": ["ABeeZee"], "sample_script": "Latn",
         "files": [file_entry("ofl/abeezee/ABeeZee-Regular.ttf", ABEEZEE)]},
        {"family": "BBH Bartle", "license": "OFL", "license_path": null, "sample_script": "Latn",
         "files": [file_entry("ofl/bbhbartle/BBHBartle-Regular.ttf", BARTLE)]},
    ]);
    let samples =
        json!({"Latn": {"text": "Whereas recognition", "method": "udhr", "language": "en_Latn"}});
    let mut fx = fixture_from(
        families,
        vec![
            ("ofl/abeezee/ABeeZee-Regular.ttf", ABEEZEE.to_vec()),
            ("ofl/abeezee/OFL.txt", OFL.to_vec()),
            ("ofl/bbhbartle/BBHBartle-Regular.ttf", BARTLE.to_vec()),
        ],
        samples,
    );
    Arc::get_mut(&mut fx.server)
        .unwrap()
        .refused_on_jsdelivr
        .insert("ofl/bbhbartle/BBHBartle-Regular.ttf".to_string());
    fx
}

#[test]
fn sync_stores_files_then_skips_them() {
    let fx = fixture();
    let port = serve(fx.server.clone());
    let dir = tempfile::tempdir().unwrap();
    let cfg = config(dir.path(), &fx.sources_sha256);

    let m = run(&cfg, &Local(port), &|_| {}).unwrap();
    assert!(m.failures.is_empty(), "{:?}", m.failures);
    assert_eq!((m.totals.fetched, m.totals.skipped), (3, 0));
    assert_eq!(
        (
            m.sources[0].families,
            m.sources[0].files,
            m.sources[0].licences
        ),
        (2, 2, 1)
    );

    let store = Store::new(dir.path());
    assert!(store.has(Kind::File, &sha256_hex(ABEEZEE), ABEEZEE.len() as u64));
    assert!(
        store.has(Kind::File, &sha256_hex(BARTLE), BARTLE.len() as u64),
        "fetched from raw after jsDelivr refused"
    );
    assert!(store.has(Kind::Licence, &sha256_hex(OFL), OFL.len() as u64));
    let on_disk: Value =
        serde_json::from_slice(&std::fs::read(store.current_manifest_path()).unwrap()).unwrap();
    assert_eq!(on_disk["index_commit"], COMMIT);
    assert_eq!(on_disk["sources_sha256"], fx.sources_sha256);

    let hits = fx.server.font_hits.load(Ordering::Relaxed);
    let m = run(&cfg, &Local(port), &|_| {}).unwrap();
    assert_eq!(
        (m.totals.fetched, m.totals.skipped, m.totals.failed),
        (0, 3, 0)
    );
    assert_eq!(
        fx.server.font_hits.load(Ordering::Relaxed),
        hits,
        "a second run fetches no font or licence"
    );
}

#[test]
fn sync_refuses_a_sources_file_that_does_not_match_the_pin() {
    let fx = fixture();
    let port = serve(fx.server.clone());
    let dir = tempfile::tempdir().unwrap();
    let err = run(&config(dir.path(), &"0".repeat(64)), &Local(port), &|_| {}).unwrap_err();
    assert!(err.contains("INDEX_SHA256"), "{err}");
    assert!(!Store::new(dir.path()).current_link().exists());
}

#[test]
fn sync_reports_a_file_no_mirror_has() {
    let mut fx = fixture();
    Arc::get_mut(&mut fx.server)
        .unwrap()
        .files
        .remove("ofl/bbhbartle/BBHBartle-Regular.ttf");
    let port = serve(fx.server.clone());
    let dir = tempfile::tempdir().unwrap();
    let m = run(
        &config(dir.path(), &fx.sources_sha256),
        &Local(port),
        &|_| {},
    )
    .unwrap();
    assert_eq!(m.totals.failed, 1);
    assert_eq!(m.failures[0].family, "BBH Bartle");
    assert_eq!(m.totals.fetched, 2);
}

#[test]
fn a_failed_fetch_run_leaves_current_unchanged() {
    let fx = fixture();
    let port = serve(fx.server.clone());
    let dir = tempfile::tempdir().unwrap();
    let cfg = config(dir.path(), &fx.sources_sha256);
    run(&cfg, &Local(port), &|_| {}).unwrap();
    let store = Store::new(dir.path());
    let before = store.current_generation().unwrap();
    let good = std::fs::read(store.current_manifest_path()).unwrap();

    let mut broken = fixture();
    Arc::get_mut(&mut broken.server)
        .unwrap()
        .files
        .remove("ofl/bbhbartle/BBHBartle-Regular.ttf");
    std::fs::remove_file(store.path(Kind::File, &sha256_hex(BARTLE))).unwrap();
    let port = serve(broken.server.clone());
    let m = run(
        &config(dir.path(), &broken.sources_sha256),
        &Local(port),
        &|_| {},
    )
    .unwrap();
    assert_eq!(m.totals.failed, 1);
    assert!(m.generation.is_none() && m.precompute.is_none());
    assert_eq!(store.current_generation().unwrap(), before);
    assert_eq!(std::fs::read(store.current_manifest_path()).unwrap(), good);
    assert!(
        !store.generation_dir(&format!("gen-{}-2", COMMIT)).exists(),
        "no generation is built"
    );
    let failed: Value =
        serde_json::from_slice(&std::fs::read(store.failed_manifest_path()).unwrap()).unwrap();
    assert_eq!(failed["totals"]["failed"], 1);
}
