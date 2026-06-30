use anyhow::{anyhow, Result};
use std::collections::HashMap;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone)]
pub struct HashEntry {
    pub hash: String,
    pub abs_path: PathBuf,
    pub manifest_path: PathBuf,
}

pub struct HashFile {
    entries: Vec<HashEntry>,
}

impl HashFile {
    /// Find hashfiles (.xxh3 or .xxh) in a directory
    pub fn find_in_dir(root: &Path, pattern: &str) -> Result<Vec<PathBuf>> {
        let mut found = Vec::new();

        fn search(dir: &Path, pattern: &str, found: &mut Vec<PathBuf>) -> Result<()> {
            for entry in std::fs::read_dir(dir)? {
                let entry = entry?;
                let path = entry.path();
                if path.is_dir() {
                    search(&path, pattern, found)?;
                } else if let Some(name) = path.file_name().and_then(|n| n.to_str()) {
                    if pattern.ends_with(".xxh3") && name.ends_with(".xxh3") {
                        // Skip already-renamed copies
                        if !name.contains("_renamed") {
                            found.push(path);
                        }
                    } else if pattern.ends_with(".xxh") && name.ends_with(".xxh")
                        && !name.ends_with(".xxh3") {
                        if !name.contains("_renamed") {
                            found.push(path);
                        }
                    }
                }
            }
            Ok(())
        }

        search(root, pattern, &mut found)?;
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

        for line in content.lines() {
            let line = line.trim_end_matches('\n').trim_end_matches('\r');

            // Skip empty lines and comments
            if line.is_empty() || line.starts_with(';') {
                continue;
            }

            // Parse line: HASH MARKER RELPATH
            let parts: Vec<&str> = line.splitn(3, ' ').collect();
            if parts.len() != 3 {
                continue; // Skip unparseable lines
            }

            let hash = parts[0].to_uppercase();
            let _marker = parts[1]; // "*" for binary, " " for text
            let rel_path_str = parts[2];

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
