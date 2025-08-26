//! Async Python-compatible API
//!
//! Python-like async API for Unity YAML processing, compatible with UnityPy.

use futures::Stream;
use std::path::Path;

use crate::{AsyncYamlDocument, AsyncYamlLoader};
use unity_asset_core_v2::{AsyncUnityClass, Result};

/// Async Python-compatible API
pub struct AsyncPythonApi {
    loader: AsyncYamlLoader,
}

impl AsyncPythonApi {
    /// Create new Python API
    pub fn new() -> Self {
        Self {
            loader: AsyncYamlLoader::new(),
        }
    }

    /// Load YAML file (Python-style)
    pub async fn load<P: AsRef<Path> + Send>(&self, path: P) -> Result<AsyncYamlDocument> {
        self.loader.load_from_path(path).await
    }

    /// Get objects by type (Python-style)
    pub async fn get_objects_of_type<'a>(
        &self,
        document: &'a AsyncYamlDocument,
        class_name: &str,
    ) -> Vec<&'a AsyncUnityClass> {
        document.classes_by_type(class_name)
    }

    /// Stream objects by type
    pub fn stream_objects_of_type<'a>(
        &self,
        document: &'a AsyncYamlDocument,
        class_name: &str,
    ) -> impl Stream<Item = Result<AsyncUnityClass>> + Send + 'a {
        let class_name = class_name.to_string();
        futures::stream::iter(
            document
                .classes()
                .iter()
                .filter(move |class| class.class_name() == class_name)
                .cloned()
                .map(Ok),
        )
    }
}

impl Default for AsyncPythonApi {
    fn default() -> Self {
        Self::new()
    }
}
