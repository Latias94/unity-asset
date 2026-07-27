use std::path::Path;

use unity_asset::workspace::{
    AssetWorkspace, SourceAdmissionBatch, SourceAdmissionOperation, SourceAdmissionPolicy,
    SourceOpenRequest,
};
use unity_asset::{AssetLoadBudget, SourceAlias, SourceId};

pub(crate) fn replace_source_path(
    workspace: &mut AssetWorkspace,
    existing: SourceId,
    path: &Path,
    alias: &str,
) -> SourceId {
    let mut budget = AssetLoadBudget::default();
    let mut batch =
        SourceAdmissionBatch::with_capacity(2, &mut budget).expect("reserve replacement batch");
    batch
        .try_push(SourceAdmissionOperation::Unload(existing), &mut budget)
        .expect("append source unload");
    batch
        .try_push(
            SourceAdmissionOperation::LoadPath(SourceOpenRequest::new(
                path,
                SourceAlias::new(alias).expect("valid source alias"),
            )),
            &mut budget,
        )
        .expect("append source reload");

    let report = workspace
        .admit_sources(batch, SourceAdmissionPolicy::Strict, &mut budget)
        .expect("replace source in one admission transaction");
    assert_eq!(report.outcomes().len(), 2);
    assert_eq!(
        report.outcomes()[0].disposition().source_id(),
        Some(existing)
    );
    report.outcomes()[1]
        .disposition()
        .source_id()
        .expect("replacement source was loaded")
}
