use std::io::Read;

use serde::de::DeserializeOwned;
use unity_asset_core::{
    AssetLoadBudget, BudgetedJsonError, ContractJsonLimits, ContractJsonResourceModel,
    read_contract_json,
};

const PARSER_WORK_MULTIPLIER: u64 = 6;
const PARSER_FIXED_WORK_BYTES: u64 = 4 * 1024;
const MAX_WIRE_DEPTH: u32 = 64;
const SMALL_MAX_ENCODED_BYTES: usize = 1024 * 1024;
const SMALL_MAX_VALUES: u64 = 100_000;
const LARGE_MAX_ENCODED_BYTES: usize = 64 * 1024 * 1024;
const LARGE_MAX_VALUES: u64 = 1_000_000;
const SMALL_RESOURCE_MODEL: ContractJsonResourceModel = ContractJsonResourceModel::new(
    PARSER_WORK_MULTIPLIER,
    PARSER_FIXED_WORK_BYTES,
    4 * 1024,
    1024,
);
const LARGE_RESOURCE_MODEL: ContractJsonResourceModel = ContractJsonResourceModel::new(
    PARSER_WORK_MULTIPLIER,
    PARSER_FIXED_WORK_BYTES,
    16 * 1024,
    2 * 1024,
);

pub(crate) const fn small_contract_limits(contract: &'static str) -> ContractJsonLimits {
    ContractJsonLimits::new(
        contract,
        SMALL_MAX_ENCODED_BYTES,
        MAX_WIRE_DEPTH,
        SMALL_MAX_VALUES,
        SMALL_MAX_VALUES,
        SMALL_RESOURCE_MODEL,
    )
}

pub(crate) const fn large_contract_limits(contract: &'static str) -> ContractJsonLimits {
    ContractJsonLimits::new(
        contract,
        LARGE_MAX_ENCODED_BYTES,
        MAX_WIRE_DEPTH,
        LARGE_MAX_VALUES,
        LARGE_MAX_VALUES,
        LARGE_RESOURCE_MODEL,
    )
}

pub(crate) fn read_json_bounded<T: DeserializeOwned>(
    reader: impl Read,
    budget: &mut AssetLoadBudget,
    limits: ContractJsonLimits,
) -> Result<T, BudgetedJsonError> {
    read_contract_json(reader, budget, limits)
}
