use std::time::Instant;

/// Public snapshot of backup statistics.
#[derive(Clone, Debug)]
pub struct BackupStats {
    pub generation: String,
    pub generation_count: u64,
    pub wal_offset: u64,
    pub wal_index: u32,
    pub last_sync_time: Option<Instant>,
    pub last_snapshot_time: Option<Instant>,
    pub total_bytes_uploaded: u64,
    pub sync_count: u64,
    pub error_count: u64,
}

/// Internal mutable tracker updated by BackupManager operations.
pub(crate) struct StatsTracker {
    pub generation_count: u64,
    pub last_sync_time: Option<Instant>,
    pub last_snapshot_time: Option<Instant>,
    pub total_bytes_uploaded: u64,
    pub sync_count: u64,
    pub error_count: u64,
}

impl StatsTracker {
    pub fn new() -> Self {
        Self {
            generation_count: 1,
            last_sync_time: None,
            last_snapshot_time: None,
            total_bytes_uploaded: 0,
            sync_count: 0,
            error_count: 0,
        }
    }

    pub fn record_snapshot(&mut self, bytes: u64) {
        self.last_snapshot_time = Some(Instant::now());
        self.total_bytes_uploaded += bytes;
    }

    pub fn record_sync(&mut self, bytes: u64) {
        self.last_sync_time = Some(Instant::now());
        self.total_bytes_uploaded += bytes;
        self.sync_count += 1;
    }

    pub fn record_new_generation(&mut self) {
        self.generation_count += 1;
    }

    pub fn record_error(&mut self) {
        self.error_count += 1;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn new_defaults() {
        let t = StatsTracker::new();
        assert_eq!(t.generation_count, 1);
        assert!(t.last_sync_time.is_none());
        assert!(t.last_snapshot_time.is_none());
        assert_eq!(t.total_bytes_uploaded, 0);
        assert_eq!(t.sync_count, 0);
        assert_eq!(t.error_count, 0);
    }

    #[test]
    fn record_snapshot_updates_stats() {
        let mut t = StatsTracker::new();
        t.record_snapshot(1024);
        assert!(t.last_snapshot_time.is_some());
        assert_eq!(t.total_bytes_uploaded, 1024);

        t.record_snapshot(512);
        assert_eq!(t.total_bytes_uploaded, 1536);
    }

    #[test]
    fn record_sync_updates_stats() {
        let mut t = StatsTracker::new();
        t.record_sync(256);
        assert!(t.last_sync_time.is_some());
        assert_eq!(t.total_bytes_uploaded, 256);
        assert_eq!(t.sync_count, 1);

        t.record_sync(128);
        assert_eq!(t.total_bytes_uploaded, 384);
        assert_eq!(t.sync_count, 2);
    }

    #[test]
    fn record_new_generation_increments() {
        let mut t = StatsTracker::new();
        assert_eq!(t.generation_count, 1);
        t.record_new_generation();
        assert_eq!(t.generation_count, 2);
        t.record_new_generation();
        assert_eq!(t.generation_count, 3);
    }

    #[test]
    fn record_error_increments() {
        let mut t = StatsTracker::new();
        assert_eq!(t.error_count, 0);
        t.record_error();
        assert_eq!(t.error_count, 1);
        t.record_error();
        assert_eq!(t.error_count, 2);
    }

    #[test]
    fn record_zero_byte_snapshot() {
        let mut t = StatsTracker::new();
        t.record_snapshot(0);
        assert!(t.last_snapshot_time.is_some());
        assert_eq!(t.total_bytes_uploaded, 0);
    }
}
