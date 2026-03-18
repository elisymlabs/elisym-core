//! Payment provider abstraction for multi-chain support.
//!
//! This module defines the [`PaymentProvider`] trait — the core payment interface
//! that all payment backends implement. The trait is always compiled (no feature gate),
//! while concrete implementations are feature-gated.
//!
//! Current implementations:
//! - [`ldk::LdkPaymentProvider`] — Lightning Network via LDK-node (feature: `payments-ldk`)
//! - [`solana::SolanaPaymentProvider`] — Solana (native SOL) (feature: `payments-solana`)

#[cfg(feature = "payments-ldk")]
pub mod ldk;

#[cfg(feature = "payments-solana")]
pub mod solana;

use crate::error::Result;

/// Supported payment chains / networks.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum PaymentChain {
    Lightning,
    BitcoinOnchain,
    Solana,
    Ethereum,
    Custom(String),
}

impl std::fmt::Display for PaymentChain {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            PaymentChain::Lightning => write!(f, "lightning"),
            PaymentChain::BitcoinOnchain => write!(f, "bitcoin-onchain"),
            PaymentChain::Solana => write!(f, "solana"),
            PaymentChain::Ethereum => write!(f, "ethereum"),
            PaymentChain::Custom(s) => write!(f, "{}", s),
        }
    }
}

/// A chain-agnostic payment request (invoice, address, etc.).
#[derive(Debug, Clone)]
pub struct PaymentRequest {
    /// Which chain this request targets.
    pub chain: PaymentChain,
    /// Amount in the chain's base unit (msat for Lightning, lamports for Solana, etc.).
    pub amount: u64,
    /// Human-readable currency unit (e.g., "msat", "sat", "lamport").
    pub currency_unit: String,
    /// The payment request string (BOLT11 invoice, Solana address, etc.).
    pub request: String,
    /// Protocol fee included in this request (if any). Populated by
    /// `create_payment_request_with_protocol_fee()` / `create_payment_request_with_fee()`.
    pub fee_amount: Option<u64>,
}

/// Result of initiating a payment.
#[derive(Debug, Clone)]
pub struct PaymentResult {
    pub payment_id: String,
    pub status: String,
}

/// Status of a payment lookup.
#[derive(Debug, Clone)]
pub struct PaymentStatus {
    /// Whether the payment has been settled / confirmed.
    pub settled: bool,
    /// Amount in the chain's base unit, if known.
    pub amount: Option<u64>,
    /// Transaction signature/hash (e.g. Solana tx signature), if known.
    pub tx_signature: Option<String>,
}

/// Core payment interface that all payment backends implement.
///
/// Implementations must be `Send + Sync + Debug` to allow storage in `AgentNode`.
///
/// **Blocking behavior:** Methods on this trait are synchronous (`&self`, not
/// `async`). Implementations may perform blocking I/O (e.g., Solana RPC calls,
/// LDK-node operations). In async contexts, wrap calls in
/// `tokio::task::spawn_blocking()` to avoid blocking the async runtime.
/// See `AgentNode::process_job_with_payment()` for an example.
pub trait PaymentProvider: Send + Sync + std::fmt::Debug {
    /// Which chain this provider operates on.
    fn chain(&self) -> PaymentChain;

    /// Create a payment request (invoice) for the given amount.
    ///
    /// # Amount units (important — wrong unit = wrong payment!)
    ///
    /// | Chain     | Unit     | 1 SOL / 1 BTC equivalent |
    /// |-----------|----------|--------------------------|
    /// | Lightning | **millisatoshi (msat)** | 100,000,000,000 msat = 1 BTC |
    /// | Solana    | **lamport**             | 1,000,000,000 lamports = 1 SOL |
    ///
    /// - `amount`: amount in the chain's base unit — see table above
    /// - `description`: human-readable description (Lightning only; Solana ignores this)
    /// - `expiry_secs`: how long the request is valid (Lightning enforced at protocol level;
    ///   Solana embeds the timestamp and checks expiry in `lookup_payment`/`pay`)
    fn create_payment_request(
        &self,
        amount: u64,
        description: &str,
        expiry_secs: u32,
    ) -> Result<PaymentRequest>;

    /// Pay a payment request string (BOLT11 invoice for Lightning, JSON for Solana, etc.).
    fn pay(&self, request: &str) -> Result<PaymentResult>;

    /// Look up the status of a payment by its request string.
    fn lookup_payment(&self, request: &str) -> Result<PaymentStatus>;

    /// Check whether a payment request has been paid.
    fn is_paid(&self, request: &str) -> Result<bool> {
        Ok(self.lookup_payment(request)?.settled)
    }

    /// Downcast to a concrete type. Enables access to backend-specific methods.
    fn as_any(&self) -> &dyn std::any::Any;
}

