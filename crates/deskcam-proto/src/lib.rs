//! Contract shared by `deskcam.exe` (frame writer) and `deskcam_source.dll`
//! (Media Foundation source loaded by the Windows Camera Frame Server).

pub mod color;
pub mod layout;
pub mod paths;
pub mod section;
pub mod stream_info;

use windows_core::GUID;

/// COM class id of the media source. Never change once installed.
pub const CLSID: GUID = GUID::from_u128(0xbbb3f03b_377b_4a7d_a4ce_9aee0b5a6441);
/// String form of [`CLSID`] as passed to `MFCreateVirtualCamera`.
pub const CLSID_STR: &str = "{BBB3F03B-377B-4A7D-A4CE-9AEE0B5A6441}";

pub use layout::{FrameRing, ReadOutcome, now_ms};
pub use section::Section;
pub use stream_info::{DEFAULT_STREAM, StreamInfo};
