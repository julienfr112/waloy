use std::env;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::extract::State;
use axum::routing::get;
use axum::{Json, Router};
use rusqlite::{Connection, params};
use serde::{Deserialize, Serialize};
use tokio::net::TcpListener;
use waloy::{BackupConfig, BackupManager, S3Config};

fn s3_config() -> Option<S3Config> {
    Some(S3Config {
        endpoint: env::var("WALOY_TEST_S3_ENDPOINT").ok()?,
        region: env::var("WALOY_TEST_S3_REGION").ok()?,
        bucket: env::var("WALOY_TEST_S3_BUCKET").ok()?,
        access_key: env::var("WALOY_TEST_S3_ACCESS_KEY").ok()?,
        secret_key: env::var("WALOY_TEST_S3_SECRET_KEY").ok()?,
        prefix: format!("test-axum-{}", uuid::Uuid::new_v4()),
    })
}

// -- axum app types and handlers --

type DbPool = Arc<Mutex<Connection>>;

#[derive(Deserialize)]
struct CreateItem {
    name: String,
}

#[derive(Serialize, Deserialize)]
struct Item {
    id: i64,
    name: String,
}

async fn create_item(
    State(pool): State<DbPool>,
    Json(payload): Json<CreateItem>,
) -> axum::http::StatusCode {
    let conn = pool.lock().unwrap();
    conn.execute(
        "INSERT INTO items (name) VALUES (?1)",
        params![payload.name],
    )
    .unwrap();
    axum::http::StatusCode::CREATED
}

async fn list_items(State(pool): State<DbPool>) -> Json<Vec<Item>> {
    let conn = pool.lock().unwrap();
    let mut stmt = conn.prepare("SELECT id, name FROM items").unwrap();
    let items = stmt
        .query_map([], |row| {
            Ok(Item {
                id: row.get(0)?,
                name: row.get(1)?,
            })
        })
        .unwrap()
        .collect::<Result<Vec<_>, _>>()
        .unwrap();
    Json(items)
}

/// Full integration test: axum server with rusqlite writing to SQLite,
/// waloy backing up in the background, then restore and verify.
#[tokio::test]
async fn test_axum_server_with_backup() {
    let Some(s3) = s3_config() else {
        eprintln!("SKIP: S3 env vars not set");
        return;
    };

    let tmp = tempfile::tempdir().expect("tempdir");
    let db_path = tmp.path().join("app.db");
    let db_path_str = db_path.to_str().unwrap().to_string();

    // -- Application DB connection: WAL mode, autocheckpoint disabled --
    let app_conn = Connection::open(&db_path).expect("open db");
    app_conn
        .execute_batch(
            "PRAGMA journal_mode = WAL;
             PRAGMA busy_timeout = 5000;
             PRAGMA wal_autocheckpoint = 0;
             CREATE TABLE items (id INTEGER PRIMARY KEY, name TEXT NOT NULL);",
        )
        .expect("setup db");
    let pool: DbPool = Arc::new(Mutex::new(app_conn));

    // -- BackupManager: takes initial snapshot on creation --
    let config = BackupConfig {
        db_path: db_path_str.clone(),
        s3: s3.clone(),
        sync_interval: Duration::from_millis(500),
        checkpoint_threshold_bytes: 4 * 1024 * 1024,
        ..Default::default()
    };
    let mgr = Arc::new(tokio::sync::Mutex::new(
        BackupManager::new(config)
            .await
            .expect("create backup manager"),
    ));

    // -- axum server on random port --
    let app = Router::new()
        .route("/items", get(list_items).post(create_item))
        .with_state(pool.clone());
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let addr = listener.local_addr().expect("addr");
    tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });

    // -- background WAL sync every 500ms --
    let mgr_sync = mgr.clone();
    let sync_handle = tokio::spawn(async move {
        loop {
            tokio::time::sleep(Duration::from_millis(500)).await;
            let mut m = mgr_sync.lock().await;
            let _ = m.sync_wal().await;
        }
    });

    let client = reqwest::Client::new();
    let base = format!("http://{addr}");

    // -- Phase 1: POST 10 items, explicit sync --
    for i in 0..10 {
        let resp = client
            .post(format!("{base}/items"))
            .json(&serde_json::json!({"name": format!("item-{i}")}))
            .send()
            .await
            .expect("POST");
        assert_eq!(resp.status().as_u16(), 201);
    }
    {
        let mut m = mgr.lock().await;
        m.sync_wal().await.expect("sync after phase 1");
    }
    println!("Phase 1: 10 items inserted and synced");

    // -- Phase 2: POST 10 more items, then checkpoint (new generation) --
    for i in 10..20 {
        let resp = client
            .post(format!("{base}/items"))
            .json(&serde_json::json!({"name": format!("item-{i}")}))
            .send()
            .await
            .expect("POST");
        assert_eq!(resp.status().as_u16(), 201);
    }
    {
        let mut m = mgr.lock().await;
        m.checkpoint().await.expect("checkpoint");
    }
    println!("Phase 2: 10 more items, checkpoint done (new generation)");

    // -- Phase 3: POST 10 more items, let background sync pick them up --
    for i in 20..30 {
        let resp = client
            .post(format!("{base}/items"))
            .json(&serde_json::json!({"name": format!("item-{i}")}))
            .send()
            .await
            .expect("POST");
        assert_eq!(resp.status().as_u16(), 201);
    }
    // let background sync run at least once
    tokio::time::sleep(Duration::from_secs(1)).await;
    println!("Phase 3: 10 more items, background sync ran");

    // -- Verify server has all 30 items --
    let items: Vec<Item> = client
        .get(format!("{base}/items"))
        .send()
        .await
        .expect("GET")
        .json()
        .await
        .expect("parse");
    assert_eq!(items.len(), 30);
    println!("Server confirms {} items via GET", items.len());

    // -- Stop sync, final flush --
    sync_handle.abort();
    {
        let mut m = mgr.lock().await;
        m.sync_wal().await.expect("final sync");
    }

    // -- Restore to a new path --
    let restore_path = tmp.path().join("restored.db");
    let restore_path_str = restore_path.to_str().unwrap().to_string();

    BackupManager::restore(&s3, &restore_path_str)
        .await
        .expect("restore");

    // -- Verify restored data --
    let restored = Connection::open(&restore_path_str).expect("open restored");
    let count: i64 = restored
        .query_row("SELECT COUNT(*) FROM items", [], |row| row.get(0))
        .expect("count");
    println!("Restored DB has {count} items");
    assert_eq!(count, 30, "all 30 items should survive backup + restore");

    // Spot-check first and last items
    let first: String = restored
        .query_row("SELECT name FROM items WHERE id = 1", [], |row| row.get(0))
        .expect("first item");
    assert_eq!(first, "item-0");

    let last: String = restored
        .query_row(
            "SELECT name FROM items ORDER BY id DESC LIMIT 1",
            [],
            |row| row.get(0),
        )
        .expect("last item");
    assert_eq!(last, "item-29");

    // -- Shutdown --
    {
        let mut m = mgr.lock().await;
        m.shutdown().await.expect("shutdown");
    }

    println!("=== test_axum_server_with_backup PASSED ===");
}
