//! How the camera is exposed to apps:
//!
//! - **Media Foundation** (Windows 11): `MFCreateVirtualCamera` registers `deskcam_source.dll`
//!   with the Frame Server, which creates the `Global\` frame section.
//! - **DirectShow** (Windows 10): `deskcam_dshow.dll` is registered once by `install.ps1` as a
//!   video capture device and loaded inside each consumer app. The app owns the `Local\` frame
//!   section here, so it exists before any consumer opens it. When `deskcam_source.dll` is
//!   registered too, it adds a software camera device that the Frame Server serves to Media
//!   Foundation and WinRT apps (AMD Privacy View, the Camera app); that media source creates the
//!   `Global\` section, exactly as on Windows 11, so the worker feeds both sections.
//!
//! Either way the worker writes frames into the same `FrameRing` and captures only while a reader
//! heartbeat is present.

use std::fmt;

use deskcam_proto::{Namespace, Section, StreamInfo, com, os};

use crate::config::{BackendSel, Config};
use crate::log::log;
use crate::vcam::VirtualCamera;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Kind {
    MediaFoundation,
    DirectShow,
}

impl fmt::Display for Kind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Kind::MediaFoundation => "Media Foundation virtual camera",
            Kind::DirectShow => "DirectShow capture filter",
        })
    }
}

pub fn resolve(sel: BackendSel) -> Kind {
    match sel {
        BackendSel::MediaFoundation => Kind::MediaFoundation,
        BackendSel::DirectShow => Kind::DirectShow,
        BackendSel::Auto if os::has_mf_virtual_camera() => Kind::MediaFoundation,
        BackendSel::Auto => Kind::DirectShow,
    }
}

/// Each variant owns what keeps the camera alive; dropping the backend removes it.
pub enum Backend {
    MediaFoundation { _camera: VirtualCamera },
    /// Holds the section open for the app's lifetime so consumers can always find it.
    DirectShow { _section: Section },
}

impl Backend {
    pub fn start(cfg: &Config, info: StreamInfo) -> Result<Backend, String> {
        match resolve(cfg.backend) {
            Kind::MediaFoundation => VirtualCamera::create(&cfg.name).map(|c| Backend::MediaFoundation { _camera: c }),
            Kind::DirectShow => {
                match com::registered_dshow_name() {
                    None => log!("warning: the DirectShow camera is not registered; run scripts\\install.ps1 as administrator"),
                    Some(name) if name != cfg.name => log!(
                        "note: the camera is registered as '{name}'; re-run install.ps1 to apply name = {}",
                        cfg.name
                    ),
                    Some(_) => {}
                }
                Section::create_local(info.width, info.height)
                    .map(|s| Backend::DirectShow { _section: s })
                    .ok_or_else(|| "cannot create the shared frame section".into())
            }
        }
    }

    pub fn kind(&self) -> Kind {
        match self {
            Backend::MediaFoundation { .. } => Kind::MediaFoundation,
            Backend::DirectShow { .. } => Kind::DirectShow,
        }
    }

    /// Frame sections the worker writes, at most two. The global one exists only while the Frame
    /// Server has the media source loaded.
    pub fn namespaces(&self) -> [Option<Namespace>; 2] {
        match self {
            Backend::MediaFoundation { .. } => [Some(Namespace::Global), None],
            Backend::DirectShow { .. } => [Some(Namespace::Local), Some(Namespace::Global)],
        }
    }

    /// Opens a frame section for writing; `None` until it exists.
    pub fn open_section(ns: Namespace, info: StreamInfo) -> Option<Section> {
        match ns {
            Namespace::Global => Section::open(info.width, info.height, true),
            Namespace::Local => Section::open_local(info.width, info.height, true),
        }
    }
}
