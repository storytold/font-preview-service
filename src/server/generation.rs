//! One loaded generation: its manifest and `faces.json`, resolved for serving.

use crate::faces::{self, FACES, Faces};
use crate::precompute::{self, ASCII, SAMPLE, VAR};
use crate::store::{MANIFEST, Store};
use crate::subset::{Instance, Naming};
use axum::body::Bytes;
use skrifa::Tag;
use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::PathBuf;
use std::sync::Arc;

pub struct Generation {
    pub name: String,
    pub dir: PathBuf,
    pub faces: HashMap<String, Face>,
    pub licences: HashSet<String>,
    /// The `/v1/manifest.json` body.
    pub manifest: Bytes,
}

pub struct Face {
    /// How subsets are renamed; an error for a licence the subset module does not know.
    pub naming: Result<Naming, String>,
    pub family: String,
    pub size: u64,
    pub variable: bool,
    /// The `list` instance.
    pub list: Instance,
    /// Named instances by their `{inst}` string.
    pub named: HashMap<String, Instance>,
    pub sample: Option<Arc<str>>,
    /// The `{h}` of the face's `sample-{h}.woff2` URLs; without it the face has none.
    pub sample_hash: Option<String>,
    pub has_sample: bool,
    pub has_ascii: bool,
    pub has_var: bool,
}

fn instance(coords: &BTreeMap<String, f32>) -> Option<Instance> {
    let pins = coords
        .iter()
        .map(|(tag, v)| Some((Tag::new_checked(tag.as_bytes()).ok()?, *v)))
        .collect::<Option<Vec<_>>>()?;
    Some(Instance::Named(pins))
}

/// What the manifest says about the index, for `/v1/manifest.json`.
#[derive(serde::Deserialize)]
struct ManifestHead {
    index_repo: String,
    index_commit: String,
    sources_sha256: String,
}

/// The service's own part of `/v1/manifest.json`.
pub struct Limits {
    pub max_codepoints: usize,
    pub rate_per_s: f64,
    pub burst: f64,
}

/// Reads generation `name` of `store`.
pub fn load(store: &Store, name: &str, limits: &Limits) -> Result<Generation, String> {
    let dir = store.generation_dir(name);
    let read = |file: &str| {
        let path = dir.join(file);
        std::fs::read(&path).map_err(|e| format!("reading {}: {e}", path.display()))
    };
    let head: ManifestHead =
        serde_json::from_slice(&read(MANIFEST)?).map_err(|e| format!("{name}/{MANIFEST}: {e}"))?;
    let record: Faces =
        serde_json::from_slice(&read(FACES)?).map_err(|e| format!("{name}/{FACES}: {e}"))?;
    if record.version != faces::VERSION {
        return Err(format!(
            "{name}/{FACES} has version {}, this server reads {}",
            record.version,
            faces::VERSION
        ));
    }
    let samples: Vec<Arc<str>> = record
        .samples
        .iter()
        .map(|s| Arc::from(s.as_str()))
        .collect();
    let mut out = HashMap::with_capacity(record.faces.len());
    for (sha256, f) in &record.faces {
        let named = f
            .instances
            .iter()
            .filter_map(|i| {
                Some((
                    faces::instance_name(&i.coordinates),
                    instance(&i.coordinates)?,
                ))
            })
            .filter(|(name, _)| !name.is_empty())
            .collect();
        let list = match &f.list {
            Some(coords) => instance(coords).unwrap_or(Instance::List),
            None => Instance::List,
        };
        let has = |inst: &str| f.statics.iter().any(|s| s == inst);
        out.insert(
            sha256.clone(),
            Face {
                naming: precompute::naming(&f.family, &f.license, &f.reserved_names, sha256),
                family: f.family.clone(),
                size: f.size,
                variable: !f.axes.is_empty(),
                list,
                named,
                sample: f.sample.and_then(|i| samples.get(i).cloned()),
                sample_hash: f.sample_hash.clone(),
                has_sample: has(SAMPLE),
                has_ascii: has(ASCII),
                has_var: has(VAR),
            },
        );
    }
    let manifest = serde_json::json!({
        "service": {"name": env!("CARGO_PKG_NAME"), "version": env!("CARGO_PKG_VERSION")},
        "index": {
            "repo": head.index_repo,
            "commit": head.index_commit,
            "sources_sha256": head.sources_sha256,
        },
        "generation": name,
        "faces": out.len(),
        "limits": {
            "max_codepoints": limits.max_codepoints,
            "ondemand_rate_per_s": limits.rate_per_s,
            "ondemand_burst": limits.burst,
        },
    });
    Ok(Generation {
        name: name.to_owned(),
        dir,
        faces: out,
        licences: record.licences.into_iter().collect(),
        manifest: Bytes::from(serde_json::to_vec(&manifest).map_err(|e| e.to_string())?),
    })
}
