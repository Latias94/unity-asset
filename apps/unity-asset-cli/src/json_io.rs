use std::fs::File;
use std::io::{self, Read, Write};
use std::path::Path;

use anyhow::{Context, Result};
use serde::Serialize;
use serde::de::DeserializeOwned;
use unity_asset::{AssetLoadBudget, ContractJsonLimits, read_contract_json};

use crate::cli_error::mark_contract_error;

pub(crate) fn read_small_contract<T: DeserializeOwned>(
    path: &Path,
    budget: &mut AssetLoadBudget,
    limits: ContractJsonLimits,
) -> Result<T> {
    read_small_contract_inner(path, budget, limits).map_err(mark_contract_error)
}

fn read_small_contract_inner<T: DeserializeOwned>(
    path: &Path,
    budget: &mut AssetLoadBudget,
    limits: ContractJsonLimits,
) -> Result<T> {
    if is_stdin(path) {
        let stdin = io::stdin();
        read_contract_json(stdin.lock(), budget, limits)
    } else {
        let file = File::open(path)
            .with_context(|| format!("Failed to open JSON contract {}", path.display()))?;
        read_contract_json(file, budget, limits)
    }
    .with_context(|| {
        format!(
            "Invalid {} JSON contract from {}",
            limits.contract(),
            input_label(path)
        )
    })
}

pub(crate) fn with_contract_reader<T>(
    path: &Path,
    read: impl FnOnce(&mut dyn Read) -> Result<T>,
) -> Result<T> {
    with_contract_reader_inner(path, read).map_err(mark_contract_error)
}

fn with_contract_reader_inner<T>(
    path: &Path,
    read: impl FnOnce(&mut dyn Read) -> Result<T>,
) -> Result<T> {
    if is_stdin(path) {
        let stdin = io::stdin();
        let mut input = stdin.lock();
        read(&mut input)
    } else {
        let mut input = File::open(path)
            .with_context(|| format!("Failed to open JSON contract {}", path.display()))?;
        read(&mut input)
    }
}

pub(crate) fn write_json(value: &impl Serialize) -> Result<()> {
    let stdout = io::stdout();
    let mut output = stdout.lock();
    serde_json::to_writer(&mut output, value).context("Failed to encode JSON response")?;
    output
        .write_all(b"\n")
        .context("Failed to terminate JSON response")?;
    output.flush().context("Failed to flush JSON response")
}

pub(crate) fn write_canonical(write: impl FnOnce(&mut dyn Write) -> Result<()>) -> Result<()> {
    let stdout = io::stdout();
    let mut output = stdout.lock();
    write(&mut output)?;
    output
        .write_all(b"\n")
        .context("Failed to terminate canonical JSON response")?;
    output
        .flush()
        .context("Failed to flush canonical JSON response")
}

fn is_stdin(path: &Path) -> bool {
    path == Path::new("-")
}

fn input_label(path: &Path) -> String {
    if is_stdin(path) {
        "stdin".to_owned()
    } else {
        path.display().to_string()
    }
}
