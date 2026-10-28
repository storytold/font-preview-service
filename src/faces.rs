//! `faces.json`: what the server needs about every face of a generation, so it never reads the
//! index. `sync` writes it into the generation directory before the manifest.
//!
//! ```json
//! {"version": 1,
//!  "samples": ["Whereas recognition of the inherent dignity", "..."],
//!  "licences": ["<sha256 of a licence or notice file>", "..."],
//!  "faces": {"<sha256>": {"family": "Orbitron", "license": "OFL", "reserved_names": ["Orbitron"],
//!    "size": 38260, "axes": ["wght"],
//!    "instances": [{"name": "Regular", "coordinates": {"wght": 400.0}}],
//!    "list": {"wght": 400.0}, "sample": 0, "sample_hash": "a79fe905fc38",
//!    "statics": ["sample", "ascii", "var"]}}}
//! ```
//!
//! The font file is at `files/<first 2 hex>/<sha256>` ([`crate::store::Store::path`]).

use crate::index12::NamedInstance;
use crate::precompute::{self, Face};
use crate::subset::Instance;
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::path::Path;

pub const FACES: &str = "faces.json";
pub const VERSION: u32 = 1;

#[derive(Debug, Default, Serialize, Deserialize, PartialEq)]
pub struct Faces {
    pub version: u32,
    /// Distinct sample texts; faces refer to them by position.
    pub samples: Vec<String>,
    /// The sha256 of every licence and notice file mirrored for this generation, sorted.
    pub licences: Vec<String>,
    pub faces: BTreeMap<String, FaceRecord>,
}

#[derive(Debug, Default, Serialize, Deserialize, PartialEq)]
pub struct FaceRecord {
    pub family: String,
    /// `OFL`, `APACHE2` or `UFL`.
    pub license: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub reserved_names: Vec<String>,
    /// Bytes of the font file.
    pub size: u64,
    /// Axis tags; empty for a static face.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub axes: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub instances: Vec<NamedInstance>,
    /// Coordinates of the `list` instance (empty: the default location). Absent when the index
    /// gives no instance data, and the subset module picks it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub list: Option<BTreeMap<String, f32>>,
    /// Position of the face's sample text in [`Faces::samples`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sample: Option<usize>,
    /// [`sample_hash`] of that text, the `{h}` of the face's `sample-{h}.woff2` URLs.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sample_hash: Option<String>,
    /// The precomputed static files present: `sample`, `ascii`, `var`.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub statics: Vec<String>,
}

/// The `{inst}` URL form of a named instance: `<tag><value>` per axis, sorted by tag in byte order,
/// joined by commas, each value in Rust's `Display` for `f32` (`wght700`, `opsz14.5`, `slnt-10`).
pub fn instance_name(coordinates: &BTreeMap<String, f32>) -> String {
    let items: Vec<String> = coordinates
        .iter()
        .map(|(tag, value)| format!("{tag}{value}"))
        .collect();
    items.join(",")
}

/// The `{h}` of `sample-{h}.woff2`: the first 12 lowercase hex characters of the sha256 of the
/// face's sample text as the index gives it (UTF-8, no normalisation, newlines kept).
pub fn sample_hash(text: &str) -> String {
    let mut h = craft_font_store::sha256_hex(text.as_bytes());
    h.truncate(12);
    h
}

/// The record of every face, with the static files precompute left in `generation`.
pub fn build(faces: &[Face], licences: &BTreeSet<String>, generation: &Path) -> Faces {
    let mut out = Faces {
        version: VERSION,
        licences: licences.iter().cloned().collect(),
        ..Faces::default()
    };
    let mut sample_ids: HashMap<String, usize> = HashMap::new();
    for face in faces {
        let sample = face.sample_text.as_ref().map(|text| {
            *sample_ids.entry(text.clone()).or_insert_with(|| {
                out.samples.push(text.clone());
                out.samples.len() - 1
            })
        });
        let statics = [precompute::SAMPLE, precompute::ASCII, precompute::VAR]
            .into_iter()
            .filter(|inst| precompute::static_path(generation, &face.sha256, inst).is_file())
            .map(str::to_owned)
            .collect();
        let list = match &face.list {
            Instance::Named(coords) => Some(
                coords
                    .iter()
                    .map(|(tag, v)| (tag.to_string(), *v))
                    .collect(),
            ),
            Instance::List | Instance::Var => None,
        };
        out.faces.insert(
            face.sha256.clone(),
            FaceRecord {
                family: face.family.clone(),
                license: face.license.clone(),
                reserved_names: face.reserved_names.clone(),
                size: face.size,
                axes: face.axes.clone(),
                instances: face.instances.clone(),
                list,
                sample,
                sample_hash: face.sample_text.as_deref().map(sample_hash),
                statics,
            },
        );
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn instance_names_follow_the_url_form() {
        let coords = |pairs: &[(&str, f32)]| -> BTreeMap<String, f32> {
            pairs.iter().map(|(t, v)| ((*t).to_owned(), *v)).collect()
        };
        assert_eq!(
            instance_name(&coords(&[("wght", 700.0), ("wdth", 100.0)])),
            "wdth100,wght700"
        );
        assert_eq!(
            instance_name(&coords(&[("opsz", 14.5), ("slnt", -10.0), ("GRAD", 0.0)])),
            "GRAD0,opsz14.5,slnt-10"
        );
        assert_eq!(instance_name(&coords(&[("wght", 0.1)])), "wght0.1");
    }

    #[test]
    fn sample_hashes_match_the_shared_vectors() {
        let vectors: serde_json::Value =
            serde_json::from_str(include_str!("../tests/data/preview-vectors.json")).unwrap();
        let cases = vectors["sample_hash"]["vectors"].as_array().unwrap();
        assert!(!cases.is_empty());
        for case in cases {
            let text = case["text"].as_str().unwrap();
            assert_eq!(sample_hash(text), case["h"].as_str().unwrap(), "{text:?}");
        }
    }
}
