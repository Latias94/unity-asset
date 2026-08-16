use std::sync::Arc;

use anyhow::{Context, Result};
use unity_asset_binary::webfile::WebFile;
use unity_asset_core::{AssetLoadBudget, SourceId, SourceKind, VerifiedSourceImage, WorkspaceId};
use unity_asset_write::artifact::{
    ArtifactBatchDeclaration, ArtifactBudget, ArtifactLimits, ArtifactPayload, LogicalArtifactName,
};
use unity_asset_write::webfile::{WebFileArtifactMember, WebFilePackingPolicy, WebFileWriter};

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct OrderedWebFileMember {
    pub(crate) name: String,
    pub(crate) bytes: Arc<[u8]>,
}

impl OrderedWebFileMember {
    pub(crate) fn new(name: impl Into<String>, bytes: impl Into<Arc<[u8]>>) -> Self {
        Self {
            name: name.into(),
            bytes: bytes.into(),
        }
    }
}

pub(crate) fn ordered_webfile_members(web: &WebFile) -> Result<Vec<OrderedWebFileMember>> {
    web.files()
        .iter()
        .map(|member| {
            Ok(OrderedWebFileMember::new(
                member.name.clone(),
                Arc::<[u8]>::from(web.extract_file_slice_by_info(member)?),
            ))
        })
        .collect()
}

pub(crate) fn prepare_webfile_bytes(
    web: &WebFile,
    members: &[OrderedWebFileMember],
    policy: WebFilePackingPolicy,
) -> Result<Vec<u8>> {
    let mut artifact_budget = ArtifactBudget::new(ArtifactLimits::default())?;
    let mut load_budget = AssetLoadBudget::default();
    let mut declaration = ArtifactBatchDeclaration::begin(&mut artifact_budget, &mut load_budget)?;
    let output = declaration.declare_output(LogicalArtifactName::new("webfile")?)?;
    let mut batch = declaration.seal_output_names()?;

    let workspace =
        WorkspaceId::from_u128(0x0057_4542_4649_4c45).expect("non-zero test workspace id");
    let mut handles = Vec::with_capacity(members.len());
    for (index, member) in members.iter().enumerate() {
        let source = SourceId::new(
            workspace,
            SourceKind::StreamedResource,
            u128::try_from(index).expect("test WebFile member index must fit u128") + 1,
        )?;
        let image =
            VerifiedSourceImage::verify(SourceKind::StreamedResource, Arc::clone(&member.bytes));
        let payload = ArtifactPayload::source_backed(source, image)?;
        handles.push(batch.prepare_verbatim_source(&payload)?);
    }

    let artifact_members = members
        .iter()
        .zip(handles)
        .map(|(member, handle)| WebFileArtifactMember::new(&batch, member.name.as_str(), handle))
        .collect::<std::result::Result<Vec<_>, _>>()?;
    let root = WebFileWriter::prepare(&mut batch, web, &artifact_members, policy)?;
    batch.bind_output(output, root)?;
    let set = batch.finish()?;
    let prepared = set.outputs().next().context("declared WebFile output")?;
    let mut bytes = Vec::new();
    prepared.artifact().stream_verified_to(&mut bytes)?;
    Ok(bytes)
}
