#![allow(missing_docs)]

use std::fs;
use std::path::Path;

use serde_json::{json, Value};
use sha2::{Digest as _, Sha256};
use ygg_agent::extension_api_v03::{
    bundle_supports_api_version, canonical_frame, canonical_json, error_object, host_offer,
    legacy_adapter, negotiate, parse_cancel_request_params, parse_disposition, parse_error_object,
    parse_initialize_request, parse_initialize_response, parse_json_rpc_envelope,
    parse_session_create_params, parse_session_fork_params, parse_session_lifecycle_result,
    parse_session_reload_params, parse_session_switch_params, parse_shutdown_params,
    parse_shutdown_result, parse_tool_call_params, parse_tool_call_result, require_method,
    runtime_supports_api_version, validate_cancel_request_params, validate_disposition,
    validate_error_object, validate_initialize_request, validate_initialize_response,
    validate_offer, CancelRequestParams, ContractOffer, ContractSelection, Disposition,
    ErrorObject, JsonRpcId, MethodDirection, Presence, ProtocolLimits, API_VERSION,
    CANONICAL_ENCODING, MAX_JSON_RPC_ID_BYTES, SCHEMA_ID,
};

fn fixture_directory() -> std::path::PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../protocol/fixtures/extension-api-v0.3")
}

fn fixture(name: &str) -> Value {
    let path = fixture_directory().join(format!("{name}.json"));
    serde_json::from_slice(&fs::read(path).expect("fixture bytes")).expect("fixture JSON")
}

fn error_code<T>(result: Result<T, ygg_agent::extension_api_v03::ContractError>) -> i64 {
    match result {
        Ok(_) => panic!("operation must fail"),
        Err(error) => error.code,
    }
}

#[test]
fn canonical_fixtures_are_byte_exact_and_manifest_hashed() {
    let directory = fixture_directory();
    let manifest: Value = serde_json::from_slice(
        &fs::read(directory.join("manifest.json")).expect("fixture manifest"),
    )
    .expect("manifest JSON");

    assert_eq!(manifest["api_version"], API_VERSION);
    assert_eq!(manifest["canonical_encoding"], CANONICAL_ENCODING);
    for entry in manifest["fixtures"].as_array().expect("fixture entries") {
        let name = entry["name"].as_str().expect("fixture name");
        let raw = fs::read(directory.join(format!("{name}.json"))).expect("fixture bytes");
        let value: Value = serde_json::from_slice(&raw).expect("fixture JSON");
        assert_eq!(
            canonical_json(&value).expect("canonical JSON").as_bytes(),
            raw
        );
        assert_eq!(format!("{:x}", Sha256::digest(&raw)), entry["sha256"]);
    }
}

#[test]
fn generated_models_cover_foundation_shapes_presence_and_envelopes() {
    validate_initialize_request(&parse_initialize_request(fixture("initialize-request")).unwrap())
        .unwrap();
    validate_initialize_response(
        &parse_initialize_response(fixture("initialize-response")).unwrap(),
    )
    .unwrap();
    parse_tool_call_params(fixture("tool-call-params")).unwrap();
    parse_tool_call_result(fixture("tool-call-result")).unwrap();
    parse_cancel_request_params(fixture("cancel-request-params")).unwrap();
    parse_session_create_params(fixture("session-create-params")).unwrap();
    parse_session_fork_params(fixture("session-fork-params")).unwrap();
    parse_session_switch_params(fixture("session-switch-params")).unwrap();
    parse_session_reload_params(fixture("session-reload-params")).unwrap();
    parse_session_lifecycle_result(fixture("session-lifecycle-result")).unwrap();
    parse_shutdown_params(fixture("shutdown-params")).unwrap();
    parse_shutdown_result(fixture("shutdown-result")).unwrap();
    validate_error_object(&parse_error_object(fixture("error-data-absent")).unwrap()).unwrap();
    validate_error_object(&parse_error_object(fixture("error-data-null")).unwrap()).unwrap();
    validate_disposition(
        &serde_json::from_value::<Disposition>(fixture("continue-disposition")).unwrap(),
    )
    .unwrap();
    for name in [
        "request-envelope",
        "notification-envelope",
        "success-envelope",
        "error-envelope",
    ] {
        parse_json_rpc_envelope(fixture(name)).unwrap();
    }

    assert!(matches!(
        parse_error_object(fixture("error-data-absent"))
            .unwrap()
            .data,
        Presence::Absent
    ));
    assert!(matches!(
        parse_error_object(fixture("error-data-null")).unwrap().data,
        Presence::Null
    ));
    assert_eq!(
        error_code(parse_disposition(json!({"kind":"continue","reason":null}))),
        -32602
    );
    assert_eq!(
        error_code(parse_tool_call_result(json!({
            "content":[{"type":"image","artifact_id":"a","mime_type":"image/png"}],
            "is_error":false,
            "metadata":null,
        }))),
        -32011
    );
    assert_eq!(
        error_code(parse_session_create_params(json!({"unexpected":true}))),
        -32602
    );
    assert_eq!(error_code(parse_session_switch_params(json!({}))), -32602);
    assert_eq!(
        error_code(parse_session_switch_params(json!({
            "session_id": "x".repeat(MAX_JSON_RPC_ID_BYTES + 1),
        }))),
        -32012
    );
    assert_eq!(
        error_code(parse_session_lifecycle_result(json!({
            "session_id": "x".repeat(MAX_JSON_RPC_ID_BYTES + 1),
        }))),
        -32012
    );
}

#[test]
fn contract_versions_and_canonical_bounds_fail_closed() {
    let offer = host_offer(usize::MAX, usize::MAX).expect("host offer");
    assert_eq!(offer.schema, SCHEMA_ID);
    assert_eq!(offer.limits.max_frame_bytes, 1_048_576);
    assert_eq!(offer.limits.max_concurrent_requests, 64);

    let selection = ContractSelection {
        schema: offer.schema.clone(),
        encoding: offer.encoding.clone(),
        capabilities: offer.required_capabilities.clone(),
        methods: offer.required_methods.clone(),
        limits: offer.limits.clone(),
    };
    let negotiated = negotiate(&offer, &selection).expect("required contract negotiation");
    require_method(&negotiated, "initialize", MethodDirection::HostToExtension).unwrap();

    let mut unbound_offer = offer.clone();
    unbound_offer.optional_capabilities.clear();
    unbound_offer.optional_methods.clear();
    validate_offer(&unbound_offer).unwrap();
    let mut unbound_selection = selection.clone();
    unbound_selection
        .capabilities
        .push("session_lifecycle".into());
    unbound_selection.methods.push("session/create".into());
    assert_eq!(
        error_code(negotiate(&unbound_offer, &unbound_selection)),
        -32011
    );
    assert_eq!(
        error_code(require_method(
            &negotiated,
            "context/collect",
            MethodDirection::HostToExtension,
        )),
        -32601
    );
    assert_eq!(
        error_code(require_method(
            &negotiated,
            "future/call",
            MethodDirection::HostToExtension,
        )),
        -32601
    );

    let mut altered_offer = offer.clone();
    altered_offer
        .required_methods
        .retain(|name| name != "shutdown");
    assert_eq!(error_code(validate_offer(&altered_offer)), -32011);
    assert_eq!(
        error_code(parse_json_rpc_envelope(
            json!({"jsonrpc":"2.0","id":null,"result":{}})
        )),
        -32600
    );
    assert_eq!(
        error_code(parse_json_rpc_envelope(json!({
            "jsonrpc":"2.0","id":1,"result":{},"error":{"code":-32600,"message":"invalid request"}
        }))),
        -32600
    );
    assert_eq!(
        error_code(canonical_frame(&json!(1.0), 128)),
        -32602,
        "floats are not canonical API 0.3 values"
    );
    assert_eq!(
        error_code(canonical_frame(&json!(9_007_199_254_740_992_u64), 128)),
        -32602,
        "integers outside the portable range are rejected"
    );
    assert_eq!(
        error_code(validate_cancel_request_params(&CancelRequestParams {
            id: JsonRpcId::Number(9_007_199_254_740_992),
            reason: None,
        })),
        -32602,
        "constructed generated models enforce canonical portable IDs too"
    );
    assert_eq!(error_code(canonical_frame(&json!({"x": "y"}), 1)), -32012);
    assert!(runtime_supports_api_version("0.1"));
    assert!(!bundle_supports_api_version("0.1"));
    assert!(bundle_supports_api_version("0.2"));
    assert!(bundle_supports_api_version("0.3"));
    assert_eq!(legacy_adapter("0.1").unwrap().status, "frozen");
    assert_eq!(legacy_adapter("0.2").unwrap().status, "supported");
    assert!(legacy_adapter("0.3").is_none());

    let invalid_error = ErrorObject {
        code: -32601,
        message: "wrong text".into(),
        data: Presence::Absent,
    };
    assert_eq!(error_code(validate_error_object(&invalid_error)), -32602);
    assert_eq!(
        error_object("unknown_method", Some(json!({"method": "future/call"})))
            .unwrap()
            .code,
        -32601
    );
}

#[test]
fn hostile_fixture_corpus_is_rejected() {
    let directory = fixture_directory().join("negative");
    let manifest: Value =
        serde_json::from_slice(&fs::read(directory.join("manifest.json")).unwrap()).unwrap();
    for entry in manifest["fixtures"].as_array().unwrap() {
        let name = entry["name"].as_str().unwrap();
        let raw = fs::read(directory.join(format!("{name}.json"))).unwrap();
        if name == "duplicate-key" {
            let value: Value = serde_json::from_slice(&raw).unwrap();
            assert_ne!(canonical_json(&value).unwrap().as_bytes(), raw);
        } else if name.contains("surrogate") {
            if let Ok(value) = serde_json::from_slice::<Value>(&raw) {
                assert!(canonical_json(&value)
                    .map(|canonical| canonical.as_bytes() != raw)
                    .unwrap_or(true));
            }
        } else if name == "optional-reason-null" {
            let value: Value = serde_json::from_slice(&raw).unwrap();
            assert_eq!(error_code(parse_disposition(value)), -32602);
        } else {
            let value: Value = serde_json::from_slice(&raw).unwrap();
            let result = parse_json_rpc_envelope(value);
            assert!(result.is_err(), "negative fixture {name} was accepted");
            assert_eq!(error_code(result), -32600);
        }
    }
}

#[test]
fn selections_cannot_increase_limits_or_drop_required_items() {
    let offer = host_offer(1024, 4).unwrap();
    let mut selection = ContractSelection {
        schema: SCHEMA_ID.into(),
        encoding: CANONICAL_ENCODING.into(),
        capabilities: offer.required_capabilities.clone(),
        methods: offer.required_methods.clone(),
        limits: ProtocolLimits {
            max_frame_bytes: 1025,
            max_concurrent_requests: 4,
            max_tools: offer.limits.max_tools,
        },
    };
    assert_eq!(error_code(negotiate(&offer, &selection)), -32011);
    selection.limits = offer.limits.clone();
    selection.capabilities.retain(|name| name != "tool_call");
    assert_eq!(error_code(negotiate(&offer, &selection)), -32011);

    let invalid_offer = ContractOffer {
        schema: SCHEMA_ID.into(),
        encoding: CANONICAL_ENCODING.into(),
        required_capabilities: vec!["core".into()],
        optional_capabilities: Vec::new(),
        required_methods: vec!["initialize".into()],
        optional_methods: Vec::new(),
        limits: offer.limits,
    };
    assert_eq!(error_code(validate_offer(&invalid_offer)), -32011);
}
