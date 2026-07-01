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
        let content = std::fs::read_to_string(manifest_path)?;
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
            let abs_path = if cfg!(windows) {
                base.join(rel_path_str)
            } else {
                let normalized = rel_path_str.replace('\\', "/");
                base.join(normalized)
            };

            entries.push(HashEntry {
                hash,
                abs_path,
                manifest_path: manifest_path.to_path_buf(),
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
                let key = entry.abs_path.to_string_lossy().to_lowercase();
                if let Some(prev) = seen.get(&key) {
                    if prev.hash != entry.hash {
                        eprintln!(
                            "Warning: {} listed with two different hashes: {} vs {}",
                            entry.abs_path.display(),
                            prev.hash,
                            entry.hash
                        );
                    }
                } else {
                    all_entries.push(entry.clone());
                    seen.insert(key, entry);
                }
            }
        }

        Ok(all_entries)
    }
}
