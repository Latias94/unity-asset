use unity_asset_core::UnityValue;

pub(crate) const YAML_FILE_ID: &str = "fileID";
pub(crate) const YAML_GUID: &str = "guid";
pub(crate) const YAML_TYPE: &str = "type";
pub(crate) const YAML_MEMBER_FILE_ID: &str = "m_FileID";
pub(crate) const YAML_MEMBER_GUID: &str = "m_GUID";
pub(crate) const YAML_MEMBER_TYPE: &str = "m_Type";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct InvalidYamlReferenceValue;

#[derive(Debug, Clone, Copy)]
pub(crate) struct YamlReferenceValue<'value> {
    file_field: &'value str,
    guid_field: Option<&'value str>,
    type_field: Option<&'value str>,
    file_id: i64,
    guid: Option<[u8; 16]>,
    type_id: Option<i64>,
}

impl<'value> YamlReferenceValue<'value> {
    pub(crate) fn read(value: &'value UnityValue) -> Result<Self, InvalidYamlReferenceValue> {
        let UnityValue::Object(fields) = value else {
            return Err(InvalidYamlReferenceValue);
        };
        let mut file = None;
        let mut guid = None;
        let mut type_id = None;
        for (name, value) in fields {
            match name.as_str() {
                YAML_FILE_ID | YAML_MEMBER_FILE_ID if file.is_none() => {
                    file = Some((name.as_str(), value));
                }
                YAML_GUID | YAML_MEMBER_GUID if guid.is_none() => {
                    guid = Some((name.as_str(), value));
                }
                YAML_TYPE | YAML_MEMBER_TYPE if type_id.is_none() => {
                    type_id = Some((name.as_str(), value));
                }
                _ => return Err(InvalidYamlReferenceValue),
            }
        }
        let (file_field, file_value) = file.ok_or(InvalidYamlReferenceValue)?;
        let file_id = file_value.as_i64().ok_or(InvalidYamlReferenceValue)?;
        let (guid_field, guid) = match guid {
            Some((field, value)) => {
                let value = value.as_str().ok_or(InvalidYamlReferenceValue)?;
                (Some(field), Some(parse_guid(value)?))
            }
            None => (None, None),
        };
        let (type_field, type_id) = match type_id {
            Some((field, value)) => (
                Some(field),
                Some(value.as_i64().ok_or(InvalidYamlReferenceValue)?),
            ),
            None => (None, None),
        };
        if guid.is_some() != type_id.is_some() {
            return Err(InvalidYamlReferenceValue);
        }
        Ok(Self {
            file_field,
            guid_field,
            type_field,
            file_id,
            guid,
            type_id,
        })
    }

    pub(crate) const fn file_field(self) -> &'value str {
        self.file_field
    }

    pub(crate) const fn file_id(self) -> i64 {
        self.file_id
    }

    pub(crate) const fn guid(self) -> Option<[u8; 16]> {
        self.guid
    }

    pub(crate) const fn type_id(self) -> Option<i64> {
        self.type_id
    }

    pub(crate) fn external_field_names(self) -> (&'value str, &'value str) {
        match (self.guid_field, self.type_field) {
            (Some(guid), Some(type_id)) => (guid, type_id),
            _ if self.file_field == YAML_MEMBER_FILE_ID => (YAML_MEMBER_GUID, YAML_MEMBER_TYPE),
            _ => (YAML_GUID, YAML_TYPE),
        }
    }
}

fn parse_guid(value: &str) -> Result<[u8; 16], InvalidYamlReferenceValue> {
    if value.len() != 32 {
        return Err(InvalidYamlReferenceValue);
    }
    let mut guid = [0_u8; 16];
    for (index, pair) in value.as_bytes().chunks_exact(2).enumerate() {
        let high = hex_value(pair[0]).ok_or(InvalidYamlReferenceValue)?;
        let low = hex_value(pair[1]).ok_or(InvalidYamlReferenceValue)?;
        guid[index] = (high << 4) | low;
    }
    Ok(guid)
}

const fn hex_value(value: u8) -> Option<u8> {
    match value {
        b'0'..=b'9' => Some(value - b'0'),
        b'a'..=b'f' => Some(value - b'a' + 10),
        b'A'..=b'F' => Some(value - b'A' + 10),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use indexmap::IndexMap;

    use super::*;

    fn object(fields: &[(&str, UnityValue)]) -> UnityValue {
        UnityValue::Object(IndexMap::from_iter(
            fields
                .iter()
                .map(|(name, value)| ((*name).to_owned(), value.clone())),
        ))
    }

    #[test]
    fn null_reference_may_retain_a_valid_external_identity() {
        let value = object(&[
            (YAML_FILE_ID, UnityValue::Integer(0)),
            (
                YAML_GUID,
                UnityValue::String("0123456789abcdef0123456789abcdef".to_owned()),
            ),
            (YAML_TYPE, UnityValue::Integer(2)),
        ]);
        let parsed = YamlReferenceValue::read(&value).unwrap();
        assert_eq!(parsed.file_id(), 0);
        assert_eq!(parsed.type_id(), Some(2));
        assert!(parsed.guid().is_some());
    }

    #[test]
    fn reference_shape_is_exact_and_guid_type_are_atomic() {
        for value in [
            object(&[
                (YAML_FILE_ID, UnityValue::Integer(1)),
                (YAML_TYPE, UnityValue::Integer(2)),
            ]),
            object(&[
                (YAML_FILE_ID, UnityValue::Integer(1)),
                (
                    YAML_GUID,
                    UnityValue::String("0123456789abcdef0123456789abcdef".to_owned()),
                ),
            ]),
            object(&[
                (YAML_FILE_ID, UnityValue::Integer(1)),
                ("unexpected", UnityValue::Integer(2)),
            ]),
        ] {
            assert!(matches!(
                YamlReferenceValue::read(&value),
                Err(InvalidYamlReferenceValue)
            ));
        }
    }

    #[test]
    fn member_alias_family_is_preserved() {
        let value = object(&[(YAML_MEMBER_FILE_ID, UnityValue::Integer(9))]);
        let parsed = YamlReferenceValue::read(&value).unwrap();
        assert_eq!(parsed.file_field(), YAML_MEMBER_FILE_ID);
        assert_eq!(
            parsed.external_field_names(),
            (YAML_MEMBER_GUID, YAML_MEMBER_TYPE)
        );
    }
}
