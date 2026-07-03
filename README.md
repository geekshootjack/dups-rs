# dups

Append xxHash3-64 checksums to media filenames so that two files can never collide by name. Same name implies same content by construction.

In data management, backups, and post-production workflows, **filename collisions** are common: multiple files with different content share the same filename. This leads to two critical problems:
- Assets overwrite each other, causing data loss
- Post-production software links to the wrong asset

Our solution: append a 16-character xxHash3-64 hash value to the filename. Since this hash is derived directly from file content, it guarantees:

- **Different content** → Different hash → Different filename → Links never point to the wrong asset
- **Same content** → Same hash → Same filename → Genuine duplicates, linking to either is fine

## Features

- **Cross-platform** — Windows, macOS (Intel & ARM), Linux
- **Safe defaults** — Dry-run preview before any changes
- **Crash-safe** — Write-ahead logging, recoverable with `undo`
- **Idempotent** — Re-run safely; already-suffixed files are skipped
- **Flexible** — Use existing `.xxh3` manifests or verify hashes on the fly
- **All file types** — Videos by default, all types with `--all-files`

## Installation

### Download pre-built binaries

Get the latest releases from [GitHub Releases](https://github.com/geekshootjack/dups-rs/releases)

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

### Commands

```bash
dups <PATH>                     # alias for `dups check <PATH>`
dups check <PATH> [--all-files]
dups rename <PATH> [--only-dupes] [--apply] [--hashfile <P>] [--verify] [--all-files]
dups generate <PATH> [-o|--output <P>] [--all-files]
dups undo <LOG>
```

### Basic workflow

```bash
# 1. Quick read-only scan for duplicate filenames (no files touched)
dups /path/to/media

# 2. Preview what would be renamed
dups rename /path/to/media

# 3. If it looks good, execute
dups rename /path/to/media --apply

# 4. If you need to undo
dups undo dups-applied-XXXXXX.csv
```

### `check` — find duplicate filenames without touching anything

`check` walks a directory read-only and reports filenames that occur more than once (in different subdirectories), together with each file's size, so you can tell at a glance whether they're likely the same content (equal size) or definitely different (different size). It never writes a file and exits non-zero when it finds collisions:

```bash
dups check /path/to/media
# or equivalently:
dups /path/to/media
```

### Options

- `check <PATH>` — Read-only duplicate-filename scan; exits 1 if any are found, 0 otherwise
  - `--all-files` — Include all file types, not just videos
- `rename <PATH>` — Plan/execute renames
  - `--only-dupes` — Only rename files that are members of a duplicate-filename group (hashes are read from a manifest when available, otherwise computed on the fly)
  - `--apply` — Actually rename files (default is dry-run preview)
  - `--hashfile <PATH>` — Specify manifest path explicitly
  - `--verify` — Verify hashes match before renaming (slow)
  - `--all-files` — Rename all file types, not just videos
- `generate <PATH>` — Scan and write a `.xxh3` manifest
  - `-o, --output <PATH>` — Output manifest path
  - `--all-files` — Include all file types, not just videos
- `undo <LOG>` — Revert a previous rename operation

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
$ dups /media/videos

检查目录: /media/videos (仅视频文件)
共扫描 128 个文件。

未发现重名文件。共 128 个文件, 文件名全部唯一。
```

Planning renames from a manifest:

```
$ dups rename /media/videos

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
$ dups rename /media/videos --apply

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
     dups undo dups-applied-20260630-153526.csv
  3. 详细日志见: dups-applied-20260630-153526.csv
```

## License

See LICENSE file.
