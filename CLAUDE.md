# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## Project Overview

Waloy is a Rust library for creating progressive backups of running SQLite databases to S3-compatible storage. It uses the same replication mechanism as [Litestream](https://litestream.io/) (see below), reimplemented as a Rust library designed to run inside a Tokio async runtime alongside frameworks like axum and database layers like sqlx.

## Build Commands

- `cargo build --release` — build release binary
- `cargo test --lib` — run unit tests
- `cargo fmt` — format code
- `cargo clippy` — lint
- `make integration-test` — run integration tests against a local Garage S3 instance (downloads Garage, starts it, runs tests, stops it)

These are also available via `make build`, `make test`, `make format`, `make lint`.

## Local S3 Development (Garage)

`make integration-test` handles everything automatically: downloads the Garage binary (v2.2.0, Linux only), creates a fresh config with `replication_factor = 1`, starts the server, configures layout/bucket/keys, runs `cargo test --test integration`, and kills Garage on exit via trap.

Garage config uses absolute paths and listens on:
- S3 API: port 3900
- RPC: port 3901
- Admin API: port 3903

## Runtime Integration

Waloy is designed to be embedded in a Tokio application (e.g. an axum web server using sqlx for database access). It runs as a background Tokio task within the same process — no sidecar or external daemon needed. The application keeps using SQLite normally through sqlx; waloy monitors and replicates in the background.

## Architecture

- **Rust edition 2024**, dependencies: `rusqlite` (bundled SQLite), `tokio`, `rust-s3`, `thiserror`, `uuid`, `tracing`, `serde`, `serde_json`
- Optional deps behind features: `lz4_flex`, `zstd`, `aes-gcm`, `argon2`, `rand`, `clap`, `anyhow`
- Features: `compression-lz4`, `compression-zstd`, `compression` (both), `encryption`, `cli`, `full` (all)
- Public API: `BackupManager`, `BackupConfig`, `S3Config`, `BackupStats`, `CompactionResult`, `GenerationManifest`, `SegmentMeta`, `CompressionAlgorithm` (from `src/lib.rs`)
- Source layout:
  - `src/config.rs` — `S3Config`, `BackupConfig`, `CompressionAlgorithm` types
  - `src/error.rs` — `Error` enum and `Result` type alias
  - `src/s3.rs` — `S3Client` wrapper around `rust-s3` Bucket (path-style, custom endpoint, delete ops)
  - `src/manager.rs` — `BackupManager` implementing all replication, recovery, PITR, retention, compaction
  - `src/stats.rs` — `BackupStats` and internal `StatsTracker`
  - `src/manifest.rs` — `GenerationManifest` and `SegmentMeta` (serialized to S3 as JSON)
  - `src/compression.rs` — compress/decompress dispatch with magic byte detection
  - `src/compression_lz4.rs` — LZ4 backend (feature-gated)
  - `src/compression_zstd.rs` — zstd backend (feature-gated)
  - `src/encryption.rs` — AES-256-GCM encrypt/decrypt with Argon2id KDF (feature-gated)
  - `src/bin/waloy.rs` — CLI binary (feature-gated behind `cli`)

## Replication Mechanism (Litestream-style)

Waloy replicates SQLite databases using the same mechanism as Litestream:

1. **Long-running read transaction**: Waloy holds a persistent read transaction on the database. In WAL mode, this prevents SQLite from auto-checkpointing, giving waloy exclusive control over when checkpoints happen.
2. **WAL monitoring**: Waloy watches the `-wal` file for new frames written by the application. New frames are copied to a local staging area before being uploaded.
3. **Generations**: Changes are organized into generations. Each generation starts with a full snapshot and is followed by sequential WAL segments. A new generation is created when waloy first starts, or when a checkpoint occurs.
4. **Async upload to S3**: WAL segments are uploaded to S3-compatible storage. S3 key layout: `{prefix}/latest` (current generation ID), `{prefix}/{gen_id}/snapshot` (DB file), `{prefix}/{gen_id}/wal/{index}` (WAL segments).
5. **Controlled checkpointing**: `checkpoint()` syncs remaining WAL, ends the read transaction, runs `PRAGMA wal_checkpoint(TRUNCATE)`, takes a new snapshot, and re-acquires the read transaction.
6. **Restore**: `BackupManager::restore()` downloads the latest snapshot, downloads and concatenates all WAL segments, places the WAL file next to the DB, and opens with SQLite to trigger replay.

### Required SQLite PRAGMAs

The application (or waloy itself) must ensure:

- `PRAGMA journal_mode = WAL;`
- `PRAGMA busy_timeout = 5000;` — needed because waloy briefly acquires write locks during checkpoints
- `PRAGMA wal_autocheckpoint = 0;` — recommended to prevent SQLite from checkpointing independently, which would cause missed WAL frames

## Implemented Features

- **Point-in-time restore**: `BackupManager::restore_to_time()` restores to a specific timestamp using generation manifests
- **Retention policies**: `BackupManager::enforce_retention()` auto-deletes old generations from S3 (`retention_duration` config)
- **Compression**: LZ4 and zstd compression via `compression-lz4`/`compression-zstd` features, auto-detected on decompress via magic bytes
- **Client-side encryption**: AES-256-GCM with Argon2id key derivation via `encryption` feature (`encryption_key` config)
- **Compaction**: `BackupManager::compact()` merges small WAL segments into larger ones on S3
- **Graceful shutdown**: `BackupManager::shutdown()` performs final WAL sync and releases read transaction; `Drop` warns if not called
- **Automatic recovery**: detects WAL discontinuities (salt change or shrink) and auto-starts a new generation with fresh snapshot
- **Auto-restore on startup**: `auto_restore` config flag restores from S3 if DB file doesn't exist
- **Snapshot scheduling**: `maybe_snapshot()` triggers checkpoint when `snapshot_interval` elapses
- **CLI tool**: `waloy` binary (behind `cli` feature) with `restore`, `generations`, `inspect` commands
- **Backup stats API**: `BackupManager::stats()` returns `BackupStats` with generation count, sync count, bytes uploaded, timestamps, errors
- **Generation manifests**: each generation stores `manifest.json` in S3 with segment metadata (timestamps, offsets, sizes)

## Non-goals

- **Checksum validation**: trust S3 data integrity rather than adding periodic verification
- **Additional storage backends**: S3-compatible only (covers GCS, Azure, R2, MinIO, etc. via S3 compatibility)
- **Built-in background task**: the app controls the sync/checkpoint loop, waloy doesn't spawn its own tokio tasks
- **Multi-database support**: one BackupManager per database, no directory watching or auto-discovery
- **Multiple simultaneous replicas**: single S3 destination per BackupManager
- **Prometheus metrics endpoint**: waloy exposes stats via its API, the app is responsible for metrics export
