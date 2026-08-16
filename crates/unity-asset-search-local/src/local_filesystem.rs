use std::io;
use std::os::fd::AsFd;
#[cfg(target_os = "macos")]
use std::os::fd::AsRawFd as _;

#[cfg(target_os = "linux")]
pub(crate) fn validate_local_directory(descriptor: impl AsFd) -> io::Result<()> {
    let filesystem = rustix::fs::fstatfs(descriptor).map_err(io::Error::from)?;
    if unsupported_linux_identity_filesystem(filesystem.f_type as u64) {
        return Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "filesystem type is not accepted as a stable local identity source",
        ));
    }
    Ok(())
}

#[cfg(target_os = "linux")]
fn unsupported_linux_identity_filesystem(filesystem_type: u64) -> bool {
    const UNSUPPORTED: &[u64] = &[
        0x0000_6969,           // NFS
        0x0000_517b,           // SMB
        0xffff_ffff_ff53_4d42, // CIFS on signed FsWord targets
        0x0000_0000_ff53_4d42, // CIFS on unsigned FsWord targets
        0x7375_7245,           // Coda
        0x5346_414f,           // AFS
        0x0000_564c,           // NCP
        0x00c3_6400,           // Ceph
        0x0102_1997,           // 9P
        0x6573_5546,           // FUSE, including sshfs
        0x0bd0_0bd0,           // Lustre
        0x4750_4653,           // GPFS
        0x0116_1970,           // GFS2
        0x7461_636f,           // OCFS2
        0xaad7_aaea,           // PanFS
    ];
    UNSUPPORTED.contains(&filesystem_type)
}

#[cfg(target_os = "macos")]
pub(crate) fn validate_local_directory(descriptor: impl AsFd) -> io::Result<()> {
    let mut filesystem = std::mem::MaybeUninit::<libc::statfs>::uninit();
    // SAFETY: the descriptor is live and the output points to writable storage.
    if unsafe { libc::fstatfs(descriptor.as_fd().as_raw_fd(), filesystem.as_mut_ptr()) } != 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: fstatfs succeeded and initialized the output structure.
    let filesystem = unsafe { filesystem.assume_init() };
    if filesystem.f_flags & (libc::MNT_LOCAL as u32) == 0 {
        return Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "filesystem is not local",
        ));
    }
    Ok(())
}

#[cfg(all(test, target_os = "linux"))]
mod tests {
    use super::*;

    #[test]
    fn known_remote_and_fuse_filesystems_are_not_identity_sources() {
        assert!(unsupported_linux_identity_filesystem(0x6969));
        assert!(unsupported_linux_identity_filesystem(0xff53_4d42));
        assert!(unsupported_linux_identity_filesystem(0x6573_5546));
        assert!(!unsupported_linux_identity_filesystem(0xef53));
    }
}
