use super::edit::StreamedResourceWrite;
use crate::Result;
use unity_asset_core::{UnityAssetError, UnityClass, UnityValue};

pub fn apply_streamed_resource_write(
    class: &mut UnityClass,
    field_name: &str,
    write: &StreamedResourceWrite,
) -> Result<()> {
    let Some(v) = class.get_mut(field_name) else {
        return Err(UnityAssetError::format(format!(
            "StreamedResource field missing: {}",
            field_name
        )));
    };

    let UnityValue::Object(map) = v else {
        return Err(UnityAssetError::format(format!(
            "StreamedResource field is not an object: {}",
            field_name
        )));
    };

    let mut set_value = |keys: &[&str], value: UnityValue| {
        for key in keys {
            if let Some(v) = map.get_mut(*key) {
                *v = value;
                return;
            }
        }

        // Best-effort insert using the first key name as canonical.
        if let Some(first) = keys.first() {
            map.insert((*first).to_string(), value);
        }
    };

    set_value(
        &["path", "m_Source"],
        UnityValue::String(write.path.clone()),
    );
    set_value(&["offset", "m_Offset"], UnityValue::from(write.offset));
    set_value(&["size", "m_Size"], UnityValue::Integer(write.size as i64));
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn streamed_resource_write_preserves_unsigned_offset_range() {
        let mut class = UnityClass::new(0, "Test".to_string(), "0".to_string());
        class.set(
            "m_StreamData".to_string(),
            UnityValue::Object(Default::default()),
        );
        let write = StreamedResourceWrite {
            path: "archive:/CAB-a/CAB-a.resS".to_string(),
            offset: u64::MAX,
            size: 1,
        };

        apply_streamed_resource_write(&mut class, "m_StreamData", &write).unwrap();

        let stream_data = class
            .get("m_StreamData")
            .and_then(UnityValue::as_object)
            .unwrap();
        assert_eq!(
            stream_data.get("offset"),
            Some(&UnityValue::Unsigned(u64::MAX))
        );
    }
}
