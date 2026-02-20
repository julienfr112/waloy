use std::time::Duration;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct S3Config {
    pub endpoint: String,
    pub region: String,
    pub bucket: String,
    pub access_key: String,
    pub secret_key: String,
    pub prefix: String,
}

/// Algorithm used for compressing snapshots and WAL segments before upload.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub enum CompressionAlgorithm {
    #[default]
    None,
    #[cfg(feature = "compression-lz4")]
    Lz4,
    #[cfg(feature = "compression-zstd")]
    Zstd,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BackupConfig {
    pub db_path: String,
    pub s3: S3Config,
    pub sync_interval: Duration,
    pub checkpoint_threshold_bytes: u64,
    /// How long to keep old generations before deleting them.
    pub retention_duration: Option<Duration>,
    /// Compression algorithm for snapshots and WAL segments.
    pub compression: CompressionAlgorithm,
    /// Passphrase for client-side encryption (AES-256-GCM).
    #[cfg(feature = "encryption")]
    pub encryption_key: Option<String>,
    /// If true, restore from S3 automatically when the DB file doesn't exist.
    pub auto_restore: bool,
    /// If set, automatically take a new snapshot at this interval.
    pub snapshot_interval: Option<Duration>,
}

impl Default for BackupConfig {
    fn default() -> Self {
        Self {
            db_path: String::new(),
            s3: S3Config {
                endpoint: String::new(),
                region: String::new(),
                bucket: String::new(),
                access_key: String::new(),
                secret_key: String::new(),
                prefix: String::new(),
            },
            sync_interval: Duration::from_secs(1),
            checkpoint_threshold_bytes: 4 * 1024 * 1024,
            retention_duration: None,
            compression: CompressionAlgorithm::default(),
            #[cfg(feature = "encryption")]
            encryption_key: None,
            auto_restore: false,
            snapshot_interval: None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn backup_config_default_values() {
        let cfg = BackupConfig::default();
        assert_eq!(cfg.sync_interval, Duration::from_secs(1));
        assert_eq!(cfg.checkpoint_threshold_bytes, 4 * 1024 * 1024);
        assert!(cfg.retention_duration.is_none());
        assert_eq!(cfg.compression, CompressionAlgorithm::None);
        assert!(!cfg.auto_restore);
        assert!(cfg.snapshot_interval.is_none());
        assert!(cfg.db_path.is_empty());
        assert!(cfg.s3.endpoint.is_empty());
        assert!(cfg.s3.prefix.is_empty());
        #[cfg(feature = "encryption")]
        assert!(cfg.encryption_key.is_none());
    }

    #[test]
    fn compression_algorithm_default_is_none() {
        assert_eq!(CompressionAlgorithm::default(), CompressionAlgorithm::None);
    }

    #[test]
    fn s3_config_clone_and_debug() {
        let cfg = S3Config {
            endpoint: "http://localhost:3900".into(),
            region: "us-east-1".into(),
            bucket: "test".into(),
            access_key: "ak".into(),
            secret_key: "sk".into(),
            prefix: "pfx".into(),
        };
        let cfg2 = cfg.clone();
        assert_eq!(cfg, cfg2);
        let dbg = format!("{:?}", cfg);
        assert!(dbg.contains("S3Config"));
    }

    #[test]
    fn backup_config_custom_construction() {
        let cfg = BackupConfig {
            db_path: "/tmp/test.db".into(),
            sync_interval: Duration::from_millis(500),
            checkpoint_threshold_bytes: 1024,
            retention_duration: Some(Duration::from_secs(3600)),
            auto_restore: true,
            snapshot_interval: Some(Duration::from_secs(60)),
            ..Default::default()
        };
        assert_eq!(cfg.db_path, "/tmp/test.db");
        assert_eq!(cfg.sync_interval, Duration::from_millis(500));
        assert_eq!(cfg.checkpoint_threshold_bytes, 1024);
        assert_eq!(cfg.retention_duration, Some(Duration::from_secs(3600)));
        assert!(cfg.auto_restore);
        assert_eq!(cfg.snapshot_interval, Some(Duration::from_secs(60)));
    }
}
