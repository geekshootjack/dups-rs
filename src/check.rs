use crate::generate::format_size;
use crate::hashfile::name_key;
use crate::rename::{has_hash_suffix, passes_filters};
use anyhow::Result;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use walkdir::WalkDir;

/// A group of files that share a filename (case-folded per platform) but live
/// in different directories.
pub struct DupeGroup {
    /// Absolute paths, sorted.
    pub members: Vec<PathBuf>,
    /// File sizes parallel to `members` (None when metadata failed).
    pub sizes: Vec<Option<u64>>,
    /// True when the shared filename's stem already ends with `_16HEX` AND all
    /// members have the same (known) file size. Since members are grouped by
    /// identical filename, they all carry the SAME suffix, and equal sizes back
    /// up the name's claim of content identity (this tool's invariant: same
    /// name => same hash => same content) — so such groups are treated as true
    /// copies: no ambiguity, nothing to rename, no need to re-hash. The name
    /// check alone is NOT enough: 16 decimal digits are valid hex, and camera
    /// timestamp names like `REC_2026070212345678.mp4` can collide with
    /// different content; requiring equal sizes catches those whenever sizes
    /// differ. Residual corner: same-size different-content files with a
    /// hash-shaped name are still (mis)classified benign — detectable only by
    /// hashing, which this zero-cost classification deliberately avoids.
    pub benign: bool,
}

/// Walk `root`, applying the standard `passes_filters` predicate, and group the
/// surviving files by filename (case-insensitive on Windows/macOS, case-sensitive
/// on Linux — see `hashfile::name_key`). Returns the total number of files
/// scanned and the duplicate-name groups (2+ members), sorted by filename, with
/// members sorted by path, sizes fetched, and each group classified as benign
/// (hash-suffixed true copies with matching sizes) or not. Shared by `check`
/// and `rename --only-dupes` so both agree on the classification by construction.
pub fn scan_duplicate_groups(root: &Path, all_files: bool) -> Result<(usize, Vec<DupeGroup>)> {
    let mut by_key: HashMap<String, Vec<PathBuf>> = HashMap::new();
    let mut total = 0usize;

    for entry in WalkDir::new(root).into_iter().filter_map(|e| e.ok()) {
        let p = entry.path();
        if !passes_filters(p, all_files) {
            continue;
        }
        total += 1;
        let abs = std::path::absolute(p).unwrap_or_else(|_| p.to_path_buf());
        by_key.entry(name_key(&abs)).or_default().push(abs);
    }

    let mut groups: Vec<DupeGroup> = by_key
        .into_values()
        .filter(|members| members.len() >= 2)
        .map(|mut members| {
            members.sort();
            let sizes: Vec<Option<u64>> = members
                .iter()
                .map(|p| std::fs::metadata(p).ok().map(|m| m.len()))
                .collect();
            let all_same_size = match sizes.first() {
                Some(Some(first)) => sizes.iter().all(|s| *s == Some(*first)),
                _ => false, // any metadata error => not provably same => not benign
            };
            let benign = has_hash_suffix(&members[0]) && all_same_size;
            DupeGroup {
                members,
                sizes,
                benign,
            }
        })
        .collect();

    groups.sort_by(|a, b| {
        let an = a.members[0].file_name().unwrap_or_default();
        let bn = b.members[0].file_name().unwrap_or_default();
        an.cmp(bn)
    });

    Ok((total, groups))
}

/// Print one duplicate-name group (members with sizes, paths relative to
/// `abs_root` where possible) under index `idx`; `label_for(all_same_size)`
/// supplies the classification text on the group header line.
fn print_group(idx: usize, group: &DupeGroup, abs_root: &Path, label_for: impl Fn(bool) -> &'static str) {
    let all_same_size = match group.sizes.first() {
        Some(Some(first)) => group.sizes.iter().all(|s| *s == Some(*first)),
        _ => false,
    };

    let display_name = group.members[0]
        .file_name()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_default();

    println!(
        "[{}] {} \u{2014} {} 个文件, {}",
        idx,
        display_name,
        group.members.len(),
        label_for(all_same_size)
    );

    for (member, size) in group.members.iter().zip(group.sizes.iter()) {
        let rel = member.strip_prefix(abs_root).unwrap_or(member);
        let size_str = match size {
            Some(s) => format_size(*s),
            None => "大小未知".to_string(),
        };
        println!("      {}  ({})", rel.display(), size_str);
    }
}

/// Read-only duplicate-filename report. Never writes any file. Returns the
/// number of REAL (ambiguous, non-benign) duplicate-name groups found; benign
/// true-copy groups (filenames already carrying the same hash suffix, with
/// matching sizes) are reported in a separate section and do not count toward
/// the result / exit code, so the diagnose -> fix -> re-check loop converges
/// to clean.
pub fn check(path: &Path, all_files: bool) -> Result<usize> {
    let (total, groups) = scan_duplicate_groups(path, all_files)?;
    let abs_root = std::path::absolute(path).unwrap_or_else(|_| path.to_path_buf());

    println!(
        "检查目录: {} ({})",
        path.display(),
        if all_files { "所有文件类型" } else { "仅视频文件" }
    );
    println!("共扫描 {} 个文件。", total);

    if groups.is_empty() {
        println!("\n未发现重名文件。共 {} 个文件, 文件名全部唯一。", total);
        return Ok(0);
    }

    let (benign, real): (Vec<&DupeGroup>, Vec<&DupeGroup>) =
        groups.iter().partition(|g| g.benign);

    if real.is_empty() {
        println!("\n未发现有歧义的重名文件。");
    } else {
        println!();
        println!("发现 {} 组重名文件:", real.len());
        println!();
        for (i, group) in real.iter().enumerate() {
            print_group(i + 1, group, &abs_root, |all_same_size| {
                if all_same_size {
                    "大小相同 (可能是同一内容的拷贝)"
                } else {
                    "大小不同 (内容必然不同, 危险!)"
                }
            });
        }
    }

    if !benign.is_empty() {
        println!();
        println!(
            "另有 {} 组同名真副本 (文件名已含相同哈希且大小一致, 无歧义):",
            benign.len()
        );
        println!();
        for (i, group) in benign.iter().enumerate() {
            print_group(i + 1, group, &abs_root, |_| "文件名已含相同哈希, 大小一致");
        }
    }

    if !real.is_empty() {
        println!();
        println!("下一步 (预演, 不改动任何文件):");
        println!("  dups rename {} --only-dupes", path.display());
        println!("确认无误后加 --apply 执行。");
    }

    Ok(real.len())
}
