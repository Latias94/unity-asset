use std::collections::TryReserveError;
use std::fmt;
use std::str::FromStr;

use thiserror::Error;
use unicode_normalization::UnicodeNormalization;
use unity_asset_core::{AssetLoadBudget, BudgetError, DigestV1};

const MAX_LOGICAL_NAME_BYTES: usize = 4_096;
const MAX_COMPONENT_BYTES: usize = 255;

/// A portable, immutable relative name for a prepared artifact.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct LogicalArtifactName {
    value: String,
    portability_key: String,
}

impl LogicalArtifactName {
    pub fn new(value: impl AsRef<str>) -> Result<Self, ArtifactNameError> {
        let value = try_copy_string(value.as_ref(), "logical artifact name")?;
        Self::from_string(value)
    }

    /// Derive a portable content-addressed name for a generated sidecar artifact.
    pub fn sidecar(
        directory: Option<&Self>,
        base_name: &str,
        digest: DigestV1,
    ) -> Result<Self, ArtifactNameError> {
        sidecar_logical_name(directory, base_name, digest)
    }

    /// Derive a sidecar name while bounding every simultaneously live construction allocation.
    ///
    /// Temporary strings are checked as a peak working set. Only the returned name's exact
    /// retained capacities are consumed from the monotonic caller budget.
    pub fn sidecar_with_budget(
        directory: Option<&Self>,
        base_name: &str,
        digest: DigestV1,
        budget: &mut AssetLoadBudget,
    ) -> Result<Self, ArtifactNameError> {
        sidecar_logical_name_with_budget(directory, base_name, digest, budget)
    }

    fn from_string(value: String) -> Result<Self, ArtifactNameError> {
        validate_logical_name(&value)?;
        let portability_key = portability_key(&value)?;
        Ok(Self {
            value,
            portability_key,
        })
    }

    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.value
    }

    /// Returns the exact heap capacity retained by this validated logical name.
    ///
    /// Callers that keep a name outside an [`ArtifactBatch`](super::ArtifactBatch) use this to
    /// charge their own allocation budget. Artifact declarations account for the same storage
    /// through the private batch hook below.
    pub fn retained_bytes(&self) -> Result<u64, ArtifactNameError> {
        self.heap_bytes()
    }

    /// Returns the portable comparison key derived when this name was validated.
    #[must_use]
    pub fn portability_key(&self) -> &str {
        &self.portability_key
    }

    pub(crate) fn heap_bytes(&self) -> Result<u64, ArtifactNameError> {
        let bytes = self
            .value
            .capacity()
            .checked_add(self.portability_key.capacity())
            .ok_or(ArtifactNameError::ArithmeticOverflow {
                resource: "logical artifact name metadata",
            })?;
        u64::try_from(bytes).map_err(|_| ArtifactNameError::ArithmeticOverflow {
            resource: "logical artifact name metadata",
        })
    }
}

impl AsRef<str> for LogicalArtifactName {
    fn as_ref(&self) -> &str {
        self.as_str()
    }
}

impl fmt::Display for LogicalArtifactName {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

impl FromStr for LogicalArtifactName {
    type Err = ArtifactNameError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        Self::new(value)
    }
}

impl TryFrom<String> for LogicalArtifactName {
    type Error = ArtifactNameError;

    fn try_from(value: String) -> Result<Self, Self::Error> {
        Self::from_string(value)
    }
}

impl TryFrom<&str> for LogicalArtifactName {
    type Error = ArtifactNameError;

    fn try_from(value: &str) -> Result<Self, Self::Error> {
        Self::new(value)
    }
}

#[derive(Debug, Error)]
pub enum ArtifactNameError {
    #[error("logical artifact name must not be empty")]
    Empty,
    #[error("logical artifact name has {actual} bytes; maximum is {max}")]
    NameTooLong { actual: usize, max: usize },
    #[error("logical artifact name must be relative")]
    AbsolutePath,
    #[error("UNC logical artifact names are not allowed")]
    UncPath,
    #[error("Windows rooted logical artifact names are not allowed")]
    WindowsRootedPath,
    #[error("Windows drive prefixes are not allowed in logical artifact names")]
    WindowsDrivePrefix,
    #[error("logical artifact name must not end with a slash")]
    TrailingSlash,
    #[error("backslash at byte {byte_offset} is not allowed in a logical artifact name")]
    Backslash { byte_offset: usize },
    #[error("colon at byte {byte_offset} is not allowed in a logical artifact name")]
    AlternateDataStream { byte_offset: usize },
    #[error(
        "control character U+{code_point:04X} at byte {byte_offset} is not allowed in a logical artifact name"
    )]
    ControlCharacter { byte_offset: usize, code_point: u32 },
    #[error(
        "component {index} contains Windows-illegal character {character:?} at component byte {byte_offset}"
    )]
    ForbiddenWindowsCharacter {
        index: usize,
        byte_offset: usize,
        character: char,
    },
    #[error("logical artifact name contains an empty component at index {index}")]
    EmptyComponent { index: usize },
    #[error("logical artifact name contains '.' at component index {index}")]
    CurrentDirectoryComponent { index: usize },
    #[error("logical artifact name contains '..' at component index {index}")]
    ParentDirectoryComponent { index: usize },
    #[error("component {index} has {actual} bytes; maximum is {max}")]
    ComponentTooLong {
        index: usize,
        actual: usize,
        max: usize,
    },
    #[error("component {index} must not end with a dot or space")]
    TrailingDotOrSpace { index: usize },
    #[error("component {index} uses reserved Windows device stem {component:?}")]
    ReservedWindowsDevice { index: usize, component: String },
    #[error("sidecar base name must be one logical path component: {base:?}")]
    SidecarBaseMustBeComponent { base: String },
    #[error("failed to allocate {requested} bytes for {resource}")]
    Allocation {
        resource: &'static str,
        requested: usize,
        #[source]
        source: TryReserveError,
    },
    #[error("logical artifact name arithmetic overflow for {resource}")]
    ArithmeticOverflow { resource: &'static str },
    #[error("logical artifact outputs {existing} and {incoming} use the same exact name")]
    ExactCollision { existing: usize, incoming: usize },
    #[error(
        "logical artifact outputs {existing} and {incoming} use the same portable filesystem name"
    )]
    PortabilityCollision { existing: usize, incoming: usize },
    #[error(transparent)]
    Budget(#[from] BudgetError),
}

pub(crate) fn validate_unique_names<'name>(
    ordinals: &mut [usize],
    name_at: impl Fn(usize) -> &'name LogicalArtifactName,
) -> Result<(), ArtifactNameError> {
    ordinals.sort_unstable_by(|left, right| name_at(*left).as_str().cmp(name_at(*right).as_str()));
    for pair in ordinals.windows(2) {
        let existing = name_at(pair[0]);
        let incoming = name_at(pair[1]);
        if existing.as_str() == incoming.as_str() {
            return Err(ArtifactNameError::ExactCollision {
                existing: pair[0],
                incoming: pair[1],
            });
        }
    }

    ordinals.sort_unstable_by(|left, right| {
        let left = name_at(*left);
        let right = name_at(*right);
        left.portability_key()
            .cmp(right.portability_key())
            .then_with(|| left.as_str().cmp(right.as_str()))
    });
    for pair in ordinals.windows(2) {
        let existing = name_at(pair[0]);
        let incoming = name_at(pair[1]);
        if existing.portability_key() == incoming.portability_key() {
            return Err(ArtifactNameError::PortabilityCollision {
                existing: pair[0],
                incoming: pair[1],
            });
        }
    }
    Ok(())
}

pub(crate) fn sidecar_logical_name(
    directory: Option<&LogicalArtifactName>,
    base_name: &str,
    digest: DigestV1,
) -> Result<LogicalArtifactName, ArtifactNameError> {
    let value = sidecar_logical_name_value(directory, base_name, digest, None)?;
    LogicalArtifactName::try_from(value)
}

fn sidecar_logical_name_with_budget(
    directory: Option<&LogicalArtifactName>,
    base_name: &str,
    digest: DigestV1,
    budget: &mut AssetLoadBudget,
) -> Result<LogicalArtifactName, ArtifactNameError> {
    let value = sidecar_logical_name_value(directory, base_name, digest, Some(budget))?;
    validate_logical_name(&value)?;
    let portability_key = portability_key_with_budget(&value, budget)?;
    let name = LogicalArtifactName {
        value,
        portability_key,
    };
    budget.consume_bytes(name.heap_bytes()?)?;
    Ok(name)
}

fn sidecar_logical_name_value(
    directory: Option<&LogicalArtifactName>,
    base_name: &str,
    digest: DigestV1,
    budget: Option<&AssetLoadBudget>,
) -> Result<String, ArtifactNameError> {
    if base_name.contains('/') {
        check_peak_bytes(budget, base_name.len(), "sidecar base name error")?;
        return Err(ArtifactNameError::SidecarBaseMustBeComponent {
            base: try_copy_string(base_name, "sidecar base name")?,
        });
    }
    validate_sidecar_base_name(base_name)?;

    let component = sidecar_component(base_name, digest, budget)?;
    let value = match directory {
        Some(directory) => {
            let requested = directory
                .as_str()
                .len()
                .checked_add(1)
                .and_then(|length| length.checked_add(component.len()))
                .ok_or(ArtifactNameError::ArithmeticOverflow {
                    resource: "sidecar logical name",
                })?;
            let peak = component.capacity().checked_add(requested).ok_or(
                ArtifactNameError::ArithmeticOverflow {
                    resource: "sidecar logical name peak allocation",
                },
            )?;
            check_peak_bytes(budget, peak, "sidecar logical name")?;
            let mut value = String::new();
            value
                .try_reserve_exact(requested)
                .map_err(|source| ArtifactNameError::Allocation {
                    resource: "sidecar logical name",
                    requested,
                    source,
                })?;
            value.push_str(directory.as_str());
            value.push('/');
            value.push_str(&component);
            value
        }
        None => component,
    };
    Ok(value)
}

fn validate_logical_name(value: &str) -> Result<(), ArtifactNameError> {
    validate_logical_name_with_component_limit(value, true)
}

fn validate_sidecar_base_name(value: &str) -> Result<(), ArtifactNameError> {
    validate_logical_name_with_component_limit(value, false)
}

fn validate_logical_name_with_component_limit(
    value: &str,
    enforce_component_limit: bool,
) -> Result<(), ArtifactNameError> {
    if value.is_empty() {
        return Err(ArtifactNameError::Empty);
    }
    if value.len() > MAX_LOGICAL_NAME_BYTES {
        return Err(ArtifactNameError::NameTooLong {
            actual: value.len(),
            max: MAX_LOGICAL_NAME_BYTES,
        });
    }
    if value.starts_with("//") || value.starts_with("\\\\") {
        return Err(ArtifactNameError::UncPath);
    }
    if has_windows_drive_prefix(value) {
        return Err(ArtifactNameError::WindowsDrivePrefix);
    }
    if value.starts_with('/') {
        return Err(ArtifactNameError::AbsolutePath);
    }
    if value.starts_with('\\') {
        return Err(ArtifactNameError::WindowsRootedPath);
    }
    if value.ends_with('/') {
        return Err(ArtifactNameError::TrailingSlash);
    }
    if let Some(byte_offset) = value.find('\\') {
        return Err(ArtifactNameError::Backslash { byte_offset });
    }
    if let Some(byte_offset) = value.find(':') {
        return Err(ArtifactNameError::AlternateDataStream { byte_offset });
    }
    if let Some((byte_offset, character)) = value
        .char_indices()
        .find(|(_, character)| character.is_control())
    {
        return Err(ArtifactNameError::ControlCharacter {
            byte_offset,
            code_point: u32::from(character),
        });
    }

    for (index, component) in value.split('/').enumerate() {
        validate_component(index, component, enforce_component_limit)?;
    }
    Ok(())
}

fn validate_component(
    index: usize,
    component: &str,
    enforce_length_limit: bool,
) -> Result<(), ArtifactNameError> {
    if component.is_empty() {
        return Err(ArtifactNameError::EmptyComponent { index });
    }
    if component == "." {
        return Err(ArtifactNameError::CurrentDirectoryComponent { index });
    }
    if component == ".." {
        return Err(ArtifactNameError::ParentDirectoryComponent { index });
    }
    if enforce_length_limit && component.len() > MAX_COMPONENT_BYTES {
        return Err(ArtifactNameError::ComponentTooLong {
            index,
            actual: component.len(),
            max: MAX_COMPONENT_BYTES,
        });
    }
    if let Some((byte_offset, character)) = component
        .char_indices()
        .find(|(_, character)| matches!(character, '<' | '>' | '"' | '|' | '?' | '*'))
    {
        return Err(ArtifactNameError::ForbiddenWindowsCharacter {
            index,
            byte_offset,
            character,
        });
    }
    if matches!(component.as_bytes().last(), Some(b'.' | b' ')) {
        return Err(ArtifactNameError::TrailingDotOrSpace { index });
    }
    if is_reserved_windows_device(component) {
        return Err(ArtifactNameError::ReservedWindowsDevice {
            index,
            component: try_copy_string(component, "reserved device component")?,
        });
    }
    Ok(())
}

fn has_windows_drive_prefix(value: &str) -> bool {
    let bytes = value.as_bytes();
    bytes.len() >= 2 && bytes[0].is_ascii_alphabetic() && bytes[1] == b':'
}

fn is_reserved_windows_device(component: &str) -> bool {
    let stem = component.split('.').next().unwrap_or(component);
    ["CON", "PRN", "AUX", "NUL", "CLOCK$", "CONIN$", "CONOUT$"]
        .iter()
        .any(|reserved| stem.eq_ignore_ascii_case(reserved))
        || reserved_numbered_device(stem, "COM")
        || reserved_numbered_device(stem, "LPT")
}

fn reserved_numbered_device(stem: &str, prefix: &str) -> bool {
    let Some(candidate_prefix) = stem.get(..prefix.len()) else {
        return false;
    };
    if !candidate_prefix.eq_ignore_ascii_case(prefix) {
        return false;
    }
    matches!(
        stem.get(prefix.len()..),
        Some("1" | "2" | "3" | "4" | "5" | "6" | "7" | "8" | "9" | "\u{b9}" | "\u{b2}" | "\u{b3}")
    )
}

fn portability_key(value: &str) -> Result<String, ArtifactNameError> {
    portability_key_inner(value, None)
}

fn portability_key_with_budget(
    value: &String,
    budget: &AssetLoadBudget,
) -> Result<String, ArtifactNameError> {
    portability_key_inner(value, Some((budget, value.capacity())))
}

fn portability_key_inner(
    value: &str,
    budget: Option<(&AssetLoadBudget, usize)>,
) -> Result<String, ArtifactNameError> {
    // NFKC plus lowercase catches common filesystem aliases without claiming full Unicode
    // case-folding semantics, which would require a separately audited dependency and policy.
    let requested =
        value
            .nfkc()
            .flat_map(char::to_lowercase)
            .try_fold(0_usize, |length, character| {
                length.checked_add(character.len_utf8()).ok_or(
                    ArtifactNameError::ArithmeticOverflow {
                        resource: "logical name portability key",
                    },
                )
            })?;
    if let Some((budget, value_capacity)) = budget {
        let peak =
            value_capacity
                .checked_add(requested)
                .ok_or(ArtifactNameError::ArithmeticOverflow {
                    resource: "logical name normalization peak allocation",
                })?;
        check_peak_bytes(Some(budget), peak, "logical name portability key")?;
    }
    let mut key = String::new();
    key.try_reserve_exact(requested)
        .map_err(|source| ArtifactNameError::Allocation {
            resource: "logical name portability key",
            requested,
            source,
        })?;
    for character in value.nfkc().flat_map(char::to_lowercase) {
        key.push(character);
    }
    Ok(key)
}

fn sidecar_component(
    base_name: &str,
    digest: DigestV1,
    budget: Option<&AssetLoadBudget>,
) -> Result<String, ArtifactNameError> {
    let suffix = digest_hex(digest, budget)?;
    let candidate_extension = base_name
        .rsplit_once('.')
        .filter(|(stem, extension)| !stem.is_empty() && !extension.is_empty());
    let digest_bytes =
        suffix
            .len()
            .checked_add(1)
            .ok_or(ArtifactNameError::ArithmeticOverflow {
                resource: "sidecar component",
            })?;
    let prefix_and_extension_bytes = MAX_COMPONENT_BYTES.checked_sub(digest_bytes).ok_or(
        ArtifactNameError::ArithmeticOverflow {
            resource: "sidecar component",
        },
    )?;

    let (prefix_source, extension) = match candidate_extension {
        Some((stem, extension)) if extension.len() < prefix_and_extension_bytes => {
            (stem, Some(extension))
        }
        _ => (base_name, None),
    };
    let extension_bytes = extension.map_or(0, |extension| extension.len() + 1);
    let prefix_bytes = prefix_and_extension_bytes
        .checked_sub(extension_bytes)
        .ok_or(ArtifactNameError::ArithmeticOverflow {
            resource: "sidecar component",
        })?;
    let prefix = utf8_prefix(prefix_source, prefix_bytes);
    let requested = prefix
        .len()
        .checked_add(digest_bytes)
        .and_then(|length| length.checked_add(extension_bytes))
        .ok_or(ArtifactNameError::ArithmeticOverflow {
            resource: "sidecar component",
        })?;
    let peak =
        suffix
            .capacity()
            .checked_add(requested)
            .ok_or(ArtifactNameError::ArithmeticOverflow {
                resource: "sidecar component peak allocation",
            })?;
    check_peak_bytes(budget, peak, "sidecar component")?;
    let mut component = String::new();
    component
        .try_reserve_exact(requested)
        .map_err(|source| ArtifactNameError::Allocation {
            resource: "sidecar component",
            requested,
            source,
        })?;
    component.push_str(prefix);
    component.push('-');
    component.push_str(&suffix);
    if let Some(extension) = extension {
        component.push('.');
        component.push_str(extension);
    }
    debug_assert!(component.len() <= MAX_COMPONENT_BYTES);
    Ok(component)
}

fn utf8_prefix(value: &str, max_bytes: usize) -> &str {
    if value.len() <= max_bytes {
        return value;
    }

    let mut end = max_bytes;
    while !value.is_char_boundary(end) {
        end -= 1;
    }
    &value[..end]
}

fn digest_hex(
    digest: DigestV1,
    budget: Option<&AssetLoadBudget>,
) -> Result<String, ArtifactNameError> {
    const HEX: &[u8; 16] = b"0123456789abcdef";

    let requested = DigestV1::BYTE_LEN * 2;
    check_peak_bytes(budget, requested, "sidecar digest suffix")?;
    let mut encoded = String::new();
    encoded
        .try_reserve_exact(requested)
        .map_err(|source| ArtifactNameError::Allocation {
            resource: "sidecar digest suffix",
            requested,
            source,
        })?;
    for byte in digest.as_bytes() {
        encoded.push(char::from(HEX[usize::from(byte >> 4)]));
        encoded.push(char::from(HEX[usize::from(byte & 0x0f)]));
    }
    Ok(encoded)
}

fn check_peak_bytes(
    budget: Option<&AssetLoadBudget>,
    bytes: usize,
    resource: &'static str,
) -> Result<(), ArtifactNameError> {
    let bytes =
        u64::try_from(bytes).map_err(|_| ArtifactNameError::ArithmeticOverflow { resource })?;
    if let Some(budget) = budget {
        budget.check_bytes(bytes)?;
    }
    Ok(())
}

fn try_copy_string(value: &str, resource: &'static str) -> Result<String, ArtifactNameError> {
    let mut copy = String::new();
    copy.try_reserve_exact(value.len())
        .map_err(|source| ArtifactNameError::Allocation {
            resource,
            requested: value.len(),
            source,
        })?;
    copy.push_str(value);
    Ok(copy)
}

#[cfg(test)]
mod tests {
    use super::*;
    use unity_asset_core::AssetLoadLimits;

    fn name(value: &str) -> LogicalArtifactName {
        LogicalArtifactName::new(value).expect("test logical artifact name should be valid")
    }

    fn validate_names(values: &[&str]) -> Result<(), ArtifactNameError> {
        let names = values.iter().map(|value| name(value)).collect::<Vec<_>>();
        let mut ordinals = (0..names.len()).collect::<Vec<_>>();
        validate_unique_names(&mut ordinals, |ordinal| &names[ordinal])
    }

    #[test]
    fn accepts_portable_slash_separated_relative_names() {
        let value = name("build/main.assets_data/CAB-content.resS");

        assert_eq!(value.as_str(), "build/main.assets_data/CAB-content.resS");
        assert_eq!(value.to_string(), value.as_str());
    }

    #[test]
    fn rejects_traversal_and_empty_components() {
        assert!(matches!(
            LogicalArtifactName::new("../outside"),
            Err(ArtifactNameError::ParentDirectoryComponent { index: 0 })
        ));
        assert!(matches!(
            LogicalArtifactName::new("inside/../outside"),
            Err(ArtifactNameError::ParentDirectoryComponent { index: 1 })
        ));
        assert!(matches!(
            LogicalArtifactName::new("./inside"),
            Err(ArtifactNameError::CurrentDirectoryComponent { index: 0 })
        ));
        assert!(matches!(
            LogicalArtifactName::new("inside//file"),
            Err(ArtifactNameError::EmptyComponent { index: 1 })
        ));
    }

    #[test]
    fn rejects_absolute_unc_drive_and_ads_names() {
        assert!(matches!(
            LogicalArtifactName::new("/absolute"),
            Err(ArtifactNameError::AbsolutePath)
        ));
        assert!(matches!(
            LogicalArtifactName::new("//server/share"),
            Err(ArtifactNameError::UncPath)
        ));
        assert!(matches!(
            LogicalArtifactName::new(r"\\server\share"),
            Err(ArtifactNameError::UncPath)
        ));
        assert!(matches!(
            LogicalArtifactName::new("C:/drive"),
            Err(ArtifactNameError::WindowsDrivePrefix)
        ));
        assert!(matches!(
            LogicalArtifactName::new("asset.resS:stream"),
            Err(ArtifactNameError::AlternateDataStream { byte_offset: 10 })
        ));
        assert!(matches!(
            LogicalArtifactName::new(r"inside\file"),
            Err(ArtifactNameError::Backslash { .. })
        ));
    }

    #[test]
    fn rejects_controls_and_enforces_byte_limits() {
        assert!(matches!(
            LogicalArtifactName::new("inside/line\nfeed"),
            Err(ArtifactNameError::ControlCharacter { .. })
        ));
        assert!(matches!(
            LogicalArtifactName::new("inside/nul\0byte"),
            Err(ArtifactNameError::ControlCharacter { code_point: 0, .. })
        ));
        assert!(matches!(
            LogicalArtifactName::new("x".repeat(MAX_COMPONENT_BYTES + 1)),
            Err(ArtifactNameError::ComponentTooLong { .. })
        ));
        assert!(matches!(
            LogicalArtifactName::new("x".repeat(MAX_LOGICAL_NAME_BYTES + 1)),
            Err(ArtifactNameError::NameTooLong {
                actual,
                max,
            }) if actual == MAX_LOGICAL_NAME_BYTES + 1 && max == MAX_LOGICAL_NAME_BYTES
        ));
    }

    #[test]
    fn rejects_windows_illegal_component_characters_with_typed_location() {
        for character in ['<', '>', '"', '|', '?', '*'] {
            let value = format!("inside/file{character}name.assets");
            assert!(matches!(
                LogicalArtifactName::new(value),
                Err(ArtifactNameError::ForbiddenWindowsCharacter {
                    index: 1,
                    byte_offset: 4,
                    character: rejected,
                }) if rejected == character
            ));
        }
    }

    #[test]
    fn rejects_trailing_slashes_dots_and_spaces() {
        assert!(matches!(
            LogicalArtifactName::new("directory/"),
            Err(ArtifactNameError::TrailingSlash)
        ));
        for value in ["artifact.", "artifact ", "directory./artifact"] {
            assert!(matches!(
                LogicalArtifactName::new(value),
                Err(ArtifactNameError::TrailingDotOrSpace { .. })
            ));
        }
    }

    #[test]
    fn rejects_windows_reserved_device_stems() {
        for value in [
            "CON",
            "con.txt",
            "nested/AuX.bin",
            "NUL.resource",
            "COM1",
            "com9.data",
            "COM\u{b9}.data",
            "LPT1",
            "lpt9.asset",
            "LPT\u{b2}.asset",
        ] {
            assert!(matches!(
                LogicalArtifactName::new(value),
                Err(ArtifactNameError::ReservedWindowsDevice { .. })
            ));
        }

        assert!(LogicalArtifactName::new("COM10").is_ok());
        assert!(LogicalArtifactName::new("LPT0.asset").is_ok());
    }

    #[test]
    fn uniqueness_validation_distinguishes_exact_and_portability_collisions() {
        assert!(matches!(
            validate_names(&["Assets/Main.assets", "Assets/Main.assets"]),
            Err(ArtifactNameError::ExactCollision {
                existing: 0,
                incoming: 1,
            })
        ));
        assert!(matches!(
            validate_names(&["Assets/Main.assets", "assets/main.ASSETS"]),
            Err(ArtifactNameError::PortabilityCollision {
                existing: 0,
                incoming: 1,
            })
        ));
    }

    #[test]
    fn uniqueness_validation_uses_nfkc_before_lowercase_for_unicode_collisions() {
        assert!(matches!(
            validate_names(&["caf\u{e9}.assets", "cafe\u{301}.assets"]),
            Err(ArtifactNameError::PortabilityCollision { .. })
        ));

        assert!(matches!(
            validate_names(&["A.assets", "\u{ff21}.assets"]),
            Err(ArtifactNameError::PortabilityCollision { .. })
        ));
    }

    #[test]
    fn sidecar_suffix_is_stable_and_inserted_before_the_extension() {
        let digest = DigestV1::from_bytes([0xab; DigestV1::BYTE_LEN]);
        let directory = name("main.assets_data");
        let expected = format!("main.assets_data/CAB-{}.resS", "ab".repeat(32));

        let first = sidecar_logical_name(Some(&directory), "CAB.resS", digest).unwrap();
        let second = sidecar_logical_name(Some(&directory), "CAB.resS", digest).unwrap();

        assert_eq!(first, second);
        assert_eq!(first.as_str(), expected);
    }

    #[test]
    fn sidecar_truncates_a_maximum_length_base_and_keeps_the_full_digest() {
        let digest = DigestV1::from_bytes([0xab; DigestV1::BYTE_LEN]);
        let base_name = "x".repeat(MAX_COMPONENT_BYTES);
        let expected_prefix_bytes = MAX_COMPONENT_BYTES - 1 - (DigestV1::BYTE_LEN * 2);

        let sidecar = sidecar_logical_name(None, &base_name, digest).unwrap();

        assert_eq!(sidecar.as_str().len(), MAX_COMPONENT_BYTES);
        assert_eq!(
            sidecar.as_str(),
            format!("{}-{}", "x".repeat(expected_prefix_bytes), "ab".repeat(32))
        );
    }

    #[test]
    fn sidecar_truncates_stems_on_utf8_boundaries_and_preserves_extensions() {
        let digest = DigestV1::from_bytes([0xcd; DigestV1::BYTE_LEN]);
        let base_name = format!("{}.asset", "\u{754c}".repeat(83));
        assert_eq!(base_name.len(), MAX_COMPONENT_BYTES);

        let first = sidecar_logical_name(None, &base_name, digest).unwrap();
        let second = sidecar_logical_name(None, &base_name, digest).unwrap();

        assert_eq!(first, second);
        assert!(first.as_str().len() <= MAX_COMPONENT_BYTES);
        assert!(first.as_str().ends_with(".asset"));
        assert!(first.as_str().contains(&format!("-{}", "cd".repeat(32))));
        assert_eq!(
            first
                .as_str()
                .chars()
                .take_while(|ch| *ch == '\u{754c}')
                .count(),
            61
        );
    }

    #[test]
    fn sidecar_accepts_an_overlong_utf8_base_and_validates_the_generated_component() {
        let digest = DigestV1::from_bytes([0x5a; DigestV1::BYTE_LEN]);
        let base_name = format!("{}.resS", "资源".repeat(200));

        let sidecar = LogicalArtifactName::sidecar(None, &base_name, digest).unwrap();

        assert!(sidecar.as_str().len() <= MAX_COMPONENT_BYTES);
        assert!(sidecar.as_str().is_char_boundary(sidecar.as_str().len()));
        assert!(sidecar.as_str().ends_with(".resS"));
        assert!(sidecar.as_str().contains(&format!("-{}", "5a".repeat(32))));
        validate_logical_name(sidecar.as_str()).unwrap();
    }

    #[test]
    fn sidecar_rejects_invalid_raw_base_content_before_truncation() {
        let digest = DigestV1::from_bytes([0x5b; DigestV1::BYTE_LEN]);
        let long_prefix = "x".repeat(MAX_COMPONENT_BYTES + 32);

        assert!(matches!(
            LogicalArtifactName::sidecar(None, &format!("{long_prefix}?tail"), digest),
            Err(ArtifactNameError::ForbiddenWindowsCharacter { character: '?', .. })
        ));
        assert!(matches!(
            LogicalArtifactName::sidecar(None, &format!("{long_prefix}\\tail"), digest),
            Err(ArtifactNameError::Backslash { .. })
        ));
        for base in [".", "..", "CON", "payload.", "payload "] {
            assert!(LogicalArtifactName::sidecar(None, base, digest).is_err());
        }
    }

    #[test]
    fn budgeted_sidecar_checks_transient_peak_and_consumes_only_retained_name() {
        let digest = DigestV1::from_bytes([0x5c; DigestV1::BYTE_LEN]);
        let directory = name("d");
        let base_name = format!("{}.resS", "Ａ".repeat(80));
        let component = sidecar_component(&base_name, digest, None).unwrap();
        let unbudgeted =
            LogicalArtifactName::sidecar(Some(&directory), &base_name, digest).unwrap();
        let transient_peak = u64::try_from(
            component
                .capacity()
                .checked_add(unbudgeted.value.capacity())
                .unwrap(),
        )
        .unwrap();
        let retained = unbudgeted.retained_bytes().unwrap();
        assert!(transient_peak > retained);

        let mut exact = AssetLoadBudget::new(AssetLoadLimits {
            max_bytes: transient_peak,
            ..AssetLoadLimits::default()
        })
        .unwrap();
        let budgeted = LogicalArtifactName::sidecar_with_budget(
            Some(&directory),
            &base_name,
            digest,
            &mut exact,
        )
        .unwrap();
        assert_eq!(budgeted, unbudgeted);
        assert_eq!(exact.usage().bytes, retained);

        let mut one_short = AssetLoadBudget::new(AssetLoadLimits {
            max_bytes: transient_peak - 1,
            ..AssetLoadLimits::default()
        })
        .unwrap();
        assert!(matches!(
            LogicalArtifactName::sidecar_with_budget(
                Some(&directory),
                &base_name,
                digest,
                &mut one_short,
            ),
            Err(ArtifactNameError::Budget(BudgetError::Exceeded {
                resource: "bytes",
                ..
            }))
        ));
        assert_eq!(one_short.usage(), Default::default());
    }

    #[test]
    fn sidecar_treats_an_unretainable_extension_as_part_of_the_truncated_base() {
        let digest = DigestV1::from_bytes([0xef; DigestV1::BYTE_LEN]);
        let base_name = format!("a.{}", "x".repeat(MAX_COMPONENT_BYTES - 2));

        let sidecar = sidecar_logical_name(None, &base_name, digest).unwrap();

        assert_eq!(sidecar.as_str().len(), MAX_COMPONENT_BYTES);
        assert!(sidecar.as_str().starts_with("a."));
        assert!(sidecar.as_str().ends_with(&"ef".repeat(32)));
    }

    #[test]
    fn uniqueness_validation_accepts_names_regardless_of_input_order() {
        assert!(validate_names(&["z.assets", "a.assets", "m.assets"]).is_ok());
        assert!(validate_names(&["m.assets", "z.assets", "a.assets"]).is_ok());
    }

    #[test]
    fn sidecar_base_must_be_one_component() {
        let digest = DigestV1::from_bytes([7; DigestV1::BYTE_LEN]);
        assert!(matches!(
            sidecar_logical_name(None, "nested/CAB.resS", digest),
            Err(ArtifactNameError::SidecarBaseMustBeComponent { .. })
        ));
        assert!(sidecar_logical_name(None, "CAB.resS", digest).is_ok());
    }
}
