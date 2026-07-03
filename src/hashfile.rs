use anyhow::{anyhow, Result};
use regex::Regex;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use walkdir::WalkDir;

#[derive(Debug, Clone)]
pub struct HashEntry {
    pub hash: String,
    pub abs_path: PathBuf,
    pub manifest_path: PathBuf,
    /// True when the hash was computed on the fly (e.g. by `rename --only-dupes`
    /// for a file not covered by any manifest) rather than read from a manifest.
    /// Freshly-computed hashes are trusted as-is and must not be re-verified.
    pub computed: bool,
}

/// Case-insensitive path key on case-insensitive filesystems (Windows, macOS),
/// case-sensitive elsewhere (Linux). Used for de-duplication and collision keys.
pub fn path_key(p: &Path) -> String {
    let s = p.to_string_lossy();
    if cfg!(windows) || cfg!(target_os = "macos") {
        s.to_lowercase()
    } else {
        s.to_string()
    }
}

/// Case-insensitive filename key (ignores directory) on case-insensitive
/// filesystems (Windows, macOS), case-sensitive elsewhere (Linux). Used to
/// group files that share a filename regardless of which directory they live in.
pub fn name_key(p: &Path) -> String {
    let s = p
        .file_name()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_default();
    if cfg!(windows) || cfg!(target_os = "macos") {
        s.to_lowercase()
    } else {
        s
    }
}

pub struct HashFile;

impl HashFile {
    /// Find hashfiles (.xxh3 or .xxh) in a directory
    pub fn find_in_dir(root: &Path, pattern: &str) -> Result<Vec<PathBuf>> {
        let mut found = Vec::new();

        for entry in WalkDir::new(root)
            .into_iter()
            .filter_map(|e| e.ok())
        {
            let path = entry.path();
            if let Some(name) = path.file_name().and_then(|n| n.to_str()) {
                if name.contains("_renamed") {
                    continue;
                }

                let is_matching = (pattern.ends_with(".xxh3") && name.ends_with(".xxh3"))
                    || (pattern.ends_with(".xxh") && name.ends_with(".xxh") && !name.ends_with(".xxh3"));

                if is_matching {
                    found.push(path.to_path_buf());
                }
            }
        }

        found.sort();
        Ok(found)
    }

    /// Parse a single .xxh3 file
    /// Format: HASH MARKER RELPATH
    /// Example: DA8E2D45A806549D *【素材】\20251215\侧机位m3\20251215-3.MP4
    pub fn parse(manifest_path: &Path) -> Result<Vec<HashEntry>> {
        let raw = std::fs::read_to_string(manifest_path)?;
        // Strip a leading UTF-8 BOM (TeraCopy and other Windows tools emit one);
        // otherwise the first entry silently fails to parse.
        let content = raw.strip_prefix('\u{feff}').unwrap_or(&raw);
        let base = manifest_path.parent().ok_or_else(|| anyhow!("No parent dir"))?;
        let mut entries = Vec::new();

        // Regex to match: 16 hex chars, space, marker (*, or space), then rest is path
        let line_re = Regex::new(r"^([0-9A-Fa-f]{16})[ ]([* ])(.+)$")?;

        for line in content.lines() {
            let line = line.trim_end_matches('\n').trim_end_matches('\r');

            // Skip empty lines and comments
            if line.is_empty() || line.starts_with(';') {
                continue;
            }

            let Some(caps) = line_re.captures(line) else {
                continue; // Skip unparseable lines
            };

            let hash = caps[1].to_uppercase();
            let _marker = &caps[2]; // "*" for binary, " " for text
            let rel_path_str = &caps[3];

            // Convert Windows paths to current OS
            let joined = if cfg!(windows) {
                base.join(rel_path_str)
            } else {
                let normalized = rel_path_str.replace('\\', "/");
                base.join(normalized)
            };
            // Absolutize so journals/CSV logs record absolute paths; undo then
            // operates on absolute paths regardless of the current directory.
            // std::path::absolute is purely lexical (no FS access, no `\\?\` prefix).
            let abs_path = std::path::absolute(&joined).unwrap_or(joined);

            entries.push(HashEntry {
                hash,
                abs_path,
                manifest_path: manifest_path.to_path_buf(),
                computed: false,
            });
        }

        Ok(entries)
    }

    /// Load and de-duplicate entries from multiple manifests
    pub fn load_all(manifests: &[PathBuf]) -> Result<Vec<HashEntry>> {
        let mut all_entries = Vec::new();
        let mut seen: HashMap<String, HashEntry> = HashMap::new();

        for manifest in manifests {
            let entries = Self::parse(manifest)?;
            for entry in entries {
                let key = path_key(&entry.abs_path);
                match seen.get(&key) {
                    Some(prev) if prev.hash == entry.hash => {
                        // Identical duplicate across manifests: drop it.
                    }
                    Some(_) => {
                        // Conflicting hashes for the same path: keep this entry too
                        // (don't silently keep only the first) so that the caller
                        // (build_plan) can detect the conflict and refuse the rename.
                        all_entries.push(entry);
                    }
                    None => {
                        all_entries.push(entry.clone());
                        seen.insert(key, entry);
                    }
                }
            }
        }

        Ok(all_entries)
    }
}
