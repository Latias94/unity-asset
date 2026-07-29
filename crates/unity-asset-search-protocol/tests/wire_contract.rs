use serde::de::DeserializeOwned;
use serde_json::{Value, json};
use unity_asset_core::{DigestV1, WorkspaceId, WorkspaceRevision};
use unity_asset_search_protocol::{
    DaemonInstanceId, FilesystemReindexIntent, GenerationIdV1, GenerationStamp, PortablePath,
    ProjectId, QueryPolicyId, ReferenceCursor, ReferenceDirection, ReferenceRequest,
    ReferenceSelector, RequestEnvelope, RequestId, RequestOperation, SearchCapabilities,
    ValidateContract,
};

const GUID: &str = "0123456789abcdef0123456789abcdef";

fn digest(seed: u8) -> DigestV1 {
    DigestV1::from_bytes([seed; 32])
}

fn generation() -> GenerationStamp {
    GenerationStamp::current(
        GenerationIdV1::new(digest(1)),
        WorkspaceId::from_u128(1).unwrap(),
        WorkspaceRevision::new(digest(2)),
    )
}

#[test]
fn portable_paths_have_one_cross_platform_wire_form() {
    #[cfg(windows)]
    {
        let path =
            PortablePath::from_path(std::path::Path::new(r"Assets\Scenes\Main.unity")).unwrap();
        assert_eq!(path.as_str(), "Assets/Scenes/Main.unity");
        assert_eq!(
            serde_json::to_value(&path).unwrap(),
            json!("Assets/Scenes/Main.unity")
        );
    }
    #[cfg(not(windows))]
    assert!(PortablePath::from_path(std::path::Path::new(r"Assets\Scenes\Main.unity")).is_err());

    for invalid in ["", "Assets\\Main.prefab", "Assets/Bad\0Name.asset"] {
        assert!(serde_json::from_value::<PortablePath>(json!(invalid)).is_err());
    }
}

#[test]
fn generation_staleness_is_not_caller_selectable() {
    let mut stamp = generation();
    stamp.stale = true;
    assert!(stamp.validate().is_err());

    let desired = WorkspaceRevision::new(digest(3));
    let stamp = generation().with_desired_revision(desired);
    assert!(stamp.stale);
    stamp.validate().unwrap();
}

#[test]
fn reference_cursor_requires_query_policy_binding() {
    let mut request = ReferenceRequest {
        direction: ReferenceDirection::Incoming,
        selector: ReferenceSelector::Guid {
            guid: GUID.to_owned(),
            file_id: Some(11_500_000),
        },
        limit: 25,
        cursor: None,
    };
    let query_binding = request.cursor_query_binding().unwrap();
    request.cursor = Some(ReferenceCursor {
        generation: generation().generation,
        query_policy_id: QueryPolicyId::from_bytes([4; 32]),
        after_stable_id: "asset:1".to_owned(),
        query_binding,
    });
    let mut value = serde_json::to_value(&request).unwrap();
    value["cursor"]
        .as_object_mut()
        .unwrap()
        .remove("query_policy_id");
    assert!(serde_json::from_value::<ReferenceRequest>(value).is_err());
}

#[test]
fn reference_cursor_binding_is_stable_and_covers_direction_and_selector() {
    let incoming = ReferenceRequest::incoming_guid(GUID, Some(11_500_000), 25);
    assert_eq!(
        incoming.cursor_query_binding().unwrap(),
        "reference-query-v1:4532a80c3931635cfb1715cea097d3757a404c72b7aed9246fa94c24756f00df"
    );

    let outgoing = ReferenceRequest::outgoing_guid(GUID, Some(11_500_000), 25);
    assert_ne!(
        incoming.cursor_query_binding().unwrap(),
        outgoing.cursor_query_binding().unwrap()
    );

    let different_selector = ReferenceRequest::incoming_guid(GUID, None, 25);
    assert_ne!(
        incoming.cursor_query_binding().unwrap(),
        different_selector.cursor_query_binding().unwrap()
    );
}

#[test]
fn reference_requests_reject_invalid_limits_guids_and_cursor_bindings() {
    assert!(
        ReferenceRequest::incoming_guid(GUID, None, 0)
            .validate()
            .is_err()
    );
    ReferenceRequest::incoming_guid(GUID, None, 500)
        .validate()
        .unwrap();
    assert!(
        ReferenceRequest::incoming_guid(GUID, None, 501)
            .validate()
            .is_err()
    );
    assert!(
        ReferenceRequest::incoming_guid("0123456789ABCDEF0123456789ABCDEF", None, 1)
            .validate()
            .is_err()
    );

    let mut request = ReferenceRequest::incoming_guid(GUID, Some(1), 25);
    let wrong_binding = ReferenceRequest::outgoing_guid(GUID, Some(1), 25)
        .cursor_query_binding()
        .unwrap();
    request.cursor = Some(ReferenceCursor {
        generation: generation().generation,
        query_policy_id: QueryPolicyId::from_bytes([4; 32]),
        after_stable_id: "asset:1".to_owned(),
        query_binding: wrong_binding,
    });
    assert!(request.validate().is_err());
}

#[test]
fn reference_cursor_policy_must_match_the_request_envelope() {
    let mut request = ReferenceRequest::incoming_guid(GUID, None, 25);
    request.cursor = Some(ReferenceCursor {
        generation: generation().generation,
        query_policy_id: QueryPolicyId::from_bytes([9; 32]),
        after_stable_id: "asset:1".to_owned(),
        query_binding: request.cursor_query_binding().unwrap(),
    });

    assert!(
        RequestEnvelope::new(
            1,
            RequestId::from_bytes([1; 16]),
            ProjectId::from_bytes([2; 32]),
            DaemonInstanceId::from_bytes([3; 16]),
            QueryPolicyId::from_bytes([4; 32]),
            RequestOperation::References(request),
        )
        .is_err()
    );
}

#[test]
fn nested_fixed_shapes_reject_unknown_fields() {
    let value = serde_json::to_value(SearchCapabilities::current()).unwrap();
    assert_unknown_field::<SearchCapabilities>(value);

    let request = ReferenceRequest::incoming_guid(GUID, None, 10);
    assert_unknown_field::<ReferenceRequest>(serde_json::to_value(request).unwrap());
}

#[test]
fn changed_path_reindex_is_portable_and_nonempty() {
    let empty = FilesystemReindexIntent::changed_paths(Vec::new());
    assert!(empty.validate().is_err());
    assert!(
        FilesystemReindexIntent::changed_paths(vec![PortablePath::new("1:/asset").unwrap()])
            .validate()
            .is_err()
    );

    let intent = FilesystemReindexIntent::changed_paths(vec![
        PortablePath::new("Assets/Player.prefab").unwrap(),
    ]);
    intent.validate().unwrap();
    assert_eq!(
        serde_json::to_value(intent).unwrap(),
        json!({
            "protocol_revision": 1,
            "scope": {
                "kind": "changed_paths",
                "paths": ["Assets/Player.prefab"]
            }
        })
    );
}

fn assert_unknown_field<T: DeserializeOwned>(mut value: Value) {
    value["unexpected"] = json!(true);
    assert!(serde_json::from_value::<T>(value).is_err());
}
