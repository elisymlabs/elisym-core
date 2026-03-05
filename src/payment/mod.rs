//! Payment provider abstraction for multi-chain support.
//!
//! This module defines the [`PaymentProvider`] trait — the core payment interface
//! that all payment backends implement. The trait is always compiled (no feature gate),
//! while concrete implementations are feature-gated.
//!
//! Current implementations:
//! - [`ldk::LdkPaymentProvider`] — Lightning Network via LDK-node (feature: `payments-ldk`)
//! - [`solana::SolanaPaymentProvider`] — Solana (SOL + SPL tokens) (feature: `payments-solana`)

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
}

/// Core payment interface that all payment backends implement.
///
/// Implementations must be `Send + Sync + Debug` to allow storage in `AgentNode`.
pub trait PaymentProvider: Send + Sync + std::fmt::Debug {
    /// Which chain this provider operates on.
    fn chain(&self) -> PaymentChain;

    /// Create a payment request (invoice) for the given amount.
    ///
    /// - `amount`: amount in the chain's base unit (msat for Lightning)
    /// - `description`: human-readable description
    /// - `expiry_secs`: how long the request is valid
    fn create_payment_request(
        &self,
        amount: u64,
        description: &str,
        expiry_secs: u32,
    ) -> Result<PaymentRequest>;

    /// Pay a payment request string (BOLT11 invoice, etc.).
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

/// Fee configuration for app developers.
///
/// Fee is expressed in **basis points** (1 bps = 0.01%, 100 bps = 1%, 300 bps = 3%).
/// All arithmetic is integer-only to avoid floating-point rounding errors.
#[derive(Debug, Clone)]
pub struct FeeConfig {
    /// App's fee in basis points (e.g., 300 = 3%).
    pub app_fee_bps: u64,
    /// Address to receive the app's fee.
    pub app_fee_address: String,
    /// Chain for the app's fee payment.
    pub app_fee_chain: PaymentChain,
}

impl FeeConfig {
    /// Calculate inclusive fee for a given payment amount.
    /// The fee is taken from the amount (not added on top).
    /// Returns `(provider_amount, fee)` where `provider_amount + fee == amount`.
    ///
    /// Uses ceiling division: `fee = (amount * bps + 9999) / 10000`.
    pub fn calculate(&self, amount: u64) -> (u64, u64) {
        let fee = amount
            .checked_mul(self.app_fee_bps)
            .map(|n| n.div_ceil(10_000))
            .unwrap_or(amount); // overflow guard: cap fee at amount
        let fee = fee.min(amount); // never exceed amount
        let provider_amount = amount - fee;
        (provider_amount, fee)
    }
}
