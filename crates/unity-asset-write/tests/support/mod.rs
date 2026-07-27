use std::sync::Arc;

use anyhow::{Context, Result, bail};
use unity_asset_binary::bundle::AssetBundle;
use unity_asset_core::{AssetLoadBudget, SourceId, SourceKind, VerifiedSourceImage, WorkspaceId};
use unity_asset_write::PackingPolicy;
use unity_asset_write::artifact::{
    ArtifactBatchDeclaration, ArtifactBudget, ArtifactLimits, ArtifactPayload, LogicalArtifactName,
};
use unity_asset_write::bundle::{BundleArtifactEntry, BundleWriter};

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum OrderedBundleEntry {
    File {
        name: String,
        flags: u32,
        bytes: Arc<[u8]>,
    },
    EmptyDirectory {
        name: String,
        flags: u32,
    },
    Deleted {
        name: String,
        flags: u32,
    },
}

pub(crate) fn ordered_bundle_entries(bundle: &AssetBundle) -> Result<Vec<OrderedBundleEntry>> {
    bundle
        .nodes
        .iter()
        .map(|node| {
            if node.is_file() {
                return Ok(OrderedBundleEntry::File {
                    name: node.name.clone(),
                    flags: node.flags,
                    bytes: bundle
                        .extract_node_data(node)
                        .with_context(|| format!("extract bundle member {:?}", node.name))?
                        .into(),
                });
            }
            if node.size != 0 {
                bail!(
                    "non-file bundle entry {:?} has unsupported payload size {}",
                    node.name,
                    node.size
                );
            }
            if node.is_deleted() {
                return Ok(OrderedBundleEntry::Deleted {
                    name: node.name.clone(),
                    flags: node.flags,
                });
            }
            if node.is_directory() {
                return Ok(OrderedBundleEntry::EmptyDirectory {
                    name: node.name.clone(),
                    flags: node.flags,
                });
            }
            bail!(
                "bundle entry {:?} has unsupported flags {:#x}",
                node.name,
                node.flags
            )
        })
        .collect()
}

pub(crate) fn prepare_bundle_bytes(
    bundle: &AssetBundle,
    entries: &[OrderedBundleEntry],
    policy: PackingPolicy,
) -> Result<Vec<u8>> {
    let mut artifact_budget = ArtifactBudget::new(ArtifactLimits::default())?;
    let mut load_budget = AssetLoadBudget::default();
    let mut declaration = ArtifactBatchDeclaration::begin(&mut artifact_budget, &mut load_budget)?;
    let output = declaration.declare_output(LogicalArtifactName::new("bundle")?)?;
    let mut batch = declaration.seal_output_names()?;

    let workspace = WorkspaceId::from_u128(0x4255_4e44_4c45).expect("non-zero test workspace id");
    let mut handles = Vec::with_capacity(entries.len());
    for (index, entry) in entries.iter().enumerate() {
        let OrderedBundleEntry::File { bytes, .. } = entry else {
            handles.push(None);
            continue;
        };
        let source = SourceId::new(
            workspace,
            SourceKind::StreamedResource,
            u128::try_from(index).expect("test bundle entry index must fit u128") + 1,
        )?;
        let image = VerifiedSourceImage::verify(SourceKind::StreamedResource, Arc::clone(bytes));
        let payload = ArtifactPayload::source_backed(source, image)?;
        handles.push(Some(batch.prepare_verbatim_source(&payload)?));
    }

    let artifact_entries = entries
        .iter()
        .zip(handles)
        .map(|(entry, handle)| match entry {
            OrderedBundleEntry::File { name, flags, .. } => BundleArtifactEntry::file(
                &batch,
                name,
                *flags,
                handle.expect("file entry has a prepared artifact"),
            ),
            OrderedBundleEntry::EmptyDirectory { name, flags } => {
                Ok(BundleArtifactEntry::EmptyDirectory {
                    name,
                    flags: *flags,
                })
            }
            OrderedBundleEntry::Deleted { name, flags } => Ok(BundleArtifactEntry::Deleted {
                name,
                flags: *flags,
            }),
        })
        .collect::<std::result::Result<Vec<_>, _>>()?;
    let root = BundleWriter::prepare_artifact(&mut batch, bundle, &artifact_entries, policy)?;
    batch.bind_output(output, root)?;
    let set = batch.finish()?;
    let prepared = set.outputs().next().context("declared bundle output")?;
    let mut bytes = Vec::new();
    prepared.artifact().stream_verified_to(&mut bytes)?;
    Ok(bytes)
}
