//! Integration tests for dups. Uses only std for temp dirs (unique names under
//! std::env::temp_dir()) so no dev-dependency is required.

use std::collections::HashSet;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use dups::check;
use dups::hashfile::{HashEntry, HashFile};
use dups::logging::undo;
use dups::rename::{self, RenameOperation};

static COUNTER: AtomicU64 = AtomicU64::new(0);

/// A unique temp directory that is removed on drop.
struct TmpDir(PathBuf);

impl TmpDir {
    fn new(tag: &str) -> Self {
        let n = COUNTER.fetch_add(1, Ordering::SeqCst);
        let pid = std::process::id();
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let dir = std::env::temp_dir().join(format!("dups_it_{tag}_{pid}_{n}_{nanos}"));
        fs::create_dir_all(&dir).unwrap();
        TmpDir(dir)
    }

    fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for TmpDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn entry(hash: &str, path: &Path) -> HashEntry {
    HashEntry {
        hash: hash.to_string(),
        abs_path: path.to_path_buf(),
        manifest_path: PathBuf::from("dummy.xxh3"),
        computed: false,
    }
}

fn computed_entry(hash: &str, path: &Path) -> HashEntry {
    HashEntry {
        hash: hash.to_string(),
        abs_path: path.to_path_buf(),
        manifest_path: PathBuf::from("<computed>"),
        computed: true,
    }
}

/// Bug 4: verify uses zero-padded {:016X}; a hash with a leading zero must still
/// be 16 chars and verify successfully.
#[test]
fn verify_zero_padding() {
    let dir = TmpDir::new("pad");
    let probe = dir.path().join("probe.bin");

    // Brute-force a small content whose hash begins with '0' (~1/16 chance).
    let mut found: Option<String> = None;
    for i in 0u32..10_000 {
        fs::write(&probe, i.to_le_bytes()).unwrap();
        let h = rename::hash_file(&probe).unwrap();
        assert_eq!(h.len(), 16, "hash must always be zero-padded to 16 chars");
        if h.starts_with('0') {
            found = Some(h);
            break;
        }
    }
    let h = found.expect("should find a leading-zero hash within 10000 tries");
    assert_eq!(h.len(), 16);
    // The correctly-padded hash must verify.
    assert!(rename::verify_hash(&probe, &h).is_ok());
    // A truncated (buggy {:X}) form must NOT verify.
    assert!(rename::verify_hash(&probe, h.trim_start_matches('0')).is_err());
}

/// Bug 2: re-running against a regenerated manifest must not double-suffix.
#[test]
fn double_suffix_idempotency() {
    let dir = TmpDir::new("idem");
    let src = dir.path().join("video.mp4");
    fs::write(&src, b"hello world").unwrap();
    let hash = rename::hash_file(&src).unwrap();

    let op =
        RenameOperation::new(dir.path().to_path_buf(), None, false, true, false, false).unwrap();

    // First plan: a rename is scheduled.
    let report = op.build_plan(&[entry(&hash, &src)]).unwrap();
    let rename_action = report
        .actions
        .iter()
        .find(|a| a.status == "rename")
        .expect("expected a rename action");
    let dst = rename_action.dst.clone().unwrap();
    assert!(dst.file_name().unwrap().to_string_lossy().contains(&hash));

    // Simulate apply.
    fs::rename(&src, &dst).unwrap();
    assert!(dst.exists());

    // Regenerated manifest now lists the already-suffixed file with same hash.
    let report2 = op.build_plan(&[entry(&hash, &dst)]).unwrap();
    assert_eq!(
        report2.actions.iter().filter(|a| a.status == "rename").count(),
        0,
        "already-suffixed file must not be renamed again"
    );
    assert!(report2.actions.iter().any(|a| a.status == "done"));

    // And no double-suffixed file exists.
    let double = dir
        .path()
        .join(format!("video_{hash}_{hash}.mp4"));
    assert!(!double.exists());
}

/// Bug 1: a path listed with two different hashes must be excluded from renaming.
#[test]
fn conflicting_hash_not_renamed() {
    let dir = TmpDir::new("conflict");
    let src = dir.path().join("a.mp4");
    fs::write(&src, b"content").unwrap();

    let op =
        RenameOperation::new(dir.path().to_path_buf(), None, false, true, false, false).unwrap();
    let entries = vec![
        entry("AAAAAAAAAAAAAAAA", &src),
        entry("BBBBBBBBBBBBBBBB", &src),
    ];
    let report = op.build_plan(&entries).unwrap();

    assert_eq!(
        report.actions.iter().filter(|a| a.status == "rename").count(),
        0,
        "conflicting-hash path must not be scheduled for rename"
    );
    assert!(
        report.actions.iter().any(|a| a.status == "conflict"),
        "expected a conflict action for the path"
    );
}

/// Bug 5: a manifest with a leading UTF-8 BOM must parse all its entries.
#[test]
fn bom_manifest_parses_all_entries() {
    let dir = TmpDir::new("bom");
    let manifest = dir.path().join("m.xxh3");
    let mut bytes = vec![0xEFu8, 0xBB, 0xBF];
    bytes.extend_from_slice(b"AAAAAAAAAAAAAAAA *a.mp4\r\nBBBBBBBBBBBBBBBB *b.mp4\r\n");
    fs::write(&manifest, bytes).unwrap();

    let entries = HashFile::parse(&manifest).unwrap();
    assert_eq!(entries.len(), 2, "BOM must not swallow the first entry");
    assert_eq!(entries[0].hash, "AAAAAAAAAAAAAAAA");
    assert_eq!(entries[1].hash, "BBBBBBBBBBBBBBBB");
}

fn write_log(path: &Path, rows: &[(&str, &str, &str)]) {
    // rows: (status, old, new). Paths here never contain commas.
    let mut s = String::from("timestamp,status,hash,old_path,new_path,note\n");
    for (status, old, new) in rows {
        s.push_str(&format!(
            "2026-07-02T00:00:00+00:00,{status},HHHHHHHHHHHHHHHH,{old},{new},\n"
        ));
    }
    fs::write(path, s).unwrap();
}

/// Bug 3/8: undo reverts a renamed file, and refuses when the original name is
/// occupied.
#[test]
fn undo_reverts_and_refuses() {
    // --- revert case ---
    let dir = TmpDir::new("undo_ok");
    let old = dir.path().join("a.mp4");
    let new = dir.path().join("a_HHHHHHHHHHHHHHHH.mp4");
    fs::write(&new, b"data").unwrap();
    let log = dir.path().join("dups-applied-x.csv");
    write_log(
        &log,
        &[(
            "renamed",
            &old.to_string_lossy(),
            &new.to_string_lossy(),
        )],
    );
    undo(&log).unwrap();
    assert!(old.exists(), "original name should be restored");
    assert!(!new.exists(), "renamed file should be gone");

    // --- refuse case: original name occupied ---
    let dir2 = TmpDir::new("undo_refuse");
    let old2 = dir2.path().join("a.mp4");
    let new2 = dir2.path().join("a_HHHHHHHHHHHHHHHH.mp4");
    fs::write(&old2, b"occupied").unwrap();
    fs::write(&new2, b"data").unwrap();
    let log2 = dir2.path().join("dups-applied-y.csv");
    write_log(
        &log2,
        &[(
            "renamed",
            &old2.to_string_lossy(),
            &new2.to_string_lossy(),
        )],
    );
    undo(&log2).unwrap();
    assert!(old2.exists());
    assert!(new2.exists(), "must refuse to overwrite occupied original");
    assert_eq!(fs::read(&old2).unwrap(), b"occupied");
}

/// Bug 8: a "pending" row with no terminal record is recovered when the disk
/// state shows the rename happened; an "error" terminal row suppresses recovery.
#[test]
fn pending_row_recovery() {
    let dir = TmpDir::new("pending");

    // Pair A: pending only, rename evidently happened (new exists, old missing).
    let old_a = dir.path().join("a.mp4");
    let new_a = dir.path().join("a_HHHHHHHHHHHHHHHH.mp4");
    fs::write(&new_a, b"a-data").unwrap();

    // Pair B: pending + terminal error -> must NOT be reverted.
    let old_b = dir.path().join("b.mp4");
    let new_b = dir.path().join("b_HHHHHHHHHHHHHHHH.mp4");
    fs::write(&new_b, b"b-data").unwrap();

    let log = dir.path().join("dups-applied-p.csv");
    write_log(
        &log,
        &[
            ("pending", &old_a.to_string_lossy(), &new_a.to_string_lossy()),
            ("pending", &old_b.to_string_lossy(), &new_b.to_string_lossy()),
            ("error", &old_b.to_string_lossy(), &new_b.to_string_lossy()),
        ],
    );
    undo(&log).unwrap();

    assert!(old_a.exists(), "pending pair A should be recovered");
    assert!(!new_a.exists());

    assert!(!old_b.exists(), "pair B with terminal error must be left alone");
    assert!(new_b.exists());
}

/// Bug 6: the plan must never schedule two renames onto the same target name.
/// (target_for is injective, so this guards against regressions in the collision
/// detection / target scheme.)
#[test]
fn no_two_renames_share_target() {
    let dir = TmpDir::new("collide");
    let f1 = dir.path().join("one.mp4");
    let f2 = dir.path().join("two.mp4");
    let f3 = dir.path().join("three.mp4");
    fs::write(&f1, b"1").unwrap();
    fs::write(&f2, b"2").unwrap();
    fs::write(&f3, b"3").unwrap();

    let op =
        RenameOperation::new(dir.path().to_path_buf(), None, false, true, false, false).unwrap();
    // Include a duplicate entry for f1 to exercise de-duplication too.
    let entries = vec![
        entry("1111111111111111", &f1),
        entry("1111111111111111", &f1),
        entry("2222222222222222", &f2),
        entry("3333333333333333", &f3),
    ];
    let report = op.build_plan(&entries).unwrap();

    let mut targets: Vec<String> = report
        .actions
        .iter()
        .filter(|a| a.status == "rename")
        .map(|a| a.dst.as_ref().unwrap().to_string_lossy().to_lowercase())
        .collect();
    let before = targets.len();
    targets.sort();
    targets.dedup();
    assert_eq!(before, targets.len(), "no two renames may share a target");
    assert_eq!(before, 3, "each distinct source should be renamed once");
}

/// `check` groups files by filename across directories; a same-named pair of a
/// non-video extension must not be grouped unless --all-files is set.
#[test]
fn check_finds_duplicate_names() {
    let dir = TmpDir::new("check_dupes");
    fs::create_dir_all(dir.path().join("a")).unwrap();
    fs::create_dir_all(dir.path().join("b")).unwrap();
    fs::create_dir_all(dir.path().join("c")).unwrap();
    fs::write(dir.path().join("a").join("x.mp4"), b"content-a").unwrap();
    fs::write(dir.path().join("b").join("x.mp4"), b"content-b-different").unwrap();
    fs::write(dir.path().join("c").join("unique.mp4"), b"unique").unwrap();
    // Same-named .txt pair: must NOT be grouped without --all-files.
    fs::write(dir.path().join("a").join("note.txt"), b"note-a").unwrap();
    fs::write(dir.path().join("b").join("note.txt"), b"note-b").unwrap();

    let (total, groups) = check::scan_duplicate_groups(dir.path(), false).unwrap();
    assert_eq!(total, 3, "only the 3 video files should be scanned");
    assert_eq!(groups.len(), 1, "expected exactly one duplicate-name group (x.mp4)");
    assert_eq!(groups[0].members.len(), 2);

    let n = check::check(dir.path(), false).unwrap();
    assert_eq!(n, 1);

    // With --all-files, the note.txt pair becomes a second group.
    let (total_all, groups_all) = check::scan_duplicate_groups(dir.path(), true).unwrap();
    assert_eq!(total_all, 5);
    assert_eq!(groups_all.len(), 2);
}

/// A directory with no duplicate filenames reports zero groups.
#[test]
fn check_clean_dir_returns_zero_groups() {
    let dir = TmpDir::new("check_clean");
    fs::write(dir.path().join("a.mp4"), b"aaa").unwrap();
    fs::write(dir.path().join("b.mp4"), b"bbb").unwrap();

    let (total, groups) = check::scan_duplicate_groups(dir.path(), false).unwrap();
    assert_eq!(total, 2);
    assert!(groups.is_empty());

    let n = check::check(dir.path(), false).unwrap();
    assert_eq!(n, 0);
}

fn is_16char_uppercase_hex(s: &str) -> bool {
    s.len() == 16 && s.chars().all(|c| c.is_ascii_hexdigit() && !c.is_ascii_lowercase())
}

/// `rename --only-dupes` with no manifest: only the two same-named files are
/// planned (hashes computed on the fly); the unique file is absent from the plan
/// and the orphan scan (which would otherwise flag it) is skipped entirely.
#[test]
fn only_dupes_renames_only_collisions() {
    let dir = TmpDir::new("only_dupes_basic");
    fs::create_dir_all(dir.path().join("a")).unwrap();
    fs::create_dir_all(dir.path().join("b")).unwrap();
    let f1 = dir.path().join("a").join("dup.mp4");
    let f2 = dir.path().join("b").join("dup.mp4");
    let f3 = dir.path().join("unique.mp4");
    fs::write(&f1, b"content-one").unwrap();
    fs::write(&f2, b"content-two-different").unwrap();
    fs::write(&f3, b"content-unique").unwrap();

    let op =
        RenameOperation::new(dir.path().to_path_buf(), None, false, true, false, true).unwrap();
    let entries = op.load_only_dupes_entries().unwrap();
    assert_eq!(entries.len(), 2, "only the two same-named files should be collected");
    for e in &entries {
        assert!(e.computed, "no manifest present, hash must be computed on the fly");
        assert!(is_16char_uppercase_hex(&e.hash));
    }

    let report = op.build_plan(&entries).unwrap();
    let rename_actions: Vec<_> = report.actions.iter().filter(|a| a.status == "rename").collect();
    assert_eq!(rename_actions.len(), 2, "exactly the two dupe-group members should be planned");

    let abs_f1 = std::path::absolute(&f1).unwrap();
    let abs_f2 = std::path::absolute(&f2).unwrap();
    let abs_f3 = std::path::absolute(&f3).unwrap();
    let planned_srcs: HashSet<PathBuf> = rename_actions.iter().map(|a| a.src.clone()).collect();
    assert!(planned_srcs.contains(&abs_f1));
    assert!(planned_srcs.contains(&abs_f2));
    assert!(
        !report.actions.iter().any(|a| a.src == abs_f3),
        "unique.mp4 must not appear in an --only-dupes plan"
    );
    assert!(report.orphans.is_empty(), "orphan scan must be skipped for --only-dupes");
}

/// `rename --only-dupes` with a manifest covering one group member: that
/// member's hash comes from the manifest (not recomputed), while the other
/// (uncovered) member's hash is computed on the fly.
#[test]
fn only_dupes_uses_manifest_hash() {
    let dir = TmpDir::new("only_dupes_manifest");
    fs::create_dir_all(dir.path().join("a")).unwrap();
    fs::create_dir_all(dir.path().join("b")).unwrap();
    let f1 = dir.path().join("a").join("dup.mp4");
    let f2 = dir.path().join("b").join("dup.mp4");
    fs::write(&f1, b"content-one").unwrap();
    fs::write(&f2, b"content-two-different").unwrap();

    let hash1 = rename::hash_file(&f1).unwrap();
    let hash2 = rename::hash_file(&f2).unwrap();

    let manifest = dir.path().join("m.xxh3");
    let rel1 = Path::new("a").join("dup.mp4");
    fs::write(&manifest, format!("{} *{}\r\n", hash1, rel1.to_string_lossy())).unwrap();

    let op = RenameOperation::new(
        dir.path().to_path_buf(),
        Some(manifest.clone()),
        false,
        true,
        false,
        true,
    )
    .unwrap();
    let entries = op.load_only_dupes_entries().unwrap();
    assert_eq!(entries.len(), 2);

    let abs_f1 = std::path::absolute(&f1).unwrap();
    let abs_f2 = std::path::absolute(&f2).unwrap();

    let e1 = entries
        .iter()
        .find(|e| e.abs_path == abs_f1)
        .expect("manifest-covered member present");
    assert_eq!(e1.hash, hash1);
    assert!(!e1.computed, "manifest-covered entry must not be marked computed");

    let e2 = entries
        .iter()
        .find(|e| e.abs_path == abs_f2)
        .expect("uncovered member present");
    assert!(e2.computed, "uncovered entry must be computed on the fly");
    assert_eq!(e2.hash, hash2);

    let report = op.build_plan(&entries).unwrap();
    let action1 = report.actions.iter().find(|a| a.src == abs_f1).unwrap();
    assert_eq!(action1.status, "rename");
    assert_eq!(action1.hash, hash1);
}

/// A `computed: true` entry must skip re-verification even when its hash is
/// wrong for the actual content: the hash was just read from disk (--only-dupes
/// with no manifest), so re-reading a huge file again to "verify" it would be
/// pointless. This documents the intended semantics.
#[test]
fn computed_entry_skips_verify() {
    let dir = TmpDir::new("computed_skip_verify");
    let src = dir.path().join("video.mp4");
    fs::write(&src, b"real content").unwrap();

    let op =
        RenameOperation::new(dir.path().to_path_buf(), None, true, true, false, false).unwrap();
    let bad_hash = "0000000000000000";
    let entries = vec![computed_entry(bad_hash, &src)];
    let report = op.build_plan(&entries).unwrap();

    assert_eq!(report.actions.len(), 1);
    let action = &report.actions[0];
    assert_eq!(
        action.status, "rename",
        "computed entries must skip verification even when the hash is wrong"
    );
    assert_eq!(action.hash, bad_hash);
    assert!(action
        .dst
        .as_ref()
        .unwrap()
        .file_name()
        .unwrap()
        .to_string_lossy()
        .contains(bad_hash));
}

/// A same-named pair whose filename already carries a hash suffix (`_16HEX`)
/// and whose sizes match is a benign true copy. `check` must classify it
/// benign and NOT count it (exit code stays 0).
#[test]
fn check_hash_suffixed_pair_is_benign() {
    let dir = TmpDir::new("check_benign");
    fs::create_dir_all(dir.path().join("a")).unwrap();
    fs::create_dir_all(dir.path().join("b")).unwrap();
    // Same content in both dirs, already suffixed with the (shared) hash.
    fs::write(dir.path().join("a").join("x_00AABBCCDDEEFF11.mp4"), b"same").unwrap();
    fs::write(dir.path().join("b").join("x_00AABBCCDDEEFF11.mp4"), b"same").unwrap();

    let (total, groups) = check::scan_duplicate_groups(dir.path(), false).unwrap();
    assert_eq!(total, 2);
    assert_eq!(groups.len(), 1);
    assert!(groups[0].benign, "hash-suffixed same-name pair must be classified benign");

    let n = check::check(dir.path(), false).unwrap();
    assert_eq!(n, 0, "benign groups must not count toward the real-collision total");
}

/// Mixed tree: one real (un-suffixed) collision plus one benign hash-suffixed
/// pair — only the real one counts.
#[test]
fn check_mixed_real_and_benign_groups() {
    let dir = TmpDir::new("check_mixed");
    fs::create_dir_all(dir.path().join("a")).unwrap();
    fs::create_dir_all(dir.path().join("b")).unwrap();
    // Real collision: same name, no hash suffix, different content.
    fs::write(dir.path().join("a").join("clip.mp4"), b"one").unwrap();
    fs::write(dir.path().join("b").join("clip.mp4"), b"two-different").unwrap();
    // Benign pair: name already carries the shared hash suffix.
    fs::write(dir.path().join("a").join("intro_0873784776488DB8.mov"), b"same").unwrap();
    fs::write(dir.path().join("b").join("intro_0873784776488DB8.mov"), b"same").unwrap();

    let (_, groups) = check::scan_duplicate_groups(dir.path(), false).unwrap();
    assert_eq!(groups.len(), 2);
    assert_eq!(groups.iter().filter(|g| g.benign).count(), 1);
    assert_eq!(groups.iter().filter(|g| !g.benign).count(), 1);

    let n = check::check(dir.path(), false).unwrap();
    assert_eq!(n, 1, "only the real (un-suffixed) collision counts");
}

/// `rename --only-dupes` on a tree containing only a benign hash-suffixed pair
/// must produce no entries at all — in particular it must not re-hash the
/// (potentially huge) true-copy files on every re-run.
#[test]
fn only_dupes_skips_benign_groups() {
    let dir = TmpDir::new("only_dupes_benign");
    fs::create_dir_all(dir.path().join("a")).unwrap();
    fs::create_dir_all(dir.path().join("b")).unwrap();
    fs::write(dir.path().join("a").join("x_00AABBCCDDEEFF11.mp4"), b"same").unwrap();
    fs::write(dir.path().join("b").join("x_00AABBCCDDEEFF11.mp4"), b"same").unwrap();

    let op =
        RenameOperation::new(dir.path().to_path_buf(), None, false, true, false, true).unwrap();
    let entries = op.load_only_dupes_entries().unwrap();
    assert!(
        entries.is_empty(),
        "benign groups must be skipped entirely (no hashing, no plan entries)"
    );
}

/// A name whose "hash" suffix is really a camera timestamp (16 decimal digits
/// are valid hex!) must NOT be classified benign when the members' sizes
/// differ — that is a real, dangerous collision, not a true copy. Benign
/// requires BOTH the hash-shaped suffix AND equal sizes.
#[test]
fn timestamp_named_collision_not_benign() {
    let dir = TmpDir::new("check_timestamp");
    fs::create_dir_all(dir.path().join("a")).unwrap();
    fs::create_dir_all(dir.path().join("b")).unwrap();
    // 16 decimal digits (YYYYMMDDHHMMSSff) — hex-shaped, but different content
    // AND different sizes: a genuine collision hiding behind a hash-like name.
    fs::write(dir.path().join("a").join("x_2026070212345678.mp4"), b"short").unwrap();
    fs::write(
        dir.path().join("b").join("x_2026070212345678.mp4"),
        b"much longer different content",
    )
    .unwrap();

    let (_, groups) = check::scan_duplicate_groups(dir.path(), false).unwrap();
    assert_eq!(groups.len(), 1);
    assert!(
        !groups[0].benign,
        "hash-shaped name with differing sizes must NOT be benign"
    );

    let n = check::check(dir.path(), false).unwrap();
    assert_eq!(n, 1, "the pair must count as a real collision");

    // And --only-dupes must include (not skip) the pair.
    let op =
        RenameOperation::new(dir.path().to_path_buf(), None, false, true, false, true).unwrap();
    let entries = op.load_only_dupes_entries().unwrap();
    assert_eq!(entries.len(), 2, "differing-size pair must be planned, not skipped");
}

/// Conversely, a hash-suffixed pair with EQUAL sizes stays benign.
#[test]
fn equal_size_hash_suffixed_pair_stays_benign() {
    let dir = TmpDir::new("check_benign_eqsize");
    fs::create_dir_all(dir.path().join("a")).unwrap();
    fs::create_dir_all(dir.path().join("b")).unwrap();
    fs::write(dir.path().join("a").join("y_0873784776488DB8.mov"), b"copy-content").unwrap();
    fs::write(dir.path().join("b").join("y_0873784776488DB8.mov"), b"copy-content").unwrap();

    let (_, groups) = check::scan_duplicate_groups(dir.path(), false).unwrap();
    assert_eq!(groups.len(), 1);
    assert!(groups[0].benign, "equal-size hash-suffixed pair must stay benign");
    assert_eq!(check::check(dir.path(), false).unwrap(), 0);
}

/// `has_hash_suffix` recognizes `_16HEX` stems without knowing the hash, and
/// rejects near-misses.
#[test]
fn has_hash_suffix_detection() {
    use dups::rename::has_hash_suffix;
    assert!(has_hash_suffix(Path::new("x_00AABBCCDDEEFF11.mp4")));
    assert!(has_hash_suffix(Path::new("x_00aabbccddeeff11.mp4"))); // lowercase hex
    assert!(has_hash_suffix(Path::new("dir/intro_0873784776488DB8.mov")));
    assert!(!has_hash_suffix(Path::new("x.mp4"))); // no suffix
    assert!(!has_hash_suffix(Path::new("x_00AABBCCDDEEFF1.mp4"))); // 15 hex
    assert!(!has_hash_suffix(Path::new("x_00AABBCCDDEEFFGG.mp4"))); // non-hex
    assert!(!has_hash_suffix(Path::new("x-00AABBCCDDEEFF11.mp4"))); // no underscore
    assert!(!has_hash_suffix(Path::new("0AABBCCDDEEFF11.mp4"))); // stem too short
}

/// CLI-level: bare `dups <PATH>` is an alias for `dups check <PATH>` — it
/// prints the duplicate-name report and exits 1 when collisions are found, 0
/// on a clean directory.
#[test]
fn cli_bare_path_check_alias() {
    let bin = env!("CARGO_BIN_EXE_dups");

    let dir = TmpDir::new("cli_dupe");
    fs::create_dir_all(dir.path().join("a")).unwrap();
    fs::create_dir_all(dir.path().join("b")).unwrap();
    fs::write(dir.path().join("a").join("dup.mp4"), b"one").unwrap();
    fs::write(dir.path().join("b").join("dup.mp4"), b"two").unwrap();

    let output = std::process::Command::new(bin)
        .arg(dir.path())
        .output()
        .unwrap();
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("重名"),
        "stdout should mention 重名 (duplicate names), got: {stdout}"
    );
    assert_eq!(output.status.code(), Some(1));

    let dir2 = TmpDir::new("cli_clean");
    fs::write(dir2.path().join("a.mp4"), b"aaa").unwrap();
    fs::write(dir2.path().join("b.mp4"), b"bbb").unwrap();

    let output2 = std::process::Command::new(bin)
        .arg(dir2.path())
        .output()
        .unwrap();
    assert_eq!(output2.status.code(), Some(0));
}
