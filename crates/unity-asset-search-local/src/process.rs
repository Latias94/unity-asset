use std::io;

use sha2::{Digest as _, Sha256};
use thiserror::Error;

use crate::{ProcessStartIdentityV1, SecurityContextError, SecurityContextIdV1};

const PROCESS_START_DOMAIN: &[u8] = b"unity-asset:process-start:v1\0";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ProcessIdentityV1 {
    process_id: u32,
    process_start_identity: ProcessStartIdentityV1,
    security_context_id: SecurityContextIdV1,
}

impl ProcessIdentityV1 {
    pub fn current() -> Result<Self, ProcessIdentityError> {
        Self::inspect(std::process::id())
    }

    pub fn inspect(process_id: u32) -> Result<Self, ProcessIdentityError> {
        if process_id == 0 {
            return Err(ProcessIdentityError::InvalidProcessId);
        }
        let material = platform::inspect(process_id)?;
        let process_start_identity = ProcessStartIdentityV1::from_bytes(derive_process_identity(
            PROCESS_START_DOMAIN,
            process_id,
            &material.start,
        ))
        .map_err(|_| ProcessIdentityError::InvalidDerivedIdentity {
            field: "process_start_identity",
        })?;
        Ok(Self {
            process_id,
            process_start_identity,
            security_context_id: material.security_context_id,
        })
    }

    #[must_use]
    pub const fn process_id(&self) -> u32 {
        self.process_id
    }

    #[must_use]
    pub const fn process_start_identity(&self) -> ProcessStartIdentityV1 {
        self.process_start_identity
    }

    #[must_use]
    pub const fn security_context_id(&self) -> SecurityContextIdV1 {
        self.security_context_id
    }
}

fn derive_process_identity(domain: &[u8], process_id: u32, material: &[u8]) -> [u8; 32] {
    let mut hasher = Sha256::new();
    hasher.update(domain);
    hasher.update(process_id.to_le_bytes());
    hash_material(hasher, material)
}

fn hash_material(mut hasher: Sha256, material: &[u8]) -> [u8; 32] {
    hasher.update(
        u64::try_from(material.len())
            .expect("identity material length fits u64")
            .to_le_bytes(),
    );
    hasher.update(material);
    hasher.finalize().into()
}

#[derive(Debug, Error)]
pub enum ProcessIdentityError {
    #[error("process ID must not be zero")]
    InvalidProcessId,
    #[error("could not inspect process {process_id} {resource}: {source}")]
    Inspect {
        process_id: u32,
        resource: &'static str,
        #[source]
        source: io::Error,
    },
    #[error("could not inspect process security context: {0}")]
    SecurityContext(#[from] SecurityContextError),
    #[error("process {process_id} changed identity while it was inspected")]
    ProcessChanged { process_id: u32 },
    #[error("derived {field} was the reserved zero value")]
    InvalidDerivedIdentity { field: &'static str },
    #[error("process identity is unsupported on this platform")]
    UnsupportedPlatform,
    #[error("process {process_id} has no supported stable {resource} identity")]
    UnsupportedStableIdentity {
        process_id: u32,
        resource: &'static str,
    },
}

struct ProcessMaterial {
    start: Vec<u8>,
    security_context_id: SecurityContextIdV1,
}

#[cfg(target_os = "linux")]
mod platform {
    use std::fs::File;
    use std::io::Read as _;

    use rustix::fs::{CWD, Mode, OFlags, openat};

    use super::{ProcessIdentityError, ProcessMaterial};
    use crate::SecurityContextIdV1;

    const MAX_PROC_STAT_BYTES: u64 = 64 * 1024;
    const MAX_PROC_STATUS_BYTES: u64 = 64 * 1024;
    const MAX_BOOT_ID_BYTES: u64 = 128;
    fn process_directory_flags() -> OFlags {
        OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC
    }

    fn process_file_flags() -> OFlags {
        OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::CLOEXEC
    }

    pub(super) fn inspect(process_id: u32) -> Result<ProcessMaterial, ProcessIdentityError> {
        let process_directory = openat(
            CWD,
            format!("/proc/{process_id}"),
            process_directory_flags(),
            Mode::empty(),
        )
        .map_err(|source| inspect_error(process_id, "process directory", source.into()))?;
        let start_ticks = read_start_ticks(&process_directory, process_id)?;
        let effective_uid = read_effective_uid(&process_directory, process_id)?;

        if start_ticks == 0 {
            return Err(ProcessIdentityError::UnsupportedStableIdentity {
                process_id,
                resource: "Linux process start",
            });
        }

        if read_start_ticks(&process_directory, process_id)? != start_ticks {
            return Err(ProcessIdentityError::ProcessChanged { process_id });
        }

        let boot_id = read_boot_id(process_id)?;
        let mut start = Vec::with_capacity(8 + boot_id.len());
        start.extend_from_slice(&start_ticks.to_le_bytes());
        start.extend_from_slice(boot_id.as_bytes());
        Ok(ProcessMaterial {
            start,
            security_context_id: SecurityContextIdV1::for_effective_uid(effective_uid)?,
        })
    }

    fn read_start_ticks(
        process_directory: &rustix::fd::OwnedFd,
        process_id: u32,
    ) -> Result<u64, ProcessIdentityError> {
        let stat = read_process_file(
            process_directory,
            process_id,
            "stat",
            "start time",
            MAX_PROC_STAT_BYTES,
        )?;
        let after_command = stat
            .rsplit_once(") ")
            .map(|(_, fields)| fields)
            .ok_or_else(|| {
                inspect_error(
                    process_id,
                    "start time",
                    std::io::Error::new(
                        std::io::ErrorKind::InvalidData,
                        "Linux process stat has no command terminator",
                    ),
                )
            })?;
        after_command
            .split_ascii_whitespace()
            .nth(19)
            .ok_or_else(|| invalid_stat(process_id))?
            .parse::<u64>()
            .map_err(|_| invalid_stat(process_id))
    }

    fn read_effective_uid(
        process_directory: &rustix::fd::OwnedFd,
        process_id: u32,
    ) -> Result<u32, ProcessIdentityError> {
        let status = read_process_file(
            process_directory,
            process_id,
            "status",
            "security context",
            MAX_PROC_STATUS_BYTES,
        )?;
        let uid_line = status
            .lines()
            .find_map(|line| line.strip_prefix("Uid:"))
            .ok_or_else(|| invalid_status(process_id, "did not contain Uid"))?;
        uid_line
            .split_ascii_whitespace()
            .nth(1)
            .ok_or_else(|| invalid_status(process_id, "did not contain an effective UID"))?
            .parse::<u32>()
            .map_err(|_| invalid_status(process_id, "contained an invalid effective UID"))
    }

    fn read_process_file(
        process_directory: &rustix::fd::OwnedFd,
        process_id: u32,
        name: &str,
        resource: &'static str,
        maximum: u64,
    ) -> Result<String, ProcessIdentityError> {
        let descriptor = openat(process_directory, name, process_file_flags(), Mode::empty())
            .map_err(|source| inspect_error(process_id, resource, source.into()))?;
        let mut encoded = String::new();
        File::from(descriptor)
            .take(maximum)
            .read_to_string(&mut encoded)
            .map_err(|source| inspect_error(process_id, resource, source))?;
        Ok(encoded)
    }

    fn read_boot_id(process_id: u32) -> Result<String, ProcessIdentityError> {
        let mut boot_id = String::new();
        std::fs::File::open("/proc/sys/kernel/random/boot_id")
            .map_err(|source| inspect_error(process_id, "boot identity", source))?
            .take(MAX_BOOT_ID_BYTES)
            .read_to_string(&mut boot_id)
            .map_err(|source| inspect_error(process_id, "boot identity", source))?;
        let boot_id = boot_id.trim();
        if boot_id.is_empty() {
            return Err(inspect_error(
                process_id,
                "boot identity",
                std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    "Linux boot identity is empty",
                ),
            ));
        }
        Ok(boot_id.to_owned())
    }

    fn invalid_stat(process_id: u32) -> ProcessIdentityError {
        inspect_error(
            process_id,
            "start time",
            std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "Linux process stat has no valid start time",
            ),
        )
    }

    fn invalid_status(process_id: u32, reason: &'static str) -> ProcessIdentityError {
        inspect_error(
            process_id,
            "security context",
            std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                format!("Linux process status {reason}"),
            ),
        )
    }

    fn inspect_error(
        process_id: u32,
        resource: &'static str,
        source: std::io::Error,
    ) -> ProcessIdentityError {
        ProcessIdentityError::Inspect {
            process_id,
            resource,
            source,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn process_start_identity_is_bound_to_the_process_id() {
        let material = b"same process start material";
        assert_ne!(
            derive_process_identity(PROCESS_START_DOMAIN, 1, material),
            derive_process_identity(PROCESS_START_DOMAIN, 2, material)
        );
    }
}

#[cfg(target_os = "macos")]
mod platform {
    use std::mem::{MaybeUninit, size_of};

    use super::{ProcessIdentityError, ProcessMaterial};
    use crate::SecurityContextIdV1;

    pub(super) fn inspect(process_id: u32) -> Result<ProcessMaterial, ProcessIdentityError> {
        let pid = i32::try_from(process_id).map_err(|_| ProcessIdentityError::InvalidProcessId)?;
        let information = query_bsd_info(process_id, pid)?;
        let mut start = Vec::with_capacity(16);
        start.extend_from_slice(&information.pbi_start_tvsec.to_le_bytes());
        start.extend_from_slice(&information.pbi_start_tvusec.to_le_bytes());

        if (information.pbi_start_tvsec == 0 && information.pbi_start_tvusec == 0) {
            return Err(ProcessIdentityError::UnsupportedStableIdentity {
                process_id,
                resource: "macOS process start",
            });
        }
        let verified = query_bsd_info(process_id, pid)?;
        if verified.pbi_start_tvsec != information.pbi_start_tvsec
            || verified.pbi_start_tvusec != information.pbi_start_tvusec
            || verified.pbi_uid != information.pbi_uid
        {
            return Err(ProcessIdentityError::ProcessChanged { process_id });
        }
        Ok(ProcessMaterial {
            start,
            security_context_id: SecurityContextIdV1::for_effective_uid(information.pbi_uid)?,
        })
    }

    fn query_bsd_info(
        process_id: u32,
        pid: i32,
    ) -> Result<libc::proc_bsdinfo, ProcessIdentityError> {
        let mut information = MaybeUninit::<libc::proc_bsdinfo>::zeroed();
        let expected = i32::try_from(size_of::<libc::proc_bsdinfo>())
            .map_err(|_| ProcessIdentityError::UnsupportedPlatform)?;
        // SAFETY: `information` is writable for exactly `expected` bytes.
        let returned = unsafe {
            libc::proc_pidinfo(
                pid,
                libc::PROC_PIDTBSDINFO,
                0,
                information.as_mut_ptr().cast(),
                expected,
            )
        };
        if returned != expected {
            return Err(inspect_error(
                process_id,
                "start time",
                std::io::Error::last_os_error(),
            ));
        }
        // SAFETY: proc_pidinfo initialized the complete structure.
        Ok(unsafe { information.assume_init() })
    }

    fn inspect_error(
        process_id: u32,
        resource: &'static str,
        source: std::io::Error,
    ) -> ProcessIdentityError {
        ProcessIdentityError::Inspect {
            process_id,
            resource,
            source,
        }
    }
}

#[cfg(windows)]
mod platform {
    use windows_sys::Win32::Foundation::{CloseHandle, FILETIME, HANDLE, INVALID_HANDLE_VALUE};
    use windows_sys::Win32::System::Threading::{
        GetProcessTimes, OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION,
    };

    use super::{ProcessIdentityError, ProcessMaterial};
    use crate::SecurityContextIdV1;

    pub(super) fn inspect(process_id: u32) -> Result<ProcessMaterial, ProcessIdentityError> {
        // SAFETY: the requested access is read-only and the returned handle is checked.
        let process = unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, process_id) };
        let process = OwnedHandle::new(process).ok_or_else(|| {
            inspect_error(
                process_id,
                "process handle",
                std::io::Error::last_os_error(),
            )
        })?;

        let mut creation = FILETIME::default();
        let mut exit = FILETIME::default();
        let mut kernel = FILETIME::default();
        let mut user = FILETIME::default();
        // SAFETY: the process handle is valid and all outputs are writable.
        if unsafe {
            GetProcessTimes(
                process.raw(),
                &raw mut creation,
                &raw mut exit,
                &raw mut kernel,
                &raw mut user,
            )
        } == 0
        {
            return Err(inspect_error(
                process_id,
                "start time",
                std::io::Error::last_os_error(),
            ));
        }
        let mut start = Vec::with_capacity(8);
        start.extend_from_slice(&creation.dwLowDateTime.to_le_bytes());
        start.extend_from_slice(&creation.dwHighDateTime.to_le_bytes());

        if creation.dwLowDateTime == 0 && creation.dwHighDateTime == 0 {
            return Err(ProcessIdentityError::UnsupportedStableIdentity {
                process_id,
                resource: "Windows process start",
            });
        }
        let security_context_id = SecurityContextIdV1::for_process_handle(process.raw())?;
        let mut verified_creation = FILETIME::default();
        // SAFETY: the process handle is valid and all outputs are writable.
        if unsafe {
            GetProcessTimes(
                process.raw(),
                &raw mut verified_creation,
                &raw mut exit,
                &raw mut kernel,
                &raw mut user,
            )
        } == 0
        {
            return Err(inspect_error(
                process_id,
                "start time verification",
                std::io::Error::last_os_error(),
            ));
        }
        if verified_creation.dwLowDateTime != creation.dwLowDateTime
            || verified_creation.dwHighDateTime != creation.dwHighDateTime
        {
            return Err(ProcessIdentityError::ProcessChanged { process_id });
        }
        Ok(ProcessMaterial {
            start,
            security_context_id,
        })
    }

    fn inspect_error(
        process_id: u32,
        resource: &'static str,
        source: std::io::Error,
    ) -> ProcessIdentityError {
        ProcessIdentityError::Inspect {
            process_id,
            resource,
            source,
        }
    }

    struct OwnedHandle(HANDLE);

    impl OwnedHandle {
        fn new(handle: HANDLE) -> Option<Self> {
            (!handle.is_null() && handle != INVALID_HANDLE_VALUE).then_some(Self(handle))
        }

        const fn raw(&self) -> HANDLE {
            self.0
        }
    }

    impl Drop for OwnedHandle {
        fn drop(&mut self) {
            // SAFETY: this wrapper uniquely owns every non-null handle it stores.
            unsafe { CloseHandle(self.0) };
        }
    }
}

#[cfg(not(any(target_os = "linux", target_os = "macos", windows)))]
mod platform {
    use super::{ProcessIdentityError, ProcessMaterial};

    pub(super) fn inspect(_process_id: u32) -> Result<ProcessMaterial, ProcessIdentityError> {
        Err(ProcessIdentityError::UnsupportedPlatform)
    }
}
