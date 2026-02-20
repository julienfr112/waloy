use crate::error::Result;

pub fn compress(data: &[u8]) -> Result<Vec<u8>> {
    zstd::encode_all(data, 3).map_err(|e| crate::error::Error::Other(format!("zstd compress: {e}")))
}

pub fn decompress(data: &[u8]) -> Result<Vec<u8>> {
    zstd::decode_all(data).map_err(|e| crate::error::Error::Other(format!("zstd decompress: {e}")))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip() {
        let data = b"test data for zstd backend roundtrip verification";
        let compressed = compress(data).unwrap();
        let decompressed = decompress(&compressed).unwrap();
        assert_eq!(decompressed, data);
    }
}
