use serde::{Deserialize, Serialize};

pub const SHIM_MAGIC: &[u8; 8] = b"SBNSHM1\0";
pub const TRAILER_SIZE: usize = 8 /* magic */ + 8 /* archive_len */ + 8 /* metadata_len */;
pub const MARKER_FILE: &str = ".sidebundle-shim.json";

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ShimMetadata {
    pub bundle_name: String,
    pub entry_name: String,
    pub default_extract_path: String,
    pub archive_sha256: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ShimTrailer {
    pub archive_len: u64,
    pub metadata_len: u64,
}

impl ShimTrailer {
    pub fn to_bytes(self) -> [u8; TRAILER_SIZE] {
        let mut buf = [0u8; TRAILER_SIZE];
        buf[..8].copy_from_slice(SHIM_MAGIC);
        buf[8..16].copy_from_slice(&self.archive_len.to_le_bytes());
        buf[16..24].copy_from_slice(&self.metadata_len.to_le_bytes());
        buf
    }

    pub fn from_bytes(buf: &[u8]) -> Option<Self> {
        if buf.len() != TRAILER_SIZE {
            return None;
        }
        if &buf[..8] != SHIM_MAGIC {
            return None;
        }
        let mut archive_len = [0u8; 8];
        archive_len.copy_from_slice(&buf[8..16]);
        let mut metadata_len = [0u8; 8];
        metadata_len.copy_from_slice(&buf[16..24]);
        Some(Self {
            archive_len: u64::from_le_bytes(archive_len),
            metadata_len: u64::from_le_bytes(metadata_len),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn trailer_round_trip() {
        let trailer = ShimTrailer {
            archive_len: 123,
            metadata_len: 456,
        };
        let bytes = trailer.to_bytes();
        assert_eq!(ShimTrailer::from_bytes(&bytes), Some(trailer));
    }
}
