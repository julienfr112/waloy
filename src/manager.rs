use std::path::Path;
use std::time::{Instant, SystemTime, UNIX_EPOCH};

use rusqlite::Connection;

use crate::compression;
use crate::config::{BackupConfig, S3Config};
use crate::error::{Error, Result};
use crate::manifest::GenerationManifest;
use crate::s3::S3Client;
use crate::stats::{BackupStats, StatsTracker};

const WAL_HEADER_SIZE: u64 = 32;
/// Offset of the salt fields in the WAL header (bytes 16..24).
const WAL_SALT_OFFSET: usize = 16;
const WAL_SALT_LEN: usize = 8;

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}

/// Result of a compaction operation.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CompactionResult {
    pub segments_before: u32,
    pub segments_after: u32,
}

/// Manages Litestream-style progressive backups of a SQLite database to S3.
pub struct BackupManager {
    config: BackupConfig,
    s3: S3Client,
    generation: String,
    /// Holds a read transaction to pin the WAL and prevent auto-checkpointing.
    read_conn: Connection,
    /// Byte offset in the WAL file up to which we've already synced.
    wal_offset: u64,
    /// Index of the next WAL segment to upload.
    wal_index: u32,
    /// WAL header salt captured on first read, used to detect discontinuities.
    wal_header_salt: Option<[u8; WAL_SALT_LEN]>,
    /// Manifest for the current generation.
    manifest: GenerationManifest,
    /// Internal stats tracker.
    stats: StatsTracker,
    /// When the last snapshot was taken (for snapshot scheduling).
    last_snapshot_time: Instant,
    /// Whether shutdown() has been called.
    shutdown_complete: bool,
}

impl BackupManager {
    /// Create a new BackupManager. This:
    /// - Optionally auto-restores from S3 if the DB file doesn't exist
    /// - Opens a dedicated connection to the database
    /// - Enables WAL mode and disables auto-checkpointing
    /// - Starts a long-running read transaction (pins the WAL)
    /// - Takes an initial full snapshot and uploads it to S3
    pub async fn new(config: BackupConfig) -> Result<Self> {
        // Auto-restore: if DB doesn't exist and flag is set, restore from S3
        if config.auto_restore && !Path::new(&config.db_path).exists() {
            tracing::info!(db_path = %config.db_path, "DB not found, auto-restoring from S3");
            Self::restore_with_config(&config, &config.db_path).await?;
        }

        let s3 = S3Client::new(&config.s3)?;

        let read_conn = Connection::open(&config.db_path)?;
        read_conn.execute_batch(
            "PRAGMA journal_mode = WAL;
             PRAGMA busy_timeout = 5000;
             PRAGMA wal_autocheckpoint = 0;",
        )?;

        // Start a read transaction — this pins the WAL, preventing checkpoints
        // from truncating frames we haven't synced yet.
        read_conn.execute_batch("BEGIN")?;
        read_conn
            .query_row("SELECT 1 FROM sqlite_master LIMIT 1", [], |_| Ok(()))?;

        let generation = uuid::Uuid::new_v4().to_string();
        let ts = now_ms();
        let manifest = GenerationManifest::new(generation.clone(), ts);

        let mut mgr = Self {
            config,
            s3,
            generation,
            read_conn,
            wal_offset: 0,
            wal_index: 0,
            wal_header_salt: None,
            manifest,
            stats: StatsTracker::new(),
            last_snapshot_time: Instant::now(),
            shutdown_complete: false,
        };

        mgr.snapshot().await?;
        Ok(mgr)
    }

    /// Begin a long-running read transaction to pin the WAL.
    fn begin_read_transaction(&self) -> Result<()> {
        self.read_conn.execute_batch("BEGIN")?;
        self.read_conn
            .query_row("SELECT 1 FROM sqlite_master LIMIT 1", [], |_| Ok(()))?;
        Ok(())
    }

    /// End the read transaction (best-effort, ignores errors).
    fn end_read_transaction(&self) {
        let _ = self.read_conn.execute_batch("COMMIT");
    }

    fn wal_path(&self) -> String {
        format!("{}-wal", self.config.db_path)
    }

    /// Apply the data pipeline before upload: compress, then optionally encrypt.
    fn pipeline_encode(&self, data: &[u8]) -> Result<Vec<u8>> {
        let compressed = compression::compress(data, &self.config.compression)?;
        #[cfg(feature = "encryption")]
        if let Some(ref key) = self.config.encryption_key {
            return crate::encryption::encrypt(&compressed, key);
        }
        Ok(compressed)
    }

    /// Apply the data pipeline after download: optionally decrypt, then decompress.
    fn pipeline_decode(data: &[u8], #[allow(unused)] config: &BackupConfig) -> Result<Vec<u8>> {
        let decrypted;
        #[cfg(feature = "encryption")]
        {
            decrypted = if let Some(ref key) = config.encryption_key {
                crate::encryption::decrypt(data, key)?
            } else {
                data.to_vec()
            };
        }
        #[cfg(not(feature = "encryption"))]
        {
            decrypted = data.to_vec();
        }
        compression::decompress(&decrypted)
    }

    /// Upload a full copy of the database file as a snapshot.
    pub async fn snapshot(&mut self) -> Result<()> {
        let raw_data = std::fs::read(&self.config.db_path)?;
        let data = self.pipeline_encode(&raw_data)?;
        let key = format!("{}/snapshot", self.generation);
        self.s3.put_object(&key, &data).await?;

        // Record this as the latest generation
        self.s3
            .put_object("latest", self.generation.as_bytes())
            .await?;

        // Reset WAL tracking — segments are relative to the snapshot
        self.wal_offset = 0;
        self.wal_index = 0;
        self.wal_header_salt = None;

        self.stats.record_snapshot(data.len() as u64);
        self.last_snapshot_time = Instant::now();

        // Update manifest
        self.manifest.snapshot_timestamp_ms = now_ms();
        self.manifest.segments.clear();
        self.upload_manifest().await?;

        tracing::info!(generation = %self.generation, "snapshot uploaded");
        Ok(())
    }

    /// Sync new WAL frames to S3. Only uploads bytes added since the last sync.
    /// Returns true if new data was uploaded.
    pub async fn sync_wal(&mut self) -> Result<bool> {
        let wal_path = self.wal_path();
        if !Path::new(&wal_path).exists() {
            return Ok(false);
        }

        let wal_data = std::fs::read(&wal_path)?;
        let wal_len = wal_data.len() as u64;

        // Nothing new, or WAL has only header (no frames)
        if wal_len <= WAL_HEADER_SIZE {
            return Ok(false);
        }

        // Check for WAL discontinuity (salt change or shrink)
        if self.wal_needs_recovery(&wal_data, wal_len) {
            tracing::warn!("WAL discontinuity detected, starting recovery");
            self.recover().await?;
            // Re-read WAL after recovery
            return Ok(false);
        }

        // Capture salt on first read
        if self.wal_header_salt.is_none() && wal_data.len() >= WAL_SALT_OFFSET + WAL_SALT_LEN {
            let mut salt = [0u8; WAL_SALT_LEN];
            salt.copy_from_slice(&wal_data[WAL_SALT_OFFSET..WAL_SALT_OFFSET + WAL_SALT_LEN]);
            self.wal_header_salt = Some(salt);
        }

        if wal_len <= self.wal_offset {
            return Ok(false);
        }

        let new_data = &wal_data[self.wal_offset as usize..];
        let encoded = self.pipeline_encode(new_data)?;
        let key = format!("{}/wal/{:08}", self.generation, self.wal_index);
        self.s3.put_object(&key, &encoded).await?;

        let segment_size = new_data.len() as u64;

        tracing::info!(
            generation = %self.generation,
            segment = self.wal_index,
            bytes = segment_size,
            "WAL segment uploaded"
        );

        // Update manifest
        self.manifest
            .add_segment(self.wal_index, now_ms(), self.wal_offset, segment_size);
        self.upload_manifest().await?;

        self.stats.record_sync(encoded.len() as u64);
        self.wal_offset = wal_len;
        self.wal_index += 1;
        Ok(true)
    }

    /// Detect WAL discontinuity: shrink or salt change.
    fn wal_needs_recovery(&self, wal_data: &[u8], wal_len: u64) -> bool {
        check_wal_discontinuity(
            self.wal_offset,
            self.wal_header_salt.as_ref(),
            wal_data,
            wal_len,
        )
    }

    /// Recover from a WAL discontinuity: start a new generation with a fresh snapshot.
    async fn recover(&mut self) -> Result<()> {
        tracing::info!("recovering: ending current transaction and starting new generation");

        // End current read transaction
        self.end_read_transaction();

        // New generation
        self.generation = uuid::Uuid::new_v4().to_string();
        self.manifest = GenerationManifest::new(self.generation.clone(), now_ms());
        self.stats.record_new_generation();

        // Take a fresh snapshot
        self.snapshot().await?;

        // Re-acquire read transaction
        self.begin_read_transaction()?;

        tracing::info!(generation = %self.generation, "recovery complete, new generation");
        Ok(())
    }

    /// Checkpoint the database: flush WAL to the main DB file, then start
    /// a new generation with a fresh snapshot.
    pub async fn checkpoint(&mut self) -> Result<()> {
        // Sync any remaining WAL data before checkpointing
        self.sync_wal().await?;

        // Release the read transaction so checkpoint can proceed.
        // Unlike recover/shutdown, we need this to succeed before checkpointing.
        self.read_conn.execute_batch("COMMIT")?;

        // Checkpoint: write WAL pages back to DB and truncate WAL
        self.read_conn
            .execute_batch("PRAGMA wal_checkpoint(TRUNCATE)")?;

        // New generation
        self.generation = uuid::Uuid::new_v4().to_string();
        self.manifest = GenerationManifest::new(self.generation.clone(), now_ms());
        self.stats.record_new_generation();

        // Take a fresh snapshot (DB is now fully up to date)
        self.snapshot().await?;

        // Re-acquire read transaction to pin the WAL again
        self.begin_read_transaction()?;

        tracing::info!(generation = %self.generation, "checkpoint complete, new generation");
        Ok(())
    }

    /// Check if a scheduled snapshot is due. If so, performs a checkpoint.
    /// Returns true if a snapshot was taken.
    pub async fn maybe_snapshot(&mut self) -> Result<bool> {
        if let Some(interval) = self.config.snapshot_interval
            && self.last_snapshot_time.elapsed() >= interval
        {
            self.checkpoint().await?;
            return Ok(true);
        }
        Ok(false)
    }

    /// Returns the current generation ID.
    pub fn generation(&self) -> &str {
        &self.generation
    }

    /// Returns a snapshot of current backup statistics.
    pub fn stats(&self) -> BackupStats {
        BackupStats {
            generation: self.generation.clone(),
            generation_count: self.stats.generation_count,
            wal_offset: self.wal_offset,
            wal_index: self.wal_index,
            last_sync_time: self.stats.last_sync_time,
            last_snapshot_time: self.stats.last_snapshot_time,
            total_bytes_uploaded: self.stats.total_bytes_uploaded,
            sync_count: self.stats.sync_count,
            error_count: self.stats.error_count,
        }
    }

    /// Graceful shutdown: final WAL sync and release of read transaction.
    /// Must be called before dropping the manager for clean shutdown.
    pub async fn shutdown(&mut self) -> Result<()> {
        if self.shutdown_complete {
            return Ok(());
        }

        tracing::info!("shutting down: performing final WAL sync");

        // Best-effort final sync
        if let Err(e) = self.sync_wal().await {
            tracing::warn!(error = %e, "final WAL sync failed during shutdown");
            self.stats.record_error();
        }

        // Release the read transaction
        self.end_read_transaction();

        self.shutdown_complete = true;
        tracing::info!("shutdown complete");
        Ok(())
    }

    /// Upload the current generation manifest to S3.
    async fn upload_manifest(&mut self) -> Result<()> {
        let json = serde_json::to_vec(&self.manifest)
            .map_err(|e| Error::Other(format!("manifest serialize: {e}")))?;
        let key = format!("{}/manifest.json", self.generation);
        self.s3.put_object(&key, &json).await
    }

    /// Enforce retention policy: delete generations older than retention_duration.
    /// Never deletes the current generation. Returns number of generations deleted.
    pub async fn enforce_retention(&mut self) -> Result<u32> {
        let duration = match self.config.retention_duration {
            Some(d) => d,
            None => return Ok(0),
        };

        let cutoff_ms = now_ms().saturating_sub(duration.as_millis() as u64);
        let manifests = self.list_generation_manifests().await?;
        let mut deleted = 0u32;

        for (gen_id, manifest) in &manifests {
            // Never delete the current generation
            if *gen_id == self.generation {
                continue;
            }
            if manifest.created_at_ms < cutoff_ms {
                self.delete_generation(gen_id).await?;
                deleted += 1;
            }
        }

        if deleted > 0 {
            tracing::info!(deleted, "retention: deleted old generations");
        }

        Ok(deleted)
    }

    /// List all generation manifests from S3.
    async fn list_generation_manifests(&mut self) -> Result<Vec<(String, GenerationManifest)>> {
        let all_keys = self.s3.list_keys("").await?;
        let mut manifests = Vec::new();

        for key in &all_keys {
            if key.ends_with("/manifest.json") {
                let gen_id = key.trim_end_matches("/manifest.json").to_string();
                match self.s3.get_object(key).await {
                    Ok(data) => {
                        if let Ok(m) = serde_json::from_slice::<GenerationManifest>(&data) {
                            manifests.push((gen_id, m));
                        }
                    }
                    Err(_) => continue,
                }
            }
        }

        Ok(manifests)
    }

    /// Delete all S3 objects belonging to a generation.
    async fn delete_generation(&mut self, gen_id: &str) -> Result<()> {
        let prefix = format!("{}/", gen_id);
        let keys = self.s3.list_keys(&prefix).await?;
        self.s3.delete_objects(&keys).await?;
        tracing::info!(
            generation = gen_id,
            objects = keys.len(),
            "generation deleted"
        );
        Ok(())
    }

    /// Compact WAL segments in the current generation: download all, merge, re-upload.
    pub async fn compact(&mut self, max_segment_size: Option<usize>) -> Result<CompactionResult> {
        let max_size = max_segment_size.unwrap_or(4 * 1024 * 1024); // 4MB default

        let wal_prefix = format!("{}/wal/", self.generation);
        let segment_keys = self.s3.list_keys(&wal_prefix).await?;

        if segment_keys.len() <= 1 {
            return Ok(CompactionResult {
                segments_before: segment_keys.len() as u32,
                segments_after: segment_keys.len() as u32,
            });
        }

        let segments_before = segment_keys.len() as u32;

        // Download all segments
        let mut all_data = Vec::new();
        for key in &segment_keys {
            let data = self.s3.get_object(key).await?;
            let decoded = Self::pipeline_decode(&data, &self.config)?;
            all_data.extend_from_slice(&decoded);
        }

        // Delete old segments
        self.s3.delete_objects(&segment_keys).await?;

        // Re-upload as fewer, larger segments
        let mut new_index = 0u32;
        let mut new_segments = Vec::new();
        let mut offset = 0usize;

        while offset < all_data.len() {
            let end = (offset + max_size).min(all_data.len());
            let chunk = &all_data[offset..end];
            let encoded = self.pipeline_encode(chunk)?;
            let key = format!("{}/wal/{:08}", self.generation, new_index);
            self.s3.put_object(&key, &encoded).await?;

            new_segments.push(crate::manifest::SegmentMeta {
                index: new_index,
                timestamp_ms: now_ms(),
                offset: offset as u64,
                size: chunk.len() as u64,
            });

            offset = end;
            new_index += 1;
        }

        let segments_after = new_index;

        // Update manifest with new segments
        self.manifest.segments = new_segments;
        self.upload_manifest().await?;

        // Update internal tracking
        self.wal_index = new_index;

        tracing::info!(
            before = segments_before,
            after = segments_after,
            "compaction complete"
        );

        Ok(CompactionResult {
            segments_before,
            segments_after,
        })
    }

    /// Restore a database from S3 to the given target path.
    ///
    /// Downloads the latest snapshot, then downloads and concatenates all WAL
    /// segments. Places the WAL file next to the DB so SQLite replays it on open.
    ///
    /// Note: this uses a default decode config (no encryption, no compression override).
    /// If the backup was encrypted, use [`restore_with_config`](Self::restore_with_config).
    pub async fn restore(s3_config: &S3Config, target_path: &str) -> Result<()> {
        let config = BackupConfig {
            s3: s3_config.clone(),
            ..Default::default()
        };
        Self::restore_inner(&config, target_path, None).await
    }

    /// Restore a database to a specific point in time (milliseconds since epoch).
    ///
    /// Note: this uses a default decode config (no encryption, no compression override).
    /// If the backup was encrypted, use [`restore_to_time_with_config`](Self::restore_to_time_with_config).
    pub async fn restore_to_time(
        s3_config: &S3Config,
        target_path: &str,
        timestamp_ms: u64,
    ) -> Result<()> {
        let config = BackupConfig {
            s3: s3_config.clone(),
            ..Default::default()
        };
        Self::restore_inner(&config, target_path, Some(timestamp_ms)).await
    }

    /// Restore a database from S3 using the full backup config (including encryption key).
    pub async fn restore_with_config(config: &BackupConfig, target_path: &str) -> Result<()> {
        Self::restore_inner(config, target_path, None).await
    }

    /// Restore a database to a specific point in time using the full backup config.
    pub async fn restore_to_time_with_config(
        config: &BackupConfig,
        target_path: &str,
        timestamp_ms: u64,
    ) -> Result<()> {
        Self::restore_inner(config, target_path, Some(timestamp_ms)).await
    }

    async fn restore_inner(
        config: &BackupConfig,
        target_path: &str,
        target_time: Option<u64>,
    ) -> Result<()> {
        let s3 = S3Client::new(&config.s3)?;

        if let Some(timestamp_ms) = target_time {
            // Point-in-time restore: find the right generation via manifests
            Self::restore_pitr(&s3, config, target_path, timestamp_ms).await
        } else {
            // Standard restore: latest generation
            Self::restore_latest(&s3, config, target_path).await
        }
    }

    async fn restore_latest(
        s3: &S3Client,
        decode_config: &BackupConfig,
        target_path: &str,
    ) -> Result<()> {
        // Find the latest generation
        let gen_bytes = s3
            .get_object("latest")
            .await
            .map_err(|_| Error::Other("no backup found: 'latest' marker missing".into()))?;
        let generation = String::from_utf8(gen_bytes)
            .map_err(|e| Error::Other(format!("invalid generation id: {e}")))?;

        tracing::info!(generation = %generation, "restoring from generation");

        // Download the snapshot
        let snapshot_key = format!("{}/snapshot", generation);
        let snapshot_data = s3.get_object(&snapshot_key).await?;
        let snapshot_decoded = Self::pipeline_decode(&snapshot_data, decode_config)?;
        std::fs::write(target_path, &snapshot_decoded)?;

        // Download WAL segments
        let wal_prefix = format!("{}/wal/", generation);
        let segment_keys = s3.list_keys(&wal_prefix).await?;

        if !segment_keys.is_empty() {
            let mut wal_data = Vec::new();
            for key in &segment_keys {
                let segment = s3.get_object(key).await?;
                let decoded = Self::pipeline_decode(&segment, decode_config)?;
                wal_data.extend_from_slice(&decoded);
            }
            let wal_path = format!("{}-wal", target_path);
            std::fs::write(&wal_path, &wal_data)?;

            tracing::info!(segments = segment_keys.len(), "WAL segments downloaded");
        }

        // Open and close the DB to trigger WAL replay, then clean up
        let conn = Connection::open(target_path)?;
        conn.execute_batch("PRAGMA journal_mode = WAL")?;
        conn.execute_batch("PRAGMA wal_checkpoint(TRUNCATE)")?;
        drop(conn);

        tracing::info!(target = target_path, "restore complete");
        Ok(())
    }

    async fn restore_pitr(
        s3: &S3Client,
        decode_config: &BackupConfig,
        target_path: &str,
        timestamp_ms: u64,
    ) -> Result<()> {
        // List all manifests to find the right generation
        let all_keys = s3.list_keys("").await?;
        let mut manifests: Vec<GenerationManifest> = Vec::new();

        for key in &all_keys {
            if key.ends_with("/manifest.json")
                && let Ok(data) = s3.get_object(key).await
                && let Ok(m) = serde_json::from_slice::<GenerationManifest>(&data)
            {
                manifests.push(m);
            }
        }

        if manifests.is_empty() {
            return Err(Error::Other(
                "no manifests found for point-in-time restore".into(),
            ));
        }

        // Sort by snapshot timestamp descending
        manifests.sort_by(|a, b| b.snapshot_timestamp_ms.cmp(&a.snapshot_timestamp_ms));

        // Find the latest generation whose snapshot is <= target time
        let manifest = manifests
            .iter()
            .find(|m| m.snapshot_timestamp_ms <= timestamp_ms)
            .ok_or_else(|| {
                Error::Other(format!(
                    "no generation found with snapshot before timestamp {timestamp_ms}"
                ))
            })?;

        tracing::info!(
            generation = %manifest.generation,
            snapshot_ts = manifest.snapshot_timestamp_ms,
            target_ts = timestamp_ms,
            "restoring to point in time"
        );

        // Download snapshot
        let snapshot_key = format!("{}/snapshot", manifest.generation);
        let snapshot_data = s3.get_object(&snapshot_key).await?;
        let snapshot_decoded = Self::pipeline_decode(&snapshot_data, decode_config)?;
        std::fs::write(target_path, &snapshot_decoded)?;

        // Download WAL segments up to target timestamp
        let segments_to_replay: Vec<&crate::manifest::SegmentMeta> = manifest
            .segments
            .iter()
            .filter(|s| s.timestamp_ms <= timestamp_ms)
            .collect();

        if !segments_to_replay.is_empty() {
            let mut wal_data = Vec::new();
            for seg in &segments_to_replay {
                let key = format!("{}/wal/{:08}", manifest.generation, seg.index);
                let data = s3.get_object(&key).await?;
                let decoded = Self::pipeline_decode(&data, decode_config)?;
                wal_data.extend_from_slice(&decoded);
            }
            let wal_path = format!("{}-wal", target_path);
            std::fs::write(&wal_path, &wal_data)?;

            tracing::info!(
                segments = segments_to_replay.len(),
                "WAL segments replayed for PITR"
            );
        }

        let conn = Connection::open(target_path)?;
        conn.execute_batch("PRAGMA journal_mode = WAL")?;
        conn.execute_batch("PRAGMA wal_checkpoint(TRUNCATE)")?;
        drop(conn);

        tracing::info!(target = target_path, "point-in-time restore complete");
        Ok(())
    }
}

/// Check if the WAL shows a discontinuity (shrink or salt change) that requires recovery.
fn check_wal_discontinuity(
    wal_offset: u64,
    wal_header_salt: Option<&[u8; WAL_SALT_LEN]>,
    wal_data: &[u8],
    wal_len: u64,
) -> bool {
    // WAL shrank below our offset — it was reset externally
    if wal_offset > 0 && wal_len < wal_offset {
        return true;
    }

    // Salt changed — WAL was restarted
    if let Some(old_salt) = wal_header_salt
        && wal_data.len() >= WAL_SALT_OFFSET + WAL_SALT_LEN
    {
        let current_salt = &wal_data[WAL_SALT_OFFSET..WAL_SALT_OFFSET + WAL_SALT_LEN];
        if current_salt != old_salt {
            return true;
        }
    }

    false
}

impl Drop for BackupManager {
    fn drop(&mut self) {
        if !self.shutdown_complete {
            tracing::warn!(
                "BackupManager dropped without calling shutdown() — \
                 releasing read transaction but final WAL sync was skipped"
            );
        }
        // Always release the read transaction
        self.end_read_transaction();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // --- check_wal_discontinuity tests ---

    #[test]
    fn no_previous_offset_no_salt_returns_false() {
        let wal_data = vec![0u8; 64];
        assert!(!check_wal_discontinuity(0, None, &wal_data, 64));
    }

    #[test]
    fn wal_shrank_below_offset_returns_true() {
        let wal_data = vec![0u8; 32];
        // offset was 100, WAL is now only 32 bytes
        assert!(check_wal_discontinuity(100, None, &wal_data, 32));
    }

    #[test]
    fn wal_salt_changed_returns_true() {
        let old_salt: [u8; WAL_SALT_LEN] = [1, 2, 3, 4, 5, 6, 7, 8];
        // Build WAL data with different salt at offset 16..24
        let mut wal_data = vec![0u8; 64];
        wal_data[WAL_SALT_OFFSET..WAL_SALT_OFFSET + WAL_SALT_LEN]
            .copy_from_slice(&[9, 9, 9, 9, 9, 9, 9, 9]);
        assert!(check_wal_discontinuity(
            0,
            Some(&old_salt),
            &wal_data,
            64
        ));
    }

    #[test]
    fn wal_salt_unchanged_returns_false() {
        let salt: [u8; WAL_SALT_LEN] = [1, 2, 3, 4, 5, 6, 7, 8];
        let mut wal_data = vec![0u8; 64];
        wal_data[WAL_SALT_OFFSET..WAL_SALT_OFFSET + WAL_SALT_LEN].copy_from_slice(&salt);
        assert!(!check_wal_discontinuity(0, Some(&salt), &wal_data, 64));
    }

    #[test]
    fn wal_data_too_short_for_salt_check_returns_false() {
        let old_salt: [u8; WAL_SALT_LEN] = [1, 2, 3, 4, 5, 6, 7, 8];
        // WAL data is only 20 bytes — not enough for salt at offset 16..24
        let wal_data = vec![0u8; 20];
        assert!(!check_wal_discontinuity(0, Some(&old_salt), &wal_data, 20));
    }

    // --- now_ms tests ---

    #[test]
    fn now_ms_returns_reasonable_value() {
        let ts = now_ms();
        // Should be after 2024-01-01 (1704067200000 ms)
        assert!(ts > 1_704_067_200_000);
        // Should be before 2100-01-01 (4102444800000 ms)
        assert!(ts < 4_102_444_800_000);
    }

    // --- CompactionResult tests ---

    #[test]
    fn compaction_result_fields() {
        let r = CompactionResult {
            segments_before: 10,
            segments_after: 3,
        };
        assert_eq!(r.segments_before, 10);
        assert_eq!(r.segments_after, 3);

        // Clone works
        let r2 = r.clone();
        assert_eq!(r2.segments_before, r.segments_before);

        // Debug works
        let dbg = format!("{:?}", r);
        assert!(dbg.contains("CompactionResult"));
    }
}
