use crate::hashfile::{HashEntry, HashFile};
use crate::logging::Journal;
use anyhow::{anyhow, Result};
use std::collections::HashMap;
use std::path::{Path, PathBuf};

const DEFAULT_VIDEO_EXTS: &[&str] = &[
    ".mp4", ".mov", ".mxf", ".avi", ".mts", ".m2ts", ".m2t", ".ts", ".mkv", ".m4v",
    ".mpg", ".mpeg", ".wmv", ".braw", ".r3d", ".ari", ".arx", ".insv", ".lrv", ".3gp",
    ".vob", ".mod", ".tod",
];

#[derive(Debug, Clone)]
pub struct Action {
    pub src: PathBuf,
    pub dst: Option<PathBuf>,
    pub hash: String,
    pub status: String, // rename, done, missing, conflict, not-video, verify-failed, error
    pub note: String,
}

#[derive(Debug)]
pub struct Report {
    pub actions: Vec<Action>,
    pub orphans: Vec<PathBuf>,
}

pub struct RenameOperation {
    path: PathBuf,
    hashfile: Option<PathBuf>,
    verify: bool,
    update_manifest: bool,
    dry_run: bool,
}

impl RenameOperation {
    pub fn new(
        path: PathBuf,
        hashfile: Option<PathBuf>,
        verify: bool,
        update_manifest: bool,
        dry_run: bool,
    ) -> Result<Self> {
        if !path.exists() || !path.is_dir() {
            return Err(anyhow!("Path does not exist or is not a directory: {}", path.display()));
        }

        Ok(RenameOperation {
            path,
            hashfile,
            verify,
            update_manifest,
            dry_run,
        })
    }

    pub fn execute(&self) -> Result<()> {
        // Find or load hashfile
        let entries = self.load_entries()?;
        if entries.is_empty() {
            println!("没有找到任何哈希条目");
            return Ok(());
        }

        // Build the plan
        let report = self.build_plan(&entries)?;

        // Print report
        self.print_report(&report)?;

        if self.dry_run {
            println!("\n*** 预演模式 (DRY-RUN) *** 未改动任何文件。");
            let to_rename = report
                .actions
                .iter()
                .filter(|a| a.status == "rename")
                .count();
            println!("计划改名 {} 个。确认无误后加 --apply 执行。", to_rename);
        } else {
            // Execute renames
            self.apply_renames(&report)?;
        }

        Ok(())
    }

    fn load_entries(&self) -> Result<Vec<HashEntry>> {
        if let Some(manifest_path) = &self.hashfile {
            if !manifest_path.exists() {
                return Err(anyhow!("Hashfile not found: {}", manifest_path.display()));
            }
            HashFile::parse(manifest_path)
        } else {
            // Find .xxh3 files in the directory
            let manifests = HashFile::find_in_dir(&self.path, "*.xxh3")?;
            if manifests.is_empty() {
                println!(
                    "未找到 .xxh3 文件。要自动生成吗? (暂不实现自动生成)"
                );
                return Ok(Vec::new());
            }
            HashFile::load_all(&manifests)
        }
    }

    fn build_plan(&self, entries: &[HashEntry]) -> Result<Report> {
        let mut report = Report {
            actions: Vec::new(),
            orphans: Vec::new(),
        };

        let mut seen_src: HashMap<String, HashEntry> = HashMap::new();
        let video_exts = self.get_video_exts();
        let sep = "_";

        // De-duplicate entries
        for entry in entries {
            let key = entry.abs_path.to_string_lossy().to_lowercase();
            if let Some(prev) = seen_src.get(&key) {
                if prev.hash != entry.hash {
                    report.actions.push(Action {
                        src: entry.abs_path.clone(),
                        dst: None,
                        hash: entry.hash.clone(),
                        status: "conflict".to_string(),
                        note: format!(
                            "same path listed with two hashes ({} vs {})",
                            prev.hash, entry.hash
                        ),
                    });
                }
            } else {
                seen_src.insert(key, entry.clone());
            }
        }

        let mut proposed: HashMap<String, Vec<Action>> = HashMap::new();

        for entry in seen_src.values() {
            let src = &entry.abs_path;

            // Check if it's a video
            if !is_video(src, &video_exts) {
                report.actions.push(Action {
                    src: src.clone(),
                    dst: None,
                    hash: entry.hash.clone(),
                    status: "not-video".to_string(),
                    note: "extension not in video allowlist".to_string(),
                });
                continue;
            }

            // Compute target name
            let target = target_for(src, &entry.hash, sep);

            // Idempotency: already suffixed
            if src.file_name() == target.file_name() {
                report.actions.push(Action {
                    src: src.clone(),
                    dst: Some(target),
                    hash: entry.hash.clone(),
                    status: "done".to_string(),
                    note: "already suffixed".to_string(),
                });
                continue;
            }

            // Check if source exists
            if !src.exists() {
                if target.exists() {
                    report.actions.push(Action {
                        src: src.clone(),
                        dst: Some(target),
                        hash: entry.hash.clone(),
                        status: "done".to_string(),
                        note: "source already renamed in a prior run".to_string(),
                    });
                } else {
                    report.actions.push(Action {
                        src: src.clone(),
                        dst: None,
                        hash: entry.hash.clone(),
                        status: "missing".to_string(),
                        note: "listed in manifest but not found on disk".to_string(),
                    });
                }
                continue;
            }

            // Check if target already exists
            if target.exists() {
                report.actions.push(Action {
                    src: src.clone(),
                    dst: Some(target),
                    hash: entry.hash.clone(),
                    status: "conflict".to_string(),
                    note: "target name already exists on disk".to_string(),
                });
                continue;
            }

            // Verify hash if requested
            if self.verify {
                match verify_hash(src, &entry.hash) {
                    Ok(_) => {
                        // Hash is correct, proceed
                    }
                    Err(e) => {
                        report.actions.push(Action {
                            src: src.clone(),
                            dst: Some(target),
                            hash: entry.hash.clone(),
                            status: "verify-failed".to_string(),
                            note: e.to_string(),
                        });
                        continue;
                    }
                }
            }

            // Plan the rename
            let action = Action {
                src: src.clone(),
                dst: Some(target.clone()),
                hash: entry.hash.clone(),
                status: "rename".to_string(),
                note: String::new(),
            };

            report.actions.push(action.clone());

            proposed
                .entry(target.to_string_lossy().to_lowercase())
                .or_insert_with(Vec::new)
                .push(action);
        }

        // Check for collisions
        for acts in proposed.values_mut() {
            if acts.len() > 1 {
                let count = acts.len();
                for a in acts.iter_mut() {
                    a.status = "conflict".to_string();
                    a.note = format!("{} different files would collide on target name", count);
                }
            }
        }

        Ok(report)
    }

    fn print_report(&self, report: &Report) -> Result<()> {
        let mut counts: HashMap<String, usize> = HashMap::new();
        for action in &report.actions {
            *counts.entry(action.status.clone()).or_insert(0) += 1;
        }

        println!("{}", "=".repeat(70));
        println!("摘要 / Summary");
        println!("{}", "-".repeat(70));

        let statuses = vec!["rename", "done", "conflict", "verify-failed", "missing", "not-video"];
        for status in statuses {
            if let Some(&count) = counts.get(status) {
                let label = match status {
                    "rename" => "将重命名",
                    "done" => "已就绪(跳过)",
                    "conflict" => "冲突(已拒绝)",
                    "verify-failed" => "校验失败(已拒绝)",
                    "missing" => "清单有、磁盘无",
                    "not-video" => "非视频(忽略)",
                    _ => status,
                };
                println!("  {:<16} {:>7}", label, count);
            }
        }

        if !report.orphans.is_empty() {
            println!(
                "  {:<16} {:>7}  (有哈希以外的视频文件, 未改动)",
                "未入清单视频",
                report.orphans.len()
            );
        }

        println!("{}", "=".repeat(70));

        Ok(())
    }

    fn apply_renames(&self, report: &Report) -> Result<()> {
        let to_rename: Vec<_> = report
            .actions
            .iter()
            .filter(|a| a.status == "rename")
            .collect();

        if to_rename.is_empty() {
            println!("没有需要改名的文件。");
            return Ok(());
        }

        println!("开始执行 {} 个重命名...", to_rename.len());

        let now = chrono::Local::now().format("%Y%m%d-%H%M%S");
        let log_path = std::env::current_dir()?
            .join(format!("rehash-applied-{}.csv", now));

        let mut journal = Journal::new(&log_path)?;

        let mut success = 0;
        let mut failed = 0;

        for action in &to_rename {
            if let Some(dst) = &action.dst {
                // Record intent before actual rename
                journal.record("rename", &action.hash, &action.src.to_string_lossy(),
                              &dst.to_string_lossy(), "")?;

                match std::fs::rename(&action.src, dst) {
                    Ok(_) => {
                        success += 1;
                        println!("  ✓ {}", action.src.file_name().unwrap_or_default().to_string_lossy());
                    }
                    Err(e) => {
                        failed += 1;
                        println!("  ✗ {}: {}", action.src.file_name().unwrap_or_default().to_string_lossy(), e);
                        journal.record("error", &action.hash, &action.src.to_string_lossy(),
                                      &dst.to_string_lossy(), &e.to_string())?;
                    }
                }
            }
        }

        println!("重命名完成: 成功 {}, 失败 {}", success, failed);
        println!("日志已写出: {}", log_path.display());

        Ok(())
    }

    fn get_video_exts(&self) -> Vec<String> {
        DEFAULT_VIDEO_EXTS.iter().map(|s| s.to_string()).collect()
    }
}

fn is_video(path: &Path, exts: &[String]) -> bool {
    if let Some(ext) = path.extension() {
        if let Some(ext_str) = ext.to_str() {
            let ext_lower = format!(".{}", ext_str.to_lowercase());
            exts.iter().any(|e| e == &ext_lower)
        } else {
            false
        }
    } else {
        false
    }
}

fn target_for(src: &Path, hash: &str, sep: &str) -> PathBuf {
    if let Some(stem) = src.file_stem().and_then(|s| s.to_str()) {
        if let Some(ext) = src.extension().and_then(|e| e.to_str()) {
            let new_name = format!("{}{}{}.{}", stem, sep, hash, ext);
            src.with_file_name(new_name)
        } else {
            src.with_file_name(format!("{}{}{}", stem, sep, hash))
        }
    } else {
        src.to_path_buf()
    }
}

fn verify_hash(path: &Path, expected: &str) -> Result<()> {
    use xxhash_rust::xxh3::Xxh3;

    let mut hasher = Xxh3::new();
    let mut file = std::fs::File::open(path)?;
    let mut buffer = [0u8; 16 * 1024 * 1024]; // 16MB chunks

    use std::io::Read;
    loop {
        let n = file.read(&mut buffer)?;
        if n == 0 {
            break;
        }
        hasher.update(&buffer[..n]);
    }

    let got = format!("{:X}", hasher.digest());
    if got == expected.to_uppercase() {
        Ok(())
    } else {
        Err(anyhow!(
            "hash mismatch (manifest {} vs actual {})",
            expected,
            got
        ))
    }
}
