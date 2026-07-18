use super::deps::{
    DiscoveryPolicy, load_reference_graph, validate_reference_format, write_reference_output,
};
use crate::shared::AppContext;
use anyhow::{Context, Result};
use std::path::{Path, PathBuf};
use tempfile::NamedTempFile;
use unity_asset::AssetLoadBudget;

fn publish_output(path: &Path, write: impl FnOnce(&mut std::fs::File) -> Result<()>) -> Result<()> {
    let parent = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    let mut staged = NamedTempFile::new_in(parent)
        .with_context(|| format!("Failed to stage graph output beside {}", path.display()))?;
    write(staged.as_file_mut())?;
    staged
        .as_file()
        .sync_all()
        .with_context(|| format!("Failed to sync graph output {}", path.display()))?;
    staged.persist(path).map_err(|error| {
        anyhow::anyhow!(
            "Failed to publish graph output {}: {}",
            path.display(),
            error.error
        )
    })?;
    Ok(())
}

pub(crate) fn run(
    input: PathBuf,
    output: Option<PathBuf>,
    yaml: bool,
    format: String,
    max_files: Option<usize>,
    max_edges: usize,
    ctx: &AppContext,
) -> Result<()> {
    if !input.is_dir() {
        anyhow::bail!(
            "project-graph input must be a directory: {}",
            input.display()
        );
    }
    validate_reference_format(&format)?;

    let mut budget = AssetLoadBudget::default();
    let loaded = load_reference_graph(
        &input,
        yaml,
        max_files,
        output.as_deref(),
        DiscoveryPolicy::UnityProject,
        ctx,
        &mut budget,
    )?;
    if let Some(path) = output {
        publish_output(&path, |staged| {
            write_reference_output(&loaded, staged, &format, max_edges, &mut budget)
        })?;
        return Ok(());
    }

    let stdout = std::io::stdout();
    let mut output = stdout.lock();
    write_reference_output(&loaded, &mut output, &format, max_edges, &mut budget)
}

#[cfg(test)]
mod tests {
    use std::io::Write as _;

    use super::*;

    #[test]
    fn staged_output_preserves_the_destination_on_failure() {
        let directory = tempfile::tempdir().unwrap();
        let output = directory.path().join("graph.json");
        std::fs::write(&output, b"original").unwrap();

        let error = publish_output(&output, |staged| {
            staged.write_all(b"partial")?;
            anyhow::bail!("injected projection failure")
        })
        .unwrap_err();
        assert!(error.to_string().contains("injected projection failure"));
        assert_eq!(std::fs::read(&output).unwrap(), b"original");

        publish_output(&output, |staged| {
            staged.write_all(b"complete")?;
            Ok(())
        })
        .unwrap();
        assert_eq!(std::fs::read(&output).unwrap(), b"complete");
    }
}
