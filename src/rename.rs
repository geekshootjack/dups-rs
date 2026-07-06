use crate::hashfile::{path_key, HashEntry, HashFile};
use crate::logging::{escape_csv, Journal};
use anyhow::{anyhow, Result};
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use walkdir::WalkDir;

pub const DEFAULT_VIDEO_EXTS: &[&str] = &[
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
    pub warnings: Vec<String>,
}

pub struct RenameOperation {
    path: PathBuf,
    hashfile: Option<PathBuf>,
    verify: bool,
    dry_run: bool,
    all_files: bool,
    only_dupes: bool,
}

impl RenameOperation {
    pub fn new(
        path: PathBuf,
        hashfile: Option<PathBuf>,
        verify: bool,
        dry_run: bool,
        all_files: bool,
        only_dupes: bool,
    ) -> Result<Self> {
        if !path.exists() || !path.is_dir() {
            return Err(anyhow!("Path does not exist or is not a directory: {}", path.display()));
        }

        Ok(RenameOperation {
            path,
            hashfile,
            verify,
            dry_run,
            all_files,
            only_dupes,
        })
    }

    pub fn execute(&self) -> Result<()> {
        // Find or load hashfile entries: the full manifest set normally, or (with
        // --only-dupes) only the entries needed for duplicate-name group members,
        // computing hashes on the fly for anything not covered by a manifest.
        let entries = if self.only_dupes {
            self.load_only_dupes_entries()?
        } else {
            self.load_entries()?
        };
        if entries.is_empty() {
            if self.only_dupes {
                println!("未发现重名文件, 无需改名。");
            } else {
                println!("没有找到任何哈希条目");
            }
            return Ok(());
        }

        // Build the plan
        let mut report = self.build_plan(&entries)?;

        // Print report
        self.print_report(&report)?;

        if self.dry_run {
            // Write dry-run report into the target directory (not the CWD).
            let now = chrono::Local::now().format("%Y%m%d-%H%M%S");
            let log_path = self.path.join(format!("dups-dryrun-{}.csv", now));
            self.write_csv_log(&report, &log_path)?;
            println!("\n日志已写出: {}", log_path.display());

            println!("\n*** 预演模式 (DRY-RUN) *** 未改动任何文件。");
            let to_rename = report
                .actions
                .iter()
                .filter(|a| a.status == "rename")
                .count();
            println!("计划改名 {} 个。确认无误后加 --apply 执行。", to_rename);
        } else {
            // Execute renames with write-ahead logging (logs are created during apply_renames)
            let log_file = self.apply_renames(&mut report)?;
            println!("\n日志已写出: {}", log_file);

            // Print summary and next steps
            self.print_apply_summary(&report, &log_file)?;
        }

        Ok(())
    }

    fn load_entries(&self) -> Result<Vec<HashEntry>> {
        self.load_manifest_entries(false)
    }

    /// Load manifest entries (explicit --hashfile, or discovered *.xxh3 files).
    /// When `quiet` is set, a missing/empty manifest set is not an error and
    /// prints nothing (used by --only-dupes, where hashes can be computed).
    fn load_manifest_entries(&self, quiet: bool) -> Result<Vec<HashEntry>> {
        if let Some(manifest_path) = &self.hashfile {
            if !manifest_path.exists() {
                return Err(anyhow!("Hashfile not found: {}", manifest_path.display()));
            }
            HashFile::parse(manifest_path)
        } else {
            // Find .xxh3 files in the directory
            let manifests = HashFile::find_in_dir(&self.path, "*.xxh3")?;
            if manifests.is_empty() {
                if !quiet {
                    println!(
                        "未找到 .xxh3 文件。可运行 dups generate <PATH> 自动生成。"
                    );
                }
                return Ok(Vec::new());
            }
            HashFile::load_all(&manifests)
        }
    }

    /// Build the entry set for `--only-dupes`: only duplicate-filename group
    /// members, sourced from any available manifest where possible, with hashes
    /// computed on the fly for members no manifest covers. Exposed (not just
    /// used internally by `execute`) so callers/tests can inspect the plan
    /// without going through the full dry-run/apply/logging flow.
    pub fn load_only_dupes_entries(&self) -> Result<Vec<HashEntry>> {
        let (_total, groups) = crate::check::scan_duplicate_groups(&self.path, self.all_files)?;
        if groups.is_empty() {
            return Ok(Vec::new());
        }

        let manifest_entries = self.load_manifest_entries(true)?;
        let mut by_path: HashMap<String, Vec<HashEntry>> = HashMap::new();
        for e in manifest_entries {
            by_path.entry(path_key(&e.abs_path)).or_default().push(e);
        }

        let mut result = Vec::new();
        for group in &groups {
            // Benign groups (shared filename already carries the same hash
            // suffix) are true copies by this tool's invariant: nothing to
            // rename, so skip them entirely — most importantly, do NOT re-hash
            // potentially huge files just to conclude "done" on every re-run.
            if group.benign {
                continue;
            }
            for member in &group.members {
                let key = path_key(member);
                if let Some(hits) = by_path.get(&key) {
                    result.extend(hits.iter().cloned());
                    continue;
                }

                let size = std::fs::metadata(member).map(|m| m.len()).unwrap_or(0);
                let rel = member.strip_prefix(&self.path).unwrap_or(member);
                println!(
                    "计算哈希: {} ({})",
                    rel.display(),
                    crate::generate::format_size(size)
                );
                let hash = hash_file_chunks(member, |_| {})?;
                result.push(HashEntry {
                    hash,
                    abs_path: member.clone(),
                    manifest_path: PathBuf::from("<computed>"),
                    computed: true,
                });
            }
        }

        Ok(result)
    }

    pub fn build_plan(&self, entries: &[HashEntry]) -> Result<Report> {
        let mut report = Report {
            actions: Vec::new(),
            orphans: Vec::new(),
            warnings: Vec::new(),
        };

        let mut seen_src: HashMap<String, HashEntry> = HashMap::new();
        let mut conflicting: HashSet<String> = HashSet::new();
        let sep = "_";

        // De-duplicate entries. A path listed with two *different* hashes is a
        // conflict: it must be excluded from renaming entirely (a single
        // "conflict" action, and removed from seen_src so it is never renamed).
        for entry in entries {
            let key = path_key(&entry.abs_path);
            if conflicting.contains(&key) {
                continue;
            }
            if let Some(prev) = seen_src.get(&key) {
                if prev.hash != entry.hash {
                    let warning = format!(
                        "Warning: {} listed with two different hashes:\n  manifest 1: {} (from {})\n  manifest 2: {} (from {})",
                        entry.abs_path.display(),
                        prev.hash,
                        prev.manifest_path.display(),
                        entry.hash,
                        entry.manifest_path.display()
                    );
                    report.warnings.push(warning.clone());
                    println!("{}", warning);

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

                    // Exclude the path from renaming: drop the first entry and
                    // remember the conflict so any further entries are ignored.
                    seen_src.remove(&key);
                    conflicting.insert(key);
                }
                // else: identical duplicate, ignore.
            } else {
                seen_src.insert(key, entry.clone());
            }
        }

        // Map target-path key -> indices into report.actions for collision detection.
        let mut proposed: HashMap<String, Vec<usize>> = HashMap::new();

        for entry in seen_src.values() {
            let src = &entry.abs_path;

            // Check if it's a system file to exclude
            if is_system_file(src) {
                report.actions.push(Action {
                    src: src.clone(),
                    dst: None,
                    hash: entry.hash.clone(),
                    status: "not-video".to_string(),
                    note: "system file (excluded)".to_string(),
                });
                continue;
            }

            // Check if it's a video (unless --all-files is set)
            if !self.all_files && !is_video(src, DEFAULT_VIDEO_EXTS) {
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

            // Idempotency: the file stem already ends with `_{hash}` (case-insensitive),
            // so it has already been suffixed. Renaming again would produce `_HASH_HASH`.
            if already_suffixed(src, &entry.hash) {
                report.actions.push(Action {
                    src: src.clone(),
                    dst: Some(src.clone()),
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
                    let note = format!(
                        "listed in manifest but not found on disk (checked: {})",
                        src.display()
                    );
                    report.actions.push(Action {
                        src: src.clone(),
                        dst: None,
                        hash: entry.hash.clone(),
                        status: "missing".to_string(),
                        note,
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

            // Verify hash if requested. Freshly-computed hashes (--only-dupes,
            // no manifest coverage) were just read from disk; re-reading a huge
            // file a second time to "verify" the hash we just computed from it
            // is pointless, so those entries skip verification.
            if self.verify && !entry.computed {
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

            let target_key = path_key(&target);
            report.actions.push(action);
            let idx = report.actions.len() - 1;
            proposed.entry(target_key).or_default().push(idx);
        }

        // Check for collisions: mutate the real actions (report.actions), not clones.
        for idxs in proposed.values() {
            if idxs.len() > 1 {
                let count = idxs.len();
                for &idx in idxs {
                    report.actions[idx].status = "conflict".to_string();
                    report.actions[idx].note =
                        format!("{} different files would collide on target name", count);
                }
            }
        }

        // Populate orphans: files in the target directory that pass the same
        // filters but are not covered by any manifest entry (not a source, not a
        // planned/done target). Reporting only; never renamed. Skipped entirely
        // for --only-dupes: everything outside the duplicate-name groups would
        // show up as a false "orphan" there.
        if !self.only_dupes {
            let mut covered: HashSet<String> = HashSet::new();
            for action in &report.actions {
                covered.insert(path_key(&action.src));
                if let Some(dst) = &action.dst {
                    covered.insert(path_key(dst));
                }
            }
            for entry in WalkDir::new(&self.path).into_iter().filter_map(|e| e.ok()) {
                let p = entry.path();
                if !passes_filters(p, self.all_files) {
                    continue;
                }
                let abs = std::path::absolute(p).unwrap_or_else(|_| p.to_path_buf());
                if covered.contains(&path_key(&abs)) {
                    continue;
                }
                report.orphans.push(abs);
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

    fn write_csv_log(&self, report: &Report, path: &Path) -> Result<()> {
        use std::fs::File;
        use std::io::Write;

        let mut file = File::create(path)?;

        // Write UTF-8 BOM for Windows compatibility
        file.write_all(&[0xEF, 0xBB, 0xBF])?;

        // Write header
        writeln!(file, "timestamp,status,hash,old_path,new_path,note")?;

        let now = chrono::Local::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, false);

        // Write each action
        for action in &report.actions {
            let old_path = action.src.to_string_lossy();
            let new_path = action.dst.as_ref().map(|p| p.to_string_lossy()).unwrap_or_default();

            writeln!(
                file,
                "{},{},{},{},{},{}",
                now,
                action.status,
                action.hash,
                escape_csv(&old_path),
                escape_csv(&new_path),
                escape_csv(&action.note)
            )?;
        }

        // Write orphans (schema: timestamp,status,hash,old_path,new_path,note)
        for orphan in &report.orphans {
            writeln!(
                file,
                "{},orphan,,{},,no hash in any manifest",
                now,
                escape_csv(&orphan.to_string_lossy())
            )?;
        }

        // Write warnings as separate rows
        for warning in &report.warnings {
            writeln!(
                file,
                "{},warning,,,,{}",
                now,
                escape_csv(warning)
            )?;
        }

        Ok(())
    }

    fn apply_renames(&self, report: &mut Report) -> Result<String> {
        let to_rename_indices: Vec<_> = report
            .actions
            .iter()
            .enumerate()
            .filter(|(_, a)| a.status == "rename")
            .map(|(i, _)| i)
            .collect();

        let now = chrono::Local::now().format("%Y%m%d-%H%M%S");
        // Write the applied log into the target directory (not the CWD).
        let log_path = self.path.join(format!("dups-applied-{}.csv", now));
        let log_display = log_path.to_string_lossy().to_string();

        if to_rename_indices.is_empty() {
            println!("没有需要改名的文件。");
            Journal::new(&log_path)?;
            return Ok(log_display);
        }

        println!("开始执行 {} 个重命名...", to_rename_indices.len());

        let mut journal = Journal::new(&log_path)?;

        let mut success = 0;
        let mut failed = 0;

        for idx in to_rename_indices {
            let (src, dst, hash) = {
                let a = &report.actions[idx];
                match &a.dst {
                    Some(dst) => (a.src.clone(), dst.clone(), a.hash.clone()),
                    None => continue,
                }
            };
            let src_name = src.file_name().unwrap_or_default().to_string_lossy().to_string();

            // Re-check disk state immediately before renaming: the plan->apply
            // gap can be large (especially with --verify), and std::fs::rename
            // silently overwrites an existing destination on Windows and Linux.
            if !src.exists() {
                failed += 1;
                let msg = "source no longer exists at apply time";
                println!("  [ERROR] {}: {}", src_name, msg);
                journal.record("error", &hash, &src.to_string_lossy(), &dst.to_string_lossy(), msg)?;
                report.actions[idx].status = "error".to_string();
                report.actions[idx].note = msg.to_string();
                continue;
            }
            if dst.exists() {
                failed += 1;
                let msg = "target now exists on disk (would overwrite); skipped";
                println!("  [ERROR] {}: {}", src_name, msg);
                journal.record("conflict", &hash, &src.to_string_lossy(), &dst.to_string_lossy(), msg)?;
                report.actions[idx].status = "conflict".to_string();
                report.actions[idx].note = msg.to_string();
                continue;
            }

            // Write-ahead logging: record intention BEFORE executing
            journal.record(
                "pending",
                &hash,
                &src.to_string_lossy(),
                &dst.to_string_lossy(),
                "about to rename",
            )?;

            // Now execute the rename
            match std::fs::rename(&src, &dst) {
                Ok(_) => {
                    success += 1;
                    println!("  [OK] {}", src_name);

                    // Record success immediately after operation
                    journal.record(
                        "renamed",
                        &hash,
                        &src.to_string_lossy(),
                        &dst.to_string_lossy(),
                        "",
                    )?;
                    report.actions[idx].status = "renamed".to_string();
                }
                Err(e) => {
                    failed += 1;
                    let error_msg = e.to_string();
                    println!("  [ERROR] {}: {}", src_name, e);

                    // Record error immediately after failed operation
                    journal.record(
                        "error",
                        &hash,
                        &src.to_string_lossy(),
                        &dst.to_string_lossy(),
                        &error_msg,
                    )?;
                    report.actions[idx].status = "error".to_string();
                    report.actions[idx].note = format!("rename failed: {}", error_msg);
                }
            }
        }

        println!("重命名完成: 成功 {}, 失败 {}", success, failed);

        Ok(log_display)
    }

    fn print_apply_summary(&self, report: &Report, log_file: &str) -> Result<()> {
        let renamed = report
            .actions
            .iter()
            .filter(|a| a.status == "renamed")
            .count();
        let errors = report
            .actions
            .iter()
            .filter(|a| a.status == "error")
            .count();

        println!("\n{}", "=".repeat(70));
        println!("执行完成");
        println!("{}", "-".repeat(70));
        println!("[OK] 成功改名: {} 个", renamed);
        if errors > 0 {
            println!("[ERROR] 改名失败: {} 个", errors);
        }
        println!("{}", "=".repeat(70));

        if renamed > 0 {
            println!("\nNext steps:");
            println!("  1. 如需撤销所有改名，运行:");
            println!("     dups undo {}", log_file);
            println!("  2. 详细日志见: {}", log_file);
        }

        Ok(())
    }

}

pub fn is_system_file(path: &Path) -> bool {
    const SYSTEM_EXTS: &[&str] = &[
        ".exe", ".dll", ".sys", ".driver", ".scr",
        ".bat", ".cmd", ".ps1", ".msi",
        ".lnk", ".url", ".desktop", ".app",
        ".ini", ".config", ".conf",
    ];

    if let Some(ext) = path.extension() {
        if let Some(ext_str) = ext.to_str() {
            let ext_lower = format!(".{}", ext_str.to_lowercase());
            SYSTEM_EXTS.iter().any(|&e| e == ext_lower)
        } else {
            false
        }
    } else {
        false
    }
}

pub fn is_video(path: &Path, exts: &[&str]) -> bool {
    if let Some(ext) = path.extension() {
        if let Some(ext_str) = ext.to_str() {
            let ext_lower = format!(".{}", ext_str.to_lowercase());
            exts.iter().any(|e| *e == ext_lower)
        } else {
            false
        }
    } else {
        false
    }
}

/// Shared file predicate used by `check`, `generate`, and the orphan scan in
/// `rename`: skip non-files, system files, non-video files (unless
/// `all_files`), hashfiles, and dups-generated CSV logs. Behavior must stay
/// identical across all three call sites.
pub fn passes_filters(path: &Path, all_files: bool) -> bool {
    if !path.is_file() {
        return false;
    }
    if is_system_file(path) {
        return false;
    }
    if !all_files && !is_video(path, DEFAULT_VIDEO_EXTS) {
        return false;
    }
    if let Some(ext) = path.extension().and_then(|e| e.to_str()) {
        if ext.eq_ignore_ascii_case("xxh3") || ext.eq_ignore_ascii_case("xxh") {
            return false;
        }
    }
    if let Some(name) = path.file_name().and_then(|n| n.to_str()) {
        if name.starts_with("dups-") && name.ends_with(".csv") {
            return false;
        }
    }
    true
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

/// True when the file stem already ends with `_{hash}` (case-insensitive 16-hex
/// compare), i.e. the file has already been suffixed. Prevents `_HASH_HASH`.
pub fn already_suffixed(src: &Path, hash: &str) -> bool {
    if let Some(stem) = src.file_stem().and_then(|s| s.to_str()) {
        let suffix = format!("_{}", hash);
        let stem_bytes = stem.as_bytes();
        if stem_bytes.len() >= suffix.len() {
            let tail = &stem_bytes[stem_bytes.len() - suffix.len()..];
            return tail.eq_ignore_ascii_case(suffix.as_bytes());
        }
    }
    false
}

/// True when the file stem ends with `_` followed by exactly 16 hex digits,
/// i.e. the name already carries *some* hash suffix (the hash itself is not
/// known a priori — contrast with `already_suffixed`, which checks a specific
/// hash). Because duplicate-name groups are keyed on the full filename, all
/// members of a group whose name has a hash suffix share the SAME suffix, and
/// the name itself proves content identity: such groups are benign true copies.
/// Kept next to `already_suffixed` so the two suffix definitions stay in sync.
pub fn has_hash_suffix(src: &Path) -> bool {
    if let Some(stem) = src.file_stem().and_then(|s| s.to_str()) {
        let bytes = stem.as_bytes();
        if bytes.len() >= 17 {
            let tail = &bytes[bytes.len() - 17..];
            return tail[0] == b'_' && tail[1..].iter().all(|b| b.is_ascii_hexdigit());
        }
    }
    false
}

/// Compute the xxHash3-64 of a file as a zero-padded 16-char uppercase hex string,
/// invoking `on_chunk(bytes_read)` after each chunk is hashed. This is the single
/// chunked reader shared by the plain `hash_file` below and by callers that want
/// progress feedback (generate.rs's multi-file progress bar, rename.rs's
/// --only-dupes on-the-fly hashing) — the hashing/read loop must not be duplicated.
pub fn hash_file_chunks<F: FnMut(u64)>(path: &Path, mut on_chunk: F) -> Result<String> {
    use std::io::Read;
    use xxhash_rust::xxh3::Xxh3;

    let mut hasher = Xxh3::new();
    let mut file = std::fs::File::open(path)?;
    let mut buffer = vec![0u8; 16 * 1024 * 1024]; // 16MB chunks

    loop {
        let n = file.read(&mut buffer)?;
        if n == 0 {
            break;
        }
        hasher.update(&buffer[..n]);
        on_chunk(n as u64);
    }

    // Zero-pad to 16 hex digits: {:X} drops leading zeros, causing ~1/16 of
    // files to appear to mismatch. generate.rs uses {:016X} for the same reason.
    Ok(format!("{:016X}", hasher.digest()))
}

/// Compute the xxHash3-64 of a file as a zero-padded 16-char uppercase hex string.
pub fn hash_file(path: &Path) -> Result<String> {
    hash_file_chunks(path, |_| {})
}

pub fn verify_hash(path: &Path, expected: &str) -> Result<()> {
    let got = hash_file(path)?;
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
