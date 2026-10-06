//! Hearing that Windows is ending the session, in the owner of the browsers.
//!
//! **The owner hears nothing else.** It runs detached from any console
//! (`spawn`), so it is sent none of the console's events, and Windows tells a
//! process that the user is signing out or the machine is shutting down
//! through the one channel left: `WM_QUERYENDSESSION` and `WM_ENDSESSION`,
//! sent to every top-level window. A process with no window is simply ended
//! when the time comes, and the owner then took its browsers down with it
//! through their job, mid-write: whatever the browser had not yet put in its
//! profile, the cookies Instagram rotated a moment before among it, was lost.
//!
//! So the owner keeps one window, never shown, on a thread of its own that
//! does nothing but answer those two messages. To the question it says yes,
//! the session may end; when it ends, it wakes the owner's loop, which closes
//! every browser the way it closes them on its way out, and the window holds
//! the message until that is done, or [`PATIENCE`] has passed: Windows ends a
//! process whose window has not answered after about five seconds.
//!
//! **A window only for messages would not do**: such windows are not sent
//! broadcasts, and the end of a session is one.

use std::sync::{Arc, Condvar, Mutex, OnceLock};
use std::time::Duration;

use tokio::sync::Notify;
use windows_sys::Win32::Foundation::{HWND, LPARAM, LRESULT, WPARAM};
use windows_sys::Win32::System::LibraryLoader::GetModuleHandleW;
use windows_sys::Win32::UI::WindowsAndMessaging::{
    CreateWindowExW, DefWindowProcW, DispatchMessageW, GWLP_USERDATA, GetMessageW,
    GetWindowLongPtrW, MSG, RegisterClassW, SetWindowLongPtrW, WM_ENDSESSION, WM_QUERYENDSESSION,
    WNDCLASSW, WS_OVERLAPPED,
};

/// How long the window holds the end of the session for the browsers to
/// close: inside the five seconds or so Windows waits for an answer.
const PATIENCE: Duration = Duration::from_millis(4_500);

/// The window's class.
const CLASS: &str = "snob-browser-owner";

/// What one window shares with the code it wakes.
#[derive(Default)]
pub(super) struct Ending {
    /// Poked when the session ends.
    heard: Notify,
    /// Set once the browsers are closed, for the window to answer.
    closed: Mutex<bool>,
    said: Condvar,
}

impl Ending {
    /// Waits for the session to end.
    pub(super) async fn heard(&self) {
        self.heard.notified().await;
    }

    /// Says the browsers are closed: the window may let the session end.
    fn closed(&self) {
        *self.closed.lock().unwrap_or_else(|e| e.into_inner()) = true;
        self.said.notify_all();
    }
}

/// The owner's window, made the first time it is asked for.
static OWNERS: OnceLock<Option<Arc<Ending>>> = OnceLock::new();

/// What the owner's loop waits on to hear that the session is ending; `None`
/// when no window could be made, which leaves the owner as it was without it.
pub(super) fn listen() -> Option<Arc<Ending>> {
    OWNERS
        .get_or_init(|| window().map(|(_, ending)| ending))
        .clone()
}

/// Says the owner's browsers are closed, for its window to let the session
/// end.
pub(super) fn closed() {
    if let Some(Some(ending)) = OWNERS.get() {
        ending.closed();
    }
}

fn wide(text: &str) -> Vec<u16> {
    text.encode_utf16().chain(Some(0)).collect()
}

/// A window of its own, on a thread of its own, and what it wakes: the window
/// as a number, since a handle does not cross threads.
fn window() -> Option<(isize, Arc<Ending>)> {
    let ending = Arc::new(Ending::default());
    let shared = Arc::clone(&ending);
    let (made, window) = std::sync::mpsc::channel();
    let spawned = std::thread::Builder::new()
        .name("end-of-session".into())
        .spawn(move || {
            let made_one = make_window(shared);
            let _ = made.send(made_one);
            if made_one.is_some() {
                pump();
            }
        });
    let Some(handle) = spawned.ok().and_then(|_| window.recv().ok().flatten()) else {
        tracing::debug!("no window to hear the end of the session in");
        return None;
    };
    Some((handle, ending))
}

/// Registers the class, once for the process.
fn class(instance: windows_sys::Win32::Foundation::HINSTANCE) -> bool {
    static REGISTERED: OnceLock<bool> = OnceLock::new();
    *REGISTERED.get_or_init(|| {
        let class = wide(CLASS);
        let registered = WNDCLASSW {
            lpfnWndProc: Some(answer),
            hInstance: instance,
            lpszClassName: class.as_ptr(),
            ..WNDCLASSW::default()
        };
        // SAFETY: the structure is complete, and its class name,
        // NUL-terminated, outlives the call, which copies it.
        let atom = unsafe { RegisterClassW(&raw const registered) };
        if atom == 0 {
            tracing::debug!(error = %std::io::Error::last_os_error(), "could not register the window class");
        }
        atom != 0
    })
}

/// Makes the window on this thread, with `ending` where its procedure finds
/// it, for the life of the process.
fn make_window(ending: Arc<Ending>) -> Option<isize> {
    // SAFETY: a null name asks for this executable's own module handle.
    let instance = unsafe { GetModuleHandleW(std::ptr::null()) };
    if !class(instance) {
        return None;
    }
    let class = wide(CLASS);
    // SAFETY: the class is registered; every other argument is a plain value
    // or null, and the window is never shown.
    let window = unsafe {
        CreateWindowExW(
            0,
            class.as_ptr(),
            class.as_ptr(),
            WS_OVERLAPPED,
            0,
            0,
            0,
            0,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            instance,
            std::ptr::null(),
        )
    };
    if window.is_null() {
        tracing::debug!(error = %std::io::Error::last_os_error(), "could not make the window");
        return None;
    }
    // Kept by the window for as long as the process lives: one reference,
    // never given back.
    let kept = Arc::into_raw(ending);
    // SAFETY: `window` was made above, and the value stored is a pointer the
    // procedure reads back as what it is.
    unsafe { SetWindowLongPtrW(window, GWLP_USERDATA, kept as isize) };
    Some(window as isize)
}

/// The window's thread: messages, for as long as the process lives.
fn pump() {
    // SAFETY: all-zero is a valid `MSG` to be filled.
    let mut message: MSG = unsafe { std::mem::zeroed() };
    // SAFETY: `message` is valid for the call to write; a null window reads
    // every message of this thread, whose only window is the one above.
    while unsafe { GetMessageW(&raw mut message, std::ptr::null_mut(), 0, 0) } > 0 {
        // SAFETY: a message `GetMessageW` just filled.
        unsafe { DispatchMessageW(&raw const message) };
    }
}

/// What the window says to what it is sent.
unsafe extern "system" fn answer(
    window: HWND,
    message: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    match message {
        // Yes, the session may end: nothing here is worth holding it for.
        WM_QUERYENDSESSION => 1,
        WM_ENDSESSION => {
            // SAFETY: reads back what `make_window` stored on this window.
            let kept = unsafe { GetWindowLongPtrW(window, GWLP_USERDATA) } as *const Ending;
            if wparam != 0 && !kept.is_null() {
                // SAFETY: the pointer is an `Arc`'s, kept alive for the life
                // of the process by the reference the window holds.
                let ending = unsafe { &*kept };
                tracing::info!("the session is ending; closing the browsers");
                ending.heard.notify_one();
                let done = ending.closed.lock().unwrap_or_else(|e| e.into_inner());
                let _ = ending
                    .said
                    .wait_timeout_while(done, PATIENCE, |done| !*done);
            }
            0
        }
        // SAFETY: every other message is the default procedure's, with the
        // arguments it was sent.
        _ => unsafe { DefWindowProcW(window, message, wparam, lparam) },
    }
}

#[cfg(test)]
mod tests {
    use windows_sys::Win32::UI::WindowsAndMessaging::SendMessageW;

    use super::*;

    /// The window says yes to the question, wakes what waits on it when the
    /// session ends, and holds the end until the browsers are closed. A
    /// window of the test's own, not the owner's, so an owner another test
    /// runs is not woken by it.
    #[tokio::test]
    async fn the_end_of_the_session_is_heard_and_waited_for() {
        let (found, ending) = window().expect("a window");

        let asked = tokio::task::spawn_blocking(move || {
            // SAFETY: a window of this process, sent a message it answers.
            unsafe { SendMessageW(found as HWND, WM_QUERYENDSESSION, 0, 0) }
        })
        .await
        .unwrap();
        assert_eq!(asked, 1, "the session may end");

        let ended = tokio::task::spawn_blocking(move || {
            let started = std::time::Instant::now();
            // SAFETY: as above.
            unsafe { SendMessageW(found as HWND, WM_ENDSESSION, 1, 0) };
            started.elapsed()
        });
        tokio::time::timeout(Duration::from_secs(5), ending.heard())
            .await
            .expect("what waits on it is woken");
        ending.closed();
        let held = ended.await.unwrap();
        assert!(
            held < PATIENCE,
            "held only until the browsers closed: {held:?}"
        );
    }
}
