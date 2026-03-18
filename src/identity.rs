use nostr_sdk::prelude::*;
use serde::{Deserialize, Serialize};

use crate::error::{ElisymError, Result};

/// Wrapper around nostr_sdk::Keys providing agent identity management.
#[derive(Debug, Clone)]
pub struct AgentIdentity {
    keys: Keys,
}

impl AgentIdentity {
    /// Generate a new random identity.
    pub fn generate() -> Self {
        Self {
            keys: Keys::generate(),
        }
    }

    /// Create identity from a hex-encoded secret key.
    pub fn from_secret_key(hex: &str) -> Result<Self> {
        let secret_key = SecretKey::parse(hex)?;
        Ok(Self {
            keys: Keys::new(secret_key),
        })
    }

    /// Create identity from an nsec bech32 string.
    ///
    /// This is an alias for [`from_secret_key`](Self::from_secret_key) since
    /// `SecretKey::parse` accepts both hex and nsec formats.
    pub fn from_nsec(nsec: &str) -> Result<Self> {
        Self::from_secret_key(nsec)
    }

    /// Get the public key.
    pub fn public_key(&self) -> PublicKey {
        self.keys.public_key()
    }

    /// Get the npub bech32 representation.
    pub fn npub(&self) -> String {
        self.keys.public_key().to_bech32().unwrap_or_default()
    }

    /// Get a reference to the underlying Keys.
    pub fn keys(&self) -> &Keys {
        &self.keys
    }
}

/// Payment configuration for an agent.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PaymentInfo {
    /// Payment chain (e.g. "solana", "lightning").
    pub chain: String,
    /// Network within the chain (e.g. "devnet", "mainnet").
    pub network: String,
    /// On-chain address for receiving payments.
    pub address: String,
    /// Price per job in base units (lamports for Solana, msats for Lightning).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub job_price: Option<u64>,
}

/// Describes an agent's capabilities, published as NIP-89 event content.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CapabilityCard {
    pub name: String,
    pub description: String,
    pub capabilities: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub payment: Option<PaymentInfo>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub version: Option<String>,
}

impl CapabilityCard {
    pub fn new(
        name: impl Into<String>,
        description: impl Into<String>,
        capabilities: Vec<String>,
    ) -> Self {
        Self {
            name: name.into(),
            description: description.into(),
            capabilities,
            payment: None,
            version: None,
        }
    }

    pub fn set_payment(&mut self, payment: PaymentInfo) {
        self.payment = Some(payment);
    }

    pub fn set_version(&mut self, version: impl Into<String>) {
        self.version = Some(version.into());
    }

    pub fn to_json(&self) -> Result<String> {
        serde_json::to_string(self).map_err(ElisymError::from)
    }

    pub fn from_json(json: &str) -> Result<Self> {
        let card: Self = serde_json::from_str(json)?;
        if card.name.is_empty() {
            return Err(ElisymError::InvalidCapabilityCard(
                "name is required".into(),
            ));
        }
        Ok(card)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_identity_generate() {
        let id = AgentIdentity::generate();
        assert!(!id.npub().is_empty());
    }

    #[test]
    fn test_identity_roundtrip() {
        let id = AgentIdentity::generate();
        let hex = id.keys().secret_key().to_secret_hex();
        let restored = AgentIdentity::from_secret_key(&hex).unwrap();
        assert_eq!(id.public_key(), restored.public_key());
    }

    #[test]
    fn test_capability_card_serde() {
        let mut card = CapabilityCard::new("test-agent", "A test agent", vec!["translation".into()]);
        card.set_payment(PaymentInfo {
            chain: "solana".into(),
            network: "devnet".into(),
            address: "So1anaAddr...".into(),
            job_price: Some(10_000_000),
        });

        let json = card.to_json().unwrap();
        let parsed = CapabilityCard::from_json(&json).unwrap();

        assert_eq!(parsed.name, "test-agent");
        assert_eq!(parsed.capabilities, vec!["translation"]);
        let payment = parsed.payment.unwrap();
        assert_eq!(payment.chain, "solana");
        assert_eq!(payment.network, "devnet");
        assert_eq!(payment.address, "So1anaAddr...");
        assert_eq!(payment.job_price, Some(10_000_000));
    }

    #[test]
    fn test_capability_card_empty_name_fails() {
        let json = r#"{"name":"","description":"x","capabilities":[]}"#;
        assert!(CapabilityCard::from_json(json).is_err());
    }

    #[test]
    fn test_from_nsec_valid() {
        let id = AgentIdentity::generate();
        let nsec = id.keys().secret_key().to_bech32().unwrap();
        let restored = AgentIdentity::from_nsec(&nsec).unwrap();
        assert_eq!(id.public_key(), restored.public_key());
    }

    #[test]
    fn test_set_version() {
        let mut card = CapabilityCard::new("agent", "desc", vec![]);
        assert!(card.version.is_none());
        card.set_version("1.2.3");
        assert_eq!(card.version.as_deref(), Some("1.2.3"));
    }

    #[test]
    fn test_capability_card_from_json_extra_fields() {
        // Forward compat: unknown fields should be silently ignored
        let json = r#"{"name":"test","description":"x","capabilities":[],"future_field":"val"}"#;
        let card = CapabilityCard::from_json(json).unwrap();
        assert_eq!(card.name, "test");
    }

    #[test]
    fn test_capability_card_no_payment() {
        let json = r#"{"name":"test","description":"x","capabilities":["summarization"]}"#;
        let card = CapabilityCard::from_json(json).unwrap();
        assert!(card.payment.is_none());
    }
}
