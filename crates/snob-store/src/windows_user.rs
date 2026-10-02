//! Who a Windows process runs as, and an ACL that lets that user alone in.
//!
//! Two things keep other accounts on the machine out: the data directory's
//! DACL (`paths`) and the owner of the browsers' pipe (`snob-cli`'s
//! `owner::socket`). Both are built from this, so the unsafe code that reads a
//! token and lays out an ACL is written once.

use std::io;

use windows_sys::Win32::Foundation::{CloseHandle, HANDLE};
use windows_sys::Win32::Security::{
    ACCESS_ALLOWED_ACE, ACE_FLAGS, ACL, ACL_REVISION, AddAccessAllowedAceEx, GetLengthSid,
    GetTokenInformation, InitializeAcl, PSID, TOKEN_QUERY, TOKEN_USER, TokenUser,
};
use windows_sys::Win32::Storage::FileSystem::FILE_ALL_ACCESS;
use windows_sys::Win32::System::Threading::{GetCurrentProcess, OpenProcessToken};

/// The user a process runs as.
///
/// The SID points into the buffer, which is why the two are kept together:
/// the pointer alone would dangle the moment the buffer dropped, and would
/// still work often enough to look correct. The buffer is `u64`s so that the
/// `TOKEN_USER` at its start is aligned.
pub struct User {
    buffer: Vec<u64>,
}

impl User {
    /// Who `process` runs as.
    ///
    /// # Safety
    ///
    /// `process` is a process handle, open for the call, with
    /// `PROCESS_QUERY_LIMITED_INFORMATION`.
    pub unsafe fn of(process: HANDLE) -> io::Result<Self> {
        let mut token: HANDLE = std::ptr::null_mut();
        // SAFETY: a process handle the caller vouches for, and an
        // out-parameter.
        if unsafe { OpenProcessToken(process, TOKEN_QUERY, &mut token) } == 0 {
            return Err(io::Error::last_os_error());
        }
        // Asked for its size first, which is the documented two-call shape.
        let mut needed: u32 = 0;
        // SAFETY: a null buffer with a zero length is how the size is asked
        // for; the call is expected to fail.
        unsafe { GetTokenInformation(token, TokenUser, std::ptr::null_mut(), 0, &mut needed) };
        let mut buffer = vec![0u64; (needed as usize).div_ceil(8)];
        // SAFETY: `buffer` holds at least the `needed` bytes just asked for.
        let read = unsafe {
            GetTokenInformation(
                token,
                TokenUser,
                buffer.as_mut_ptr().cast(),
                needed,
                &mut needed,
            )
        };
        let failure = io::Error::last_os_error();
        // SAFETY: a token handle this function opened.
        unsafe { CloseHandle(token) };
        if read == 0 {
            return Err(failure);
        }
        Ok(Self { buffer })
    }

    /// Who this process runs as.
    pub fn this_process() -> io::Result<Self> {
        // SAFETY: the pseudo-handle for this process, always open and needing
        // no closing, with every access.
        unsafe { Self::of(GetCurrentProcess()) }
    }

    /// The user's SID, alive as long as `self`.
    pub fn sid(&self) -> PSID {
        // SAFETY: `buffer` holds an aligned `TOKEN_USER` written by
        // `GetTokenInformation`, whose `User.Sid` points inside it.
        unsafe { (*self.buffer.as_ptr().cast::<TOKEN_USER>()).User.Sid }
    }
}

/// An ACL with one entry, which allows one user everything.
///
/// The buffer is `u32`s, the alignment an ACL and its entries need.
pub struct OneUserAcl {
    buffer: Vec<u32>,
}

impl OneUserAcl {
    /// Allows `user` in and nobody else; `inheritance` is what the entry
    /// passes on to what is created inside (`0` for an object with no inside).
    pub fn new(user: &User, inheritance: ACE_FLAGS) -> io::Result<Self> {
        let sid = user.sid();
        // SAFETY: a SID alive in `user`.
        let sid_length = unsafe { GetLengthSid(sid) } as usize;
        // An `ACCESS_ALLOWED_ACE` carries the first `u32` of the SID inside
        // itself, so the SID's length replaces that field rather than adding
        // to it. Getting this wrong is how an ACL ends up one DWORD short and
        // `AddAccessAllowedAceEx` fails with a length error nobody can read.
        let ace =
            std::mem::size_of::<ACCESS_ALLOWED_ACE>() - std::mem::size_of::<u32>() + sid_length;
        let size = std::mem::size_of::<ACL>() + ace;
        let mut this = Self {
            buffer: vec![0u32; size.div_ceil(4)],
        };
        let acl = this.as_ptr();
        // SAFETY: `acl` points at at least `size` bytes, which is what is
        // declared.
        if unsafe { InitializeAcl(acl, size as u32, ACL_REVISION) } == 0 {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: the ACL was initialized with room for exactly this entry;
        // the SID is copied into it.
        if unsafe { AddAccessAllowedAceEx(acl, ACL_REVISION, inheritance, FILE_ALL_ACCESS, sid) }
            == 0
        {
            return Err(io::Error::last_os_error());
        }
        Ok(this)
    }

    /// The ACL, alive as long as `self`.
    pub fn as_ptr(&mut self) -> *mut ACL {
        self.buffer.as_mut_ptr().cast()
    }
}
