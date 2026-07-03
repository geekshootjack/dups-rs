use anyhow::{anyhow, Result};
use std::fs::OpenOptions;
use std::io::{BufWriter, Write};
use std::path::Path;

pub struct Journal {
    writer: BufWriter<std::fs::File>,
}

impl Journal {
    pub fn new(path: &Path) -> Result<Self> {
        let file = OpenOptions::new()
            .create(true)
            .write(true)
            .truncate(true)
            .open(path)?;

        let mut writer = BufWriter::new(file);

        // Write UTF-8 BOM for Windows Excel compatibility
        writer.write_all(&[0xEF, 0xBB, 0xBF])?;

        // Write header
        writeln!(
            writer,
            "timestamp,status,hash,old_path,new_path,note"
        )?;
        writer.flush()?;

        // fsync after header to ensure it's written
        writer.get_ref().sync_all()?;

        Ok(Journal { writer })
    }

pub fn record(
        &mut self,
        status: &str,
        hash: &str,
        old: &str,
        new: &str,
        note: &str,
    ) -> Result<()> {
        let now = chrono::Local::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, false);
        let line = format!(
            "{},{},{},{},{},{}",
            now,
            status,
            escape_csv(hash),
            escape_csv(old),
            escape_csv(new),
            escape_csv(note)
        );
        writeln!(self.writer, "{}", line)?;
        self.writer.flush()?;

        // fsync after each write for write-ahead logging crash-safety
        // This ensures the record is durably persisted before we execute the operation
        self.writer.get_ref().sync_all()?;

        Ok(())
    }
}

/// Shared CSV field escaper. Quotes when the value contains a comma, quote,
/// or a CR/LF (a bare `\r` also breaks CSV rows, so it must trigger quoting).
pub fn escape_csv(s: &str) -> String {
    if s.is_empty() {
        String::new()
    } else if s.contains(',') || s.contains('"') || s.contains('\n') || s.contains('\r') {
        format!("\"{}\"", s.replace('"', "\"\""))
    } else {
        s.to_string()
    }
}

/// Undo a previous rename operation based on log file
pub fn undo(log_path: &Path) -> Result<()> {
    if !log_path.exists() {
        println!("找不到日志: {}", log_path.display());
        return Err(anyhow!("Log not found"));
    }

    // Read all rename pairs from log using proper CSV parser
    let file = std::fs::File::open(log_path)?;
    let mut reader = csv::Reader::from_reader(file);

    // We revert two kinds of rows:
    //   1. status == "renamed": a rename that completed and was journaled.
    //   2. status == "pending" with no terminal ("renamed"/"error") row for the
    //      same (old,new) pair: a crash may have occurred after the rename but
    //      before the terminal record was written. We rely on the per-pair disk
    //      checks below to decide whether the rename actually happened.
    let mut renamed_pairs: Vec<(String, String)> = Vec::new();
    let mut pending_pairs: Vec<(String, String)> = Vec::new();
    let mut terminal: std::collections::HashSet<(String, String)> =
        std::collections::HashSet::new();

    for record in reader.records() {
        let record = record?;
        if record.len() < 6 {
            continue;
        }

        // Format: timestamp, status, hash, old_path, new_path, note
        let status = record.get(1).unwrap_or("");
        let old = record.get(3).unwrap_or("").to_string();
        let new = record.get(4).unwrap_or("").to_string();
        if old.is_empty() || new.is_empty() {
            continue;
        }

        match status {
            "renamed" => {
                terminal.insert((old.clone(), new.clone()));
                renamed_pairs.push((old, new));
            }
            "error" => {
                // A terminal row: the rename failed, so there is nothing to
                // revert, and it should suppress any earlier pending row.
                terminal.insert((old, new));
            }
            "pending" => {
                pending_pairs.push((old, new));
            }
            _ => {}
        }
    }

    // Build the ordered, de-duplicated revert list: all renamed pairs first,
    // then pending pairs that have no terminal row (disk-state-aware recovery).
    let mut pairs: Vec<(String, String)> = Vec::new();
    let mut seen_set: std::collections::HashSet<(String, String)> = std::collections::HashSet::new();
    for (old, new) in renamed_pairs {
        if seen_set.insert((old.clone(), new.clone())) {
            pairs.push((old, new));
        }
    }
    for (old, new) in pending_pairs {
        if terminal.contains(&(old.clone(), new.clone())) {
            continue;
        }
        if seen_set.insert((old.clone(), new.clone())) {
            pairs.push((old, new));
        }
    }

    if pairs.is_empty() {
        println!("该日志里没有改名记录, 无需回退。");
        return Ok(());
    }

    println!("将回退 {} 条改名 (new -> old)", pairs.len());

    let now = chrono::Local::now().format("%Y%m%d-%H%M%S");
    let undo_log = log_path.with_file_name(format!(
        "undo-{}.{}",
        now,
        log_path
            .extension()
            .and_then(|e| e.to_str())
            .unwrap_or("csv")
    ));

    let mut journal = Journal::new(&undo_log)?;
    let mut ok = 0;
    let mut skip = 0;
    let mut fail = 0;

    for (old, new) in pairs {
        let new_path = Path::new(&new);
        let old_path = Path::new(&old);

        if !new_path.exists() {
            skip += 1;
            journal.record("skip", "", &old, &new, "new name not present")?;
            continue;
        }

        if old_path.exists() {
            fail += 1;
            journal.record("refuse", "", &old, &new, "original name occupied")?;
            println!("  [SKIP] 拒绝(原名已被占用): {}", old);
            continue;
        }

        match std::fs::rename(new_path, old_path) {
            Ok(_) => {
                ok += 1;
                journal.record("reverted", "", &old, &new, "")?;
            }
            Err(e) => {
                fail += 1;
                journal.record("error", "", &old, &new, &e.to_string())?;
                println!("  [ERROR] {}: {}", new, e);
            }
        }
    }

    println!(
        "回退完成: 成功 {}, 跳过 {}, 失败 {}",
        ok, skip, fail
    );
    Ok(())
}
