//! Public API contract for the default `unity-asset-yaml` package.

pub use unity_asset_yaml::{
    BudgetedYamlError, BudgetedYamlSource, PreparedYamlProof, UnityYamlSerializer, YamlDocument,
    YamlInspection, YamlReferenceClassification, YamlReferenceOccurrence,
    classify_reference_value, load_budgeted_yaml_path, parse_budgeted_yaml_source,
    scan_reference_occurrences,
};
