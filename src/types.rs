use nostr_sdk::Kind;
use serde::{Deserialize, Serialize};

/// NIP-89 Application Handler (parameterized replaceable event)
pub const KIND_APP_HANDLER: u16 = 31990;

/// NIP-90 Data Vending Machine job request base kind
pub const KIND_JOB_REQUEST_BASE: u16 = 5000;

/// NIP-90 Data Vending Machine job result base kind
pub const KIND_JOB_RESULT_BASE: u16 = 6000;

/// NIP-90 Data Vending Machine job feedback kind
pub const KIND_JOB_FEEDBACK: u16 = 7000;

/// Protocol version identifier
pub const PROTOCOL_VERSION: &str = "elisym/0.1";

/// Default relays for the network
pub const DEFAULT_RELAYS: &[&str] = &[
    "wss://relay.damus.io",
    "wss://nos.lol",
    "wss://relay.nostr.band",
];

/// Helper to create a Kind from a u16
pub fn kind(k: u16) -> Kind {
    Kind::from(k)
}

/// Compute job request kind (5000 + offset) with overflow check.
pub fn job_request_kind(offset: u16) -> Option<Kind> {
    KIND_JOB_REQUEST_BASE
        .checked_add(offset)
        .filter(|&k| k < KIND_JOB_RESULT_BASE)
        .map(kind)
}

/// Compute job result kind (6000 + offset) with overflow check.
pub fn job_result_kind(offset: u16) -> Option<Kind> {
    KIND_JOB_RESULT_BASE
        .checked_add(offset)
        .filter(|&k| k < KIND_JOB_FEEDBACK)
        .map(kind)
}

/// Job status for NIP-90 feedback events
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum JobStatus {
    PaymentRequired,
    Processing,
    Error,
    Success,
    Partial,
}

impl JobStatus {
    pub fn as_str(&self) -> &'static str {
        match self {
            JobStatus::PaymentRequired => "payment-required",
            JobStatus::Processing => "processing",
            JobStatus::Error => "error",
            JobStatus::Success => "success",
            JobStatus::Partial => "partial",
        }
    }
}

impl std::fmt::Display for JobStatus {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}
