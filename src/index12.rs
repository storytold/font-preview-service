//! The index format 1.2 fields precompute needs, read from the verified index JSON.
//!
//! craft-font-store at the pinned rev reads format 1.1 only. This view gives way to its types
//! once it reads 1.2. Every field defaults, so a 1.1 index reads as one without samples.

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

#[derive(Debug, Default, Deserialize)]
pub struct Index {
    #[serde(default)]
    pub samples: BTreeMap<String, Sample>,
    #[serde(default)]
    pub families: Vec<Family>,
}

#[derive(Debug, Default, Deserialize)]
pub struct Sample {
    #[serde(default)]
    pub text: String,
    #[serde(default)]
    pub method: String,
    #[serde(default)]
    pub language: String,
}

#[derive(Debug, Default, Deserialize)]
pub struct Family {
    #[serde(default)]
    pub family: String,
    /// `OFL`, `APACHE2` or `UFL`.
    #[serde(default)]
    pub license: String,
    #[serde(default)]
    pub reserved_names: Vec<String>,
    #[serde(default)]
    pub sample_script: Option<String>,
    /// A notice file the licence requires beside it (Apache 2.0 `NOTICE`), mirrored like the
    /// licence.
    #[serde(default)]
    pub notice_file: Option<craft_font_store::LicenseFile>,
    #[serde(default)]
    pub files: Vec<File>,
}

#[derive(Debug, Default, Deserialize)]
pub struct File {
    #[serde(default)]
    pub sha256: String,
    #[serde(default)]
    pub size: u64,
    #[serde(default)]
    pub sample_script: Option<String>,
    #[serde(default)]
    pub sample_text: Option<String>,
    #[serde(default)]
    pub instances: Vec<NamedInstance>,
    /// Position in `instances` of the instance a font list shows; absent means the default
    /// location.
    #[serde(default)]
    pub list_instance: Option<usize>,
    #[serde(default)]
    pub axes: Vec<Axis>,
}

#[derive(Clone, Debug, Default, PartialEq, Deserialize, Serialize)]
pub struct NamedInstance {
    #[serde(default)]
    pub name: String,
    /// User coordinates by axis tag.
    #[serde(default)]
    pub coordinates: BTreeMap<String, f32>,
}

#[derive(Debug, Default, Deserialize)]
pub struct Axis {
    #[serde(default)]
    pub tag: String,
}

impl Index {
    pub fn from_json(bytes: &[u8]) -> Result<Self, serde_json::Error> {
        serde_json::from_slice(bytes)
    }

    /// A file's preview text: its `sample_text`, else the sample of its own `sample_script`, else
    /// of its family's.
    pub fn sample_text<'a>(&'a self, family: &'a Family, file: &'a File) -> Option<&'a str> {
        if let Some(text) = &file.sample_text {
            return Some(text);
        }
        let script = file
            .sample_script
            .as_ref()
            .or(family.sample_script.as_ref())?;
        self.samples.get(script).map(|s| s.text.as_str())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sample_text_prefers_the_file_then_its_script_then_the_family_script() {
        let index = Index::from_json(
            br#"{"version":"1.2","samples":{"Latn":{"text":"latin","method":"udhr","language":"en_Latn"},
                "Thai":{"text":"thai","method":"udhr","language":"th_Thai"}},
              "families":[{"family":"F","license":"OFL","sample_script":"Thai","files":[
                {"sha256":"a"},{"sha256":"b","sample_script":"Latn"},
                {"sha256":"c","sample_script":"Latn","sample_text":"own"},
                {"sha256":"d","sample_script":"Zsym"}]}]}"#,
        )
        .unwrap();
        let family = &index.families[0];
        let texts: Vec<_> = family
            .files
            .iter()
            .map(|f| index.sample_text(family, f))
            .collect();
        assert_eq!(texts, [Some("thai"), Some("latin"), Some("own"), None]);
    }
}
