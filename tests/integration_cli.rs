use std::env;
use std::process::{Command, ExitStatus};

use rusqlite::{Connection, params};
use waloy::{BackupConfig, BackupManager, S3Config};

fn s3_config() -> Option<S3Config> {
    Some(S3Config {
        endpoint: env::var("WALOY_TEST_S3_ENDPOINT").ok()?,
        region: env::var("WALOY_TEST_S3_REGION").ok()?,
        bucket: env::var("WALOY_TEST_S3_BUCKET").ok()?,
        access_key: env::var("WALOY_TEST_S3_ACCESS_KEY").ok()?,
        secret_key: env::var("WALOY_TEST_S3_SECRET_KEY").ok()?,
        prefix: format!("cli-test-{}", uuid::Uuid::new_v4()),
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
             value INTEGER NOT NULL
         );",
    )
    .expect("setup db");
    conn
}

fn insert_rows(conn: &Connection, start: i64, count: i64) {
    for i in start..start + count {
        conn.execute(
            "INSERT INTO items (name, value) VALUES (?1, ?2)",
            params![format!("item-{i}"), i * 10],
        )
        .expect("insert row");
    }
}

fn count_rows(conn: &Connection) -> i64 {
    conn.query_row("SELECT COUNT(*) FROM items", [], |row| row.get(0))
        .expect("count rows")
}

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

/// Run the waloy CLI binary with S3 args and the given subcommand args.
/// Returns (exit_status, stdout, stderr).
fn run_cli(s3: &S3Config, args: &[&str]) -> (ExitStatus, String, String) {
    let bin = env!("CARGO_BIN_EXE_waloy");
    let output = Command::new(bin)
        .args([
            "--endpoint",
            &s3.endpoint,
            "--region",
            &s3.region,
            "--bucket",
            &s3.bucket,
            "--access-key",
            &s3.access_key,
            "--secret-key",
            &s3.secret_key,
            "--prefix",
            &s3.prefix,
        ])
        .args(args)
        .output()
        .expect("failed to execute waloy binary");

    let stdout = String::from_utf8_lossy(&output.stdout).to_string();
    let stderr = String::from_utf8_lossy(&output.stderr).to_string();
    (output.status, stdout, stderr)
}

#[tokio::test]
async fn test_cli_generations_inspect_restore() {
    let Some(s3) = s3_config() else {
        eprintln!("SKIP: S3 env vars not set");
        return;
    };

    let tmp = tempfile::tempdir().expect("tempdir");
    let db_path = tmp.path().join("source.db");
    let db_path_str = db_path.to_str().unwrap().to_string();

    // --- Create a backup using the library API ---
    let app_conn = create_test_db(&db_path_str);
    insert_rows(&app_conn, 1, 10);

    let config = test_config(db_path_str.clone(), s3.clone());
    let mut mgr = BackupManager::new(config).await.expect("create manager");

    insert_rows(&app_conn, 11, 20);
    mgr.sync_wal().await.expect("sync wal");

    let original_count = count_rows(&app_conn);
    let original_sum = sum_values(&app_conn);
    assert_eq!(original_count, 30);
    println!("Backup created: {original_count} rows, sum={original_sum}");

    // --- Test `waloy generations` ---
    let (status, stdout, stderr) = run_cli(&s3, &["generations"]);
    println!("--- generations stdout ---\n{stdout}");
    if !stderr.is_empty() {
        eprintln!("--- generations stderr ---\n{stderr}");
    }
    assert!(status.success(), "waloy generations failed: {stderr}");
    assert!(
        stdout.contains("Generations (1)"),
        "expected exactly 1 generation, got:\n{stdout}"
    );
    assert!(
        stdout.contains("(latest)"),
        "expected (latest) marker, got:\n{stdout}"
    );

    // --- Test `waloy inspect` ---
    let (status, stdout, stderr) = run_cli(&s3, &["inspect"]);
    println!("--- inspect stdout ---\n{stdout}");
    if !stderr.is_empty() {
        eprintln!("--- inspect stderr ---\n{stderr}");
    }
    assert!(status.success(), "waloy inspect failed: {stderr}");
    assert!(
        stdout.contains("Generation:"),
        "expected 'Generation:' in output, got:\n{stdout}"
    );
    assert!(
        stdout.contains("WAL segments:"),
        "expected 'WAL segments:' in output, got:\n{stdout}"
    );
    // Should have at least one segment line with offset/size/timestamp
    assert!(
        stdout.contains("offset="),
        "expected segment details with 'offset=' in output, got:\n{stdout}"
    );

    // --- Test `waloy restore` ---
    let restore_path = tmp.path().join("cli_restored.db");
    let restore_path_str = restore_path.to_str().unwrap().to_string();

    let (status, stdout, stderr) = run_cli(&s3, &["restore", "--output", &restore_path_str]);
    println!("--- restore stdout ---\n{stdout}");
    if !stderr.is_empty() {
        eprintln!("--- restore stderr ---\n{stderr}");
    }
    assert!(status.success(), "waloy restore failed: {stderr}");
    assert!(
        stdout.contains("Restored to:"),
        "expected 'Restored to:' in output, got:\n{stdout}"
    );

    // Verify restored data
    let restored_conn = Connection::open(&restore_path_str).expect("open restored db");
    let restored_count = count_rows(&restored_conn);
    let restored_sum = sum_values(&restored_conn);
    println!("Restored: count={restored_count}, sum={restored_sum}");

    assert_eq!(
        restored_count, original_count,
        "row count mismatch after CLI restore"
    );
    assert_eq!(
        restored_sum, original_sum,
        "value sum mismatch after CLI restore"
    );

    // --- Shutdown ---
    mgr.shutdown().await.expect("shutdown");

    println!("=== test_cli_generations_inspect_restore PASSED ===");
}
