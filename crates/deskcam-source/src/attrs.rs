//! Forwards `IMFAttributes` on our COM objects to an inner store created by `MFCreateAttributes`.

use windows::Win32::Media::MediaFoundation::{IMFAttributes, MFCreateAttributes};
use windows_core::Result;

pub fn new_store() -> Result<IMFAttributes> {
    let mut store = None;
    unsafe { MFCreateAttributes(&mut store, 8)? };
    store.ok_or_else(|| windows_core::Error::from(windows::Win32::Foundation::E_OUTOFMEMORY))
}

/// `forward_attributes!(Foo_Impl, attrs)` implements `IMFAttributes_Impl` by delegating every
/// method to `self.attrs: IMFAttributes`.
macro_rules! forward_attributes {
    ($impl_ty:ty, $field:ident) => {
        #[allow(non_snake_case)]
        impl windows::Win32::Media::MediaFoundation::IMFAttributes_Impl for $impl_ty {
            fn GetItem(
                &self,
                key: *const windows_core::GUID,
                value: *mut windows::Win32::System::Com::StructuredStorage::PROPVARIANT,
            ) -> windows_core::Result<()> {
                unsafe { self.$field.GetItem(key, (!value.is_null()).then_some(value)) }
            }
            fn GetItemType(
                &self,
                key: *const windows_core::GUID,
            ) -> windows_core::Result<windows::Win32::Media::MediaFoundation::MF_ATTRIBUTE_TYPE> {
                unsafe { self.$field.GetItemType(key) }
            }
            fn CompareItem(
                &self,
                key: *const windows_core::GUID,
                value: *const windows::Win32::System::Com::StructuredStorage::PROPVARIANT,
            ) -> windows_core::Result<windows_core::BOOL> {
                unsafe { self.$field.CompareItem(key, value) }
            }
            fn Compare(
                &self,
                theirs: windows_core::Ref<windows::Win32::Media::MediaFoundation::IMFAttributes>,
                match_type: windows::Win32::Media::MediaFoundation::MF_ATTRIBUTES_MATCH_TYPE,
            ) -> windows_core::Result<windows_core::BOOL> {
                unsafe { self.$field.Compare(theirs.as_ref(), match_type) }
            }
            fn GetUINT32(&self, key: *const windows_core::GUID) -> windows_core::Result<u32> {
                unsafe { self.$field.GetUINT32(key) }
            }
            fn GetUINT64(&self, key: *const windows_core::GUID) -> windows_core::Result<u64> {
                unsafe { self.$field.GetUINT64(key) }
            }
            fn GetDouble(&self, key: *const windows_core::GUID) -> windows_core::Result<f64> {
                unsafe { self.$field.GetDouble(key) }
            }
            fn GetGUID(&self, key: *const windows_core::GUID) -> windows_core::Result<windows_core::GUID> {
                unsafe { self.$field.GetGUID(key) }
            }
            fn GetStringLength(&self, key: *const windows_core::GUID) -> windows_core::Result<u32> {
                unsafe { self.$field.GetStringLength(key) }
            }
            fn GetString(
                &self,
                key: *const windows_core::GUID,
                value: windows_core::PWSTR,
                size: u32,
                length: *mut u32,
            ) -> windows_core::Result<()> {
                unsafe {
                    (windows_core::Interface::vtable(&self.$field).GetString)(
                        windows_core::Interface::as_raw(&self.$field),
                        key,
                        value,
                        size,
                        length,
                    )
                    .ok()
                }
            }
            fn GetAllocatedString(
                &self,
                key: *const windows_core::GUID,
                value: *mut windows_core::PWSTR,
                length: *mut u32,
            ) -> windows_core::Result<()> {
                unsafe { self.$field.GetAllocatedString(key, value, length) }
            }
            fn GetBlobSize(&self, key: *const windows_core::GUID) -> windows_core::Result<u32> {
                unsafe { self.$field.GetBlobSize(key) }
            }
            fn GetBlob(
                &self,
                key: *const windows_core::GUID,
                buf: *mut u8,
                size: u32,
                blob_size: *mut u32,
            ) -> windows_core::Result<()> {
                unsafe {
                    (windows_core::Interface::vtable(&self.$field).GetBlob)(
                        windows_core::Interface::as_raw(&self.$field),
                        key,
                        buf,
                        size,
                        blob_size,
                    )
                    .ok()
                }
            }
            fn GetAllocatedBlob(
                &self,
                key: *const windows_core::GUID,
                buf: *mut *mut u8,
                size: *mut u32,
            ) -> windows_core::Result<()> {
                unsafe { self.$field.GetAllocatedBlob(key, buf, size) }
            }
            fn GetUnknown(
                &self,
                key: *const windows_core::GUID,
                riid: *const windows_core::GUID,
                ppv: *mut *mut core::ffi::c_void,
            ) -> windows_core::Result<()> {
                unsafe {
                    (windows_core::Interface::vtable(&self.$field).GetUnknown)(
                        windows_core::Interface::as_raw(&self.$field),
                        key,
                        riid,
                        ppv,
                    )
                    .ok()
                }
            }
            fn SetItem(
                &self,
                key: *const windows_core::GUID,
                value: *const windows::Win32::System::Com::StructuredStorage::PROPVARIANT,
            ) -> windows_core::Result<()> {
                unsafe { self.$field.SetItem(key, value) }
            }
            fn DeleteItem(&self, key: *const windows_core::GUID) -> windows_core::Result<()> {
                unsafe { self.$field.DeleteItem(key) }
            }
            fn DeleteAllItems(&self) -> windows_core::Result<()> {
                unsafe { self.$field.DeleteAllItems() }
            }
            fn SetUINT32(&self, key: *const windows_core::GUID, value: u32) -> windows_core::Result<()> {
                unsafe { self.$field.SetUINT32(key, value) }
            }
            fn SetUINT64(&self, key: *const windows_core::GUID, value: u64) -> windows_core::Result<()> {
                unsafe { self.$field.SetUINT64(key, value) }
            }
            fn SetDouble(&self, key: *const windows_core::GUID, value: f64) -> windows_core::Result<()> {
                unsafe { self.$field.SetDouble(key, value) }
            }
            fn SetGUID(
                &self,
                key: *const windows_core::GUID,
                value: *const windows_core::GUID,
            ) -> windows_core::Result<()> {
                unsafe { self.$field.SetGUID(key, value) }
            }
            fn SetString(
                &self,
                key: *const windows_core::GUID,
                value: &windows_core::PCWSTR,
            ) -> windows_core::Result<()> {
                unsafe { self.$field.SetString(key, *value) }
            }
            fn SetBlob(&self, key: *const windows_core::GUID, buf: *const u8, size: u32) -> windows_core::Result<()> {
                let slice = if buf.is_null() || size == 0 {
                    &[][..]
                } else {
                    unsafe { core::slice::from_raw_parts(buf, size as usize) }
                };
                unsafe { self.$field.SetBlob(key, slice) }
            }
            fn SetUnknown(
                &self,
                key: *const windows_core::GUID,
                unknown: windows_core::Ref<windows_core::IUnknown>,
            ) -> windows_core::Result<()> {
                unsafe { self.$field.SetUnknown(key, unknown.as_ref()) }
            }
            fn LockStore(&self) -> windows_core::Result<()> {
                unsafe { self.$field.LockStore() }
            }
            fn UnlockStore(&self) -> windows_core::Result<()> {
                unsafe { self.$field.UnlockStore() }
            }
            fn GetCount(&self) -> windows_core::Result<u32> {
                unsafe { self.$field.GetCount() }
            }
            fn GetItemByIndex(
                &self,
                index: u32,
                key: *mut windows_core::GUID,
                value: *mut windows::Win32::System::Com::StructuredStorage::PROPVARIANT,
            ) -> windows_core::Result<()> {
                unsafe { self.$field.GetItemByIndex(index, key, (!value.is_null()).then_some(value)) }
            }
            fn CopyAllItems(
                &self,
                dest: windows_core::Ref<windows::Win32::Media::MediaFoundation::IMFAttributes>,
            ) -> windows_core::Result<()> {
                unsafe { self.$field.CopyAllItems(dest.as_ref()) }
            }
        }
    };
}

pub(crate) use forward_attributes;
