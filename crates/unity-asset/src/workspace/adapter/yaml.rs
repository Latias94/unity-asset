//! Workspace compatibility names for the budgeted YAML parser.

pub(crate) use unity_asset_yaml::{
    BudgetedYamlError as YamlAdapterError, parse_budgeted_yaml_source as parse_yaml_source,
    parse_prebudgeted_yaml_source,
};
