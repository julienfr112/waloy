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
use waloy::BackupManager;
use tokio;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let manager = BackupManager::new(
        "database.sqlite",
        "my-bucket",
        "us-east-1",
        "access-key",
        "secret-key"
    ).await?;

    manager.full_backup().await?;
    manager.incremental_backup().await?;

    Ok(())
}
```

## Configuration

```rust
use waloy::{BackupManager, BackupConfig};

let config = BackupConfig {
    backup_interval: std::time::Duration::from_secs(3600),
    max_retries: 3,
    chunk_size: 5 * 1024 * 1024,
    ..Default::default()
};

let manager = BackupManager::with_config(
    "database.sqlite",
    "bucket",
    "region",
    "key",
    "secret",
    config
).await?;
```

## How It Works

Waloy uses the same replication strategy as [Litestream](https://litestream.io/how-it-works/):

1. **Long-running read transaction**: Waloy holds a persistent read transaction on the database, preventing SQLite from auto-checkpointing and giving waloy exclusive control over the WAL lifecycle.
2. **WAL monitoring**: New frames written to the `-wal` file by your application are detected and copied to a local staging area.
3. **Async S3 upload**: Staged WAL segments are uploaded to S3 asynchronously (~1s default sync interval).
4. **Controlled checkpointing**: Waloy manages checkpoints on a tiered strategy (PASSIVE at ~4 MB, TRUNCATE at ~500 MB, periodic cleanup) to keep the WAL bounded without blocking your application.
5. **Generations & snapshots**: Changes are grouped into generations, each starting with a full snapshot followed by sequential WAL segments, ensuring a complete recovery path.
6. **Restore**: Reconstruct a database by downloading the latest snapshot and replaying subsequent WAL segments.

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

## Error Handling

```rust
match manager.backup().await {
    Ok(info) => println!("Backup complete: {:?}", info),
    Err(e) => eprintln!("Error: {}", e),
}
```

## Performance

- Configurable memory usage via chunk sizes
- Minimal network bandwidth for incremental backups
- Adjustable compression levels
- Low CPU overhead

## License

MIT
