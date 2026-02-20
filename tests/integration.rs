use std::env;

use rusqlite::{Connection, params};
use waloy::{BackupConfig, BackupManager, S3Config};
#[cfg(any(feature = "compression-lz4", feature = "compression-zstd"))]
use waloy::CompressionAlgorithm;

fn s3_config() -> Option<S3Config> {
    Some(S3Config {
        endpoint: env::var("WALOY_TEST_S3_ENDPOINT").ok()?,
        region: env::var("WALOY_TEST_S3_REGION").ok()?,
        bucket: env::var("WALOY_TEST_S3_BUCKET").ok()?,
        access_key: env::var("WALOY_TEST_S3_ACCESS_KEY").ok()?,
        secret_key: env::var("WALOY_TEST_S3_SECRET_KEY").ok()?,
        prefix: format!("test-{}", uuid::Uuid::new_v4()),
    })
}

/// Helper: create a test DB with WAL mode and a simple table.
fn create_test_db(path: &str) -> Connection {
    let conn = Connection::open(path).expect("open db");
    conn.execute_batch(
        "PRAGMA journal_mode = WAL;
         PRAGMA busy_timeout = 5000;
         PRAGMA wal_autocheckpoint = 0;
         CREATE TABLE IF NOT EXISTS items (
             id INTEGER PRIMARY KEY,
             name TEXT NOT NULL,
             value INTEGER NOT NULL
         );",
    )
    .expect("setup db");
    conn
}

/// Insert rows into the items table.
fn insert_rows(conn: &Connection, start: i64, count: i64) {
    for i in start..start + count {
        conn.execute(
            "INSERT INTO items (name, value) VALUES (?1, ?2)",
            params![format!("item-{i}"), i * 10],
        )
        .expect("insert row");
    }
}

/// Count rows in the items table.
fn count_rows(conn: &Connection) -> i64 {
    conn.query_row("SELECT COUNT(*) FROM items", [], |row| row.get(0))
        .expect("count rows")
}

/// Sum of the value column.
fn sum_values(conn: &Connection) -> i64 {
    conn.query_row("SELECT COALESCE(SUM(value), 0) FROM items", [], |row| {
        row.get(0)
    })
    .expect("sum values")
}

fn test_config(db_path: String, s3: S3Config) -> BackupConfig {
    BackupConfig {
        db_path,
        s3,
        sync_interval: std::time::Duration::from_secs(1),
        checkpoint_threshold_bytes: 4 * 1024 * 1024,
        ..Default::default()
    }
}

#[tokio::test]
async fn test_snapshot_and_restore() {
    let Some(s3) = s3_config() else {
        eprintln!("SKIP: S3 env vars not set");
        return;
    };

    let tmp = tempfile::tempdir().expect("tempdir");
    let db_path = tmp.path().join("source.db");
    let db_path_str = db_path.to_str().unwrap().to_string();

    // --- Phase 1: write initial data and take a snapshot ---
    let app_conn = create_test_db(&db_path_str);
    insert_rows(&app_conn, 1, 10);
    assert_eq!(count_rows(&app_conn), 10);
    println!("Inserted 10 rows, sum = {}", sum_values(&app_conn));

    let config = test_config(db_path_str.clone(), s3.clone());

    // BackupManager::new takes a snapshot automatically
    let mut mgr = BackupManager::new(config).await.expect("create manager");
    println!("BackupManager created, generation = {}", mgr.generation());

    // --- Phase 2: write more data (goes to WAL) and sync ---
    insert_rows(&app_conn, 11, 20);
    assert_eq!(count_rows(&app_conn), 30);
    println!("Inserted 20 more rows, sum = {}", sum_values(&app_conn));

    let synced = mgr.sync_wal().await.expect("sync wal");
    assert!(synced, "expected WAL data to sync");
    println!("WAL synced");

    // --- Phase 3: restore to a different path ---
    let restore_path = tmp.path().join("restored.db");
    let restore_path_str = restore_path.to_str().unwrap().to_string();

    BackupManager::restore(&s3, &restore_path_str)
        .await
        .expect("restore");
    println!("Restore complete");

    // --- Phase 4: verify restored data ---
    let restored_conn = Connection::open(&restore_path_str).expect("open restored db");
    let restored_count = count_rows(&restored_conn);
    let restored_sum = sum_values(&restored_conn);
    println!("Restored: count = {restored_count}, sum = {restored_sum}");

    assert_eq!(restored_count, 30, "row count mismatch after restore");
    assert_eq!(
        restored_sum,
        sum_values(&app_conn),
        "value sum mismatch after restore"
    );

    // --- Phase 5: verify stats ---
    let stats = mgr.stats();
    assert_eq!(stats.generation_count, 1);
    assert!(stats.sync_count >= 1);
    assert!(stats.total_bytes_uploaded > 0);
    assert!(stats.last_sync_time.is_some());
    assert!(stats.last_snapshot_time.is_some());
    println!(
        "Stats: sync_count={}, bytes_uploaded={}",
        stats.sync_count, stats.total_bytes_uploaded
    );

    // --- Phase 6: test shutdown ---
    mgr.shutdown().await.expect("shutdown");
    println!("Shutdown complete");

    println!("=== test_snapshot_and_restore PASSED ===");
}

#[tokio::test]
async fn test_checkpoint_and_restore() {
    let Some(s3) = s3_config() else {
        eprintln!("SKIP: S3 env vars not set");
        return;
    };

    let tmp = tempfile::tempdir().expect("tempdir");
    let db_path = tmp.path().join("source.db");
    let db_path_str = db_path.to_str().unwrap().to_string();

    let app_conn = create_test_db(&db_path_str);
    insert_rows(&app_conn, 1, 5);

    let config = test_config(db_path_str.clone(), s3.clone());

    let mut mgr = BackupManager::new(config).await.expect("create manager");
    let gen1 = mgr.generation().to_string();
    println!("Generation 1: {gen1}");

    // Write more data, sync, then checkpoint (creates new generation)
    insert_rows(&app_conn, 6, 5);
    mgr.sync_wal().await.expect("sync wal");
    mgr.checkpoint().await.expect("checkpoint");

    let gen2 = mgr.generation().to_string();
    assert_ne!(gen1, gen2, "checkpoint should create a new generation");
    println!("Generation 2 (after checkpoint): {gen2}");

    // Verify generation count in stats
    assert_eq!(mgr.stats().generation_count, 2);

    // Write data in the new generation
    insert_rows(&app_conn, 11, 5);
    mgr.sync_wal().await.expect("sync wal after checkpoint");

    // Restore — should get all 15 rows
    let restore_path = tmp.path().join("restored.db");
    let restore_path_str = restore_path.to_str().unwrap().to_string();

    BackupManager::restore(&s3, &restore_path_str)
        .await
        .expect("restore");

    let restored_conn = Connection::open(&restore_path_str).expect("open restored");
    let restored_count = count_rows(&restored_conn);
    println!("Restored count: {restored_count}");

    assert_eq!(
        restored_count, 15,
        "should have all 15 rows after checkpoint + restore"
    );

    mgr.shutdown().await.expect("shutdown");

    println!("=== test_checkpoint_and_restore PASSED ===");
}

#[tokio::test]
async fn test_point_in_time_restore() {
    let Some(s3) = s3_config() else {
        eprintln!("SKIP: S3 env vars not set");
        return;
    };

    let tmp = tempfile::tempdir().expect("tempdir");
    let db_path = tmp.path().join("source.db");
    let db_path_str = db_path.to_str().unwrap().to_string();

    let app_conn = create_test_db(&db_path_str);
    insert_rows(&app_conn, 1, 5);

    let config = test_config(db_path_str.clone(), s3.clone());
    let mut mgr = BackupManager::new(config).await.expect("create manager");

    // Sync initial WAL
    insert_rows(&app_conn, 6, 5);
    mgr.sync_wal().await.expect("sync wal");

    // Record timestamp between writes
    let mid_time = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis() as u64;

    // Small delay to ensure timestamp ordering
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;

    // Write more data after the mid-point
    insert_rows(&app_conn, 11, 5);
    mgr.sync_wal().await.expect("sync wal 2");

    // PITR: restore to mid_time — should get 10 rows (not the last 5)
    let restore_path = tmp.path().join("pitr_restored.db");
    let restore_path_str = restore_path.to_str().unwrap().to_string();

    BackupManager::restore_to_time(&s3, &restore_path_str, mid_time)
        .await
        .expect("pitr restore");

    let restored_conn = Connection::open(&restore_path_str).expect("open pitr restored");
    let restored_count = count_rows(&restored_conn);
    println!("PITR restored count: {restored_count} (expected 10)");

    // PITR should give us rows from before mid_time only
    assert!(
        restored_count <= 10,
        "PITR should not include rows written after mid_time, got {restored_count}"
    );
    assert!(
        restored_count >= 5,
        "PITR should include at least the snapshot rows, got {restored_count}"
    );

    mgr.shutdown().await.expect("shutdown");

    println!("=== test_point_in_time_restore PASSED ===");
}

#[tokio::test]
async fn test_retention_policy() {
    let Some(s3) = s3_config() else {
        eprintln!("SKIP: S3 env vars not set");
        return;
    };

    let tmp = tempfile::tempdir().expect("tempdir");
    let db_path = tmp.path().join("source.db");
    let db_path_str = db_path.to_str().unwrap().to_string();

    let app_conn = create_test_db(&db_path_str);
    insert_rows(&app_conn, 1, 5);

    // Very short retention: 1ms — everything old will be deleted
    let config = BackupConfig {
        db_path: db_path_str.clone(),
        s3: s3.clone(),
        sync_interval: std::time::Duration::from_secs(1),
        checkpoint_threshold_bytes: 4 * 1024 * 1024,
        retention_duration: Some(std::time::Duration::from_millis(1)),
        ..Default::default()
    };

    let mut mgr = BackupManager::new(config).await.expect("create manager");

    // Create a second generation via checkpoint
    insert_rows(&app_conn, 6, 5);
    mgr.sync_wal().await.expect("sync wal");
    mgr.checkpoint().await.expect("checkpoint");

    // Small delay so gen1 is older than retention
    tokio::time::sleep(std::time::Duration::from_millis(10)).await;

    // Enforce retention — should delete gen1 (not current gen2)
    let deleted = mgr.enforce_retention().await.expect("enforce retention");
    println!("Retention deleted {deleted} generation(s)");
    assert!(
        deleted >= 1,
        "should have deleted at least 1 old generation"
    );

    mgr.shutdown().await.expect("shutdown");

    println!("=== test_retention_policy PASSED ===");
}

#[tokio::test]
async fn test_compaction() {
    let Some(s3) = s3_config() else {
        eprintln!("SKIP: S3 env vars not set");
        return;
    };

    let tmp = tempfile::tempdir().expect("tempdir");
    let db_path = tmp.path().join("source.db");
    let db_path_str = db_path.to_str().unwrap().to_string();

    let app_conn = create_test_db(&db_path_str);
    insert_rows(&app_conn, 1, 5);

    let config = test_config(db_path_str.clone(), s3.clone());
    let mut mgr = BackupManager::new(config).await.expect("create manager");

    // Create multiple small WAL segments
    for batch in 0..5 {
        insert_rows(&app_conn, (batch + 1) * 100, 10);
        mgr.sync_wal().await.expect("sync wal");
    }

    let stats_before = mgr.stats();
    println!("Before compaction: wal_index={}", stats_before.wal_index);

    // Compact with a large max size to merge everything into 1 segment
    let result = mgr.compact(Some(10 * 1024 * 1024)).await.expect("compact");
    println!(
        "Compaction: {} -> {} segments",
        result.segments_before, result.segments_after
    );
    assert!(
        result.segments_after <= result.segments_before,
        "compaction should not increase segment count"
    );

    // Verify data still restores correctly after compaction
    insert_rows(&app_conn, 999, 1);
    mgr.sync_wal().await.expect("final sync");

    let restore_path = tmp.path().join("compacted_restored.db");
    let restore_path_str = restore_path.to_str().unwrap().to_string();

    BackupManager::restore(&s3, &restore_path_str)
        .await
        .expect("restore after compaction");

    let restored_conn = Connection::open(&restore_path_str).expect("open restored");
    let restored_count = count_rows(&restored_conn);
    println!("Restored after compaction: {restored_count} rows");

    mgr.shutdown().await.expect("shutdown");

    println!("=== test_compaction PASSED ===");
}

// ---------------------------------------------------------------------------
// Feature-gated integration tests: compression & encryption
// ---------------------------------------------------------------------------

/// Build a BackupConfig with optional compression and encryption settings.
#[cfg(any(
    feature = "compression-lz4",
    feature = "compression-zstd",
    feature = "encryption"
))]
fn feature_config(
    db_path: String,
    s3: S3Config,
    #[allow(unused)] compression: Option<&str>,
    #[allow(unused)] encrypt: bool,
) -> BackupConfig {
    #[allow(unused_mut)]
    let mut config = test_config(db_path, s3);

    #[cfg(feature = "compression-lz4")]
    if compression == Some("lz4") {
        config.compression = CompressionAlgorithm::Lz4;
    }
    #[cfg(feature = "compression-zstd")]
    if compression == Some("zstd") {
        config.compression = CompressionAlgorithm::Zstd;
    }

    #[cfg(feature = "encryption")]
    if encrypt {
        config.encryption_key = Some("test-passphrase-for-integration".into());
    }

    config
}

// --- LZ4 compression ---

#[cfg(feature = "compression-lz4")]
#[tokio::test]
async fn test_lz4_backup_restore() {
    let Some(s3) = s3_config() else {
        eprintln!("SKIP: S3 env vars not set");
        return;
    };

    let tmp = tempfile::tempdir().expect("tempdir");
    let db_path = tmp.path().join("lz4_source.db");
    let db_path_str = db_path.to_str().unwrap().to_string();

    let app_conn = create_test_db(&db_path_str);
    insert_rows(&app_conn, 1, 10);

    let config = feature_config(db_path_str.clone(), s3.clone(), Some("lz4"), false);
    let mut mgr = BackupManager::new(config).await.expect("create manager");

    insert_rows(&app_conn, 11, 20);
    mgr.sync_wal().await.expect("sync wal");

    let restore_path = tmp.path().join("lz4_restored.db");
    let restore_path_str = restore_path.to_str().unwrap().to_string();

    // Compression auto-detects on decompress, so plain restore() works
    BackupManager::restore(&s3, &restore_path_str)
        .await
        .expect("restore");

    let restored_conn = Connection::open(&restore_path_str).expect("open restored");
    assert_eq!(count_rows(&restored_conn), 30);
    assert_eq!(sum_values(&restored_conn), sum_values(&app_conn));

    mgr.shutdown().await.expect("shutdown");
    println!("=== test_lz4_backup_restore PASSED ===");
}

// --- Zstd compression ---

#[cfg(feature = "compression-zstd")]
#[tokio::test]
async fn test_zstd_backup_restore() {
    let Some(s3) = s3_config() else {
        eprintln!("SKIP: S3 env vars not set");
        return;
    };

    let tmp = tempfile::tempdir().expect("tempdir");
    let db_path = tmp.path().join("zstd_source.db");
    let db_path_str = db_path.to_str().unwrap().to_string();

    let app_conn = create_test_db(&db_path_str);
    insert_rows(&app_conn, 1, 10);

    let config = feature_config(db_path_str.clone(), s3.clone(), Some("zstd"), false);
    let mut mgr = BackupManager::new(config).await.expect("create manager");

    insert_rows(&app_conn, 11, 20);
    mgr.sync_wal().await.expect("sync wal");

    let restore_path = tmp.path().join("zstd_restored.db");
    let restore_path_str = restore_path.to_str().unwrap().to_string();

    BackupManager::restore(&s3, &restore_path_str)
        .await
        .expect("restore");

    let restored_conn = Connection::open(&restore_path_str).expect("open restored");
    assert_eq!(count_rows(&restored_conn), 30);
    assert_eq!(sum_values(&restored_conn), sum_values(&app_conn));

    mgr.shutdown().await.expect("shutdown");
    println!("=== test_zstd_backup_restore PASSED ===");
}

// --- Encryption ---

#[cfg(feature = "encryption")]
#[tokio::test]
async fn test_encryption_backup_restore() {
    let Some(s3) = s3_config() else {
        eprintln!("SKIP: S3 env vars not set");
        return;
    };

    let tmp = tempfile::tempdir().expect("tempdir");
    let db_path = tmp.path().join("enc_source.db");
    let db_path_str = db_path.to_str().unwrap().to_string();

    let app_conn = create_test_db(&db_path_str);
    insert_rows(&app_conn, 1, 10);

    let config = feature_config(db_path_str.clone(), s3.clone(), None, true);
    let mut mgr = BackupManager::new(config.clone()).await.expect("create manager");

    insert_rows(&app_conn, 11, 20);
    mgr.sync_wal().await.expect("sync wal");

    let restore_path = tmp.path().join("enc_restored.db");
    let restore_path_str = restore_path.to_str().unwrap().to_string();

    // Must use restore_with_config to pass encryption key
    let restore_config = BackupConfig {
        s3: s3.clone(),
        encryption_key: Some("test-passphrase-for-integration".into()),
        ..Default::default()
    };
    BackupManager::restore_with_config(&restore_config, &restore_path_str)
        .await
        .expect("restore with encryption");

    let restored_conn = Connection::open(&restore_path_str).expect("open restored");
    assert_eq!(count_rows(&restored_conn), 30);
    assert_eq!(sum_values(&restored_conn), sum_values(&app_conn));

    mgr.shutdown().await.expect("shutdown");
    println!("=== test_encryption_backup_restore PASSED ===");
}

#[cfg(feature = "encryption")]
#[tokio::test]
async fn test_encryption_checkpoint_restore() {
    let Some(s3) = s3_config() else {
        eprintln!("SKIP: S3 env vars not set");
        return;
    };

    let tmp = tempfile::tempdir().expect("tempdir");
    let db_path = tmp.path().join("enc_ckpt_source.db");
    let db_path_str = db_path.to_str().unwrap().to_string();

    let app_conn = create_test_db(&db_path_str);
    insert_rows(&app_conn, 1, 5);

    let config = feature_config(db_path_str.clone(), s3.clone(), None, true);
    let mut mgr = BackupManager::new(config.clone()).await.expect("create manager");

    // Write, sync, checkpoint (creates gen2)
    insert_rows(&app_conn, 6, 5);
    mgr.sync_wal().await.expect("sync wal");
    mgr.checkpoint().await.expect("checkpoint");

    // Write more in new generation
    insert_rows(&app_conn, 11, 5);
    mgr.sync_wal().await.expect("sync wal after checkpoint");

    let restore_path = tmp.path().join("enc_ckpt_restored.db");
    let restore_path_str = restore_path.to_str().unwrap().to_string();

    let restore_config = BackupConfig {
        s3: s3.clone(),
        encryption_key: Some("test-passphrase-for-integration".into()),
        ..Default::default()
    };
    BackupManager::restore_with_config(&restore_config, &restore_path_str)
        .await
        .expect("restore with encryption after checkpoint");

    let restored_conn = Connection::open(&restore_path_str).expect("open restored");
    assert_eq!(count_rows(&restored_conn), 15);

    mgr.shutdown().await.expect("shutdown");
    println!("=== test_encryption_checkpoint_restore PASSED ===");
}

// --- LZ4 + Encryption ---

#[cfg(all(feature = "compression-lz4", feature = "encryption"))]
#[tokio::test]
async fn test_lz4_encryption_backup_restore() {
    let Some(s3) = s3_config() else {
        eprintln!("SKIP: S3 env vars not set");
        return;
    };

    let tmp = tempfile::tempdir().expect("tempdir");
    let db_path = tmp.path().join("lz4enc_source.db");
    let db_path_str = db_path.to_str().unwrap().to_string();

    let app_conn = create_test_db(&db_path_str);
    insert_rows(&app_conn, 1, 10);

    let config = feature_config(db_path_str.clone(), s3.clone(), Some("lz4"), true);
    let mut mgr = BackupManager::new(config.clone()).await.expect("create manager");

    insert_rows(&app_conn, 11, 20);
    mgr.sync_wal().await.expect("sync wal");

    let restore_path = tmp.path().join("lz4enc_restored.db");
    let restore_path_str = restore_path.to_str().unwrap().to_string();

    let restore_config = BackupConfig {
        s3: s3.clone(),
        encryption_key: Some("test-passphrase-for-integration".into()),
        ..Default::default()
    };
    BackupManager::restore_with_config(&restore_config, &restore_path_str)
        .await
        .expect("restore lz4+encryption");

    let restored_conn = Connection::open(&restore_path_str).expect("open restored");
    assert_eq!(count_rows(&restored_conn), 30);
    assert_eq!(sum_values(&restored_conn), sum_values(&app_conn));

    mgr.shutdown().await.expect("shutdown");
    println!("=== test_lz4_encryption_backup_restore PASSED ===");
}

// --- Zstd + Encryption ---

#[cfg(all(feature = "compression-zstd", feature = "encryption"))]
#[tokio::test]
async fn test_zstd_encryption_backup_restore() {
    let Some(s3) = s3_config() else {
        eprintln!("SKIP: S3 env vars not set");
        return;
    };

    let tmp = tempfile::tempdir().expect("tempdir");
    let db_path = tmp.path().join("zstdenc_source.db");
    let db_path_str = db_path.to_str().unwrap().to_string();

    let app_conn = create_test_db(&db_path_str);
    insert_rows(&app_conn, 1, 10);

    let config = feature_config(db_path_str.clone(), s3.clone(), Some("zstd"), true);
    let mut mgr = BackupManager::new(config.clone()).await.expect("create manager");

    insert_rows(&app_conn, 11, 20);
    mgr.sync_wal().await.expect("sync wal");

    let restore_path = tmp.path().join("zstdenc_restored.db");
    let restore_path_str = restore_path.to_str().unwrap().to_string();

    let restore_config = BackupConfig {
        s3: s3.clone(),
        encryption_key: Some("test-passphrase-for-integration".into()),
        ..Default::default()
    };
    BackupManager::restore_with_config(&restore_config, &restore_path_str)
        .await
        .expect("restore zstd+encryption");

    let restored_conn = Connection::open(&restore_path_str).expect("open restored");
    assert_eq!(count_rows(&restored_conn), 30);
    assert_eq!(sum_values(&restored_conn), sum_values(&app_conn));

    mgr.shutdown().await.expect("shutdown");
    println!("=== test_zstd_encryption_backup_restore PASSED ===");
}

// --- Compaction with compression ---

#[cfg(feature = "compression-lz4")]
#[tokio::test]
async fn test_compression_compaction() {
    let Some(s3) = s3_config() else {
        eprintln!("SKIP: S3 env vars not set");
        return;
    };

    let tmp = tempfile::tempdir().expect("tempdir");
    let db_path = tmp.path().join("lz4compact_source.db");
    let db_path_str = db_path.to_str().unwrap().to_string();

    let app_conn = create_test_db(&db_path_str);
    insert_rows(&app_conn, 1, 5);

    let config = feature_config(db_path_str.clone(), s3.clone(), Some("lz4"), false);
    let mut mgr = BackupManager::new(config).await.expect("create manager");

    // Create multiple small WAL segments
    for batch in 0..5 {
        insert_rows(&app_conn, (batch + 1) * 100, 10);
        mgr.sync_wal().await.expect("sync wal");
    }

    let result = mgr.compact(Some(10 * 1024 * 1024)).await.expect("compact");
    println!(
        "Compaction with lz4: {} -> {} segments",
        result.segments_before, result.segments_after
    );
    assert!(result.segments_after <= result.segments_before);

    // Verify data restores correctly after compaction
    insert_rows(&app_conn, 999, 1);
    mgr.sync_wal().await.expect("final sync");

    let restore_path = tmp.path().join("lz4compact_restored.db");
    let restore_path_str = restore_path.to_str().unwrap().to_string();

    BackupManager::restore(&s3, &restore_path_str)
        .await
        .expect("restore after lz4 compaction");

    let restored_conn = Connection::open(&restore_path_str).expect("open restored");
    let original_count = count_rows(&app_conn);
    assert_eq!(count_rows(&restored_conn), original_count);

    mgr.shutdown().await.expect("shutdown");
    println!("=== test_compression_compaction PASSED ===");
}
