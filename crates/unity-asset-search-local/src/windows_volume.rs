use std::io;
use std::mem::size_of;

use windows_sys::Wdk::Storage::FileSystem::{
    FileFsDeviceInformation, NtQueryVolumeInformationFile,
};
use windows_sys::Wdk::System::SystemServices::{
    FILE_FS_DEVICE_INFORMATION, FILE_REMOTE_DEVICE, FILE_REMOTE_DEVICE_VSMB,
};
use windows_sys::Win32::Foundation::{HANDLE, NTSTATUS, RtlNtStatusToDosError};
use windows_sys::Win32::System::IO::IO_STATUS_BLOCK;

pub(crate) fn validate_local_volume(handle: HANDLE) -> io::Result<()> {
    let mut io_status = IO_STATUS_BLOCK::default();
    let mut device_information = FILE_FS_DEVICE_INFORMATION::default();
    let expected_length = u32::try_from(size_of::<FILE_FS_DEVICE_INFORMATION>())
        .map_err(|_| io::Error::other("Windows volume device information size exceeds u32"))?;
    // SAFETY: the handle remains live, and both output structures are writable for the supplied
    // length until the synchronous query returns.
    let status = unsafe {
        NtQueryVolumeInformationFile(
            handle,
            &raw mut io_status,
            (&raw mut device_information).cast(),
            expected_length,
            FileFsDeviceInformation,
        )
    };
    ntstatus_result(status)?;
    if io_status.Information != expected_length as usize {
        return Err(io::Error::other(
            "Windows returned an invalid volume device information length",
        ));
    }
    validate_device_characteristics(device_information.Characteristics)
}

fn validate_device_characteristics(characteristics: u32) -> io::Result<()> {
    if characteristics & (FILE_REMOTE_DEVICE | FILE_REMOTE_DEVICE_VSMB) != 0 {
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "opened directory handle belongs to a remote volume",
        ))
    } else {
        Ok(())
    }
}

fn ntstatus_result(status: NTSTATUS) -> io::Result<()> {
    if status >= 0 {
        return Ok(());
    }
    // SAFETY: translating an NTSTATUS has no pointer preconditions.
    let win32 = unsafe { RtlNtStatusToDosError(status) };
    let raw = i32::try_from(win32).map_err(|_| {
        io::Error::other(format!(
            "volume device query failed with NTSTATUS {status:#010x} and unmapped Win32 code {win32}"
        ))
    })?;
    Err(io::Error::from_raw_os_error(raw))
}

#[cfg(test)]
mod tests {
    use windows_sys::Wdk::System::SystemServices::{FILE_REMOTE_DEVICE, FILE_REMOTE_DEVICE_VSMB};

    use super::validate_device_characteristics;

    #[test]
    fn remote_device_characteristics_are_rejected() {
        assert!(validate_device_characteristics(FILE_REMOTE_DEVICE).is_err());
        assert!(validate_device_characteristics(FILE_REMOTE_DEVICE_VSMB).is_err());
        assert!(
            validate_device_characteristics(FILE_REMOTE_DEVICE | FILE_REMOTE_DEVICE_VSMB).is_err()
        );
    }

    #[test]
    fn local_device_characteristics_are_accepted() {
        assert!(validate_device_characteristics(0).is_ok());
    }
}
