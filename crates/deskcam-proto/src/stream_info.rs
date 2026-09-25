//! `stream.bin`: the format the running app produces, read by the media source on activation.

use std::{fs, io, path::Path};

const MAGIC: u32 = 0x5453_4B44; // b"DKST"
const VERSION: u32 = 1;
const FILE_SIZE: usize = 32;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct StreamInfo {
    pub width: u32,
    pub height: u32,
    pub fps: u32,
}

pub const DEFAULT_STREAM: StreamInfo = StreamInfo { width: 1920, height: 1080, fps: 30 };

impl StreamInfo {
    pub fn validate(&self) -> bool {
        self.width % 2 == 0
            && self.height % 2 == 0
            && (320..=3840).contains(&self.width)
            && (180..=2160).contains(&self.height)
            && (1..=240).contains(&self.fps)
    }

    pub fn to_bytes(&self) -> [u8; FILE_SIZE] {
        let mut out = [0u8; FILE_SIZE];
        for (i, v) in [MAGIC, VERSION, self.width, self.height, self.fps].into_iter().enumerate() {
            out[i * 4..i * 4 + 4].copy_from_slice(&v.to_le_bytes());
        }
        out
    }

    /// Parses untrusted bytes; `None` unless the content is a valid, in-range record.
    pub fn from_bytes(bytes: &[u8]) -> Option<StreamInfo> {
        if bytes.len() != FILE_SIZE {
            return None;
        }
        let word = |i: usize| u32::from_le_bytes(bytes[i * 4..i * 4 + 4].try_into().unwrap());
        if word(0) != MAGIC || word(1) != VERSION {
            return None;
        }
        let info = StreamInfo { width: word(2), height: word(3), fps: word(4) };
        info.validate().then_some(info)
    }
}

/// Writes `path` via a temp file + rename so readers never see a partial record.
pub fn write_atomic(path: &Path, info: StreamInfo) -> io::Result<()> {
    let tmp = path.with_extension("bin.tmp");
    fs::write(&tmp, info.to_bytes())?;
    fs::rename(&tmp, path)
}

pub fn load_or_default(path: &Path) -> StreamInfo {
    fs::read(path).ok().and_then(|b| StreamInfo::from_bytes(&b)).unwrap_or(DEFAULT_STREAM)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trip() {
        let info = StreamInfo { width: 1280, height: 720, fps: 60 };
        assert_eq!(StreamInfo::from_bytes(&info.to_bytes()), Some(info));
    }

    #[test]
    fn rejects_bad_records() {
        let good = StreamInfo { width: 1280, height: 720, fps: 60 };
        let mut bad_magic = good.to_bytes();
        bad_magic[0] ^= 0xFF;
        assert_eq!(StreamInfo::from_bytes(&bad_magic), None);
        let odd = StreamInfo { width: 1281, ..good };
        assert_eq!(StreamInfo::from_bytes(&odd.to_bytes()), None);
        let zero_fps = StreamInfo { fps: 0, ..good };
        assert_eq!(StreamInfo::from_bytes(&zero_fps.to_bytes()), None);
        assert_eq!(StreamInfo::from_bytes(&good.to_bytes()[..31]), None);
    }
}
