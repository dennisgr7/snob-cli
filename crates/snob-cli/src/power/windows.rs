//! A Windows power request. See the module above.

use windows_sys::Win32::Foundation::{CloseHandle, HANDLE, INVALID_HANDLE_VALUE};
use windows_sys::Win32::System::Power::{
    PowerClearRequest, PowerCreateRequest, PowerRequestSystemRequired, PowerSetRequest,
};
use windows_sys::Win32::System::Threading::{
    POWER_REQUEST_CONTEXT_SIMPLE_STRING, REASON_CONTEXT, REASON_CONTEXT_0,
};

/// `POWER_REQUEST_CONTEXT_VERSION`, which `windows-sys` keeps under a feature
/// of its own for this one zero.
const POWER_REQUEST_CONTEXT_VERSION: u32 = 0;

/// A power request this process made and has set: cleared and closed on drop.
pub(super) struct Request(HANDLE);

// SAFETY: a power request handle is a kernel object handle, usable from any
// thread of the process that owns it; nothing here is tied to the thread that
// made it.
unsafe impl Send for Request {}

pub(super) fn request(reason: &'static str) -> Option<Request> {
    let mut wide: Vec<u16> = reason.encode_utf16().chain(Some(0)).collect();
    let context = REASON_CONTEXT {
        Version: POWER_REQUEST_CONTEXT_VERSION,
        Flags: POWER_REQUEST_CONTEXT_SIMPLE_STRING,
        Reason: REASON_CONTEXT_0 {
            SimpleReasonString: wide.as_mut_ptr(),
        },
    };
    // SAFETY: `context` is a valid structure whose string is NUL-terminated
    // and outlives the call, which copies it.
    let handle = unsafe { PowerCreateRequest(&context) };
    if handle.is_null() || handle == INVALID_HANDLE_VALUE {
        tracing::debug!(error = %std::io::Error::last_os_error(), "no power request could be made");
        return None;
    }
    // SAFETY: `handle` is the power request just created above.
    if unsafe { PowerSetRequest(handle, PowerRequestSystemRequired) } == 0 {
        tracing::debug!(error = %std::io::Error::last_os_error(), "the power request could not be set");
        // SAFETY: the handle is ours and is not used after this.
        unsafe { CloseHandle(handle) };
        return None;
    }
    Some(Request(handle))
}

impl Drop for Request {
    fn drop(&mut self) {
        // SAFETY: the handle is the request set in `request`, owned by this
        // value and closed exactly once, here.
        unsafe {
            PowerClearRequest(self.0, PowerRequestSystemRequired);
            CloseHandle(self.0);
        }
    }
}
