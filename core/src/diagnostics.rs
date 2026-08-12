//! Shared, transport-agnostic diagnostics for the single unified `localdb.db`
//! file — on-disk size (main file + WAL sidecar) and the derived
//! bytes-per-chunk figure.
//!
//! Lives in `core` (issue #187 stage 5) rather than `cli` because both the
//! embedded CLI path and the daemon's `GET /v1/status` handler need the
//! *identical* computation to report identical numbers: before this move,
//! `cli/src/cmds/status.rs` owned this logic alone and the daemon had no way
//! to answer it at all, which is exactly the kind of hand-rolled-per-surface
//! divergence this stage exists to eliminate. `core` has no I/O frameworks,
//! but plain `std::fs` stat calls (as already used by `core::source` and
//! `core::config::loader`) are within that bound.

use std::path::{Path, PathBuf};

/// On-disk size of the single unified `localdb.db` file shared by every
/// store, plus its `-wal` sidecar.
///
/// specs/03-config.md: there is exactly one physical file for the whole
/// database — file size is never a per-store figure.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct DbFileSize {
    /// Bytes in `localdb.db` itself. `None` if the file doesn't exist yet,
    /// or a stat error of any kind — callers degrade to "unknown" rather
    /// than failing.
    pub main_bytes: Option<u64>,
    /// Bytes in the `-wal` sidecar, if one exists.
    ///
    /// Deliberately included in `total_bytes`, not just `main_bytes`: WAL
    /// mode defers committed pages there until the next checkpoint, so on a
    /// store with recent writes a large share of genuine on-disk usage can
    /// live in the WAL rather than the main file.
    pub wal_bytes: Option<u64>,
}

impl DbFileSize {
    /// `main_bytes + wal_bytes`, treating a missing file/sidecar as 0 bytes.
    pub fn total_bytes(&self) -> u64 {
        self.main_bytes.unwrap_or(0) + self.wal_bytes.unwrap_or(0)
    }
}

/// Stat `db_path` and its `-wal` sidecar.
///
/// Never fails: any stat error (missing file, permissions, ...) degrades the
/// corresponding field to `None` rather than propagating.
pub fn compute_db_file_size(db_path: &Path) -> DbFileSize {
    let main_bytes = std::fs::metadata(db_path).ok().map(|m| m.len());

    let mut wal_name = db_path.as_os_str().to_owned();
    wal_name.push("-wal");
    let wal_path = PathBuf::from(wal_name);
    let wal_bytes = std::fs::metadata(&wal_path).ok().map(|m| m.len());

    DbFileSize {
        main_bytes,
        wal_bytes,
    }
}

/// `total on-disk bytes / total chunks` — the single number that makes an
/// over-sized index obvious at a glance (issues #179, #177). `None` when
/// there are no chunks to divide by (avoids a division by zero and a
/// meaningless "0 bytes/chunk").
pub fn bytes_per_chunk(total_bytes: u64, total_chunks: u64) -> Option<u64> {
    total_bytes.checked_div(total_chunks)
}

/// Human-readable byte size, e.g. `128.4 MB`. Binary (1024) units, matching
/// the `du -h` convention.
pub fn format_bytes(bytes: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KB", "MB", "GB", "TB"];
    let mut value = bytes as f64;
    let mut unit = 0usize;
    while value >= 1024.0 && unit < UNITS.len() - 1 {
        value /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{bytes} {}", UNITS[unit])
    } else {
        format!("{value:.1} {}", UNITS[unit])
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn compute_db_file_size_on_missing_file_is_all_none() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("does-not-exist.db");
        let size = compute_db_file_size(&path);
        assert_eq!(size.main_bytes, None);
        assert_eq!(size.wal_bytes, None);
        assert_eq!(size.total_bytes(), 0);
    }

    #[test]
    fn compute_db_file_size_reports_main_file_len() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("localdb.db");
        std::fs::write(&path, vec![0u8; 1234]).unwrap();
        let size = compute_db_file_size(&path);
        assert_eq!(size.main_bytes, Some(1234));
        assert_eq!(size.wal_bytes, None);
        assert_eq!(size.total_bytes(), 1234);
    }

    #[test]
    fn compute_db_file_size_includes_wal_sidecar_in_total() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("localdb.db");
        std::fs::write(&path, vec![0u8; 1000]).unwrap();
        let wal_path = dir.path().join("localdb.db-wal");
        std::fs::write(&wal_path, vec![0u8; 500]).unwrap();

        let size = compute_db_file_size(&path);
        assert_eq!(size.main_bytes, Some(1000));
        assert_eq!(size.wal_bytes, Some(500));
        assert_eq!(size.total_bytes(), 1500);
    }

    #[test]
    fn format_bytes_covers_all_magnitudes() {
        assert_eq!(format_bytes(0), "0 B");
        assert_eq!(format_bytes(512), "512 B");
        assert_eq!(format_bytes(1536), "1.5 KB");
        assert_eq!(format_bytes(1024 * 1024), "1.0 MB");
        assert_eq!(format_bytes(45 * 1024 * 1024 * 1024), "45.0 GB");
    }

    #[test]
    fn bytes_per_chunk_none_when_no_chunks() {
        assert_eq!(bytes_per_chunk(1_000_000, 0), None);
    }

    #[test]
    fn bytes_per_chunk_divides_total_by_count() {
        assert_eq!(bytes_per_chunk(1_000, 10), Some(100));
    }
}
