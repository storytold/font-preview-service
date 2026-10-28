//! The content-addressed layout of the data volume.
//!
//! ```text
//! <root>/files/<first 2 hex of sha256>/<sha256>      upstream font files
//! <root>/licences/<first 2 hex of sha256>/<sha256>   licence texts
//! <root>/gen-<index commit>-<n>/manifest.json       a clean sync's manifest and precompute report
//! <root>/gen-<index commit>-<n>/static/<sha256>/<inst>.woff2
//!                                                    static subsets (`sample`, `ascii`, `var`)
//! <root>/current -> gen-<index commit>-<n>           the generation servers read
//! <root>/manifest.failed.json                        report of the last sync with fetch failures
//! ```
//!
//! `sync` keeps the two generations before the new one, since servers may still read the previous
//! one until they reload, and removes the others.

use craft_font_store::sha256_hex;
use std::fs;
use std::io::{self, Write};
use std::path::{Path, PathBuf};

pub const MANIFEST: &str = "manifest.json";
const CURRENT: &str = "current";
const GEN_PREFIX: &str = "gen-";

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Kind {
    File,
    Licence,
}

impl Kind {
    fn dir(self) -> &'static str {
        match self {
            Kind::File => "files",
            Kind::Licence => "licences",
        }
    }
}

#[derive(Clone, Debug)]
pub struct Store {
    root: PathBuf,
}

impl Store {
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    pub fn current_link(&self) -> PathBuf {
        self.root.join(CURRENT)
    }

    /// The manifest of the generation `current` points to.
    pub fn current_manifest_path(&self) -> PathBuf {
        self.current_link().join(MANIFEST)
    }

    pub fn generation_dir(&self, name: &str) -> PathBuf {
        self.root.join(name)
    }

    /// The name `current` points to, if it is a symlink.
    pub fn current_generation(&self) -> Option<String> {
        let target = fs::read_link(self.current_link()).ok()?;
        Some(target.file_name()?.to_str()?.to_owned())
    }

    /// Names of the generation directories, all commits.
    fn generations(&self) -> Vec<String> {
        let Ok(entries) = fs::read_dir(&self.root) else {
            return Vec::new();
        };
        entries
            .flatten()
            .filter(|e| e.file_type().is_ok_and(|t| t.is_dir()))
            .filter_map(|e| e.file_name().into_string().ok())
            .filter(|name| name.starts_with(GEN_PREFIX))
            .collect()
    }

    /// Creates `gen-<commit>-<n>`, `n` one above the highest existing for this commit.
    pub fn new_generation(&self, commit: &str) -> io::Result<(String, PathBuf)> {
        let prefix = format!("{GEN_PREFIX}{commit}-");
        let n = self
            .generations()
            .iter()
            .filter_map(|name| name.strip_prefix(&prefix)?.parse::<u64>().ok())
            .max()
            .unwrap_or(0)
            + 1;
        let name = format!("{prefix}{n}");
        let dir = self.generation_dir(&name);
        fs::create_dir(&dir)?;
        Ok((name, dir))
    }

    /// Points `current` at generation `name`: a new symlink renamed over the old one, which
    /// readers see atomically.
    pub fn switch_current(&self, name: &str) -> io::Result<()> {
        let tmp = self.root.join(".current.tmp");
        let _ = fs::remove_file(&tmp);
        std::os::unix::fs::symlink(name, &tmp)?;
        fs::rename(&tmp, self.current_link())?;
        fs::File::open(&self.root)?.sync_all()
    }

    pub fn failed_manifest_path(&self) -> PathBuf {
        self.root.join("manifest.failed.json")
    }

    /// Fsyncs every file and directory under `dir`, and `dir` itself.
    pub fn sync_tree(&self, dir: &Path) -> io::Result<()> {
        for entry in fs::read_dir(dir)? {
            let entry = entry?;
            if entry.file_type()?.is_dir() {
                self.sync_tree(&entry.path())?;
            } else {
                fs::File::open(entry.path())?.sync_all()?;
            }
        }
        fs::File::open(dir)?.sync_all()
    }

    /// Clean-up once `current` points to generation `name`: removes the failure report, a
    /// `manifest.json` at the root (volumes synced by releases before generations have one), and
    /// every generation except `name`, `previous` (what `current` pointed to before, which an
    /// operator may have rolled back to and servers may still serve) and the `keep` newest
    /// complete others, by their manifest's time (`previous` is usually among them). Returns what it could not remove.
    pub fn after_switch(&self, name: &str, previous: Option<&str>, keep: usize) -> Vec<String> {
        let mut problems = Vec::new();
        for path in [self.failed_manifest_path(), self.root.join(MANIFEST)] {
            match fs::remove_file(&path) {
                Err(e) if e.kind() != io::ErrorKind::NotFound => {
                    problems.push(format!("removing {}: {e}", path.display()));
                }
                _ => {}
            }
        }
        let mut others: Vec<(Option<std::time::SystemTime>, String)> = self
            .generations()
            .into_iter()
            .filter(|g| g != name)
            .map(|g| {
                let built = fs::metadata(self.generation_dir(&g).join(MANIFEST))
                    .and_then(|m| m.modified())
                    .ok();
                (built, g)
            })
            .collect();
        // Newest first; a generation without a manifest is incomplete and goes.
        others.sort_by(|a, b| b.cmp(a));
        let kept = others
            .iter()
            .filter(|(built, _)| built.is_some())
            .take(keep);
        let mut kept: Vec<String> = kept.map(|(_, g)| g.clone()).collect();
        kept.extend(previous.map(str::to_owned));
        for (_, g) in others.iter().filter(|(_, g)| !kept.contains(g)) {
            if let Err(e) = fs::remove_dir_all(self.generation_dir(g)) {
                problems.push(format!("removing generation {g}: {e}"));
            }
        }
        problems
    }

    /// Removes the failure report, if any.
    pub fn remove_failed_manifest(&self) {
        let _ = fs::remove_file(self.failed_manifest_path());
    }

    /// Removes every generation without a `manifest.json` except the one `current` points to:
    /// builds that were killed or refused and never got switched in.
    pub fn remove_incomplete_generations(&self) -> Vec<String> {
        let current = self.current_generation();
        let mut problems = Vec::new();
        for g in self.generations() {
            let dir = self.generation_dir(&g);
            if current.as_deref() != Some(g.as_str())
                && !dir.join(MANIFEST).exists()
                && let Err(e) = fs::remove_dir_all(&dir)
            {
                problems.push(format!("removing incomplete generation {g}: {e}"));
            }
        }
        problems
    }

    /// Deletes the temporary files that interrupted writes left: `.*.tmp` in the blob
    /// directories, and `.current.tmp` and `manifest*.tmp` in the root.
    pub fn remove_stale_tmp(&self) {
        if let Ok(entries) = fs::read_dir(&self.root) {
            for entry in entries.flatten() {
                let name = entry.file_name();
                let name = name.to_string_lossy();
                if name.ends_with(".tmp")
                    && (name == ".current.tmp" || name.starts_with("manifest"))
                    && entry.file_type().is_ok_and(|t| !t.is_dir())
                {
                    let _ = fs::remove_file(entry.path());
                }
            }
        }
        for kind in [Kind::File, Kind::Licence] {
            let Ok(subdirs) = fs::read_dir(self.root.join(kind.dir())) else {
                continue;
            };
            for sub in subdirs.flatten() {
                let Ok(entries) = fs::read_dir(sub.path()) else {
                    continue;
                };
                for entry in entries.flatten() {
                    let name = entry.file_name();
                    let name = name.to_string_lossy();
                    if name.starts_with('.') && name.ends_with(".tmp") {
                        let _ = fs::remove_file(entry.path());
                    }
                }
            }
        }
    }

    /// Where the blob with this sha256 lives. `sha256` must be 64 lowercase hex digits.
    pub fn path(&self, kind: Kind, sha256: &str) -> PathBuf {
        self.root.join(kind.dir()).join(&sha256[..2]).join(sha256)
    }

    /// Whether the blob is on disk with exactly this size and sha256.
    pub fn has(&self, kind: Kind, sha256: &str, size: u64) -> bool {
        let path = self.path(kind, sha256);
        match fs::metadata(&path) {
            Ok(m) if m.is_file() && m.len() == size => {}
            _ => return false,
        }
        fs::read(&path).is_ok_and(|bytes| sha256_hex(&bytes) == sha256)
    }

    /// Writes the blob to a temporary name in its directory, then renames it into place.
    pub fn put(&self, kind: Kind, sha256: &str, bytes: &[u8]) -> io::Result<()> {
        let path = self.path(kind, sha256);
        let dir = path
            .parent()
            .ok_or_else(|| io::Error::other("no parent directory"))?;
        fs::create_dir_all(dir)?;
        let tmp = dir.join(format!(".{sha256}.{}.tmp", std::process::id()));
        let result = write_synced(&tmp, bytes).and_then(|()| fs::rename(&tmp, &path));
        if result.is_err() {
            let _ = fs::remove_file(&tmp);
        }
        result
    }
}

fn write_synced(path: &Path, bytes: &[u8]) -> io::Result<()> {
    let mut f = fs::File::create(path)?;
    f.write_all(bytes)?;
    f.sync_all()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn layout_uses_the_first_two_hex_digits() {
        let store = Store::new("/data");
        let sha = "ab".to_string() + &"0".repeat(62);
        assert_eq!(
            store.path(Kind::File, &sha),
            PathBuf::from(format!("/data/files/ab/{sha}"))
        );
        assert_eq!(
            store.path(Kind::Licence, &sha),
            PathBuf::from(format!("/data/licences/ab/{sha}"))
        );
        assert_eq!(
            store.current_manifest_path(),
            PathBuf::from("/data/current/manifest.json")
        );
    }

    #[test]
    fn has_requires_matching_size_and_hash() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::new(dir.path());
        let bytes = b"hello font";
        let sha = sha256_hex(bytes);
        assert!(!store.has(Kind::File, &sha, bytes.len() as u64));
        store.put(Kind::File, &sha, bytes).unwrap();
        assert!(store.has(Kind::File, &sha, bytes.len() as u64));
        assert!(!store.has(Kind::File, &sha, 3), "wrong size");
        assert!(
            !store.has(Kind::Licence, &sha, bytes.len() as u64),
            "other kind"
        );
        std::fs::write(store.path(Kind::File, &sha), b"HELLO font").unwrap();
        assert!(
            !store.has(Kind::File, &sha, bytes.len() as u64),
            "corrupt content is refetched"
        );
    }

    #[test]
    fn put_leaves_no_temporary_file() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::new(dir.path());
        let sha = sha256_hex(b"x");
        store.put(Kind::Licence, &sha, b"x").unwrap();
        let names: Vec<_> = std::fs::read_dir(store.path(Kind::Licence, &sha).parent().unwrap())
            .unwrap()
            .map(|e| e.unwrap().file_name())
            .collect();
        assert_eq!(names, vec![std::ffi::OsString::from(&sha)]);
    }
}
