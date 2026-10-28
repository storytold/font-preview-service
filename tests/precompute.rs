mod common;

use common::{COMMIT, Local, config, file_entry, fixture_from, serve};
use craft_font_store::sha256_hex;
use font_preview_service::precompute::static_path;
use font_preview_service::store::Store;
use font_preview_service::sync::run;
use serde_json::{Value, json};
use skrifa::{FontRef, MetadataProvider};

const ABEEZEE: &[u8] = include_bytes!("data/abeezee/ABeeZee-Regular.ttf");
const SARABUN: &[u8] = include_bytes!("data/sarabun/Sarabun-Regular.ttf");
const UBUNTU: &[u8] = include_bytes!("data/ubuntu/Ubuntu-Regular.ttf");
const ORBITRON: &[u8] = include_bytes!("data/orbitron/Orbitron[wght].ttf");
const CJK: &[u8] = include_bytes!("data/notosanscjk/NotoSansCJKsc-test.otf");
const THAI: &str = "โดยที่การไม่นำพาและการหมิ่นในคุณค่าของสิทธิมนุษยชน";

fn samples() -> Value {
    json!({
        "Latn": {"text": "Whereas recognition of the inherent dignity", "method": "udhr", "language": "en_Latn"},
        "Thai": {"text": THAI, "method": "udhr", "language": "th_Thai"},
    })
}

fn family(name: &str, license: &str, script: &str, file: Value) -> Value {
    json!({"family": name, "license": license, "license_path": null, "reserved_names": [],
           "sample_script": script, "files": [file]})
}

/// ABeeZee (OFL with a Reserved Font Name), Sarabun (Thai), Ubuntu (UFL), Orbitron (variable)
/// and a CFF font whose family name its CFF strings still carry, so its subsets fail.
fn faces() -> common::Fixture {
    let mut abeezee = family(
        "ABeeZee",
        "OFL",
        "Latn",
        file_entry("ofl/abeezee/ABeeZee-Regular.ttf", ABEEZEE),
    );
    abeezee["reserved_names"] = json!(["ABeeZee"]);
    let mut orbitron = file_entry("ofl/orbitron/Orbitron[wght].ttf", ORBITRON);
    orbitron["axes"] = json!([{"tag": "wght", "min": 400.0, "max": 900.0}]);
    orbitron["instances"] = json!([{"name": "Regular", "coordinates": {"wght": 400.0}},
                                   {"name": "Bold", "coordinates": {"wght": 700.0}}]);
    orbitron["list_instance"] = json!(0);
    let mut cjk = file_entry("ofl/adobe/NotoSansCJKsc-test.otf", CJK);
    // "Adobe" is this CID font's ROS Registry, a string renaming leaves alone.
    cjk["sample_text"] = json!(
        FontRef::new(CJK)
            .unwrap()
            .charmap()
            .mappings()
            .filter_map(|(cp, _)| char::from_u32(cp))
            .filter(|c| !c.is_ascii())
            .take(5)
            .collect::<String>()
    );
    let families = json!([
        abeezee,
        family("Adobe", "APACHE2", "Hans", cjk),
        family("Orbitron", "OFL", "Latn", orbitron),
        family(
            "Sarabun",
            "OFL",
            "Thai",
            file_entry("ofl/sarabun/Sarabun-Regular.ttf", SARABUN)
        ),
        family(
            "Ubuntu",
            "UFL",
            "Latn",
            file_entry("ufl/ubuntu/Ubuntu-Regular.ttf", UBUNTU)
        ),
    ]);
    fixture_from(
        families,
        vec![
            ("ofl/abeezee/ABeeZee-Regular.ttf", ABEEZEE.to_vec()),
            ("ofl/adobe/NotoSansCJKsc-test.otf", CJK.to_vec()),
            ("ofl/orbitron/Orbitron[wght].ttf", ORBITRON.to_vec()),
            ("ofl/sarabun/Sarabun-Regular.ttf", SARABUN.to_vec()),
            ("ufl/ubuntu/Ubuntu-Regular.ttf", UBUNTU.to_vec()),
        ],
        samples(),
    )
}

fn notdefs(ttf: &[u8], text: &str) -> usize {
    let face = rustybuzz::Face::from_slice(ttf, 0).unwrap();
    let mut buffer = rustybuzz::UnicodeBuffer::new();
    buffer.push_str(text);
    rustybuzz::shape(&face, &[], buffer)
        .glyph_infos()
        .iter()
        .filter(|g| g.glyph_id == 0)
        .count()
}

#[test]
fn a_clean_sync_precomputes_every_face_and_reports_failures() {
    let fx = faces();
    let port = serve(fx.server.clone());
    let dir = tempfile::tempdir().unwrap();
    let mut cfg = config(dir.path(), &fx.sources_sha256);
    cfg.tuning.max_failed_percent = 100.0;
    let m = run(&cfg, &Local(port), &|_| {}).unwrap();
    assert!(m.succeeded(), "{:?}", m.failures);
    let store = Store::new(dir.path());
    let generation = format!("gen-{COMMIT}-1");
    assert_eq!(m.generation.as_deref(), Some(generation.as_str()));
    assert_eq!(
        store.current_generation().as_deref(),
        Some(generation.as_str())
    );
    let current = store.current_link();
    let exists = |font: &[u8], inst: &str| static_path(&current, &sha256_hex(font), inst).exists();
    for font in [ABEEZEE, SARABUN, UBUNTU, ORBITRON] {
        assert!(exists(font, "sample") && exists(font, "ascii"));
    }
    assert!(exists(ORBITRON, "var"));
    assert!(!exists(ABEEZEE, "var"), "static faces get no var subset");

    let report = m.precompute.as_ref().unwrap();
    assert_eq!(report.faces, 5);
    let cjk = sha256_hex(CJK);
    let cjk_failures: Vec<_> = report.failures.iter().filter(|f| f.sha256 == cjk).collect();
    assert!(
        cjk_failures
            .iter()
            .any(|f| f.inst == "sample" && f.family == "Adobe" && f.reason.contains("CFF")),
        "{:?}",
        report.failures
    );
    assert_eq!(
        cjk_failures.len(),
        report.failures.len(),
        "only the CFF face fails"
    );
    assert!(!exists(CJK, "sample"), "a failed subset gets no file");

    let on_disk: Value =
        serde_json::from_slice(&std::fs::read(store.current_manifest_path()).unwrap()).unwrap();
    assert_eq!(on_disk["generation"], generation.as_str());
    assert_eq!(on_disk["precompute"]["written"]["var"], 1);
    assert_eq!(on_disk["precompute"]["failures"][0]["family"], "Adobe");

    // The sample has SARA AM (นำ): the closure keeps the characters HarfBuzz splits it into.
    let woff2 = std::fs::read(static_path(&current, &sha256_hex(SARABUN), "sample")).unwrap();
    let ttf = wuff::decompress_woff2(&woff2).unwrap();
    assert_eq!(notdefs(&ttf, THAI), 0);
}

fn abeezee_only() -> common::Fixture {
    fixture_from(
        json!([family(
            "ABeeZee",
            "OFL",
            "Latn",
            file_entry("ofl/abeezee/ABeeZee-Regular.ttf", ABEEZEE)
        )]),
        vec![("ofl/abeezee/ABeeZee-Regular.ttf", ABEEZEE.to_vec())],
        samples(),
    )
}

#[test]
fn forced_syncs_keep_the_new_generation_and_the_two_before() {
    let fx = abeezee_only();
    let port = serve(fx.server.clone());
    let dir = tempfile::tempdir().unwrap();
    let mut cfg = config(dir.path(), &fx.sources_sha256);
    cfg.tuning.force = true;
    let store = Store::new(dir.path());
    let generation = |n: u32| format!("gen-{COMMIT}-{n}");
    let present = |n| store.generation_dir(&generation(n)).exists();
    // A failure report and a root manifest from an older layout go once a switch succeeds.
    std::fs::write(store.failed_manifest_path(), b"{}").unwrap();
    std::fs::write(dir.path().join("manifest.json"), b"{}").unwrap();

    for n in 1..=4 {
        let m = run(&cfg, &Local(port), &|_| {}).unwrap();
        assert_eq!(m.generation, Some(generation(n)));
        assert_eq!(store.current_generation(), Some(generation(n)));
        assert!(static_path(&store.current_link(), &sha256_hex(ABEEZEE), "sample").exists());
        assert!((1..=n).all(|k| present(k) == (k + 2 >= n)), "after run {n}");
    }
    assert!(!store.failed_manifest_path().exists());
    assert!(!dir.path().join("manifest.json").exists());
}

#[test]
fn a_sync_with_nothing_new_keeps_current() {
    let fx = abeezee_only();
    let port = serve(fx.server.clone());
    let dir = tempfile::tempdir().unwrap();
    let cfg = config(dir.path(), &fx.sources_sha256);
    let store = Store::new(dir.path());
    let first = run(&cfg, &Local(port), &|_| {}).unwrap();
    assert!(!first.unchanged);
    let m = run(&cfg, &Local(port), &|_| {}).unwrap();
    assert!(m.unchanged && m.succeeded());
    assert_eq!(m.generation, first.generation);
    assert_eq!(store.current_generation(), first.generation);
    assert!(!store.generation_dir(&format!("gen-{COMMIT}-2")).exists());
}

#[test]
fn too_many_failed_faces_keep_current_and_fail_the_run() {
    let fx = faces();
    let port = serve(fx.server.clone());
    let dir = tempfile::tempdir().unwrap();
    let mut cfg = config(dir.path(), &fx.sources_sha256);
    cfg.tuning.max_failed_percent = 100.0;
    run(&cfg, &Local(port), &|_| {}).unwrap();
    let store = Store::new(dir.path());
    let good = std::fs::read(store.current_manifest_path()).unwrap();

    // One face of five fails, above the default 1%.
    cfg.tuning.max_failed_percent = 1.0;
    cfg.tuning.force = true;
    let m = run(&cfg, &Local(port), &|_| {}).unwrap();
    assert!(!m.succeeded());
    assert!(
        m.refused_switch
            .as_deref()
            .unwrap()
            .contains("1 of 5 faces")
    );
    assert_eq!(store.current_generation(), Some(format!("gen-{COMMIT}-1")));
    assert_eq!(std::fs::read(store.current_manifest_path()).unwrap(), good);
    assert!(!store.generation_dir(&format!("gen-{COMMIT}-2")).exists());
    let failed: Value =
        serde_json::from_slice(&std::fs::read(store.failed_manifest_path()).unwrap()).unwrap();
    assert_eq!(failed["precompute"]["failures"][0]["family"], "Adobe");
    assert!(failed["refused_switch"].is_string());
}

#[test]
fn a_run_removes_incomplete_generations_but_not_the_current_one() {
    let fx = abeezee_only();
    let port = serve(fx.server.clone());
    let dir = tempfile::tempdir().unwrap();
    let cfg = config(dir.path(), &fx.sources_sha256);
    let store = Store::new(dir.path());
    run(&cfg, &Local(port), &|_| {}).unwrap();
    let leftover = store.generation_dir(&format!("gen-{COMMIT}-7"));
    std::fs::create_dir(&leftover).unwrap();
    std::fs::write(leftover.join("partial"), b"x").unwrap();
    std::fs::write(dir.path().join("manifest.failed.json.99.tmp"), b"{").unwrap();

    // Unchanged run: the leftover goes, current stays.
    let m = run(&cfg, &Local(port), &|_| {}).unwrap();
    assert!(m.unchanged);
    assert!(!leftover.exists());
    assert!(!dir.path().join("manifest.failed.json.99.tmp").exists());
    assert!(store.generation_dir(&format!("gen-{COMMIT}-1")).exists());

    // Even a current generation without a manifest is not removed.
    std::fs::remove_file(store.current_manifest_path()).unwrap();
    let mut forced = config(dir.path(), &fx.sources_sha256);
    forced.tuning.force = true;
    run(&forced, &Local(port), &|_| {}).unwrap();
    assert_eq!(store.current_generation(), Some(format!("gen-{COMMIT}-2")));
}

#[test]
fn a_rolled_back_generation_survives_the_next_run() {
    let fx = abeezee_only();
    let port = serve(fx.server.clone());
    let dir = tempfile::tempdir().unwrap();
    let mut cfg = config(dir.path(), &fx.sources_sha256);
    cfg.tuning.force = true;
    let store = Store::new(dir.path());
    let generation = |n: u32| format!("gen-{COMMIT}-{n}");
    for _ in 1..=4 {
        run(&cfg, &Local(port), &|_| {}).unwrap();
    }
    // Generation 1 was already pruned; roll back to the oldest kept one, 2.
    store.switch_current(&generation(2)).unwrap();
    run(&cfg, &Local(port), &|_| {}).unwrap();
    assert_eq!(store.current_generation(), Some(generation(5)));
    let present = |n| store.generation_dir(&generation(n)).exists();
    assert!(present(2) && present(5) && present(4) && present(3));
    assert!(!present(1));
}

#[test]
fn a_skipped_run_removes_an_old_failure_report() {
    let fx = abeezee_only();
    let port = serve(fx.server.clone());
    let dir = tempfile::tempdir().unwrap();
    let cfg = config(dir.path(), &fx.sources_sha256);
    let store = Store::new(dir.path());
    run(&cfg, &Local(port), &|_| {}).unwrap();
    std::fs::write(store.failed_manifest_path(), b"{}").unwrap();
    let m = run(&cfg, &Local(port), &|_| {}).unwrap();
    assert!(m.unchanged);
    assert!(!store.failed_manifest_path().exists());
}
