use serde::{Deserialize, Serialize};

/// Metadata for a single WAL segment.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SegmentMeta {
    pub index: u32,
    pub timestamp_ms: u64,
    pub offset: u64,
    pub size: u64,
}

/// Manifest for a generation, stored as `{gen}/manifest.json` in S3.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct GenerationManifest {
    pub generation: String,
    pub created_at_ms: u64,
    pub snapshot_timestamp_ms: u64,
    pub segments: Vec<SegmentMeta>,
}

impl GenerationManifest {
    pub fn new(generation: String, now_ms: u64) -> Self {
        Self {
            generation,
            created_at_ms: now_ms,
            snapshot_timestamp_ms: now_ms,
            segments: Vec::new(),
        }
    }

    pub fn add_segment(&mut self, index: u32, timestamp_ms: u64, offset: u64, size: u64) {
        self.segments.push(SegmentMeta {
            index,
            timestamp_ms,
            offset,
            size,
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn new_initializes_correctly() {
        let m = GenerationManifest::new("gen-1".into(), 1000);
        assert_eq!(m.generation, "gen-1");
        assert_eq!(m.created_at_ms, 1000);
        assert_eq!(m.snapshot_timestamp_ms, 1000);
        assert!(m.segments.is_empty());
    }

    #[test]
    fn add_segment_appends() {
        let mut m = GenerationManifest::new("gen-1".into(), 1000);
        m.add_segment(0, 1001, 0, 512);
        m.add_segment(1, 1002, 512, 256);
        assert_eq!(m.segments.len(), 2);
        assert_eq!(m.segments[0].index, 0);
        assert_eq!(m.segments[0].offset, 0);
        assert_eq!(m.segments[0].size, 512);
        assert_eq!(m.segments[1].index, 1);
        assert_eq!(m.segments[1].offset, 512);
        assert_eq!(m.segments[1].size, 256);
    }

    #[test]
    fn serde_roundtrip() {
        let mut m = GenerationManifest::new("gen-abc".into(), 5000);
        m.add_segment(0, 5001, 0, 1024);
        m.add_segment(1, 5002, 1024, 2048);

        let json = serde_json::to_string(&m).unwrap();
        let m2: GenerationManifest = serde_json::from_str(&json).unwrap();

        assert_eq!(m, m2);
    }

    #[test]
    fn empty_manifest_serde_roundtrip() {
        let m = GenerationManifest::new("empty-gen".into(), 0);
        let json = serde_json::to_string(&m).unwrap();
        let m2: GenerationManifest = serde_json::from_str(&json).unwrap();
        assert_eq!(m, m2);
        assert!(m2.segments.is_empty());
    }
}
