use std::env;
use std::time::Instant;

use rusqlite::{Connection, params};
use waloy::{BackupConfig, BackupManager, S3Config};

fn s3_config() -> Option<S3Config> {
    Some(S3Config {
        endpoint: env::var("WALOY_TEST_S3_ENDPOINT").ok()?,
        region: env::var("WALOY_TEST_S3_REGION").ok()?,
        bucket: env::var("WALOY_TEST_S3_BUCKET").ok()?,
        access_key: env::var("WALOY_TEST_S3_ACCESS_KEY").ok()?,
        secret_key: env::var("WALOY_TEST_S3_SECRET_KEY").ok()?,
        prefix: format!("perf-{}", uuid::Uuid::new_v4()),
    })
}

fn create_test_db(path: &str) -> Connection {
    let conn = Connection::open(path).expect("open db");
    conn.execute_batch(
        "PRAGMA journal_mode = WAL;
         PRAGMA busy_timeout = 5000;
         PRAGMA wal_autocheckpoint = 0;
         CREATE TABLE IF NOT EXISTS items (
             id INTEGER PRIMARY KEY,
             name TEXT NOT NULL,
             value BLOB NOT NULL
         );",
    )
    .expect("setup db");
    conn
}

fn insert_rows(conn: &Connection, start: i64, count: i64, blob_size: usize) {
    let blob: Vec<u8> = (0..blob_size).map(|i| (i % 251) as u8).collect();
    for i in start..start + count {
        conn.execute(
            "INSERT INTO items (name, value) VALUES (?1, ?2)",
            params![format!("item-{i}"), blob],
        )
        .expect("insert row");
    }
}

fn count_rows(conn: &Connection) -> i64 {
    conn.query_row("SELECT COUNT(*) FROM items", [], |row| row.get(0))
        .expect("count rows")
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

struct PerfResult {
    name: &'static str,
    duration: std::time::Duration,
}

impl PerfResult {
    fn new(name: &'static str, duration: std::time::Duration) -> Self {
        Self { name, duration }
    }
}

fn print_summary(results: &[PerfResult]) {
    println!("\n{:=<60}", "");
    println!("  PERFORMANCE SUMMARY");
    println!("{:=<60}", "");
    println!("  {:<35} {:>15}", "Operation", "Duration");
    println!("  {:-<35} {:->15}", "", "");
    for r in results {
        println!("  {:<35} {:>12.1?}", r.name, r.duration);
    }
    println!("{:=<60}\n", "");
}

#[tokio::test]
async fn perf_end_to_end() {
    let Some(s3) = s3_config() else {
        eprintln!("SKIP: S3 env vars not set");
        return;
    };

    let tmp = tempfile::tempdir().expect("tempdir");
    let db_path = tmp.path().join("perf.db");
    let db_path_str = db_path.to_str().unwrap().to_string();
    let mut results = Vec::new();

    // --- Snapshot creation ---
    let app_conn = create_test_db(&db_path_str);
    insert_rows(&app_conn, 1, 100, 256);

    let t = Instant::now();
    let config = test_config(db_path_str.clone(), s3.clone());
    let mut mgr = BackupManager::new(config).await.expect("create manager");
    results.push(PerfResult::new("snapshot (100 rows, 256B blobs)", t.elapsed()));

    // --- Small WAL sync (10 rows) ---
    insert_rows(&app_conn, 101, 10, 256);
    let t = Instant::now();
    mgr.sync_wal().await.expect("sync small");
    results.push(PerfResult::new("sync_wal (10 rows, small)", t.elapsed()));

    // --- Larger WAL sync (500 rows, 1KB blobs) ---
    insert_rows(&app_conn, 201, 500, 1024);
    let t = Instant::now();
    mgr.sync_wal().await.expect("sync large");
    results.push(PerfResult::new("sync_wal (500 rows, 1KB blobs)", t.elapsed()));

    // --- Another sync to accumulate segments ---
    insert_rows(&app_conn, 701, 200, 512);
    mgr.sync_wal().await.expect("sync extra");

    // --- Compaction ---
    let t = Instant::now();
    let compaction = mgr.compact(None).await.expect("compact");
    results.push(PerfResult::new("compact", t.elapsed()));
    println!(
        "  compaction: {} -> {} segments",
        compaction.segments_before, compaction.segments_after
    );

    // --- Checkpoint ---
    let t = Instant::now();
    mgr.checkpoint().await.expect("checkpoint");
    results.push(PerfResult::new("checkpoint", t.elapsed()));

    // --- Shutdown ---
    let t = Instant::now();
    mgr.shutdown().await.expect("shutdown");
    results.push(PerfResult::new("shutdown", t.elapsed()));
    drop(app_conn);

    // --- Restore ---
    let restore_path = tmp.path().join("restored.db");
    let restore_path_str = restore_path.to_str().unwrap().to_string();
    let t = Instant::now();
    BackupManager::restore(&s3, &restore_path_str)
        .await
        .expect("restore");
    results.push(PerfResult::new("restore", t.elapsed()));

    // Verify restored data
    let restored_conn = Connection::open(&restore_path_str).expect("open restored");
    let row_count = count_rows(&restored_conn);
    println!("  restored row count: {row_count}");
    assert!(row_count > 0, "restored DB should have rows");

    print_summary(&results);
}
