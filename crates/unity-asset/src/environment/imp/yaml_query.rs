use super::object_graph::YamlObjectKey;
use super::path::canonicalize_if_exists;
use super::*;

fn value_matches_string(value: &UnityValue, expected: &str) -> bool {
    value.as_str() == Some(expected)
}

impl Environment {
    /// Iterate YAML Unity objects with stable keys (path + anchor).
    pub fn yaml_objects_with_keys(
        &self,
    ) -> impl Iterator<Item = (YamlObjectKey, &UnityClass)> + '_ {
        self.yaml_documents.iter().flat_map(|(path, doc)| {
            doc.entries().iter().map(move |obj| {
                (
                    YamlObjectKey {
                        path: path.clone(),
                        anchor: obj.anchor.clone(),
                    },
                    obj,
                )
            })
        })
    }

    /// Find YAML objects in already-loaded YAML documents by class name + dot path + string value.
    pub fn find_yaml_object_keys_by_field_string(
        &self,
        class_name: Option<&str>,
        field_path: &str,
        expected: &str,
    ) -> Vec<YamlObjectKey> {
        let mut out: Vec<YamlObjectKey> = Vec::new();

        for (key, obj) in self.yaml_objects_with_keys() {
            if let Some(class_name) = class_name
                && obj.class_name != class_name
            {
                continue;
            }

            let Some(value) = super::pptr_path::get_value_at_path(obj, field_path) else {
                continue;
            };
            if value_matches_string(value, expected) {
                out.push(key);
            }
        }

        out
    }

    /// Ensure a YAML file is loaded, then search within that file.
    pub fn find_yaml_object_keys_in_file_by_field_string(
        &mut self,
        yaml_path: &Path,
        class_name: Option<&str>,
        field_path: &str,
        expected: &str,
    ) -> Result<Vec<YamlObjectKey>> {
        let yaml_path = canonicalize_if_exists(yaml_path);
        let yaml_key = self.ensure_yaml_loaded(&yaml_path)?;
        let doc = self
            .yaml_documents
            .get(&yaml_key)
            .expect("ensure_yaml_loaded inserts yaml_documents");

        let mut out: Vec<YamlObjectKey> = Vec::new();
        for obj in doc.entries() {
            if let Some(class_name) = class_name
                && obj.class_name != class_name
            {
                continue;
            }
            let Some(value) = super::pptr_path::get_value_at_path(obj, field_path) else {
                continue;
            };
            if value_matches_string(value, expected) {
                out.push(YamlObjectKey {
                    path: yaml_key.clone(),
                    anchor: obj.anchor.clone(),
                });
            }
        }

        Ok(out)
    }
}

impl<'a> EnvironmentEditSession<'a> {
    /// Ensure a YAML file is loaded, then return the unique matching object key.
    pub fn find_yaml_object_key_in_file_by_field_string_unique(
        &mut self,
        yaml_path: &Path,
        class_name: Option<&str>,
        field_path: &str,
        expected: &str,
    ) -> Result<YamlObjectKey> {
        let matches = self
            .env_mut()
            .find_yaml_object_keys_in_file_by_field_string(
                yaml_path, class_name, field_path, expected,
            )?;

        match matches.as_slice() {
            [only] => Ok(only.clone()),
            [] => Err(UnityAssetError::format(format!(
                "YAML object not found: file={} class={:?} {} == {:?}",
                yaml_path.display(),
                class_name,
                field_path,
                expected
            ))),
            many => Err(UnityAssetError::format(format!(
                "YAML object query is not unique: file={} class={:?} {} == {:?} (matches={})",
                yaml_path.display(),
                class_name,
                field_path,
                expected,
                many.len()
            ))),
        }
    }
}
