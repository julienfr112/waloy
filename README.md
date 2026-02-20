# Waloy - SQLite to S3 Progressive Backup

A Rust library that replicates running SQLite databases to S3-compatible storage using the same mechanism as [Litestream](https://litestream.io/), designed to run inside a Tokio async runtime alongside frameworks like [axum](https://github.com/tokio-rs/axum) and database layers like [sqlx](https://github.com/launchbadge/sqlx).

## Features

- **Litestream-style replication**: WAL-based change capture with controlled checkpointing
- **Tokio native**: Runs as a background task in your async application — no sidecar process needed
- **Framework friendly**: Works alongside axum, sqlx, or any Tokio-based stack
- **S3 Compatible**: Works with AWS S3, Garage, MinIO, or any S3-compatible storage
- **Live Database Support**: Monitors a running SQLite database without interrupting writes
- **Progressive Backup**: Only uploads changed WAL frames since last sync

## Installation

```toml
[dependencies]
waloy = "0.1.0"
tokio = { version = "1.0", features = ["full"] }
```

## Quick Start

```rust
use std::time::Duration;
use waloy::{BackupConfig, BackupManager, S3Config};

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    // Set up SQLite with required PRAGMAs
    let conn = rusqlite::Connection::open("app.db")?;
    conn.execute_batch(
        "PRAGMA journal_mode = WAL;
         PRAGMA busy_timeout = 5000;
         PRAGMA wal_autocheckpoint = 0;"
    )?;

    // Configure and start the backup manager
    let config = BackupConfig {
        db_path: "app.db".into(),
        s3: S3Config {
            endpoint: "https://s3.amazonaws.com".into(),
            region: "us-east-1".into(),
            bucket: "my-backup-bucket".into(),
            access_key: "AKIAIOSFODNN7EXAMPLE".into(),
            secret_key: "wJalrXUtnFEMI/K7MDENG/bPxRfiCYEXAMPLEKEY".into(),
            prefix: "backups/myapp".into(),
        },
        sync_interval: Duration::from_secs(1),
        ..Default::default()
    };

    let mut mgr = BackupManager::new(config).await?;

    // Your app writes to SQLite normally...
    conn.execute("CREATE TABLE IF NOT EXISTS items (id INTEGER PRIMARY KEY, name TEXT)", [])?;
    conn.execute("INSERT INTO items (name) VALUES (?1)", ["hello"])?;

    // Sync new WAL frames to S3
    mgr.sync_wal().await?;

    // Graceful shutdown (final sync + release read transaction)
    mgr.shutdown().await?;
    Ok(())
}
```

## Usage with axum and sqlx

A typical pattern: the application uses axum for HTTP and rusqlite for database access, while waloy runs a background sync loop in the same Tokio runtime.

```rust
use std::sync::Arc;
use std::time::Duration;

use axum::{Router, routing::get, extract::State, Json};
use rusqlite::{Connection, params};
use tokio::sync::Mutex;
use waloy::{BackupConfig, BackupManager, S3Config};

type Db = Arc<std::sync::Mutex<Connection>>;

async fn list_items(State(db): State<Db>) -> Json<Vec<String>> {
    let conn = db.lock().unwrap();
    let mut stmt = conn.prepare("SELECT name FROM items").unwrap();
    let names: Vec<String> = stmt
        .query_map([], |row| row.get(0))
        .unwrap()
        .collect::<Result<_, _>>()
        .unwrap();
    Json(names)
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    // Open the database with required PRAGMAs
    let conn = Connection::open("app.db")?;
    conn.execute_batch(
        "PRAGMA journal_mode = WAL;
         PRAGMA busy_timeout = 5000;
         PRAGMA wal_autocheckpoint = 0;
         CREATE TABLE IF NOT EXISTS items (id INTEGER PRIMARY KEY, name TEXT NOT NULL);"
    )?;
    let db: Db = Arc::new(std::sync::Mutex::new(conn));

    // Start the backup manager (takes an initial snapshot)
    let config = BackupConfig {
        db_path: "app.db".into(),
        s3: S3Config {
            endpoint: "http://localhost:3900".into(),
            region: "garage".into(),
            bucket: "my-bucket".into(),
            access_key: "ACCESS_KEY".into(),
            secret_key: "SECRET_KEY".into(),
            prefix: "backups/myapp".into(),
        },
        sync_interval: Duration::from_secs(1),
        ..Default::default()
    };
    let mgr = Arc::new(Mutex::new(BackupManager::new(config).await?));

    // Background sync loop
    let mgr_bg = mgr.clone();
    tokio::spawn(async move {
        loop {
            tokio::time::sleep(Duration::from_secs(1)).await;
            let mut m = mgr_bg.lock().await;
            let _ = m.sync_wal().await;
            let _ = m.maybe_snapshot().await;
        }
    });

    // axum server
    let app = Router::new()
        .route("/items", get(list_items))
        .with_state(db);
    let listener = tokio::net::TcpListener::bind("0.0.0.0:3000").await?;
    axum::serve(listener, app).await?;

    // On shutdown: flush remaining WAL and release the read transaction
    mgr.lock().await.shutdown().await?;
    Ok(())
}
```

## Restore

```rust
use waloy::{BackupManager, S3Config};

// Restore the latest backup to a new file
BackupManager::restore(&s3_config, "restored.db").await?;

// Or restore to a specific point in time
BackupManager::restore_to_time(&s3_config, "restored.db", timestamp_ms).await?;
```

## How It Works

### SQLite WAL mode

By default SQLite uses a rollback journal: every write locks the entire database, copies the original pages to a `-journal` file, then modifies the database file in place. This is simple but slow — readers block writers and vice versa.

**WAL (Write-Ahead Logging)** reverses the approach. Instead of modifying the main database file directly, SQLite appends new or changed pages as **frames** to a separate `-wal` file. The main database file stays untouched during writes.

This gives two key properties:

- **Readers never block writers.** Readers see a consistent snapshot of the database at the moment their transaction started by reading from the main file and only the WAL frames that existed at that point. New frames appended by writers are invisible to existing readers.
- **Writers never block readers.** A writer only needs to append to the end of the WAL file, so it doesn't interfere with readers scanning the main database file.

#### Checkpointing: merging the WAL back

The WAL file grows as writes accumulate. To keep it bounded, SQLite periodically runs a **checkpoint** — a process that copies committed frames from the WAL back into the main database file.

SQLite offers several checkpoint modes:

- **PASSIVE**: copies frames that no reader is currently using. Non-blocking — it skips frames that are still needed by active read transactions. The WAL file is not truncated.
- **FULL**: waits until there are no readers, then copies all frames. Blocks new readers until done.
- **TRUNCATE**: same as FULL, but also truncates the WAL file to zero bytes afterward, reclaiming disk space.
- **RESTART**: same as FULL, but resets the WAL header so new writes start from the beginning.

By default, SQLite runs a PASSIVE checkpoint automatically every 1000 frames (`wal_autocheckpoint`). This is fine for most applications, but it means frames can disappear from the WAL at any time — a problem for replication.

### How waloy replicates

Waloy uses the same strategy as [Litestream](https://litestream.io/how-it-works/) to capture every change before SQLite checkpoints it away:

1. **Disable auto-checkpoint.** `PRAGMA wal_autocheckpoint = 0` prevents SQLite from checkpointing on its own. Waloy controls when checkpoints happen.

2. **Hold a long-running read transaction.** Waloy opens a dedicated connection and begins a read transaction that it never commits. This pins the WAL — SQLite cannot checkpoint past the point that an active reader is using, so all frames remain available for waloy to read.

3. **Monitor the WAL file.** On each sync cycle (~1s), waloy checks if the `-wal` file has grown since the last sync. New frames are read and uploaded to S3 as a WAL **segment**.

4. **Upload segments to S3.** Each segment is uploaded with a sequential index. The S3 layout is:
   ```
   {prefix}/latest                     # current generation ID
   {prefix}/{gen_id}/snapshot           # full database file
   {prefix}/{gen_id}/manifest.json      # segment metadata
   {prefix}/{gen_id}/wal/0              # first WAL segment
   {prefix}/{gen_id}/wal/1              # second WAL segment
   ...
   ```

5. **Controlled checkpointing.** When waloy decides to checkpoint (threshold-based or scheduled), it: syncs any remaining WAL frames, releases its read transaction, runs `PRAGMA wal_checkpoint(TRUNCATE)` to merge all frames back into the main file and truncate the WAL, takes a fresh snapshot (uploads the full database), starts a new **generation**, and re-acquires the read transaction.

6. **Generations.** Each checkpoint starts a new generation. A generation is a self-contained recovery unit: one snapshot plus a sequence of WAL segments. To restore, waloy downloads the latest snapshot and replays all segments from that generation on top of it.

7. **Restore.** Download the snapshot, download and concatenate all WAL segments into a `-wal` file next to the database, then open with SQLite — it automatically replays the WAL on open.

## Requirements

- SQLite 3.x with WAL mode enabled
- Tokio async runtime (e.g. via axum, sqlx, or direct tokio usage)
- S3-compatible storage with write permissions

### Required SQLite PRAGMAs

```sql
PRAGMA journal_mode = WAL;
PRAGMA busy_timeout = 5000;         -- waloy briefly locks during checkpoints
PRAGMA wal_autocheckpoint = 0;      -- let waloy control checkpointing
```

## Optional Features

```toml
waloy = { version = "0.1.0", features = ["compression", "encryption"] }
```

| Feature | Description |
|---------|-------------|
| `compression-lz4` | LZ4 compression for snapshots and WAL segments |
| `compression-zstd` | zstd compression |
| `compression` | Both LZ4 and zstd |
| `encryption` | AES-256-GCM client-side encryption with Argon2id KDF |
| `cli` | `waloy` CLI binary (`restore`, `generations`, `inspect`) |
| `full` | All of the above |

## License

MIT
