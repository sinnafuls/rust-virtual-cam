//! The capture filter: graph state (Stopped/Paused/Running), clock and the single output pin.
//!
//! State shared by the filter, its pin and the delivery thread lives in [`Core`]. The pin refers
//! back to the filter through a weak reference, so the two COM objects never keep each other
//! alive.

use std::ffi::c_void;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex, MutexGuard};
use std::thread::JoinHandle;

use deskcam_proto::{DSHOW_CLSID, StreamInfo};
use windows::Win32::Foundation::{E_NOTIMPL, E_POINTER};
use windows::Win32::Media::DirectShow::{
    AM_FILTER_MISC_FLAGS_IS_SOURCE, FILTER_INFO, FILTER_STATE, IAMFilterMiscFlags, IAMFilterMiscFlags_Impl,
    IBaseFilter, IBaseFilter_Impl, IEnumPins, IFilterGraph, IMediaFilter_Impl, IMemAllocator, IMemInputPin, IPin,
    State_Paused, State_Running, State_Stopped, VFW_E_NO_CLOCK, VFW_E_NOT_FOUND,
};
use windows::Win32::Media::IReferenceClock;
use windows::Win32::System::Com::IPersist_Impl;
use windows_core::{ComObject, GUID, Interface, PCWSTR, PWSTR, Ref, implement};

use crate::enums::EnumPins;
use crate::format::{Format, VideoType};
use crate::pin::{OutputPin, PIN_ID};
use crate::{ObjGuard, stream};

/// An established connection to a downstream input pin.
pub(crate) struct Connection {
    pub peer: IPin,
    pub input: IMemInputPin,
    pub allocator: IMemAllocator,
}

pub(crate) struct State {
    pub filter_state: FILTER_STATE,
    pub clock: Option<IReferenceClock>,
    /// Reference time passed to `Run`; stream time = clock time - `t_start`.
    pub t_start: i64,
    /// The graph we belong to. Not reference counted: the graph owns us, not the other way round.
    pub graph: *mut c_void,
    pub graph_name: String,
    /// The type the pin connects with (or will, once connected).
    pub vtype: VideoType,
    /// Set when `vtype` changed on a live connection; the next sample carries the new type.
    pub type_changed: bool,
    pub conn: Option<Connection>,
    pub thread: Option<JoinHandle<()>>,
}

// DirectShow filters are called from arbitrary threads; all access to these COM pointers is
// serialized by the mutex in `Core`.
unsafe impl Send for State {}

pub(crate) struct Core {
    /// The mode advertised to consumers (from `stream.bin` when the filter was created).
    pub info: StreamInfo,
    pub st: Mutex<State>,
    pub wake: Condvar,
    pub frames: AtomicU64,
    /// Held for the whole of Stop/Pause/Run, including the thread join, so a concurrent Pause can
    /// never start a new stream while an old one is still shutting down. Never taken by the
    /// delivery thread's own calls (a downstream callback may Stop the graph from it).
    pub transition: Mutex<()>,
}

impl Core {
    pub fn lock(&self) -> MutexGuard<'_, State> {
        self.st.lock().unwrap_or_else(|p| p.into_inner())
    }

    fn transition_lock(&self) -> Option<MutexGuard<'_, ()>> {
        (!stream::on_delivery_thread()).then(|| self.transition.lock().unwrap_or_else(|p| p.into_inner()))
    }

    /// Stops streaming: decommits the allocator (unblocking `GetBuffer`), flushes downstream and
    /// joins the delivery thread, so no sample is delivered after this returns.
    ///
    /// Called on the delivery thread itself (a downstream callback stopped the graph), it cannot
    /// join itself: the thread exits once the callback returns and sees `Stopped`, and the next
    /// Pause joins it.
    pub fn stop(&self) {
        let _transition = self.transition_lock();
        self.stop_locked();
    }

    fn stop_locked(&self) {
        let own_thread = stream::on_delivery_thread();
        let (thread, peer, allocator) = {
            let mut st = self.lock();
            st.filter_state = State_Stopped;
            let peer = st.conn.as_ref().map(|c| c.peer.clone());
            let allocator = st.conn.as_ref().map(|c| c.allocator.clone());
            (if own_thread { None } else { st.thread.take() }, peer, allocator)
        };
        self.wake.notify_all();
        unsafe {
            if let Some(a) = &allocator {
                let _ = a.Decommit();
            }
            if let Some(p) = &peer {
                let _ = p.BeginFlush();
            }
        }
        if let Some(t) = thread {
            let _ = t.join();
        }
        if let Some(p) = &peer {
            let _ = unsafe { p.EndFlush() };
        }
    }

    /// Stopped → Paused: commits the allocator and starts a delivery thread unless one is still
    /// usable. Caller holds `transition` (or is the delivery thread itself).
    fn pause_locked(&self, core: &Arc<Core>) -> windows_core::Result<()> {
        // A thread left behind by a Stop from its own callback has seen `Stopped` and is exiting;
        // join it. If we *are* that thread, keep it: it carries on once the callback returns.
        let leftover = {
            let mut st = self.lock();
            let foreign = st.thread.as_ref().is_some_and(|t| t.thread().id() != std::thread::current().id());
            if st.filter_state == State_Stopped && foreign { st.thread.take() } else { None }
        };
        if let Some(t) = leftover {
            let _ = t.join();
        }
        let mut st = self.lock();
        if st.filter_state == State_Stopped
            && let Some(conn) = &st.conn
        {
            unsafe { conn.allocator.Commit()? };
            if st.thread.is_none() {
                st.thread = Some(stream::spawn(core.clone()));
            }
        }
        st.filter_state = State_Paused;
        drop(st);
        self.wake.notify_all();
        Ok(())
    }
}

#[implement(IBaseFilter, IAMFilterMiscFlags)]
pub struct Filter {
    core: Arc<Core>,
    pin: ComObject<OutputPin>,
    _guard: ObjGuard,
}

pub fn create(info: StreamInfo) -> windows_core::Result<ComObject<Filter>> {
    let core = Arc::new(Core {
        info,
        st: Mutex::new(State {
            filter_state: State_Stopped,
            clock: None,
            t_start: 0,
            graph: std::ptr::null_mut(),
            graph_name: String::new(),
            vtype: VideoType::new(Format::Nv12, info),
            type_changed: false,
            conn: None,
            thread: None,
        }),
        wake: Condvar::new(),
        frames: AtomicU64::new(0),
        transition: Mutex::new(()),
    });
    let pin = ComObject::new(OutputPin::new(core.clone()));
    let filter = ComObject::new(Filter { core, pin: pin.clone(), _guard: ObjGuard::new() });
    pin.set_filter(filter.to_interface::<IBaseFilter>().downgrade()?);
    Ok(filter)
}

impl Filter {
    /// Samples handed to the downstream pin so far.
    pub fn frames_delivered(&self) -> u64 {
        self.core.frames.load(Ordering::Relaxed)
    }
}

impl Drop for Filter {
    fn drop(&mut self) {
        // Only reached with a running graph if the app released us without stopping it.
        if self.core.lock().thread.is_some() {
            self.core.stop();
        }
        // An app that releases us without disconnecting would leave a cycle: downstream pin →
        // our pin → Core → downstream pin. Break it so both pins and the DLL can go away.
        let conn = self.core.lock().conn.take();
        if let Some(conn) = conn {
            let _ = unsafe { conn.peer.Disconnect() };
        }
    }
}

fn copy_name(dst: &mut [u16; 128], name: &str) {
    for (d, s) in dst.iter_mut().zip(name.encode_utf16().take(127).chain(std::iter::repeat(0))) {
        *d = s;
    }
}

impl IPersist_Impl for Filter_Impl {
    fn GetClassID(&self) -> windows_core::Result<GUID> {
        Ok(DSHOW_CLSID)
    }
}

impl IMediaFilter_Impl for Filter_Impl {
    fn Stop(&self) -> windows_core::Result<()> {
        let _transition = self.core.transition_lock();
        if self.core.lock().filter_state != State_Stopped {
            self.core.stop_locked();
        }
        Ok(())
    }

    fn Pause(&self) -> windows_core::Result<()> {
        let _transition = self.core.transition_lock();
        self.core.pause_locked(&self.core)
    }

    fn Run(&self, tstart: i64) -> windows_core::Result<()> {
        let _transition = self.core.transition_lock();
        if self.core.lock().filter_state == State_Stopped {
            self.core.pause_locked(&self.core)?;
        }
        let mut st = self.core.lock();
        st.t_start = tstart;
        st.filter_state = State_Running;
        drop(st);
        self.core.wake.notify_all();
        Ok(())
    }

    fn GetState(&self, _timeout_ms: u32) -> windows_core::Result<FILTER_STATE> {
        Ok(self.core.lock().filter_state)
    }

    fn SetSyncSource(&self, clock: Ref<IReferenceClock>) -> windows_core::Result<()> {
        self.core.lock().clock = clock.cloned();
        Ok(())
    }

    fn GetSyncSource(&self) -> windows_core::Result<IReferenceClock> {
        self.core.lock().clock.clone().ok_or_else(|| VFW_E_NO_CLOCK.into())
    }
}

impl IBaseFilter_Impl for Filter_Impl {
    fn EnumPins(&self) -> windows_core::Result<IEnumPins> {
        Ok(EnumPins::new(self.pin.to_interface()).into())
    }

    fn FindPin(&self, id: &PCWSTR) -> windows_core::Result<IPin> {
        if id.is_null() {
            return Err(E_POINTER.into());
        }
        match unsafe { id.to_string() } {
            Ok(s) if s == PIN_ID => Ok(self.pin.to_interface()),
            _ => Err(VFW_E_NOT_FOUND.into()),
        }
    }

    fn QueryFilterInfo(&self, info: *mut FILTER_INFO) -> windows_core::Result<()> {
        if info.is_null() {
            return Err(E_POINTER.into());
        }
        let st = self.core.lock();
        let mut out = FILTER_INFO::default();
        copy_name(&mut out.achName, &st.graph_name);
        let graph = unsafe { IFilterGraph::from_raw_borrowed(&st.graph) }.cloned();
        out.pGraph = std::mem::ManuallyDrop::new(graph);
        unsafe { info.write(out) };
        Ok(())
    }

    fn JoinFilterGraph(&self, graph: Ref<IFilterGraph>, name: &PCWSTR) -> windows_core::Result<()> {
        let mut st = self.core.lock();
        st.graph = graph.as_ref().map_or(std::ptr::null_mut(), |g| g.as_raw());
        st.graph_name = if name.is_null() { String::new() } else { unsafe { name.to_string() }.unwrap_or_default() };
        Ok(())
    }

    fn QueryVendorInfo(&self) -> windows_core::Result<PWSTR> {
        Err(E_NOTIMPL.into())
    }
}

impl IAMFilterMiscFlags_Impl for Filter_Impl {
    fn GetMiscFlags(&self) -> u32 {
        AM_FILTER_MISC_FLAGS_IS_SOURCE.0 as u32
    }
}

/// Whether the graph is currently running (as opposed to paused) — timestamps are only
/// meaningful once `Run` supplied a start time.
pub(crate) fn is_running(st: &State) -> bool {
    st.filter_state == State_Running
}
