pub mod compression;
#[cfg(feature = "compression-lz4")]
mod compression_lz4;
#[cfg(feature = "compression-zstd")]
mod compression_zstd;
mod config;
#[cfg(feature = "encryption")]
pub mod encryption;
mod error;
mod manager;
mod manifest;
mod s3;
mod stats;

pub use config::{BackupConfig, CompressionAlgorithm, S3Config};
pub use error::{Error, Result};
pub use manager::{BackupManager, CompactionResult};
pub use manifest::{GenerationManifest, SegmentMeta};
pub use stats::BackupStats;
