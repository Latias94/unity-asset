use std::cmp::Ordering;
use std::path::Path;

#[cfg(windows)]
use std::ffi::OsStr;

#[cfg(windows)]
const WINDOWS_COMPONENT_STACK_UNITS: usize = 96;

pub(crate) fn compare_portable_paths(left: &str, right: &str) -> Ordering {
    let mut left_components = left.split('/');
    let mut right_components = right.split('/');
    loop {
        match (left_components.next(), right_components.next()) {
            (None, None) => return Ordering::Equal,
            (None, Some(_)) => return Ordering::Less,
            (Some(_), None) => return Ordering::Greater,
            (Some(left), Some(right)) => {
                let ordering = portable_component_cmp(left, right);
                if ordering != Ordering::Equal {
                    return ordering;
                }
            }
        }
    }
}

#[cfg(not(windows))]
fn portable_component_cmp(left: &str, right: &str) -> Ordering {
    left.cmp(right)
}

#[cfg(windows)]
fn portable_component_cmp(left: &str, right: &str) -> Ordering {
    windows_component_cmp(OsStr::new(left), OsStr::new(right))
}

#[cfg(windows)]
enum WindowsWideUnits {
    Stack {
        units: [u16; WINDOWS_COMPONENT_STACK_UNITS],
        len: usize,
    },
    Heap(Vec<u16>),
}

#[cfg(windows)]
impl WindowsWideUnits {
    fn encode(value: &OsStr) -> Self {
        use std::os::windows::ffi::OsStrExt as _;

        let mut encoded = value.encode_wide();
        let mut units = [0_u16; WINDOWS_COMPONENT_STACK_UNITS];
        let mut len = 0;
        while len < units.len() {
            let Some(unit) = encoded.next() else {
                return Self::Stack { units, len };
            };
            units[len] = unit;
            len += 1;
        }
        let Some(unit) = encoded.next() else {
            return Self::Stack { units, len };
        };
        let mut heap = Vec::from(units);
        heap.push(unit);
        heap.extend(encoded);
        Self::Heap(heap)
    }

    fn as_slice(&self) -> &[u16] {
        match self {
            Self::Stack { units, len } => &units[..*len],
            Self::Heap(units) => units,
        }
    }
}

#[cfg(not(windows))]
pub(crate) fn strip_prefix<'path>(prefix: &Path, path: &'path Path) -> Result<&'path Path, ()> {
    path.strip_prefix(prefix).map_err(|_| ())
}

#[cfg(windows)]
pub(crate) fn strip_prefix<'path>(prefix: &Path, path: &'path Path) -> Result<&'path Path, ()> {
    use std::path::Component;

    let mut prefix_components = prefix.components();
    let mut path_components = path.components();
    let (Some(Component::Prefix(prefix_root)), Some(Component::Prefix(path_root))) =
        (prefix_components.next(), path_components.next())
    else {
        return Err(());
    };
    if !windows_prefix_eq(prefix_root.kind(), path_root.kind())
        || !matches!(prefix_components.next(), Some(Component::RootDir))
        || !matches!(path_components.next(), Some(Component::RootDir))
    {
        return Err(());
    }
    for component in prefix_components {
        let Component::Normal(expected) = component else {
            return Err(());
        };
        let Some(Component::Normal(actual)) = path_components.next() else {
            return Err(());
        };
        if !windows_component_eq(expected, actual) {
            return Err(());
        }
    }
    Ok(path_components.as_path())
}

#[cfg(windows)]
fn windows_prefix_eq(left: std::path::Prefix<'_>, right: std::path::Prefix<'_>) -> bool {
    use std::path::Prefix;

    match (left, right) {
        (Prefix::Disk(left), Prefix::Disk(right))
        | (Prefix::Disk(left), Prefix::VerbatimDisk(right))
        | (Prefix::VerbatimDisk(left), Prefix::Disk(right))
        | (Prefix::VerbatimDisk(left), Prefix::VerbatimDisk(right)) => {
            left.eq_ignore_ascii_case(&right)
        }
        (Prefix::UNC(left_server, left_share), Prefix::UNC(right_server, right_share))
        | (Prefix::UNC(left_server, left_share), Prefix::VerbatimUNC(right_server, right_share))
        | (Prefix::VerbatimUNC(left_server, left_share), Prefix::UNC(right_server, right_share))
        | (
            Prefix::VerbatimUNC(left_server, left_share),
            Prefix::VerbatimUNC(right_server, right_share),
        ) => {
            windows_component_eq(left_server, right_server)
                && windows_component_eq(left_share, right_share)
        }
        _ => false,
    }
}

#[cfg(windows)]
pub(crate) fn windows_component_eq(left: &OsStr, right: &OsStr) -> bool {
    windows_component_cmp(left, right) == Ordering::Equal
}

#[cfg(windows)]
pub(crate) fn windows_component_cmp(left: &OsStr, right: &OsStr) -> Ordering {
    use windows_sys::Win32::Globalization::{
        CSTR_EQUAL, CSTR_GREATER_THAN, CSTR_LESS_THAN, CompareStringOrdinal,
    };

    let left = WindowsWideUnits::encode(left);
    let right = WindowsWideUnits::encode(right);
    let left_units = left.as_slice();
    let right_units = right.as_slice();
    let (Ok(left_len), Ok(right_len)) = (
        i32::try_from(left_units.len()),
        i32::try_from(right_units.len()),
    ) else {
        return left_units.cmp(right_units);
    };
    // SAFETY: both pointers remain valid for their exact encoded lengths during the call.
    match unsafe {
        CompareStringOrdinal(
            left_units.as_ptr(),
            left_len,
            right_units.as_ptr(),
            right_len,
            1,
        )
    } {
        CSTR_LESS_THAN => Ordering::Less,
        CSTR_EQUAL => Ordering::Equal,
        CSTR_GREATER_THAN => Ordering::Greater,
        _ => left_units.cmp(right_units),
    }
}

#[cfg(all(test, windows))]
mod tests {
    use std::cmp::Ordering;
    use std::ffi::OsString;

    use super::{WINDOWS_COMPONENT_STACK_UNITS, windows_component_cmp};

    #[test]
    fn ordinal_comparison_matches_across_stack_and_heap_encodings() {
        let stack_upper = OsString::from("A".repeat(WINDOWS_COMPONENT_STACK_UNITS));
        let stack_lower = OsString::from("a".repeat(WINDOWS_COMPONENT_STACK_UNITS));
        let heap_upper = OsString::from("B".repeat(WINDOWS_COMPONENT_STACK_UNITS + 1));
        let heap_lower = OsString::from("b".repeat(WINDOWS_COMPONENT_STACK_UNITS + 1));

        assert_eq!(
            windows_component_cmp(&stack_upper, &stack_lower),
            Ordering::Equal
        );
        assert_eq!(
            windows_component_cmp(&heap_upper, &heap_lower),
            Ordering::Equal
        );
        assert_eq!(
            windows_component_cmp(&stack_upper, &heap_upper),
            Ordering::Less
        );
    }
}
