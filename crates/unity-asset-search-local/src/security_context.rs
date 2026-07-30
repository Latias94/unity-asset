use std::fmt;
use std::io;
use std::str::FromStr;

use serde::{Deserialize, Deserializer, Serialize, Serializer};
use sha2::{Digest as _, Sha256};
use thiserror::Error;

use crate::ids::{
    LocalIdentityParseError, deserialize_fixed_id, format_fixed_id, parse_fixed_id,
    serialize_fixed_id, validate_nonzero,
};

const SECURITY_CONTEXT_DOMAIN: &[u8] = b"unity-asset:security-context:v1\0";
const SECURITY_CONTEXT_PREFIX: &str = "security-context-v1:";
const SECURITY_CONTEXT_BYTES: usize = 32;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct SecurityContextIdV1([u8; SECURITY_CONTEXT_BYTES]);

impl SecurityContextIdV1 {
    pub fn from_bytes(
        bytes: [u8; SECURITY_CONTEXT_BYTES],
    ) -> Result<Self, LocalIdentityParseError> {
        validate_nonzero(bytes).map(Self)
    }

    #[must_use]
    pub const fn as_bytes(&self) -> &[u8; SECURITY_CONTEXT_BYTES] {
        &self.0
    }

    pub fn current() -> Result<Self, SecurityContextError> {
        CurrentSecurityContextSnapshot::current().map(|snapshot| snapshot.id)
    }

    pub fn for_process(process_id: u32) -> Result<Self, SecurityContextError> {
        if process_id == 0 {
            return Err(SecurityContextError::InvalidProcessId);
        }
        derive_security_context(platform::process_material(process_id)?)
    }

    #[cfg(any(target_os = "linux", target_os = "macos"))]
    pub(crate) fn for_effective_uid(effective_uid: u32) -> Result<Self, SecurityContextError> {
        derive_security_context(platform::unix_material(effective_uid))
    }

    #[cfg(windows)]
    pub(crate) fn for_process_handle(
        process: windows_sys::Win32::Foundation::HANDLE,
    ) -> Result<Self, SecurityContextError> {
        derive_security_context(platform::process_handle_material(process)?)
    }

    #[cfg(windows)]
    pub(crate) fn for_impersonated_thread() -> Result<Self, SecurityContextError> {
        derive_security_context(platform::thread_material()?)
    }

    #[cfg(windows)]
    pub(crate) fn path_component(self) -> String {
        hex::encode(self.0)
    }
}

pub(crate) struct CurrentSecurityContextSnapshot {
    id: SecurityContextIdV1,
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    effective_uid: u32,
    #[cfg(windows)]
    user_sid: Vec<u8>,
    #[cfg(windows)]
    logon_sid: Vec<u8>,
}

impl CurrentSecurityContextSnapshot {
    pub(crate) fn current() -> Result<Self, SecurityContextError> {
        let material = platform::current_material()?;
        #[cfg(windows)]
        if material.restricted || material.app_container.is_some() {
            return Err(SecurityContextError::UnsupportedRestrictedWindowsContext);
        }
        #[cfg(any(target_os = "linux", target_os = "macos"))]
        let effective_uid = u32::from_le_bytes(
            material
                .user
                .as_slice()
                .try_into()
                .map_err(|_| SecurityContextError::InvalidMaterial)?,
        );
        #[cfg(windows)]
        let user_sid = material.user.clone();
        #[cfg(windows)]
        let logon_sid = material
            .logon
            .clone()
            .ok_or(SecurityContextError::InvalidMaterial)?;
        let id = derive_security_context(material)?;
        Ok(Self {
            id,
            #[cfg(any(target_os = "linux", target_os = "macos"))]
            effective_uid,
            #[cfg(windows)]
            user_sid,
            #[cfg(windows)]
            logon_sid,
        })
    }

    pub(crate) const fn id(&self) -> SecurityContextIdV1 {
        self.id
    }

    #[cfg(any(target_os = "linux", target_os = "macos"))]
    pub(crate) const fn effective_uid(&self) -> u32 {
        self.effective_uid
    }

    #[cfg(windows)]
    pub(crate) fn windows_user_sid(&self) -> &[u8] {
        &self.user_sid
    }

    #[cfg(windows)]
    pub(crate) fn windows_logon_sid(&self) -> &[u8] {
        &self.logon_sid
    }
}

impl fmt::Display for SecurityContextIdV1 {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        format_fixed_id(formatter, SECURITY_CONTEXT_PREFIX, &self.0)
    }
}

impl FromStr for SecurityContextIdV1 {
    type Err = LocalIdentityParseError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        parse_fixed_id(value, SECURITY_CONTEXT_PREFIX).map(Self)
    }
}

impl Serialize for SecurityContextIdV1 {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serialize_fixed_id(serializer, SECURITY_CONTEXT_PREFIX, &self.0)
    }
}

impl<'de> Deserialize<'de> for SecurityContextIdV1 {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        deserialize_fixed_id(deserializer, SECURITY_CONTEXT_PREFIX).map(Self)
    }
}

#[derive(Debug, Error)]
pub enum SecurityContextError {
    #[error("security-context identity is unsupported on this platform")]
    UnsupportedPlatform,
    #[error("could not derive the effective security context: {0}")]
    Unavailable(#[source] io::Error),
    #[error("the derived security-context identity was the reserved zero value")]
    InvalidDerivedIdentity,
    #[error("security-context material exceeds the canonical encoding limits")]
    InvalidMaterial,
    #[error("process ID must not be zero")]
    InvalidProcessId,
    #[error("an ambient Windows thread impersonation token differs from the process principal")]
    AmbientImpersonationUnsupported,
    #[error(
        "restricted-token and AppContainer execution contexts are unsupported for local daemon mode"
    )]
    UnsupportedRestrictedWindowsContext,
}

#[derive(PartialEq, Eq)]
struct SecurityContextMaterial {
    platform: &'static [u8],
    user: Vec<u8>,
    logon: Option<Vec<u8>>,
    integrity: Option<Vec<u8>>,
    elevation: u8,
    restricted: bool,
    restricted_sids: Vec<u8>,
    app_container: Option<Vec<u8>>,
    capabilities: Vec<u8>,
}

fn derive_security_context(
    material: SecurityContextMaterial,
) -> Result<SecurityContextIdV1, SecurityContextError> {
    let mut hasher = Sha256::new();
    hasher.update(SECURITY_CONTEXT_DOMAIN);
    hash_field(&mut hasher, b"platform", material.platform)?;
    hash_field(&mut hasher, b"user", &material.user)?;
    hash_optional_field(&mut hasher, b"logon", material.logon.as_deref())?;
    hash_optional_field(&mut hasher, b"integrity", material.integrity.as_deref())?;
    hash_field(&mut hasher, b"elevation", &[material.elevation])?;
    hash_field(&mut hasher, b"restricted", &[u8::from(material.restricted)])?;
    hash_field(&mut hasher, b"restricted_sids", &material.restricted_sids)?;
    hash_optional_field(
        &mut hasher,
        b"app_container",
        material.app_container.as_deref(),
    )?;
    hash_field(&mut hasher, b"capabilities", &material.capabilities)?;
    let bytes: [u8; SECURITY_CONTEXT_BYTES] = hasher.finalize().into();
    SecurityContextIdV1::from_bytes(bytes).map_err(|_| SecurityContextError::InvalidDerivedIdentity)
}

fn hash_field(hasher: &mut Sha256, name: &[u8], value: &[u8]) -> Result<(), SecurityContextError> {
    let name_len = u32::try_from(name.len()).map_err(|_| SecurityContextError::InvalidMaterial)?;
    let value_len =
        u32::try_from(value.len()).map_err(|_| SecurityContextError::InvalidMaterial)?;
    hasher.update(name_len.to_le_bytes());
    hasher.update(name);
    hasher.update(value_len.to_le_bytes());
    hasher.update(value);
    Ok(())
}

fn hash_optional_field(
    hasher: &mut Sha256,
    name: &[u8],
    value: Option<&[u8]>,
) -> Result<(), SecurityContextError> {
    hash_field(hasher, name, &[u8::from(value.is_some())])?;
    if let Some(value) = value {
        hash_field(hasher, b"value", value)?;
    }
    Ok(())
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
mod platform {
    use std::io;

    use super::{SecurityContextError, SecurityContextMaterial};

    pub(super) fn current_material() -> Result<SecurityContextMaterial, SecurityContextError> {
        Ok(unix_material(rustix::process::geteuid().as_raw()))
    }

    pub(super) fn process_material(
        process_id: u32,
    ) -> Result<SecurityContextMaterial, SecurityContextError> {
        #[cfg(target_os = "linux")]
        let effective_uid = linux_process_effective_uid(process_id)?;
        #[cfg(target_os = "macos")]
        let effective_uid = macos_process_effective_uid(process_id)?;
        Ok(unix_material(effective_uid))
    }

    pub(super) fn unix_material(effective_uid: u32) -> SecurityContextMaterial {
        #[cfg(target_os = "linux")]
        let platform = b"linux".as_slice();
        #[cfg(target_os = "macos")]
        let platform = b"macos".as_slice();
        SecurityContextMaterial {
            platform,
            user: effective_uid.to_le_bytes().to_vec(),
            logon: None,
            integrity: None,
            elevation: 0,
            restricted: false,
            restricted_sids: Vec::new(),
            app_container: None,
            capabilities: Vec::new(),
        }
    }

    #[cfg(target_os = "linux")]
    fn linux_process_effective_uid(process_id: u32) -> Result<u32, SecurityContextError> {
        use std::io::Read as _;

        const MAX_STATUS_BYTES: u64 = 64 * 1024;
        let path = format!("/proc/{process_id}/status");
        let file = std::fs::File::open(path).map_err(SecurityContextError::Unavailable)?;
        let mut encoded = String::new();
        file.take(MAX_STATUS_BYTES)
            .read_to_string(&mut encoded)
            .map_err(SecurityContextError::Unavailable)?;
        let uid_line = encoded
            .lines()
            .find_map(|line| line.strip_prefix("Uid:"))
            .ok_or_else(|| {
                SecurityContextError::Unavailable(io::Error::other(
                    "Linux process status did not contain Uid",
                ))
            })?;
        uid_line
            .split_ascii_whitespace()
            .nth(1)
            .ok_or_else(|| {
                SecurityContextError::Unavailable(io::Error::other(
                    "Linux process status did not contain an effective UID",
                ))
            })?
            .parse::<u32>()
            .map_err(|source| {
                SecurityContextError::Unavailable(io::Error::new(
                    io::ErrorKind::InvalidData,
                    source,
                ))
            })
    }

    #[cfg(target_os = "macos")]
    fn macos_process_effective_uid(process_id: u32) -> Result<u32, SecurityContextError> {
        use std::mem::{MaybeUninit, size_of};

        let process_id = i32::try_from(process_id).map_err(|_| {
            SecurityContextError::Unavailable(io::Error::other(
                "macOS process ID does not fit pid_t",
            ))
        })?;
        let mut information = MaybeUninit::<libc::proc_bsdinfo>::zeroed();
        let expected = i32::try_from(size_of::<libc::proc_bsdinfo>()).map_err(|_| {
            SecurityContextError::Unavailable(io::Error::other(
                "macOS process information size does not fit c_int",
            ))
        })?;
        // SAFETY: `information` is writable for exactly `expected` bytes.
        let returned = unsafe {
            libc::proc_pidinfo(
                process_id,
                libc::PROC_PIDTBSDINFO,
                0,
                information.as_mut_ptr().cast(),
                expected,
            )
        };
        if returned != expected {
            return Err(SecurityContextError::Unavailable(io::Error::last_os_error()));
        }
        // SAFETY: proc_pidinfo initialized the complete structure.
        Ok(unsafe { information.assume_init() }.pbi_uid)
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
        GetLengthSid, GetTokenInformation, IsValidSid, PSID, SID, SID_AND_ATTRIBUTES,
        TOKEN_APPCONTAINER_INFORMATION, TOKEN_ELEVATION_TYPE, TOKEN_GROUPS,
        TOKEN_INFORMATION_CLASS, TOKEN_MANDATORY_LABEL, TOKEN_QUERY, TOKEN_USER,
        TokenAppContainerSid, TokenCapabilities, TokenElevationType,
        TokenElevationTypeDefault as TOKEN_ELEVATION_TYPE_DEFAULT,
        TokenElevationTypeFull as TOKEN_ELEVATION_TYPE_FULL,
        TokenElevationTypeLimited as TOKEN_ELEVATION_TYPE_LIMITED, TokenIntegrityLevel,
        TokenIsRestricted, TokenLogonSid, TokenRestrictedSids, TokenUser,
    };
    use windows_sys::Win32::System::Threading::{
        GetCurrentProcess, GetCurrentThread, OpenProcess, OpenProcessToken, OpenThreadToken,
        PROCESS_QUERY_LIMITED_INFORMATION,
    };

    use super::{SecurityContextError, SecurityContextMaterial};

    const MAX_TOKEN_INFORMATION_BYTES: usize = 64 * 1024;
    const SID_HEADER_BYTES: usize = size_of::<SID>() - size_of::<u32>();

    pub(super) fn current_material() -> Result<SecurityContextMaterial, SecurityContextError> {
        let process_token = open_process_token().map_err(SecurityContextError::Unavailable)?;
        let process_material =
            windows_material(process_token.raw()).map_err(SecurityContextError::Unavailable)?;
        if let Some(thread_token) =
            open_thread_token().map_err(SecurityContextError::Unavailable)?
        {
            let thread_material =
                windows_material(thread_token.raw()).map_err(SecurityContextError::Unavailable)?;
            if thread_material != process_material {
                return Err(SecurityContextError::AmbientImpersonationUnsupported);
            }
        }
        Ok(process_material)
    }

    pub(super) fn process_material(
        process_id: u32,
    ) -> Result<SecurityContextMaterial, SecurityContextError> {
        // SAFETY: the requested access is read-only and the returned handle is checked.
        let process = unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, process_id) };
        if process.is_null() || process == INVALID_HANDLE_VALUE {
            return Err(SecurityContextError::Unavailable(io::Error::last_os_error()));
        }
        let process = OwnedHandle(process);
        process_handle_material(process.raw())
    }

    pub(super) fn process_handle_material(
        process: HANDLE,
    ) -> Result<SecurityContextMaterial, SecurityContextError> {
        let mut token = std::ptr::null_mut();
        // SAFETY: the process handle is valid and the output pointer is writable.
        if unsafe { OpenProcessToken(process, TOKEN_QUERY, &raw mut token) } == 0 {
            return Err(SecurityContextError::Unavailable(io::Error::last_os_error()));
        }
        let token = OwnedHandle(token);
        windows_material(token.raw()).map_err(SecurityContextError::Unavailable)
    }

    pub(super) fn thread_material() -> Result<SecurityContextMaterial, SecurityContextError> {
        let token = open_thread_token()
            .map_err(SecurityContextError::Unavailable)?
            .ok_or_else(|| {
                SecurityContextError::Unavailable(io::Error::other(
                    "the current thread is not impersonating a named-pipe client",
                ))
            })?;
        windows_material(token.raw()).map_err(SecurityContextError::Unavailable)
    }

    fn windows_material(token: HANDLE) -> io::Result<SecurityContextMaterial> {
        let user = query_user_sid(token)?;

        let logon = query_token_information(token, TokenLogonSid, size_of::<TOKEN_GROUPS>())?;
        let groups = logon.read::<TOKEN_GROUPS>()?;
        if groups.GroupCount != 1 {
            return Err(io::Error::other(
                "Windows effective token must expose exactly one logon SID",
            ));
        }
        let logon_sid = logon.copy_sid(groups.Groups[0].Sid)?;

        let integrity = query_token_information(
            token,
            TokenIntegrityLevel,
            size_of::<TOKEN_MANDATORY_LABEL>(),
        )?;
        let mandatory_label = integrity.read::<TOKEN_MANDATORY_LABEL>()?;
        let integrity_sid = integrity.copy_sid(mandatory_label.Label.Sid)?;

        let elevation =
            query_token_information(token, TokenElevationType, size_of::<TOKEN_ELEVATION_TYPE>())?
                .read::<TOKEN_ELEVATION_TYPE>()?;
        let elevation = match elevation {
            TOKEN_ELEVATION_TYPE_DEFAULT => 1,
            TOKEN_ELEVATION_TYPE_FULL => 2,
            TOKEN_ELEVATION_TYPE_LIMITED => 3,
            _ => {
                return Err(io::Error::other(
                    "Windows returned an unknown elevation type",
                ));
            }
        };

        let restricted =
            query_token_information(token, TokenIsRestricted, size_of::<u32>())?.read::<u32>()?;
        let restricted = match restricted {
            0 => false,
            1 => true,
            _ => {
                return Err(io::Error::other(
                    "Windows returned an invalid restricted flag",
                ));
            }
        };
        let (restricted_sids, _) = query_sid_groups(token, TokenRestrictedSids)?;

        let app_container = query_token_information(
            token,
            TokenAppContainerSid,
            size_of::<TOKEN_APPCONTAINER_INFORMATION>(),
        )?;
        let app_container_info = app_container.read::<TOKEN_APPCONTAINER_INFORMATION>()?;
        let app_container_sid = if app_container_info.TokenAppContainer.is_null() {
            None
        } else {
            Some(app_container.copy_sid(app_container_info.TokenAppContainer)?)
        };
        let (capabilities, _) = query_sid_groups(token, TokenCapabilities)?;

        Ok(SecurityContextMaterial {
            platform: b"windows",
            user,
            logon: Some(logon_sid),
            integrity: Some(integrity_sid),
            elevation,
            restricted,
            restricted_sids,
            app_container: app_container_sid,
            capabilities,
        })
    }

    fn query_sid_groups(
        token: HANDLE,
        class: TOKEN_INFORMATION_CLASS,
    ) -> io::Result<(Vec<u8>, Vec<Vec<u8>>)> {
        let information = query_token_information(token, class, size_of::<u32>())?;
        information.canonical_sid_groups()
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

        fn canonical_sid_groups(&self) -> io::Result<(Vec<u8>, Vec<Vec<u8>>)> {
            let count = usize::try_from(self.read::<u32>()?)
                .map_err(|_| io::Error::other("Windows SID group count does not fit usize"))?;
            let entries_offset = std::mem::offset_of!(TOKEN_GROUPS, Groups);
            if count == 0 {
                return Ok((0_u32.to_le_bytes().to_vec(), Vec::new()));
            }
            let entries_bytes = count
                .checked_mul(size_of::<SID_AND_ATTRIBUTES>())
                .and_then(|bytes| entries_offset.checked_add(bytes))
                .ok_or_else(|| io::Error::other("Windows SID group array size overflow"))?;
            if entries_bytes > self.returned_len {
                return Err(io::Error::other(
                    "Windows token returned a truncated SID group array",
                ));
            }
            // SAFETY: the complete flexible-array prefix was checked inside the retained buffer.
            let entries = unsafe {
                std::slice::from_raw_parts(
                    self.storage
                        .as_ptr()
                        .cast::<u8>()
                        .add(entries_offset)
                        .cast::<SID_AND_ATTRIBUTES>(),
                    count,
                )
            };
            let mut canonical = Vec::new();
            canonical.try_reserve_exact(count).map_err(|source| {
                io::Error::other(format!("could not allocate SID group list: {source}"))
            })?;
            for entry in entries {
                canonical.push((self.copy_sid(entry.Sid)?, entry.Attributes));
            }
            canonical.sort_unstable();

            let mut encoded = Vec::new();
            encoded.extend_from_slice(
                &u32::try_from(canonical.len())
                    .map_err(|_| io::Error::other("Windows SID group count does not fit u32"))?
                    .to_le_bytes(),
            );
            let sid_values = canonical.iter().map(|(sid, _)| sid.clone()).collect();
            for (sid, attributes) in canonical {
                encoded.extend_from_slice(&attributes.to_le_bytes());
                encoded.extend_from_slice(
                    &u32::try_from(sid.len())
                        .map_err(|_| io::Error::other("Windows SID length does not fit u32"))?
                        .to_le_bytes(),
                );
                encoded.extend_from_slice(&sid);
            }
            Ok((encoded, sid_values))
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
    use super::{SecurityContextError, SecurityContextMaterial};

    pub(super) fn current_material() -> Result<SecurityContextMaterial, SecurityContextError> {
        Err(SecurityContextError::UnsupportedPlatform)
    }

    pub(super) fn process_material(
        _process_id: u32,
    ) -> Result<SecurityContextMaterial, SecurityContextError> {
        Err(SecurityContextError::UnsupportedPlatform)
    }
}

#[cfg(all(test, any(target_os = "linux", target_os = "macos", windows)))]
mod tests {
    use super::*;

    fn material() -> SecurityContextMaterial {
        SecurityContextMaterial {
            platform: b"windows",
            user: vec![1, 2],
            logon: Some(vec![3, 4]),
            integrity: Some(vec![5, 6]),
            elevation: 2,
            restricted: false,
            restricted_sids: vec![7, 8],
            app_container: None,
            capabilities: vec![9, 10],
        }
    }

    #[test]
    fn every_security_field_is_bound_into_the_identity() {
        let baseline = derive_security_context(material()).unwrap();

        let mut changed = material();
        changed.platform = b"linux";
        assert_ne!(derive_security_context(changed).unwrap(), baseline);
        let mut changed = material();
        changed.user.push(9);
        assert_ne!(derive_security_context(changed).unwrap(), baseline);
        let mut changed = material();
        changed.logon = Some(vec![9]);
        assert_ne!(derive_security_context(changed).unwrap(), baseline);
        let mut changed = material();
        changed.integrity = Some(vec![9]);
        assert_ne!(derive_security_context(changed).unwrap(), baseline);
        let mut changed = material();
        changed.elevation = 3;
        assert_ne!(derive_security_context(changed).unwrap(), baseline);
        let mut changed = material();
        changed.restricted = true;
        assert_ne!(derive_security_context(changed).unwrap(), baseline);
        let mut changed = material();
        changed.restricted_sids.push(9);
        assert_ne!(derive_security_context(changed).unwrap(), baseline);
        let mut changed = material();
        changed.app_container = Some(vec![9]);
        assert_ne!(derive_security_context(changed).unwrap(), baseline);
        let mut changed = material();
        changed.capabilities.push(9);
        assert_ne!(derive_security_context(changed).unwrap(), baseline);
    }
}
