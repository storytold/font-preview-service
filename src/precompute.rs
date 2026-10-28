//! Static preview subsets for every face, written into a generation directory at the end of a
//! clean sync, with the checks of architecture §4 ("Checks at every sync").

use crate::index12;
use crate::store::{Kind, Store};
use crate::subset::{self, Instance, Licence, Naming, Quality, closure, codepoints};
use serde::Serialize;
use skrifa::{FontRef, MetadataProvider, Tag};
use std::collections::{BTreeSet, HashSet};
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::path::Path;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Condvar, Mutex};

/// The `{inst}` names of the static files.
pub const SAMPLE: &str = "sample";
pub const ASCII: &str = "ascii";
pub const VAR: &str = "var";
/// Report entries for a face that fails before any subset is made.
pub const ALL: &str = "*";

/// One distinct font file to precompute, with what its index says about it.
#[derive(Debug, Clone)]
pub struct Face {
    pub sha256: String,
    pub family: String,
    pub license: String,
    pub reserved_names: Vec<String>,
    pub sample_text: Option<String>,
    /// The instance the font list shows, from the index's `list_instance`.
    pub list: Instance,
    pub variable: bool,
    /// Axis tags, from the index.
    pub axes: Vec<String>,
    /// Named instances, from the index.
    pub instances: Vec<index12::NamedInstance>,
    /// Bytes of the font file, which weigh the face against the memory budget.
    pub size: u64,
    /// Index data precompute could not use.
    pub warnings: Vec<String>,
}

/// Adds the faces of `index` not already in `seen` (by sha256), so a file several indexes list is
/// precomputed once, with the first index's family.
pub fn collect_faces(index: &index12::Index, seen: &mut HashSet<String>, faces: &mut Vec<Face>) {
    for family in &index.families {
        for file in &family.files {
            if !seen.insert(file.sha256.clone()) {
                continue;
            }
            let mut warnings = Vec::new();
            faces.push(Face {
                sha256: file.sha256.clone(),
                family: family.family.clone(),
                license: family.license.clone(),
                reserved_names: family.reserved_names.clone(),
                sample_text: index.sample_text(family, file).map(str::to_owned),
                list: list_instance(file, &mut warnings),
                variable: !file.axes.is_empty(),
                axes: file.axes.iter().map(|a| a.tag.clone()).collect(),
                instances: file.instances.clone(),
                size: file.size,
                warnings,
            });
        }
    }
}

/// The index's `list_instance` as axis values. Without one, a file whose index lists instances
/// shows the default location; a file without instance data falls back to the subset module's
/// own rule, which is the same rule.
fn list_instance(file: &index12::File, warnings: &mut Vec<String>) -> Instance {
    if let Some(i) = file.list_instance
        && i >= file.instances.len()
    {
        warnings.push(format!(
            "list_instance {i} is out of range ({} instances)",
            file.instances.len()
        ));
    }
    match file.list_instance.and_then(|i| file.instances.get(i)) {
        Some(inst) => Instance::Named(
            inst.coordinates
                .iter()
                .filter_map(|(tag, v)| match Tag::new_checked(tag.as_bytes()) {
                    Ok(t) => Some((t, *v)),
                    Err(_) => {
                        warnings.push(format!(
                            "invalid axis tag `{tag}` in instance `{}`",
                            inst.name
                        ));
                        None
                    }
                })
                .collect(),
        ),
        None if !file.instances.is_empty() => Instance::Named(Vec::new()),
        None => Instance::List,
    }
}

#[derive(Debug, Serialize, PartialEq, Eq)]
pub struct Failure {
    pub sha256: String,
    pub family: String,
    /// `sample`, `ascii`, `var`, or `*` when the face failed before any subset.
    pub inst: String,
    pub reason: String,
}

#[derive(Debug, Serialize, PartialEq, Eq)]
pub struct Warning {
    pub sha256: String,
    pub family: String,
    pub reason: String,
}

#[derive(Debug, Default, Serialize)]
pub struct Report {
    pub faces: usize,
    pub written: Written,
    pub failures: Vec<Failure>,
    pub warnings: Vec<Warning>,
}

impl Report {
    /// Faces with at least one failure.
    pub fn failed_faces(&self) -> usize {
        self.failures
            .iter()
            .map(|f| &f.sha256)
            .collect::<HashSet<_>>()
            .len()
    }
}

#[derive(Debug, Default, Serialize)]
pub struct Written {
    pub sample: usize,
    pub ascii: usize,
    pub var: usize,
}

/// Where a static subset lives inside a generation directory.
pub fn static_path(generation: &Path, sha256: &str, inst: &str) -> std::path::PathBuf {
    generation
        .join("static")
        .join(sha256)
        .join(format!("{inst}.woff2"))
}

/// How much precompute may run at once.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Limits {
    pub threads: usize,
    /// Summed font file sizes of the faces in progress. A face larger than this runs alone.
    pub memory_bytes: u64,
}

/// A counting semaphore over bytes.
struct Budget {
    total: u64,
    used: Mutex<u64>,
    freed: Condvar,
}

struct Held<'a> {
    budget: &'a Budget,
    bytes: u64,
}

impl Budget {
    fn acquire(&self, bytes: u64) -> Held<'_> {
        let bytes = bytes.min(self.total);
        let mut used = self
            .used
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        while *used + bytes > self.total {
            used = self
                .freed
                .wait(used)
                .unwrap_or_else(std::sync::PoisonError::into_inner);
        }
        *used += bytes;
        Held {
            budget: self,
            bytes,
        }
    }
}

impl Drop for Held<'_> {
    fn drop(&mut self) {
        let mut used = self
            .budget
            .used
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        *used -= self.bytes;
        self.budget.freed.notify_all();
    }
}

fn panic_message(payload: &(dyn std::any::Any + Send)) -> String {
    payload
        .downcast_ref::<&str>()
        .map(|s| (*s).to_owned())
        .or_else(|| payload.downcast_ref::<String>().cloned())
        .unwrap_or_else(|| "unknown panic".into())
}

/// Precomputes every face into `generation`. A face that panics is reported and the worker goes
/// on with the next one.
pub fn run(faces: &[Face], store: &Store, generation: &Path, limits: Limits) -> Report {
    let threads = limits.threads.min(faces.len()).max(1);
    let budget = Budget {
        total: limits.memory_bytes.max(1),
        used: Mutex::new(0),
        freed: Condvar::new(),
    };
    let next = AtomicUsize::new(0);
    let report = Mutex::new(Report {
        faces: faces.len(),
        ..Report::default()
    });
    std::thread::scope(|scope| {
        for _ in 0..threads {
            scope.spawn(|| {
                while let Some(face) = faces.get(next.fetch_add(1, Ordering::Relaxed)) {
                    let held = budget.acquire(face.size);
                    let outcome = catch_unwind(AssertUnwindSafe(|| {
                        precompute_face(face, store, generation)
                    }))
                    .unwrap_or_else(|payload| {
                        let reason = format!("panicked: {}", panic_message(&*payload));
                        (vec![(ALL, Err(reason))], Vec::new())
                    });
                    drop(held);
                    let mut report = report
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner);
                    record(&mut report, face, outcome);
                }
            });
        }
    });
    let mut report = report
        .into_inner()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    report
        .failures
        .sort_by(|a, b| (&a.family, &a.sha256, &a.inst).cmp(&(&b.family, &b.sha256, &b.inst)));
    report
        .warnings
        .sort_by(|a, b| (&a.family, &a.sha256).cmp(&(&b.family, &b.sha256)));
    report
}

fn record(report: &mut Report, face: &Face, (outcome, notes): (Outcome, Vec<String>)) {
    for (inst, result) in outcome {
        match result {
            Ok(()) => match inst {
                SAMPLE => report.written.sample += 1,
                ASCII => report.written.ascii += 1,
                _ => report.written.var += 1,
            },
            Err(reason) => report.failures.push(Failure {
                sha256: face.sha256.clone(),
                family: face.family.clone(),
                inst: inst.to_owned(),
                reason,
            }),
        }
    }
    report
        .warnings
        .extend(face.warnings.iter().chain(&notes).map(|reason| Warning {
            sha256: face.sha256.clone(),
            family: face.family.clone(),
            reason: reason.clone(),
        }));
}

/// How subsets of a face are renamed, from its index data.
pub fn naming(
    family: &str,
    license: &str,
    reserved_names: &[String],
    sha256: &str,
) -> Result<Naming, String> {
    Ok(Naming {
        licence: licence(license, reserved_names)?,
        family_name: family.to_owned(),
        sha256: sha256_bytes(sha256)?,
    })
}

fn licence(license: &str, reserved_names: &[String]) -> Result<Licence, String> {
    match license {
        "OFL" => Ok(Licence::Ofl {
            reserved_names: reserved_names.to_vec(),
        }),
        "UFL" => Ok(Licence::Ufl),
        "APACHE2" => Ok(Licence::Apache),
        other => Err(format!("unknown licence `{other}`")),
    }
}

fn sha256_bytes(hex: &str) -> Result<[u8; 32], String> {
    let mut out = [0u8; 32];
    if hex.len() != 64 {
        return Err(format!("sha256 `{hex}` is not 64 hex digits"));
    }
    for (i, byte) in out.iter_mut().enumerate() {
        *byte = u8::from_str_radix(&hex[2 * i..2 * i + 2], 16)
            .map_err(|_| format!("sha256 `{hex}` is not hex"))?;
    }
    Ok(out)
}

type Outcome = Vec<(&'static str, Result<(), String>)>;

#[cfg(test)]
const PANIC_FAMILY: &str = "panics in precompute";

fn precompute_face(face: &Face, store: &Store, generation: &Path) -> (Outcome, Vec<String>) {
    #[cfg(test)]
    assert_ne!(face.family, PANIC_FAMILY, "test hook");
    let setup = || -> Result<(Vec<u8>, Naming), String> {
        let naming = naming(
            &face.family,
            &face.license,
            &face.reserved_names,
            &face.sha256,
        )?;
        let path = store.path(Kind::File, &face.sha256);
        let font = std::fs::read(&path).map_err(|e| format!("reading {}: {e}", path.display()))?;
        Ok((font, naming))
    };
    let (font, naming) = match setup() {
        Ok(v) => v,
        Err(reason) => return (vec![(ALL, Err(reason))], Vec::new()),
    };
    let full = match FontRef::new(&font) {
        Ok(full) => full,
        Err(e) => {
            return (
                vec![(ALL, Err(format!("cannot read font: {e}")))],
                Vec::new(),
            );
        }
    };
    let sample_texts = face
        .sample_text
        .as_ref()
        .map(|text| vec![text.clone(), face.family.clone()]);
    let make = |inst: &'static str, texts: Option<&[String]>, instance: &Instance| {
        let result = texts
            .ok_or_else(|| "the index gives no sample text".to_string())
            .and_then(|texts| make_one(&font, &full, texts, instance, &naming))
            .and_then(|woff2| write_static(&static_path(generation, &face.sha256, inst), &woff2));
        (inst, result)
    };
    let mut outcome = vec![make(SAMPLE, sample_texts.as_deref(), &face.list)];
    if covers_latin(&full) {
        outcome.push(make(ASCII, Some(&[ascii()]), &face.list));
    }
    if face.variable {
        outcome.push(make(VAR, sample_texts.as_deref(), &Instance::Var));
    }
    let notes = if shapes_with_morx(&full) {
        vec!["check (c) skipped: font shapes with morx".to_owned()]
    } else {
        Vec::new()
    };
    (outcome, notes)
}

/// HarfBuzz and rustybuzz prefer AAT `morx` over `GSUB`. The subset drops `morx`, so the glyph
/// counts differ legitimately (Chenla, Content and Siemreap: 59 against 62 glyphs in HarfBuzz
/// itself), and (c) cannot compare them.
fn shapes_with_morx(font: &FontRef) -> bool {
    font.table_data(Tag::new(b"morx")).is_some()
}

/// How far the summed x advance of the subset may differ from the full font's for `glyphs`
/// glyphs. rustybuzz rounds variable advances and kerning differently from HarfBuzz, one unit per
/// glyph at most (verified on Figtree, Bricolage Grotesque and Georama, where HarfBuzz gives
/// identical totals for the subset and the full font). Static fonts and `var` subsets stay exact.
fn advance_tolerance(variable: bool, instance: &Instance, glyphs: usize) -> i64 {
    if variable && !matches!(instance, Instance::Var) {
        i64::try_from(glyphs).unwrap_or(i64::MAX)
    } else {
        0
    }
}

/// The text of an `ascii` subset: printable ASCII.
pub fn ascii() -> String {
    (0x20u8..=0x7e).map(char::from).collect()
}

/// Whether the cmap maps at least 90% of A–Z and a–z.
fn covers_latin(font: &FontRef) -> bool {
    let charmap = font.charmap();
    let mapped = ('A'..='Z')
        .chain('a'..='z')
        .filter(|&c| charmap.map(c).is_some())
        .count();
    mapped * 10 >= 52 * 9
}

/// Subsets `texts` (with the closure), checks the subset against the full font and returns it as
/// WOFF2.
fn make_one(
    font: &[u8],
    full: &FontRef,
    texts: &[String],
    instance: &Instance,
    naming: &Naming,
) -> Result<Vec<u8>, String> {
    let cps: Vec<u32> = texts
        .iter()
        .flat_map(|t| codepoints::of_text(t))
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect();
    let sfnt = subset::subset_renamed(font, &closure(&cps), instance, naming)
        .map_err(|e| e.to_string())?;
    check(font, full, &sfnt, &cps, texts, instance)?;
    subset::encode_woff2(&sfnt, Quality::Static).map_err(|e| e.to_string())
}

/// What shaping one text gives.
#[derive(Debug, Default, PartialEq, Eq, Clone, Copy)]
struct Shaped {
    glyphs: usize,
    notdefs: usize,
    /// Summed x advances after positioning.
    advance: i64,
    /// Summed advances of the shaped glyphs from `hmtx` and `HVAR`, without positioning.
    nominal: i64,
}

/// The checks of architecture §4 for a subset of `font` at `instance`:
/// (a) it maps every character of `cps` the full font maps;
/// (b) shaping `texts` gives no more `.notdef` glyphs than with the full font;
/// (c) shaping `texts` gives as many glyphs, with the same summed x advance, as the full font at
/// the same axis location.
///
/// Away from the default location, (c) sums the glyphs' `hmtx` and `HVAR` advances instead of the
/// positioned ones: rustybuzz applies `GPOS` variation deltas differently from HarfBuzz (Orbitron
/// at wght 900 kerns by its default-location values), while the subset is instanced by HarfBuzz.
/// (c) is skipped for [`Instance::List`] on a variable font, whose location only the subset module
/// knows; the index gives one for every variable font in 1.2.
fn check(
    font: &[u8],
    full: &FontRef,
    sfnt: &[u8],
    cps: &[u32],
    texts: &[String],
    instance: &Instance,
) -> Result<(), String> {
    let sub = FontRef::new(sfnt).map_err(|e| format!("cannot read the subset: {e}"))?;
    let (full_map, sub_map) = (full.charmap(), sub.charmap());
    if let Some(cp) = cps
        .iter()
        .find(|&&cp| full_map.map(cp).is_some() && sub_map.map(cp).is_none())
    {
        return Err(format!(
            "check (a) cmap: the subset does not map U+{cp:04X}"
        ));
    }
    let location: Option<&[(Tag, f32)]> = match instance {
        Instance::Named(coords) => Some(coords),
        Instance::Var => Some(&[]),
        Instance::List if full.axes().is_empty() => Some(&[]),
        Instance::List => None,
    };
    let at_default = location.is_some_and(|coords| {
        full.axes().iter().all(|axis| {
            coords
                .iter()
                .find(|(tag, _)| *tag == axis.tag())
                .is_none_or(|(_, v)| *v == axis.default_value())
        })
    });
    let full_shaped = shape(font, texts, location.unwrap_or(&[]))?;
    let sub_shaped = shape(sfnt, texts, &[])?;
    for (text, (f, s)) in texts.iter().zip(full_shaped.iter().zip(&sub_shaped)) {
        if s.notdefs > f.notdefs {
            return Err(format!(
                "check (b) .notdef: shaping {text:?} gives {} .notdef glyphs with the subset, {} \
                 with the font",
                s.notdefs, f.notdefs
            ));
        }
        let advance = |x: &Shaped| if at_default { x.advance } else { x.nominal };
        let tolerance = advance_tolerance(!full.axes().is_empty(), instance, f.glyphs);
        if location.is_some()
            && !shapes_with_morx(full)
            && (s.glyphs != f.glyphs || (advance(s) - advance(f)).abs() > tolerance)
        {
            return Err(format!(
                "check (c) shaping: {text:?} gives {} glyphs advancing {} with the subset, {} \
                 advancing {} with the font",
                s.glyphs,
                advance(s),
                f.glyphs,
                advance(f)
            ));
        }
    }
    Ok(())
}

fn shape(font: &[u8], texts: &[String], location: &[(Tag, f32)]) -> Result<Vec<Shaped>, String> {
    let mut face = rustybuzz::Face::from_slice(font, 0).ok_or("rustybuzz cannot read the font")?;
    if !location.is_empty() {
        let variations: Vec<rustybuzz::Variation> = location
            .iter()
            .map(|(tag, value)| rustybuzz::Variation {
                tag: rustybuzz::ttf_parser::Tag::from_bytes(&tag.to_be_bytes()),
                value: *value,
            })
            .collect();
        face.set_variations(&variations);
    }
    Ok(texts
        .iter()
        .map(|text| {
            let mut buffer = rustybuzz::UnicodeBuffer::new();
            buffer.push_str(text);
            buffer.guess_segment_properties();
            let shaped = rustybuzz::shape(&face, &[], buffer);
            Shaped {
                glyphs: shaped.len(),
                notdefs: shaped
                    .glyph_infos()
                    .iter()
                    .filter(|g| g.glyph_id == 0)
                    .count(),
                advance: shaped
                    .glyph_positions()
                    .iter()
                    .map(|p| i64::from(p.x_advance))
                    .sum(),
                nominal: shaped
                    .glyph_infos()
                    .iter()
                    .filter_map(|g| {
                        let id = u16::try_from(g.glyph_id).ok()?;
                        face.glyph_hor_advance(rustybuzz::ttf_parser::GlyphId(id))
                    })
                    .map(i64::from)
                    .sum(),
            }
        })
        .collect())
}

/// Temp name + rename, so a reader never sees a partial file.
fn write_static(path: &Path, bytes: &[u8]) -> Result<(), String> {
    let dir = path.parent().ok_or("no parent directory")?;
    let tmp = path.with_extension("woff2.tmp");
    std::fs::create_dir_all(dir)
        .and_then(|()| std::fs::write(&tmp, bytes))
        .and_then(|()| std::fs::rename(&tmp, path))
        .map_err(|e| {
            let _ = std::fs::remove_file(&tmp);
            format!("writing {}: {e}", path.display())
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn file(json: &str) -> index12::File {
        serde_json::from_str(json).unwrap()
    }

    #[test]
    fn shaping_check_catches_a_subset_without_the_closure() {
        let font = std::fs::read(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/tests/data/sarabun/Sarabun-Regular.ttf"
        ))
        .unwrap();
        let full = FontRef::new(&font).unwrap();
        let texts = ["นำ".to_string()];
        let cps: Vec<u32> = texts[0].chars().map(u32::from).collect();
        let naming = Naming {
            licence: Licence::Ofl {
                reserved_names: Vec::new(),
            },
            family_name: "Sarabun".into(),
            sha256: [7; 32],
        };
        let bare: BTreeSet<u32> = cps.iter().copied().collect();
        let without = subset::subset_renamed(&font, &bare, &Instance::List, &naming).unwrap();
        let err = check(&font, &full, &without, &cps, &texts, &Instance::List).unwrap_err();
        assert!(err.contains("check (b) .notdef"), "{err}");
        let with = subset::subset_renamed(&font, &closure(&cps), &Instance::List, &naming).unwrap();
        check(&font, &full, &with, &cps, &texts, &Instance::List).unwrap();
    }

    #[test]
    fn shaping_check_compares_advances_at_the_subset_location() {
        let font = std::fs::read(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/tests/data/orbitron/Orbitron[wght].ttf"
        ))
        .unwrap();
        let full = FontRef::new(&font).unwrap();
        let texts = ["Orbitron".to_string()];
        let cps: Vec<u32> = texts[0].chars().map(u32::from).collect();
        let naming = Naming {
            licence: Licence::Ofl {
                reserved_names: vec!["Orbitron".into()],
            },
            family_name: "Orbitron".into(),
            sha256: [7; 32],
        };
        let bold = Instance::Named(vec![(Tag::new(b"wght"), 900.0)]);
        let sfnt = subset::subset_renamed(&font, &closure(&cps), &bold, &naming).unwrap();
        check(&font, &full, &sfnt, &cps, &texts, &bold).unwrap();
        let regular = Instance::Named(Vec::new());
        let err = check(&font, &full, &sfnt, &cps, &texts, &regular).unwrap_err();
        assert!(err.contains("check (c) shaping"), "{err}");
    }

    #[test]
    fn advance_tolerance_applies_to_instanced_variable_faces_only() {
        let named = Instance::Named(Vec::new());
        assert_eq!(advance_tolerance(true, &named, 12), 12);
        assert_eq!(advance_tolerance(true, &Instance::Var, 12), 0);
        assert_eq!(advance_tolerance(false, &Instance::List, 12), 0);
    }

    #[test]
    fn morx_is_detected() {
        let bytes = std::fs::read(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/tests/data/abeezee/ABeeZee-Regular.ttf"
        ))
        .unwrap();
        let plain = FontRef::new(&bytes).unwrap();
        assert!(!shapes_with_morx(&plain));
        let mut builder = write_fonts::FontBuilder::new();
        builder.add_raw(Tag::new(b"morx"), vec![0u8; 8]);
        builder.copy_missing_tables(plain);
        let with = builder.build();
        assert!(shapes_with_morx(&FontRef::new(&with).unwrap()));
    }

    #[test]
    fn list_instance_follows_the_index() {
        let tag = |s: &[u8; 4]| Tag::new(s);
        let mut warnings = Vec::new();
        let mut list = |json: &str| list_instance(&file(json), &mut warnings);
        assert_eq!(
            list(
                r#"{"instances":[{"name":"Thin","coordinates":{"wght":100}},
                    {"name":"Regular","coordinates":{"wght":400,"wdth":100,"toolong":1}}],
                    "list_instance":1}"#
            ),
            Instance::Named(vec![(tag(b"wdth"), 100.0), (tag(b"wght"), 400.0)])
        );
        assert_eq!(
            list(r#"{"instances":[{"name":"Thin","coordinates":{"wght":100}}]}"#),
            Instance::Named(Vec::new()),
            "no list_instance: the default location"
        );
        assert_eq!(
            list(r#"{"instances":[{"name":"Thin","coordinates":{"wght":100}}],"list_instance":4}"#),
            Instance::Named(Vec::new())
        );
        assert_eq!(list("{}"), Instance::List);
        assert_eq!(
            warnings,
            [
                "invalid axis tag `toolong` in instance `Regular`",
                "list_instance 4 is out of range (1 instances)"
            ]
        );
    }

    #[test]
    fn a_panicking_face_is_reported_and_the_worker_goes_on() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::new(dir.path());
        let font = std::fs::read(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/tests/data/abeezee/ABeeZee-Regular.ttf"
        ))
        .unwrap();
        let sha256 = craft_font_store::sha256_hex(&font);
        store.put(Kind::File, &sha256, &font).unwrap();
        let face = |family: &str| Face {
            sha256: sha256.clone(),
            family: family.into(),
            license: "OFL".into(),
            reserved_names: Vec::new(),
            sample_text: Some("Whereas".into()),
            list: Instance::List,
            variable: false,
            axes: Vec::new(),
            instances: Vec::new(),
            size: font.len() as u64,
            warnings: Vec::new(),
        };
        let generation = dir.path().join("gen");
        let limits = Limits {
            threads: 1,
            memory_bytes: 1,
        };
        let report = run(
            &[face(PANIC_FAMILY), face("ABeeZee")],
            &store,
            &generation,
            limits,
        );
        assert_eq!(report.failures.len(), 1, "{:?}", report.failures);
        assert_eq!(report.failures[0].inst, ALL);
        assert!(report.failures[0].reason.starts_with("panicked: "));
        assert_eq!((report.written.sample, report.written.ascii), (1, 1));
        assert!(static_path(&generation, &sha256, SAMPLE).exists());
    }
}
