//! Embeds the contract's JSON Schemas and builds a validator from them, so
//! that `$ref` between schema files resolves from the embedded copies
//! rather than the network.

use jsonschema::Resource;
use serde_json::Value;
use std::collections::HashMap;

const SCHEMA_BASE_URL: &str = "https://zriz.io/contract/";

/// One embedded schema file: its bare name (no `.json`) and raw JSON text.
/// Kept in one place so the validator, the deny-keys reader, and the drift
/// test all walk the same list.
pub const SCHEMAS: &[(&str, &str)] = &[
    (
        "environment",
        include_str!("../../contract/environment.json"),
    ),
    ("error", include_str!("../../contract/error.json")),
    (
        "exchange-request",
        include_str!("../../contract/exchange-request.json"),
    ),
    (
        "exchange-response",
        include_str!("../../contract/exchange-response.json"),
    ),
    ("frame", include_str!("../../contract/frame.json")),
    (
        "login-device-poll-request",
        include_str!("../../contract/login-device-poll-request.json"),
    ),
    (
        "login-device-poll-response",
        include_str!("../../contract/login-device-poll-response.json"),
    ),
    (
        "login-device-start-request",
        include_str!("../../contract/login-device-start-request.json"),
    ),
    (
        "login-device-start-response",
        include_str!("../../contract/login-device-start-response.json"),
    ),
    ("op", include_str!("../../contract/op.json")),
    ("pipeline", include_str!("../../contract/pipeline.json")),
    ("project", include_str!("../../contract/project.json")),
    ("redact", include_str!("../../contract/redact.json")),
    ("resource", include_str!("../../contract/resource.json")),
    ("result", include_str!("../../contract/result.json")),
    ("run-event", include_str!("../../contract/run-event.json")),
    (
        "run-request",
        include_str!("../../contract/run-request.json"),
    ),
    ("run-result", include_str!("../../contract/run-result.json")),
    (
        "too-many-runs",
        include_str!("../../contract/too-many-runs.json"),
    ),
    (
        "too-many-streams",
        include_str!("../../contract/too-many-streams.json"),
    ),
    ("invite", include_str!("../../contract/invite.json")),
    (
        "invite-request",
        include_str!("../../contract/invite-request.json"),
    ),
    (
        "member-role-request",
        include_str!("../../contract/member-role-request.json"),
    ),
    ("members", include_str!("../../contract/members.json")),
    (
        "project-name",
        include_str!("../../contract/project-name.json"),
    ),
    (
        "space-contract",
        include_str!("../../contract/space-contract.json"),
    ),
    (
        "space-contract-head",
        include_str!("../../contract/space-contract-head.json"),
    ),
    (
        "space-contract-request",
        include_str!("../../contract/space-contract-request.json"),
    ),
    (
        "space-contracts",
        include_str!("../../contract/space-contracts.json"),
    ),
    (
        "space-cursor-request",
        include_str!("../../contract/space-cursor-request.json"),
    ),
    (
        "space-inbox",
        include_str!("../../contract/space-inbox.json"),
    ),
    (
        "space-person",
        include_str!("../../contract/space-person.json"),
    ),
    ("space-post", include_str!("../../contract/space-post.json")),
    (
        "space-post-request",
        include_str!("../../contract/space-post-request.json"),
    ),
    ("space-ref", include_str!("../../contract/space-ref.json")),
    (
        "space-reply-request",
        include_str!("../../contract/space-reply-request.json"),
    ),
    (
        "space-thread",
        include_str!("../../contract/space-thread.json"),
    ),
    (
        "space-threads",
        include_str!("../../contract/space-threads.json"),
    ),
];

/// Errors from decoding, compiling, or validating against the contract.
#[derive(Debug, thiserror::Error)]
pub enum ContractError {
    /// A JSON document failed to decode.
    #[error("contract: decode: {0}")]
    Decode(#[source] serde_json::Error),
    /// An embedded schema failed to parse as JSON.
    #[error("contract: parse schema {0}: {1}")]
    ParseSchema(String, serde_json::Error),
    /// An embedded schema could not be registered as a resource.
    #[error("contract: add resource {0}: {1}")]
    Resource(String, String),
    /// An embedded schema failed to compile.
    #[error("contract: compile {0}: {1}")]
    Compile(String, String),
    /// `validate` was called with a schema name that is not registered.
    #[error("contract: unknown schema {0:?}")]
    UnknownSchema(String),
    /// The document failed schema validation.
    #[error("contract: validate {0}: {1}")]
    Validate(String, String),
}

/// Validates decoded JSON documents against the embedded contract schemas.
pub struct Validator {
    schemas: HashMap<String, jsonschema::Validator>,
}

impl Validator {
    /// Registers every embedded schema under its `$id`, so `$ref` between
    /// files resolves from the embedded copies, then compiles each one once.
    pub fn new() -> std::result::Result<Self, ContractError> {
        let mut docs: Vec<(String, Value)> = Vec::with_capacity(SCHEMAS.len());
        for (name, text) in SCHEMAS {
            let doc: Value = serde_json::from_str(text)
                .map_err(|e| ContractError::ParseSchema((*name).to_string(), e))?;
            docs.push(((*name).to_string(), doc));
        }

        let mut schemas = HashMap::with_capacity(docs.len());
        for (name, doc) in &docs {
            let mut resources = Vec::with_capacity(docs.len() - 1);
            for (other_name, other_doc) in &docs {
                if other_name == name {
                    continue;
                }
                let uri = format!("{SCHEMA_BASE_URL}{other_name}.json");
                let resource = Resource::from_contents(other_doc.clone())
                    .map_err(|e| ContractError::Resource(uri.clone(), e.to_string()))?;
                resources.push((uri, resource));
            }
            let compiled = jsonschema::options()
                .with_resources(resources.into_iter())
                .build(doc)
                .map_err(|e| ContractError::Compile(name.clone(), e.to_string()))?;
            schemas.insert(name.clone(), compiled);
        }
        Ok(Self { schemas })
    }

    /// Checks `doc` (decoded JSON, ideally via [`crate::contract::decode`])
    /// against the named schema, e.g. `"op"`, `"result"`, `"frame"`.
    pub fn validate(&self, name: &str, doc: &Value) -> std::result::Result<(), ContractError> {
        let schema = self
            .schemas
            .get(name)
            .ok_or_else(|| ContractError::UnknownSchema(name.to_string()))?;
        schema
            .validate(doc)
            .map_err(|e| ContractError::Validate(name.to_string(), e.to_string()))
    }
}

/// Returns the key names that must be redacted from evidence, as declared
/// in the embedded `redact.json`.
pub fn deny_keys() -> std::result::Result<Vec<String>, ContractError> {
    let text = SCHEMAS
        .iter()
        .find(|(name, _)| *name == "redact")
        .map(|(_, text)| *text)
        .ok_or_else(|| ContractError::UnknownSchema("redact".to_string()))?;

    #[derive(serde::Deserialize)]
    struct Doc {
        #[serde(rename = "deny-keys")]
        deny_keys: Vec<String>,
    }
    let doc: Doc = serde_json::from_str(text).map_err(ContractError::Decode)?;
    Ok(doc.deny_keys)
}
