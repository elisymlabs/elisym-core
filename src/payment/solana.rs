//! Solana payment provider — native SOL transfers.
//!
//! Uses a reference-based payment detection approach: each payment request includes
//! a unique ephemeral reference pubkey added as a read-only non-signer to the transfer
//! instruction. The provider detects payment via `getSignaturesForAddress(reference)`.

use std::collections::HashMap;
use std::sync::Mutex;

use solana_client::rpc_client::RpcClient;
use solana_sdk::{
    commitment_config::CommitmentConfig,
    instruction::{AccountMeta, Instruction},
    message::Message,
    pubkey::Pubkey,
    signature::{Keypair, Signature, Signer},
    transaction::Transaction,
};
use solana_transaction_status_client_types::{
    EncodedTransaction, UiMessage, UiTransactionEncoding,
};

use crate::error::{ElisymError, Result};
use crate::payment::{PaymentChain, PaymentProvider, PaymentRequest, PaymentResult, PaymentStatus};

/// Solana network selection.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SolanaNetwork {
    Mainnet,
    Devnet,
    Testnet,
    Custom(String),
}

impl SolanaNetwork {
    /// Default RPC URL for this network.
    pub fn rpc_url(&self) -> String {
        match self {
            SolanaNetwork::Mainnet => "https://api.mainnet-beta.solana.com".to_string(),
            SolanaNetwork::Devnet => "https://api.devnet.solana.com".to_string(),
            SolanaNetwork::Testnet => "https://api.testnet.solana.com".to_string(),
            SolanaNetwork::Custom(url) => url.clone(),
        }
    }
}

/// Configuration for the Solana payment provider.
#[derive(Debug, Clone)]
pub struct SolanaPaymentConfig {
    /// Network to connect to.
    pub network: SolanaNetwork,
    /// Custom RPC URL (overrides the network default if set).
    pub rpc_url: Option<String>,
}

impl Default for SolanaPaymentConfig {
    fn default() -> Self {
        Self {
            network: SolanaNetwork::Devnet,
            rpc_url: None,
        }
    }
}

impl SolanaPaymentConfig {
    fn effective_rpc_url(&self) -> String {
        self.rpc_url
            .clone()
            .unwrap_or_else(|| self.network.rpc_url())
    }
}

/// Internal request format serialized as JSON in the payment request string.
#[derive(serde::Serialize, serde::Deserialize, Debug, Clone)]
struct SolanaPaymentRequestData {
    /// Recipient's base58 public key.
    recipient: String,
    /// Amount in lamports.
    amount: u64,
    /// Ephemeral reference pubkey for payment detection.
    reference: String,
    /// Fee recipient address (omitted when no fee configured).
    #[serde(skip_serializing_if = "Option::is_none")]
    fee_address: Option<String>,
    /// Fee amount in lamports (omitted when no fee configured).
    #[serde(skip_serializing_if = "Option::is_none")]
    fee_amount: Option<u64>,
}

/// Tracks a pending payment request.
#[derive(Debug)]
struct PendingPayment {
    amount: u64,
    settled: bool,
}

/// Maximum number of entries allowed in the pending map before cleanup.
const PENDING_MAP_CAP: usize = 10_000;

/// Solana payment provider supporting native SOL transfers.
///
/// Uses reference-based payment detection: each payment request includes a unique
/// ephemeral reference pubkey. Payment confirmation is done by checking
/// `getSignaturesForAddress(reference)`.
pub struct SolanaPaymentProvider {
    config: SolanaPaymentConfig,
    keypair: Keypair,
    rpc_client: RpcClient,
    pending: Mutex<HashMap<String, PendingPayment>>,
}

impl std::fmt::Debug for SolanaPaymentProvider {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SolanaPaymentProvider")
            .field("config", &self.config)
            .field("address", &self.keypair.pubkey().to_string())
            .finish()
    }
}

impl SolanaPaymentProvider {
    /// Create a new Solana payment provider with the given config and keypair.
    pub fn new(config: SolanaPaymentConfig, keypair: Keypair) -> Self {
        let rpc_url = config.effective_rpc_url();
        let rpc_client = RpcClient::new_with_commitment(rpc_url, CommitmentConfig::confirmed());
        Self {
            config,
            keypair,
            rpc_client,
            pending: Mutex::new(HashMap::new()),
        }
    }

    /// Create from a base58-encoded secret key.
    pub fn from_secret_key(config: SolanaPaymentConfig, base58_secret: &str) -> Result<Self> {
        let bytes = bs58::decode(base58_secret)
            .into_vec()
            .map_err(|e| ElisymError::Payment(format!("Invalid base58 secret key: {}", e)))?;
        Self::from_bytes(config, &bytes)
    }

    /// Create from raw secret key bytes.
    pub fn from_bytes(config: SolanaPaymentConfig, bytes: &[u8]) -> Result<Self> {
        let keypair = Keypair::try_from(bytes)
            .map_err(|e| ElisymError::Payment(format!("Invalid keypair bytes: {}", e)))?;
        Ok(Self::new(config, keypair))
    }

    /// Get this provider's Solana address (base58).
    pub fn address(&self) -> String {
        self.keypair.pubkey().to_string()
    }

    /// Get the SOL balance in lamports.
    pub fn balance(&self) -> Result<u64> {
        self.rpc_client
            .get_balance(&self.keypair.pubkey())
            .map_err(|e| ElisymError::Payment(format!("Failed to get balance: {}", e)))
    }

    /// Create a payment request with an embedded fee split.
    ///
    /// The fee is inclusive (subtracted from `amount`, not added on top).
    /// When a customer calls `pay()` on this request, the transaction will
    /// send `amount - fee_amount` to the provider and `fee_amount` to `fee_address`.
    ///
    /// Fee calculation is the caller's responsibility — this method only embeds
    /// the pre-computed values into the payment request.
    ///
    /// # Example
    ///
    /// With `amount = 100_000` lamports and `fee_amount = 3_000` lamports (3%),
    /// the provider receives 97_000 lamports and the fee address receives 3_000.
    pub fn create_payment_request_with_fee(
        &self,
        amount: u64,
        description: &str,
        expiry_secs: u32,
        fee_address: &str,
        fee_amount: u64,
    ) -> Result<PaymentRequest> {
        if fee_amount >= amount {
            return Err(ElisymError::Payment(
                "fee_amount must be less than amount".into(),
            ));
        }
        self.create_payment_request_inner(
            amount,
            description,
            expiry_secs,
            if fee_amount > 0 { Some((fee_address.to_string(), fee_amount)) } else { None },
        )
    }

    /// Request an airdrop of SOL (devnet/testnet only).
    pub fn request_airdrop(&self, lamports: u64) -> Result<String> {
        let sig = self
            .rpc_client
            .request_airdrop(&self.keypair.pubkey(), lamports)
            .map_err(|e| ElisymError::Payment(format!("Airdrop failed: {}", e)))?;
        Ok(sig.to_string())
    }

    /// Build a native SOL transfer instruction with reference key.
    /// If `fee_params` is provided, adds a second transfer for the fee amount
    /// and sends `(amount - fee)` to the recipient.
    fn build_transfer(
        &self,
        recipient: &Pubkey,
        amount: u64,
        reference: &Pubkey,
        fee_params: Option<&(Pubkey, u64)>,
    ) -> Result<Transaction> {
        let mut instructions: Vec<Instruction> = Vec::new();

        let provider_amount = if let Some((_, fee_amount)) = fee_params {
            amount.saturating_sub(*fee_amount)
        } else {
            amount
        };

        // Provider transfer with reference key
        #[allow(deprecated)]
        let mut transfer_ix =
            solana_sdk::system_instruction::transfer(&self.keypair.pubkey(), recipient, provider_amount);
        transfer_ix
            .accounts
            .push(AccountMeta::new_readonly(*reference, false));
        instructions.push(transfer_ix);

        // Fee transfer
        if let Some((fee_address, fee_amount)) = fee_params {
            #[allow(deprecated)]
            let fee_ix =
                solana_sdk::system_instruction::transfer(&self.keypair.pubkey(), fee_address, *fee_amount);
            instructions.push(fee_ix);
        }

        let recent_blockhash = self
            .rpc_client
            .get_latest_blockhash()
            .map_err(|e| ElisymError::Payment(format!("Failed to get blockhash: {}", e)))?;

        let message =
            Message::new_with_blockhash(&instructions, Some(&self.keypair.pubkey()), &recent_blockhash);
        let tx = Transaction::new(&[&self.keypair], message, recent_blockhash);
        Ok(tx)
    }

    /// Shared logic for creating a payment request, with optional fee.
    fn create_payment_request_inner(
        &self,
        amount: u64,
        _description: &str,
        _expiry_secs: u32,
        fee: Option<(String, u64)>,
    ) -> Result<PaymentRequest> {
        if amount == 0 {
            return Err(ElisymError::Payment(
                "Payment amount must be greater than 0".into(),
            ));
        }

        // Generate ephemeral reference keypair for payment detection
        let reference_keypair = Keypair::new();
        let reference = reference_keypair.pubkey();

        let (fee_address, fee_amount) = match fee {
            Some((addr, amt)) => (Some(addr), Some(amt)),
            None => (None, None),
        };

        let data = SolanaPaymentRequestData {
            recipient: self.keypair.pubkey().to_string(),
            amount,
            reference: reference.to_string(),
            fee_address,
            fee_amount,
        };

        let request = serde_json::to_string(&data)
            .map_err(|e| ElisymError::Payment(format!("Failed to serialize request: {}", e)))?;

        // Track this pending payment
        {
            let mut pending = self
                .pending
                .lock()
                .map_err(|_| ElisymError::Payment("internal lock poisoned".into()))?;

            // Cap the pending map size by clearing settled entries
            if pending.len() >= PENDING_MAP_CAP {
                pending.retain(|_, v| !v.settled);
            }

            pending.insert(
                request.clone(),
                PendingPayment {
                    amount,
                    settled: false,
                },
            );
        }

        Ok(PaymentRequest {
            chain: PaymentChain::Solana,
            amount,
            currency_unit: "lamport".to_string(),
            request,
        })
    }
}

impl PaymentProvider for SolanaPaymentProvider {
    fn chain(&self) -> PaymentChain {
        PaymentChain::Solana
    }

    fn create_payment_request(
        &self,
        amount: u64,
        description: &str,
        expiry_secs: u32,
    ) -> Result<PaymentRequest> {
        self.create_payment_request_inner(amount, description, expiry_secs, None)
    }

    /// Pay a Solana payment request by sending a SOL transfer on-chain.
    ///
    /// # Security
    ///
    /// The caller **MUST** validate `fee_address` and `fee_amount` from the
    /// deserialized payment request before calling this method. The payment request
    /// is untrusted data — a malicious provider could set an arbitrary fee address
    /// or inflate the fee amount. Always verify that the fee parameters match the
    /// expected application fee configuration.
    fn pay(&self, request: &str) -> Result<PaymentResult> {
        let data: SolanaPaymentRequestData = serde_json::from_str(request)
            .map_err(|e| ElisymError::Payment(format!("Invalid payment request: {}", e)))?;

        let recipient: Pubkey = data
            .recipient
            .parse()
            .map_err(|e| ElisymError::Payment(format!("Invalid recipient address: {:?}", e)))?;

        let reference: Pubkey = data
            .reference
            .parse()
            .map_err(|e| ElisymError::Payment(format!("Invalid reference pubkey: {:?}", e)))?;

        // Parse optional fee parameters.
        // SECURITY: fee_address and fee_amount come from the untrusted payment
        // request created by the provider. Callers MUST validate these values
        // against their expected fee configuration before calling pay().
        let fee_params = match (data.fee_address, data.fee_amount) {
            (Some(addr), Some(amt)) if amt > 0 => {
                tracing::warn!(
                    fee_address = %addr,
                    fee_amount = amt,
                    total_amount = data.amount,
                    fee_pct = format!("{:.1}%", (amt as f64 / data.amount as f64) * 100.0),
                    "Payment request contains fee parameters — ensure these were validated before calling pay()"
                );
                if amt >= data.amount {
                    return Err(ElisymError::Payment(format!(
                        "fee_amount ({}) must be less than total amount ({})",
                        amt, data.amount
                    )));
                }
                let fee_pubkey: Pubkey = addr.parse().map_err(|e| {
                    ElisymError::Payment(format!("Invalid fee address: {:?}", e))
                })?;
                Some((fee_pubkey, amt))
            }
            _ => None,
        };

        let tx = self.build_transfer(&recipient, data.amount, &reference, fee_params.as_ref())?;

        let sig = self
            .rpc_client
            .send_and_confirm_transaction(&tx)
            .map_err(|e| ElisymError::Payment(format!("Transaction failed: {}", e)))?;

        Ok(PaymentResult {
            payment_id: sig.to_string(),
            status: "confirmed".to_string(),
        })
    }

    fn lookup_payment(&self, request: &str) -> Result<PaymentStatus> {
        // Check local cache first
        {
            let pending = self
                .pending
                .lock()
                .map_err(|_| ElisymError::Payment("internal lock poisoned".into()))?;
            if let Some(p) = pending.get(request) {
                if p.settled {
                    return Ok(PaymentStatus {
                        settled: true,
                        amount: Some(p.amount),
                    });
                }
            }
        }

        // Parse the request to get the reference pubkey
        let data: SolanaPaymentRequestData = serde_json::from_str(request)
            .map_err(|e| ElisymError::Payment(format!("Invalid payment request: {}", e)))?;

        let reference: Pubkey = data
            .reference
            .parse()
            .map_err(|e| ElisymError::Payment(format!("Invalid reference pubkey: {:?}", e)))?;

        // Expected net amount the provider should receive
        let expected_net = data.amount.saturating_sub(data.fee_amount.unwrap_or(0));

        // Query for signatures on the reference address
        let sigs = self
            .rpc_client
            .get_signatures_for_address(&reference)
            .map_err(|e| {
                ElisymError::Payment(format!("Failed to query signatures: {}", e))
            })?;

        if sigs.is_empty() {
            return Ok(PaymentStatus {
                settled: false,
                amount: None,
            });
        }

        // Verify the on-chain transfer amount
        for sig_info in &sigs {
            if sig_info.err.is_some() {
                continue; // skip failed transactions
            }

            let sig: Signature = sig_info.signature.parse().map_err(|e| {
                ElisymError::Payment(format!("Invalid signature: {:?}", e))
            })?;

            let tx_response = self
                .rpc_client
                .get_transaction(&sig, UiTransactionEncoding::Json)
                .map_err(|e| {
                    ElisymError::Payment(format!("Failed to get transaction: {}", e))
                })?;

            let meta = match &tx_response.transaction.meta {
                Some(m) => m,
                None => continue,
            };

            // Extract account keys from the transaction
            let account_keys: Vec<String> = match &tx_response.transaction.transaction {
                EncodedTransaction::Json(ui_tx) => match &ui_tx.message {
                    UiMessage::Parsed(parsed) => {
                        parsed.account_keys.iter().map(|k| k.pubkey.clone()).collect()
                    }
                    UiMessage::Raw(raw) => raw.account_keys.clone(),
                },
                _ => continue,
            };

            // Find recipient's index and verify SOL balance change
            if let Some(idx) = account_keys.iter().position(|k| k == &data.recipient) {
                let pre = meta.pre_balances[idx];
                let post = meta.post_balances[idx];
                let received = post.saturating_sub(pre);

                if received >= expected_net {
                    // Payment verified — remove from pending (no longer needed)
                    let mut pending = self
                        .pending
                        .lock()
                        .map_err(|_| ElisymError::Payment("internal lock poisoned".into()))?;
                    pending.remove(request);
                    return Ok(PaymentStatus {
                        settled: true,
                        amount: Some(received),
                    });
                }
            }
        }

        // Signature found but amount insufficient
        Ok(PaymentStatus {
            settled: false,
            amount: None,
        })
    }

    fn as_any(&self) -> &dyn std::any::Any {
        self
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_config_defaults() {
        let config = SolanaPaymentConfig::default();
        assert_eq!(config.network, SolanaNetwork::Devnet);
        assert!(config.rpc_url.is_none());
    }

    #[test]
    fn test_network_rpc_urls() {
        assert_eq!(
            SolanaNetwork::Mainnet.rpc_url(),
            "https://api.mainnet-beta.solana.com"
        );
        assert_eq!(
            SolanaNetwork::Devnet.rpc_url(),
            "https://api.devnet.solana.com"
        );
        assert_eq!(
            SolanaNetwork::Testnet.rpc_url(),
            "https://api.testnet.solana.com"
        );
        assert_eq!(
            SolanaNetwork::Custom("http://localhost:8899".to_string()).rpc_url(),
            "http://localhost:8899"
        );
    }

    #[test]
    fn test_custom_rpc_url_overrides_network() {
        let config = SolanaPaymentConfig {
            network: SolanaNetwork::Devnet,
            rpc_url: Some("http://my-rpc:8899".to_string()),
        };
        assert_eq!(config.effective_rpc_url(), "http://my-rpc:8899");
    }

    #[test]
    fn test_request_serialization_roundtrip_sol() {
        let data = SolanaPaymentRequestData {
            recipient: "11111111111111111111111111111111".to_string(),
            amount: 10_000_000,
            reference: "22222222222222222222222222222222".to_string(),
            fee_address: None,
            fee_amount: None,
        };
        let json = serde_json::to_string(&data).unwrap();
        assert!(!json.contains("fee_address"));
        assert!(!json.contains("fee_amount"));
        let parsed: SolanaPaymentRequestData = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.recipient, data.recipient);
        assert_eq!(parsed.amount, data.amount);
        assert_eq!(parsed.reference, data.reference);
        assert!(parsed.fee_address.is_none());
        assert!(parsed.fee_amount.is_none());
    }

    #[test]
    fn test_request_serialization_with_fee() {
        let data = SolanaPaymentRequestData {
            recipient: "11111111111111111111111111111111".to_string(),
            amount: 100_000,
            reference: "22222222222222222222222222222222".to_string(),
            fee_address: Some("33333333333333333333333333333333".to_string()),
            fee_amount: Some(3_000),
        };
        let json = serde_json::to_string(&data).unwrap();
        assert!(json.contains("fee_address"));
        assert!(json.contains("fee_amount"));
        let parsed: SolanaPaymentRequestData = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.fee_address.as_deref(), Some("33333333333333333333333333333333"));
        assert_eq!(parsed.fee_amount, Some(3_000));
    }

    #[test]
    fn test_backwards_compat_no_fee() {
        // Old format without fee fields should still parse
        let json = r#"{"recipient":"11111111111111111111111111111111","amount":100000,"reference":"22222222222222222222222222222222"}"#;
        let parsed: SolanaPaymentRequestData = serde_json::from_str(json).unwrap();
        assert_eq!(parsed.amount, 100_000);
        assert!(parsed.fee_address.is_none());
        assert!(parsed.fee_amount.is_none());
    }

    #[test]
    fn test_zero_amount_rejected() {
        let keypair = Keypair::new();
        let provider = SolanaPaymentProvider::new(SolanaPaymentConfig::default(), keypair);
        let result = provider.create_payment_request(0, "test", 3600);
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("greater than 0"));
    }

    #[test]
    fn test_chain_returns_solana() {
        let keypair = Keypair::new();
        let provider = SolanaPaymentProvider::new(SolanaPaymentConfig::default(), keypair);
        assert_eq!(provider.chain(), PaymentChain::Solana);
    }

    #[test]
    fn test_address() {
        let keypair = Keypair::new();
        let expected = keypair.pubkey().to_string();
        let provider = SolanaPaymentProvider::new(SolanaPaymentConfig::default(), keypair);
        assert_eq!(provider.address(), expected);
    }

    #[test]
    fn test_create_payment_request_sol() {
        let keypair = Keypair::new();
        let provider = SolanaPaymentProvider::new(SolanaPaymentConfig::default(), keypair);
        let req = provider
            .create_payment_request(10_000_000, "test payment", 3600)
            .unwrap();
        assert_eq!(req.chain, PaymentChain::Solana);
        assert_eq!(req.amount, 10_000_000);
        assert_eq!(req.currency_unit, "lamport");

        // Verify the request string is valid JSON with expected fields
        let data: SolanaPaymentRequestData = serde_json::from_str(&req.request).unwrap();
        assert_eq!(data.amount, 10_000_000);
    }

    #[test]
    fn test_pending_map_cap_cleanup() {
        let keypair = Keypair::new();
        let provider = SolanaPaymentProvider::new(SolanaPaymentConfig::default(), keypair);

        // Insert settled entries up to the cap
        {
            let mut pending = provider.pending.lock().unwrap();
            for i in 0..PENDING_MAP_CAP {
                pending.insert(
                    format!("request_{}", i),
                    PendingPayment {
                        amount: 1000,
                        settled: true,
                    },
                );
            }
            assert_eq!(pending.len(), PENDING_MAP_CAP);
        }

        // Creating a new request should trigger cleanup of settled entries
        let req = provider.create_payment_request(1000, "test", 3600).unwrap();
        {
            let pending = provider.pending.lock().unwrap();
            // Only the new request should remain (all old ones were settled and cleaned)
            assert_eq!(pending.len(), 1);
            assert!(pending.contains_key(&req.request));
        }
    }
}
