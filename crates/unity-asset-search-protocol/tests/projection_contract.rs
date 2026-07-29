use unity_asset_search_core::{
    CandidateField, HighlightRange, SearchDiagnostic, SearchDiagnosticSeverity,
};
use unity_asset_search_protocol::{
    CandidateFieldV1, HighlightRangeV1, SearchDiagnosticV1, WireProjectionError,
};

#[test]
fn platform_width_search_evidence_projects_to_fixed_width_values() {
    assert_eq!(
        HighlightRangeV1::try_from(HighlightRange { start: 2, end: 7 }).unwrap(),
        HighlightRangeV1 { start: 2, end: 7 }
    );
    assert_eq!(
        CandidateFieldV1::from(CandidateField::ContainerSourcePath),
        CandidateFieldV1::ContainerSourcePath
    );

    if usize::BITS > u32::BITS {
        let overflow = usize::try_from(u64::from(u32::MAX) + 1).unwrap();
        assert!(matches!(
            HighlightRangeV1::try_from(HighlightRange {
                start: overflow,
                end: overflow,
            }),
            Err(WireProjectionError::NumericOverflow { .. })
        ));
    }
}

#[test]
fn closed_wire_diagnostics_reject_unknown_domain_extensions() {
    let known = SearchDiagnosticV1::try_from(SearchDiagnostic::QueryByteLimitExceeded {
        actual: 8,
        limit: 4,
    })
    .unwrap();
    assert!(matches!(
        known,
        SearchDiagnosticV1::QueryByteLimitExceeded {
            actual: 8,
            limit: 4
        }
    ));

    let unknown = SearchDiagnostic::Unknown {
        contract_version: 99,
        code: "future".to_owned(),
        severity: SearchDiagnosticSeverity::Warning,
        blocks_execution: false,
        details: serde_json::json!({}),
    };
    assert!(matches!(
        SearchDiagnosticV1::try_from(unknown),
        Err(WireProjectionError::UnsupportedVariant {
            field: "search diagnostic"
        })
    ));
}
