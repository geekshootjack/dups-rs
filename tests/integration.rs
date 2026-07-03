//! Integration tests for dups. Uses only std for temp dirs (unique names under
//! std::env::temp_dir()) so no dev-dependency is required.

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

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

    let op = RenameOperation::new(dir.path().to_path_buf(), None, false, true, false).unwrap();

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

    let op = RenameOperation::new(dir.path().to_path_buf(), None, false, true, false).unwrap();
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

    let op = RenameOperation::new(dir.path().to_path_buf(), None, false, true, false).unwrap();
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
