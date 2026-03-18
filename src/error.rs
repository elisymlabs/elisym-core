use thiserror::Error;

pub type Result<T> = std::result::Result<T, ElisymError>;

#[derive(Debug, Error)]
pub enum ElisymError {
    #[error("Nostr client error: {0}")]
    Nostr(#[from] nostr_sdk::client::Error),

    #[error("Nostr key error: {0}")]
    NostrKey(#[from] nostr_sdk::nostr::key::Error),

    #[error("Nostr event builder error: {0}")]
    NostrEventBuilder(#[from] nostr_sdk::nostr::event::builder::Error),

    #[error("Nostr tag error: {0}")]
    NostrTag(#[from] nostr_sdk::nostr::event::tag::Error),

    #[error("JSON serialization error: {0}")]
    Json(#[from] serde_json::Error),

    #[error("Invalid capability card: {0}")]
    InvalidCapabilityCard(String),

    #[error("Payment error: {0}")]
    Payment(String),

    #[error("Configuration error: {0}")]
    Config(String),
}
