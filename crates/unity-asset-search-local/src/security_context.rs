use std::io;

use thiserror::Error;

pub(crate) struct CurrentFilesystemAuthority {
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    effective_uid: u32,
    #[cfg(windows)]
    user_sid: Vec<u8>,
}

impl CurrentFilesystemAuthority {
    pub(crate) fn current() -> Result<Self, FilesystemAuthorityError> {
        platform::current()
    }

    #[cfg(any(target_os = "linux", target_os = "macos"))]
    pub(crate) const fn effective_uid(&self) -> u32 {
        self.effective_uid
    }

    #[cfg(windows)]
    pub(crate) fn windows_user_sid(&self) -> &[u8] {
        &self.user_sid
    }
}

#[derive(Debug, Error)]
pub enum FilesystemAuthorityError {
    #[error("private filesystem authority is unsupported on this platform")]
    UnsupportedPlatform,
    #[error("could not determine the current filesystem user authority: {0}")]
    Unavailable(#[source] io::Error),
    #[error("an ambient Windows thread impersonation token belongs to a different user")]
    AmbientUserImpersonationUnsupported,
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
mod platform {
    use super::{CurrentFilesystemAuthority, FilesystemAuthorityError};

    pub(super) fn current() -> Result<CurrentFilesystemAuthority, FilesystemAuthorityError> {
        Ok(CurrentFilesystemAuthority {
            effective_uid: rustix::process::geteuid().as_raw(),
        })
    }
}

#[cfg(windows)]
mod platform {
    use std::io;
    use std::mem::{MaybeUninit, size_of};

    use windows_sys::Win32::Foundation::{
        CloseHandle, ERROR_INSUFFICIENT_BUFFER, ERROR_NO_TOKEN, HANDLE, INVALID_HANDLE_VALUE,
    };
    use windows_sys::Win32::Security::{
        GetLengthSid, GetTokenInformation, IsValidSid, PSID, SID, TOKEN_INFORMATION_CLASS,
        TOKEN_QUERY, TOKEN_USER, TokenUser,
    };
    use windows_sys::Win32::System::Threading::{
        GetCurrentProcess, GetCurrentThread, OpenProcessToken, OpenThreadToken,
    };

    use super::{CurrentFilesystemAuthority, FilesystemAuthorityError};

    const MAX_TOKEN_INFORMATION_BYTES: usize = 64 * 1024;
    const SID_HEADER_BYTES: usize = size_of::<SID>() - size_of::<u32>();

    pub(super) fn current() -> Result<CurrentFilesystemAuthority, FilesystemAuthorityError> {
        let process_token = open_process_token().map_err(FilesystemAuthorityError::Unavailable)?;
        let user_sid =
            query_user_sid(process_token.raw()).map_err(FilesystemAuthorityError::Unavailable)?;
        if let Some(thread_token) =
            open_thread_token().map_err(FilesystemAuthorityError::Unavailable)?
        {
            let thread_user_sid = query_user_sid(thread_token.raw())
                .map_err(FilesystemAuthorityError::Unavailable)?;
            if thread_user_sid != user_sid {
                return Err(FilesystemAuthorityError::AmbientUserImpersonationUnsupported);
            }
        }
        Ok(CurrentFilesystemAuthority { user_sid })
    }

    fn query_user_sid(token: HANDLE) -> io::Result<Vec<u8>> {
        let user = query_token_information(token, TokenUser, size_of::<TOKEN_USER>())?;
        let token_user = user.read::<TOKEN_USER>()?;
        user.copy_sid(token_user.User.Sid)
    }

    struct TokenInformation {
        storage: Vec<MaybeUninit<usize>>,
        returned_len: usize,
    }

    impl TokenInformation {
        fn read<T: Copy>(&self) -> io::Result<T> {
            if self.returned_len < size_of::<T>() {
                return Err(io::Error::other(
                    "Windows token information returned a truncated structure",
                ));
            }
            // SAFETY: the retained allocation is initialized for `returned_len` bytes. The
            // explicit size check ensures a complete `T` is readable.
            Ok(unsafe { self.storage.as_ptr().cast::<T>().read_unaligned() })
        }

        fn copy_sid(&self, sid: PSID) -> io::Result<Vec<u8>> {
            if sid.is_null() {
                return Err(io::Error::other("Windows token returned a null SID"));
            }
            let storage_start = self.storage.as_ptr() as usize;
            let storage_end = storage_start
                .checked_add(self.returned_len)
                .ok_or_else(|| io::Error::other("Windows token buffer address overflow"))?;
            let sid_start = sid as usize;
            let header_end = sid_start
                .checked_add(SID_HEADER_BYTES)
                .ok_or_else(|| io::Error::other("Windows SID address overflow"))?;
            if sid_start < storage_start || header_end > storage_end {
                return Err(io::Error::other(
                    "Windows token returned an out-of-bounds SID",
                ));
            }
            // SAFETY: the SID revision and sub-authority count bytes are inside the retained
            // token-information allocation.
            let sub_authorities = unsafe { *sid.cast::<u8>().add(1) } as usize;
            let sid_len = SID_HEADER_BYTES
                .checked_add(
                    sub_authorities
                        .checked_mul(size_of::<u32>())
                        .ok_or_else(|| io::Error::other("Windows SID length overflow"))?,
                )
                .ok_or_else(|| io::Error::other("Windows SID length overflow"))?;
            let sid_end = sid_start
                .checked_add(sid_len)
                .ok_or_else(|| io::Error::other("Windows SID length overflow"))?;
            if sid_end > storage_end {
                return Err(io::Error::other("Windows token returned a truncated SID"));
            }
            // SAFETY: the complete SID lies inside the retained allocation.
            if unsafe { IsValidSid(sid) } == 0 {
                return Err(io::Error::other("Windows token returned an invalid SID"));
            }
            // SAFETY: IsValidSid succeeded for the bounded SID.
            let reported_len = unsafe { GetLengthSid(sid) } as usize;
            if reported_len != sid_len {
                return Err(io::Error::other(
                    "Windows token returned an inconsistent SID length",
                ));
            }
            // SAFETY: `sid_len` bytes were checked to lie inside the retained allocation.
            Ok(unsafe { std::slice::from_raw_parts(sid.cast::<u8>(), sid_len) }.to_vec())
        }
    }

    fn query_token_information(
        token: HANDLE,
        class: TOKEN_INFORMATION_CLASS,
        minimum_len: usize,
    ) -> io::Result<TokenInformation> {
        let mut required = 0_u32;
        // SAFETY: this is the documented size-query form and `required` is writable.
        let first = unsafe {
            GetTokenInformation(token, class, std::ptr::null_mut(), 0, &raw mut required)
        };
        let first_error = io::Error::last_os_error();
        if first != 0
            || first_error.raw_os_error()
                != Some(i32::try_from(ERROR_INSUFFICIENT_BUFFER).expect("Win32 code fits i32"))
        {
            return Err(first_error);
        }
        let required = usize::try_from(required)
            .map_err(|_| io::Error::other("Windows token buffer size does not fit usize"))?;
        if required < minimum_len || required > MAX_TOKEN_INFORMATION_BYTES {
            return Err(io::Error::other(
                "Windows token information has an unsupported size",
            ));
        }
        let units = required.div_ceil(size_of::<usize>());
        let mut storage = Vec::new();
        storage.try_reserve_exact(units).map_err(|source| {
            io::Error::other(format!("could not allocate token buffer: {source}"))
        })?;
        storage.resize_with(units, MaybeUninit::zeroed);
        let capacity = storage
            .len()
            .checked_mul(size_of::<usize>())
            .and_then(|bytes| u32::try_from(bytes).ok())
            .ok_or_else(|| io::Error::other("Windows token buffer size overflow"))?;
        let mut returned = capacity;
        // SAFETY: the aligned allocation is writable for `capacity` bytes and remains retained.
        if unsafe {
            GetTokenInformation(
                token,
                class,
                storage.as_mut_ptr().cast(),
                capacity,
                &raw mut returned,
            )
        } == 0
        {
            return Err(io::Error::last_os_error());
        }
        let returned_len = usize::try_from(returned)
            .map_err(|_| io::Error::other("Windows returned token size does not fit usize"))?;
        if returned_len < minimum_len || returned_len > capacity as usize {
            return Err(io::Error::other(
                "Windows returned an invalid token information length",
            ));
        }
        Ok(TokenInformation {
            storage,
            returned_len,
        })
    }

    fn open_thread_token() -> io::Result<Option<OwnedHandle>> {
        let mut handle = std::ptr::null_mut();
        // SAFETY: the pseudo thread handle is valid and the output is writable.
        if unsafe { OpenThreadToken(GetCurrentThread(), TOKEN_QUERY, 1, &raw mut handle) } != 0 {
            return Ok(Some(OwnedHandle(handle)));
        }
        let error = io::Error::last_os_error();
        if error.raw_os_error() == Some(i32::try_from(ERROR_NO_TOKEN).expect("Win32 code fits i32"))
        {
            Ok(None)
        } else {
            Err(error)
        }
    }

    fn open_process_token() -> io::Result<OwnedHandle> {
        let mut handle = std::ptr::null_mut();
        // SAFETY: the pseudo process handle is valid and the output is writable.
        if unsafe { OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &raw mut handle) } == 0 {
            Err(io::Error::last_os_error())
        } else {
            Ok(OwnedHandle(handle))
        }
    }

    struct OwnedHandle(HANDLE);

    impl OwnedHandle {
        const fn raw(&self) -> HANDLE {
            self.0
        }
    }

    impl Drop for OwnedHandle {
        fn drop(&mut self) {
            if !self.0.is_null() && self.0 != INVALID_HANDLE_VALUE {
                // SAFETY: this wrapper uniquely owns the token handle.
                unsafe { CloseHandle(self.0) };
            }
        }
    }
}

#[cfg(not(any(target_os = "linux", target_os = "macos", windows)))]
mod platform {
    use super::{CurrentFilesystemAuthority, FilesystemAuthorityError};

    pub(super) fn current() -> Result<CurrentFilesystemAuthority, FilesystemAuthorityError> {
        Err(FilesystemAuthorityError::UnsupportedPlatform)
    }
}
