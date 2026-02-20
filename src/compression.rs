use crate::config::CompressionAlgorithm;
use crate::error::{Error, Result};

/// Magic bytes for identifying compressed data.
const MAGIC_LZ4: &[u8; 4] = b"WL4\x01";
const MAGIC_ZSTD: &[u8; 4] = b"WZS\x01";

/// Compress data using the specified algorithm. Returns raw data if None.
pub fn compress(data: &[u8], algo: &CompressionAlgorithm) -> Result<Vec<u8>> {
    match algo {
        CompressionAlgorithm::None => Ok(data.to_vec()),
        #[cfg(feature = "compression-lz4")]
        CompressionAlgorithm::Lz4 => {
            let compressed = crate::compression_lz4::compress(data)?;
            let mut out = Vec::with_capacity(4 + compressed.len());
            out.extend_from_slice(MAGIC_LZ4);
            out.extend_from_slice(&compressed);
            Ok(out)
        }
        #[cfg(feature = "compression-zstd")]
        CompressionAlgorithm::Zstd => {
            let compressed = crate::compression_zstd::compress(data)?;
            let mut out = Vec::with_capacity(4 + compressed.len());
            out.extend_from_slice(MAGIC_ZSTD);
            out.extend_from_slice(&compressed);
            Ok(out)
        }
    }
}

/// Decompress data, auto-detecting algorithm from magic bytes.
/// Passes through uncompressed data unchanged.
pub fn decompress(data: &[u8]) -> Result<Vec<u8>> {
    if data.len() < 4 {
        return Ok(data.to_vec());
    }

    let magic = &data[..4];

    #[cfg(feature = "compression-lz4")]
    if magic == MAGIC_LZ4 {
        return crate::compression_lz4::decompress(&data[4..]);
    }

    #[cfg(feature = "compression-zstd")]
    if magic == MAGIC_ZSTD {
        return crate::compression_zstd::decompress(&data[4..]);
    }

    // Check if data has a known magic but feature is not enabled
    if magic == MAGIC_LZ4 {
        return Err(Error::Other(
            "data is lz4-compressed but compression-lz4 feature is not enabled".into(),
        ));
    }
    if magic == MAGIC_ZSTD {
        return Err(Error::Other(
            "data is zstd-compressed but compression-zstd feature is not enabled".into(),
        ));
    }

    Ok(data.to_vec())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn compress_none_passthrough() {
        let data = b"hello world";
        let result = compress(data, &CompressionAlgorithm::None).unwrap();
        assert_eq!(result, data);
    }

    #[cfg(feature = "compression-lz4")]
    #[test]
    fn compress_lz4_prepends_magic() {
        let data = b"some data to compress with lz4";
        let result = compress(data, &CompressionAlgorithm::Lz4).unwrap();
        assert_eq!(&result[..4], MAGIC_LZ4);
        assert_ne!(&result[4..], data);
    }

    #[cfg(feature = "compression-zstd")]
    #[test]
    fn compress_zstd_prepends_magic() {
        let data = b"some data to compress with zstd";
        let result = compress(data, &CompressionAlgorithm::Zstd).unwrap();
        assert_eq!(&result[..4], MAGIC_ZSTD);
        assert_ne!(&result[4..], data);
    }

    #[test]
    fn decompress_short_data_passthrough() {
        let data = b"abc";
        let result = decompress(data).unwrap();
        assert_eq!(result, data);
    }

    #[test]
    fn decompress_empty_passthrough() {
        let result = decompress(b"").unwrap();
        assert!(result.is_empty());
    }

    #[test]
    fn decompress_unknown_magic_passthrough() {
        let data = b"XXXX some random data here";
        let result = decompress(data).unwrap();
        assert_eq!(result, data);
    }

    #[cfg(feature = "compression-lz4")]
    #[test]
    fn lz4_roundtrip() {
        let data = b"roundtrip test data for lz4 compression";
        let compressed = compress(data, &CompressionAlgorithm::Lz4).unwrap();
        let decompressed = decompress(&compressed).unwrap();
        assert_eq!(decompressed, data);
    }

    #[cfg(feature = "compression-zstd")]
    #[test]
    fn zstd_roundtrip() {
        let data = b"roundtrip test data for zstd compression";
        let compressed = compress(data, &CompressionAlgorithm::Zstd).unwrap();
        let decompressed = decompress(&compressed).unwrap();
        assert_eq!(decompressed, data);
    }

    #[cfg(not(feature = "compression-lz4"))]
    #[test]
    fn decompress_lz4_magic_without_feature_errors() {
        let mut data = Vec::from(MAGIC_LZ4 as &[u8]);
        data.extend_from_slice(b"fake payload");
        let err = decompress(&data).unwrap_err();
        assert!(err.to_string().contains("compression-lz4 feature is not enabled"));
    }

    #[cfg(not(feature = "compression-zstd"))]
    #[test]
    fn decompress_zstd_magic_without_feature_errors() {
        let mut data = Vec::from(MAGIC_ZSTD as &[u8]);
        data.extend_from_slice(b"fake payload");
        let err = decompress(&data).unwrap_err();
        assert!(err.to_string().contains("compression-zstd feature is not enabled"));
    }

    #[test]
    fn compress_none_empty_data() {
        let result = compress(b"", &CompressionAlgorithm::None).unwrap();
        assert!(result.is_empty());
    }

    #[test]
    fn decompress_none_empty_roundtrip() {
        let compressed = compress(b"", &CompressionAlgorithm::None).unwrap();
        let decompressed = decompress(&compressed).unwrap();
        assert!(decompressed.is_empty());
    }

    #[test]
    fn decompress_exactly_4_bytes_non_magic_passthrough() {
        let data = b"ABCD";
        let result = decompress(data).unwrap();
        assert_eq!(result, data);
    }

    #[cfg(feature = "compression-lz4")]
    #[test]
    fn lz4_empty_data_roundtrip() {
        let compressed = compress(b"", &CompressionAlgorithm::Lz4).unwrap();
        assert_eq!(&compressed[..4], MAGIC_LZ4);
        let decompressed = decompress(&compressed).unwrap();
        assert!(decompressed.is_empty());
    }

    #[cfg(feature = "compression-zstd")]
    #[test]
    fn zstd_empty_data_roundtrip() {
        let compressed = compress(b"", &CompressionAlgorithm::Zstd).unwrap();
        assert_eq!(&compressed[..4], MAGIC_ZSTD);
        let decompressed = decompress(&compressed).unwrap();
        assert!(decompressed.is_empty());
    }
}
