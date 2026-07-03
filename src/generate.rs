use anyhow::{anyhow, Result};
use std::collections::HashSet;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::Instant;
use walkdir::WalkDir;

use crate::hashfile::{path_key, HashFile};
use crate::rename::{hash_file_chunks, passes_filters};

struct FileEntry {
    path: PathBuf,
    size: u64,
}

struct Progress {
    bytes_done: u64,
    total_bytes: u64,
    total_files: usize,
    idx_width: usize,
    start: Instant,
}

impl Progress {
    fn stats(&self) -> (f64, f64, std::time::Duration) {
        let pct = if self.total_bytes > 0 {
            self.bytes_done as f64 / self.total_bytes as f64 * 100.0
        } else {
            100.0
        };
        let elapsed = self.start.elapsed().as_secs_f64();
        let speed = if elapsed > 0.0 {
            self.bytes_done as f64 / elapsed
        } else {
            0.0
        };
        let eta = if speed > 0.0 {
            let remaining = self.total_bytes.saturating_sub(self.bytes_done);
            std::time::Duration::from_secs_f64(remaining as f64 / speed)
        } else {
            std::time::Duration::ZERO
        };
        (pct, speed, eta)
    }
}

pub fn generate(path: &Path, output: Option<&Path>, all_files: bool) -> Result<()> {
    if !path.exists() || !path.is_dir() {
        return Err(anyhow!(
            "路径不存在或不是目录: {}",
            path.display()
        ));
    }

    println!("扫描目录: {}", path.display());

    let mut files: Vec<FileEntry> = Vec::new();
    for entry in WalkDir::new(path).into_iter().filter_map(|e| e.ok()) {
        let p = entry.path();
        if !passes_filters(p, all_files) {
            continue;
        }
        let size = std::fs::metadata(p).map(|m| m.len()).unwrap_or(0);
        files.push(FileEntry {
            path: p.to_path_buf(),
            size,
        });
    }

    files.sort_by(|a, b| a.path.cmp(&b.path));

    let total_files = files.len();
    let total_bytes: u64 = files.iter().map(|f| f.size).sum();

    if total_files == 0 {
        println!("未找到符合条件的文件。");
        return Ok(());
    }

    println!(
        "找到 {} 个文件, 总计 {}",
        total_files,
        format_size(total_bytes)
    );

    // Orphan detection: compare against existing hashfiles
    let existing_manifests = HashFile::find_in_dir(path, "*.xxh3")?;
    if !existing_manifests.is_empty() {
        let existing_entries = HashFile::load_all(&existing_manifests)?;
        let existing_paths: HashSet<String> =
            existing_entries.iter().map(|e| path_key(&e.abs_path)).collect();

        let mut covered = 0usize;
        let mut orphans = 0usize;
        for file in &files {
            let abs = std::path::absolute(&file.path).unwrap_or_else(|_| file.path.clone());
            if existing_paths.contains(&path_key(&abs)) {
                covered += 1;
            } else {
                orphans += 1;
            }
        }

        println!(
            "已有清单覆盖 {} 个, 新发现 {} 个未覆盖文件",
            covered, orphans
        );
    }

    println!();

    // Hash each file with progress
    let mut progress = Progress {
        bytes_done: 0,
        total_bytes,
        total_files,
        idx_width: total_files.to_string().len(),
        start: Instant::now(),
    };
    let mut results: Vec<(PathBuf, String)> = Vec::new();
    let mut errors: usize = 0;

    for (i, file) in files.iter().enumerate() {
        let rel_path = file.path.strip_prefix(path).unwrap_or(&file.path);

        match hash_file_with_progress(&file.path, file.size, i + 1, rel_path, &mut progress) {
            Ok(hash) => {
                let (overall_pct, speed, eta) = progress.stats();

                // Clear any intermediate progress line
                print!("\r{}\r", " ".repeat(79));
                println!(
                    "[{:>w$}/{}] {:>5.1}%  {:>9}/s  剩余 {}  {}  ({})",
                    i + 1,
                    total_files,
                    overall_pct,
                    format_size(speed as u64),
                    format_duration(eta),
                    rel_path.display(),
                    format_size(file.size),
                    w = progress.idx_width,
                );

                results.push((file.path.clone(), hash));
            }
            Err(e) => {
                print!("\r{}\r", " ".repeat(79));
                println!(
                    "[{:>w$}/{}] [ERROR] {}: {}",
                    i + 1,
                    total_files,
                    rel_path.display(),
                    e,
                    w = progress.idx_width,
                );
                progress.bytes_done += file.size;
                errors += 1;
            }
        }
    }

    let total_elapsed = progress.start.elapsed();
    let avg_speed = if total_elapsed.as_secs_f64() > 0.0 {
        total_bytes as f64 / total_elapsed.as_secs_f64()
    } else {
        0.0
    };

    // Write output file
    let output_path = if let Some(out) = output {
        out.to_path_buf()
    } else {
        let now = chrono::Local::now().format("%Y%m%d-%H%M%S");
        path.join(format!("dups-manifest-{}.xxh3", now))
    };

    write_hashfile(&output_path, path, &results)?;

    // Summary
    println!();
    println!("{}", "=".repeat(70));
    println!("生成完成");
    println!("{}", "-".repeat(70));
    println!("  输出文件: {}", output_path.display());
    println!("  文件数量: {} 个", results.len());
    if errors > 0 {
        println!("  读取失败: {} 个", errors);
    }
    println!("  总数据量: {}", format_size(total_bytes));
    println!("  总用时:   {}", format_duration(total_elapsed));
    println!("  平均速度: {}/s", format_size(avg_speed as u64));
    println!("{}", "=".repeat(70));

    println!("\n可直接用此清单执行重命名:");
    println!(
        "  dups rename {} --hashfile {}",
        path.display(),
        output_path.display()
    );

    Ok(())
}

fn hash_file_with_progress(
    path: &Path,
    file_size: u64,
    file_num: usize,
    rel_path: &Path,
    progress: &mut Progress,
) -> Result<String> {
    let mut file_bytes_read: u64 = 0;
    let mut last_progress = Instant::now();
    let large_file = file_size > 256 * 1024 * 1024;

    // Reuses the single chunked xxh3 reader (rename::hash_file_chunks); this
    // closure only adds the multi-file progress-bar rendering on top of it.
    hash_file_chunks(path, |n| {
        file_bytes_read += n;
        progress.bytes_done += n;

        if large_file && last_progress.elapsed().as_millis() >= 500 {
            last_progress = Instant::now();

            let (overall_pct, speed, eta) = progress.stats();
            let file_pct = if file_size > 0 {
                file_bytes_read as f64 / file_size as f64 * 100.0
            } else {
                100.0
            };

            let display_name = rel_path
                .file_name()
                .unwrap_or_default()
                .to_string_lossy();
            print!(
                "\r[{:>w$}/{}] {:>5.1}%  {:>9}/s  剩余 {}  {} ({:.0}% of {})     ",
                file_num,
                progress.total_files,
                overall_pct,
                format_size(speed as u64),
                format_duration(eta),
                display_name,
                file_pct,
                format_size(file_size),
                w = progress.idx_width,
            );
            std::io::stdout().flush().ok();
        }
    })
}

fn write_hashfile(output: &Path, base: &Path, entries: &[(PathBuf, String)]) -> Result<()> {
    let mut file = std::fs::File::create(output)?;

    for (path, hash) in entries {
        let rel = path.strip_prefix(base).unwrap_or(path);
        let rel_str = rel.to_string_lossy();
        let rel_normalized = if cfg!(windows) {
            rel_str.to_string()
        } else {
            rel_str.replace('/', "\\")
        };
        writeln!(file, "{} *{}", hash, rel_normalized)?;
    }

    file.sync_all()?;
    Ok(())
}

pub fn format_size(bytes: u64) -> String {
    const KB: u64 = 1024;
    const MB: u64 = 1024 * KB;
    const GB: u64 = 1024 * MB;
    const TB: u64 = 1024 * GB;

    if bytes >= TB {
        format!("{:.1} TB", bytes as f64 / TB as f64)
    } else if bytes >= GB {
        format!("{:.1} GB", bytes as f64 / GB as f64)
    } else if bytes >= MB {
        format!("{:.1} MB", bytes as f64 / MB as f64)
    } else if bytes >= KB {
        format!("{:.1} KB", bytes as f64 / KB as f64)
    } else {
        format!("{} B", bytes)
    }
}

fn format_duration(d: std::time::Duration) -> String {
    let secs = d.as_secs();
    if secs == 0 {
        return "00:00".to_string();
    }
    let hours = secs / 3600;
    let mins = (secs % 3600) / 60;
    let s = secs % 60;
    if hours > 0 {
        format!("{:02}:{:02}:{:02}", hours, mins, s)
    } else {
        format!("{:02}:{:02}", mins, s)
    }
}
