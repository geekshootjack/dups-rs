# dups — Collision-proof media filenames via xxHash3

Append xxHash3-64 checksums to media filenames so that two files can never collide by name. Same name implies same content by construction.

## Features

- **Cross-platform** — Windows, macOS (Intel & ARM), Linux
- **Safe defaults** — Dry-run preview before any changes
- **Crash-safe** — Write-ahead logging, recoverable with `undo`
- **Idempotent** — Re-run safely; already-suffixed files are skipped
- **Flexible** — Use existing `.xxh3` manifests or verify hashes on the fly
- **All file types** — Videos by default, all types with `--all-files`

## Installation

### Download pre-built binaries

Get the latest releases from [GitHub Releases](https://github.com/geekshootjack/dups/releases)

- `dups-windows-x86_64.exe` — Windows
- `dups-macos-x86_64` — macOS Intel
- `dups-macos-aarch64` — macOS Apple Silicon
- `dups-linux-x86_64` — Linux

### Build from source

```bash
cd dups-rs
cargo build --release
./target/release/dups --help
```

## Usage

### Basic workflow

```bash
# 1. Preview what would be renamed
cargo run -q -- /path/to/media

# 2. If it looks good, execute
cargo run -q -- /path/to/media --apply

# 3. If you need to undo
cargo run -q -- undo dups-applied-XXXXXX.csv
```

### Options

- `<PATH>` — Directory containing media files and .xxh3 manifest
- `--apply` — Actually rename files (default is dry-run preview)
- `--hashfile <PATH>` — Specify manifest path explicitly
- `--verify` — Verify hashes match before renaming (slow)
- `--all-files` — Rename all file types, not just videos
- `--undo <LOG>` — Revert previous rename operation

## How it works

1. Scans for TeraCopy-compatible `.xxh3` hashfiles in the directory
2. Reads the xxHash3-64 checksums for each file
3. Plans renames: `originalname.ext` → `originalname_CHECKSUM.ext`
4. Detects conflicts, idempotent operations, missing files
5. In dry-run: shows preview; in --apply: executes with crash-safe logging
6. Undo via CSV log: reads actual renames, reverts them safely

## Safety Features

- **Dry-run by default** — Nothing changes unless you add `--apply`
- **Idempotent** — Already-suffixed files are skipped; safe to re-run
- **Conflict detection** — Refuses to overwrite existing target names
- **Crash-safe journal** — Each rename intent is fsync'd before execution
- **Undo tracking** — Only reverts files actually renamed (disk-state aware)

## Example

```
$ cargo run -q -- /media/videos

======================================================================
摘要 / Summary
----------------------------------------------------------------------
  将重命名                    12
  已就绪(跳过)                 3
  清单有、磁盘无               2
======================================================================

日志已写出: dups-dryrun-20260630-153526.csv

*** 预演模式 (DRY-RUN) *** 未改动任何文件。
计划改名 12 个。确认无误后加 --apply 执行。
```

Then with `--apply`:

```
$ cargo run -q -- /media/videos --apply

开始执行 12 个重命名...
  [OK] video1.mp4
  [OK] video2.mp4
  ...

重命名完成: 成功 12, 失败 0

======================================================================
执行完成
----------------------------------------------------------------------
[OK] 成功改名: 12 个
======================================================================

Next steps:
  1. 检查结果是否满意
  2. 如需撤销所有改名，运行:
     cargo run -q -- undo dups-applied-20260630-153526.csv
  3. 详细日志见: dups-applied-20260630-153526.csv
```

## License

See LICENSE file.
