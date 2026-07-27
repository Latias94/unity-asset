use unity_asset::extraction::{ExtractionPath, ExtractionRepresentationPolicy, ExtractionRequest};
use unity_asset::{
    AssetLoadBudget, AssetLoadLimits, BudgetError, BudgetedJsonError, ObjectAddress, SourceLocator,
};

fn request() -> ExtractionRequest {
    let source = SourceLocator::path("game.assets").unwrap();
    ExtractionRequest::addresses(
        [
            ObjectAddress::binary_at(source.clone(), 20).unwrap(),
            ObjectAddress::binary_at(source, -4).unwrap(),
        ],
        ExtractionRepresentationPolicy::PreferDecoded,
    )
    .unwrap()
}

fn budget_with_bytes(max_bytes: u64) -> AssetLoadBudget {
    let limits = AssetLoadLimits {
        max_bytes,
        ..AssetLoadLimits::default()
    };
    AssetLoadBudget::new(limits).unwrap()
}

#[test]
fn request_normalization_makes_input_order_irrelevant() {
    let forward = request();
    let source = SourceLocator::path("game.assets").unwrap();
    let reverse = ExtractionRequest::addresses(
        [
            ObjectAddress::binary_at(source.clone(), -4).unwrap(),
            ObjectAddress::binary_at(source, 20).unwrap(),
        ],
        ExtractionRepresentationPolicy::PreferDecoded,
    )
    .unwrap();

    assert_eq!(forward, reverse);
    assert_eq!(forward.digest().unwrap(), reverse.digest().unwrap());
    assert_eq!(
        forward.canonical_json().unwrap(),
        reverse.canonical_json().unwrap()
    );
}

#[test]
fn request_json_requires_exact_caller_owned_resource_budget() {
    let request = request();
    let encoded = request.canonical_json().unwrap();
    let mut measured = AssetLoadBudget::default();
    let measured_request = ExtractionRequest::read_json(encoded.as_slice(), &mut measured).unwrap();
    assert_eq!(measured_request, request);
    let required_bytes = measured.usage().bytes;

    let decoded =
        ExtractionRequest::read_json(encoded.as_slice(), &mut budget_with_bytes(required_bytes))
            .unwrap();
    assert_eq!(decoded, request);

    let error = ExtractionRequest::read_json(
        encoded.as_slice(),
        &mut budget_with_bytes(required_bytes - 1),
    )
    .unwrap_err();
    assert!(matches!(
        error,
        BudgetedJsonError::Budget(BudgetError::Exceeded {
            resource: "bytes",
            limit,
            requested,
        }) if limit == required_bytes - 1 && requested == required_bytes
    ));
}

#[test]
fn request_json_rejects_structure_beyond_its_contract_profile() {
    let nested = format!("{}0{}", "[".repeat(64), "]".repeat(64));
    let encoded = format!(r#"{{"unexpected":{nested}}}"#);
    let error = ExtractionRequest::read_json(encoded.as_bytes(), &mut AssetLoadBudget::default())
        .unwrap_err();

    assert!(matches!(
        error,
        BudgetedJsonError::StructureLimitExceeded {
            contract: "unity_asset.extraction_request",
            resource: "depth",
            limit: 64,
            requested: 65,
        }
    ));
}

#[test]
fn request_json_rejects_unknown_versions_and_fields() {
    let encoded = String::from_utf8(request().canonical_json().unwrap()).unwrap();
    let unknown_version = encoded.replacen("\"version\":1", "\"version\":2", 1);
    assert!(
        ExtractionRequest::read_json(unknown_version.as_bytes(), &mut AssetLoadBudget::default(),)
            .is_err()
    );

    let unknown_field = encoded.replacen("{", "{\"unexpected\":true,", 1);
    assert!(
        ExtractionRequest::read_json(unknown_field.as_bytes(), &mut AssetLoadBudget::default(),)
            .is_err()
    );

    let wrong_contract = encoded.replacen(
        "\"unity_asset.extraction_request\"",
        "\"unity_asset.extraction_manifest\"",
        1,
    );
    assert!(
        ExtractionRequest::read_json(wrong_contract.as_bytes(), &mut AssetLoadBudget::default(),)
            .is_err()
    );
}

#[test]
fn extraction_paths_reject_non_portable_or_escaping_names() {
    let overlong_component = "x".repeat(256);
    for invalid in [
        "",
        "/absolute.bin",
        "C:/absolute.bin",
        "//server/share.bin",
        "../escape.bin",
        "nested/../../escape.bin",
        "nested\\windows.bin",
        "CON.txt",
        "nested/trailing. ",
        overlong_component.as_str(),
    ] {
        assert!(
            ExtractionPath::new(invalid).is_err(),
            "{invalid:?} must not be a portable extraction path"
        );
    }

    assert!(ExtractionPath::new("assets/textures/icon.png").is_ok());
}
