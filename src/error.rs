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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_payment_error_display() {
        let err = ElisymError::Payment("insufficient funds".into());
        assert!(err.to_string().contains("insufficient funds"));
    }

    #[test]
    fn test_config_error_display() {
        let err = ElisymError::Config("missing key".into());
        assert!(err.to_string().contains("missing key"));
    }

    #[test]
    fn test_invalid_capability_card_display() {
        let err = ElisymError::InvalidCapabilityCard("bad card".into());
        assert!(err.to_string().contains("bad card"));
    }

    #[test]
    fn test_from_serde_json_error() {
        let json_err = serde_json::from_str::<String>("not-json").unwrap_err();
        let err: ElisymError = json_err.into();
        matches!(err, ElisymError::Json(_));
    }
}
