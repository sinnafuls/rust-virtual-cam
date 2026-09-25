//! Contract shared by `deskcam.exe` (frame writer) and `deskcam_source.dll`
//! (Media Foundation source loaded by the Windows Camera Frame Server).

pub mod color;
pub mod com;
pub mod convert;
pub mod layout;
pub mod os;
pub mod paths;
pub mod section;
pub mod stream_info;

use windows_core::GUID;

/// COM class id of the media source. Never change once installed.
pub const CLSID: GUID = GUID::from_u128(0xbbb3f03b_377b_4a7d_a4ce_9aee0b5a6441);
/// String form of [`CLSID`] as passed to `MFCreateVirtualCamera`.
pub const CLSID_STR: &str = "{BBB3F03B-377B-4A7D-A4CE-9AEE0B5A6441}";

/// COM class id of the DirectShow capture filter (`deskcam_dshow.dll`, Windows 10 backend).
/// Never change once installed.
pub const DSHOW_CLSID: GUID = GUID::from_u128(0x566d2a35_2ff9_4009_9c41_04054e6a4bd3);
/// String form of [`DSHOW_CLSID`].
pub const DSHOW_CLSID_STR: &str = "{566D2A35-2FF9-4009-9C41-04054E6A4BD3}";

pub use layout::{FrameRing, Namespace, ReadOutcome, now_ms};
pub use section::Section;
pub use stream_info::{DEFAULT_STREAM, StreamInfo};
