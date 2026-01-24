use super::path::canonicalize_if_exists;
use super::*;

impl Environment {
    pub(crate) fn ensure_yaml_loaded(
        &mut self,
        path: &std::path::Path,
    ) -> Result<std::path::PathBuf> {
        let path = canonicalize_if_exists(path);
        if self.yaml_documents.contains_key(&path) {
            return Ok(path);
        }

        match YamlDocument::load_yaml_with_warnings(&path, false) {
            Ok((doc, warnings)) => {
                for w in warnings {
                    self.push_warning(EnvironmentWarning::YamlDocumentSkipped {
                        path: path.clone(),
                        doc_index: w.doc_index,
                        error: w.error,
                    });
                }
                self.yaml_documents.insert(path.clone(), doc);
                Ok(path)
            }
            Err(err) => Err(UnityAssetError::format(format!(
                "Failed to load YAML document {}: {}",
                path.display(),
                err
            ))),
        }
    }

    fn yaml_doc_for_edit_mut(
        &mut self,
        path: &std::path::Path,
    ) -> Result<(std::path::PathBuf, &mut YamlDocument)> {
        let key = self.ensure_yaml_loaded(path)?;
        let base_doc = self
            .yaml_documents
            .get(&key)
            .expect("ensure_yaml_loaded inserts yaml_documents")
            .clone();

        let doc = self
            .write_state
            .yaml_documents
            .entry(key.clone())
            .or_insert(base_doc);

        Ok((key, doc))
    }

    pub fn edit_yaml_object_anchor(
        &mut self,
        path: &std::path::Path,
        anchor: &str,
        f: impl FnOnce(&mut UnityClass) -> Result<()>,
    ) -> Result<()> {
        let (path_key, doc) = self.yaml_doc_for_edit_mut(path)?;

        let Some(obj) = doc.entries_mut().iter_mut().find(|c| c.anchor == anchor) else {
            return Err(UnityAssetError::format(format!(
                "YAML object anchor not found: {} (file: {})",
                anchor,
                path_key.display()
            )));
        };

        f(obj)
    }
}

impl<'a> EnvironmentEditSession<'a> {
    pub fn set_yaml_value_at_path(
        &mut self,
        yaml_path: &std::path::Path,
        anchor: &str,
        field_path: &str,
        value: UnityValue,
    ) -> Result<()> {
        self.env_mut()
            .edit_yaml_object_anchor(yaml_path, anchor, |class| {
                super::pptr_path::set_value_at_path(class, field_path, value)
            })
    }
}
