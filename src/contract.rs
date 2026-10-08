//! The wire types and JSON Schemas shared between the runner and the cloud:
//! frames, ops, results, errors, and the exchange envelope, plus a validator
//! built from the embedded schemas.

mod frame;
pub mod schema;

pub use frame::{Error, ExchangeRequest, ExchangeResponse, Frame, Op, Result, Runner, Timing};
pub use schema::{ContractError, Validator, deny_keys};

use serde::de::DeserializeOwned;

/// Decodes a JSON document from bytes into `T`.
pub fn decode<T: DeserializeOwned>(bytes: &[u8]) -> std::result::Result<T, ContractError> {
    serde_json::from_slice(bytes).map_err(ContractError::Decode)
}
