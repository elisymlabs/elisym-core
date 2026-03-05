//! Solana payment provider — SOL and SPL token transfers.
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
    signature::{Keypair, Signer},
    transaction::Transaction,
};

use crate::error::{ElisymError, Result};
use crate::payment::{FeeConfig, PaymentChain, PaymentProvider, PaymentRequest, PaymentResult, PaymentStatus};

/// USDC SPL token mint on Solana mainnet.
pub const USDC_MINT_MAINNET: &str = "EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v";

/// USDC SPL token mint on Solana devnet.
pub const USDC_MINT_DEVNET: &str = "4zMMC9srt5Ri5X14GAgXhaHii3GnPAEERYPJgZJDncDU";

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

/// Token type for Solana payments.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SolanaToken {
    /// Native SOL (amounts in lamports).
    Sol,
    /// SPL token with its mint address and decimals.
    Spl { mint: Pubkey, decimals: u8 },
}

/// Configuration for the Solana payment provider.
#[derive(Debug, Clone)]
pub struct SolanaPaymentConfig {
    /// Network to connect to.
    pub network: SolanaNetwork,
    /// Custom RPC URL (overrides the network default if set).
    pub rpc_url: Option<String>,
    /// Token type (native SOL or SPL token).
    pub token: SolanaToken,
}

impl Default for SolanaPaymentConfig {
    fn default() -> Self {
        Self {
            network: SolanaNetwork::Devnet,
            rpc_url: None,
            token: SolanaToken::Sol,
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
    /// Amount in base units (lamports for SOL, smallest unit for SPL).
    amount: u64,
    /// Ephemeral reference pubkey for payment detection.
    reference: String,
    /// SPL token mint (omitted for native SOL).
    #[serde(skip_serializing_if = "Option::is_none")]
    mint: Option<String>,
    /// Token decimals (omitted for native SOL).
    #[serde(skip_serializing_if = "Option::is_none")]
    decimals: Option<u8>,
    /// Fee recipient address (omitted when no fee configured).
    #[serde(skip_serializing_if = "Option::is_none")]
    fee_address: Option<String>,
    /// Fee amount in base units (omitted when no fee configured).
    #[serde(skip_serializing_if = "Option::is_none")]
    fee_amount: Option<u64>,
}

/// Tracks a pending payment request.
#[derive(Debug)]
struct PendingPayment {
    amount: u64,
    settled: bool,
}

/// Solana payment provider supporting native SOL and SPL token transfers.
///
/// Uses reference-based payment detection: each payment request includes a unique
/// ephemeral reference pubkey. Payment confirmation is done by checking
/// `getSignaturesForAddress(reference)`.
pub struct SolanaPaymentProvider {
    config: SolanaPaymentConfig,
    keypair: Keypair,
    rpc_client: RpcClient,
    pending: Mutex<HashMap<String, PendingPayment>>,
    fee_config: Option<FeeConfig>,
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
            fee_config: None,
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

    /// Get SPL token balance (in token base units) for the configured token.
    /// Returns 0 if no token account exists.
    pub fn token_balance(&self) -> Result<u64> {
        let mint = match &self.config.token {
            SolanaToken::Sol => {
                return self.balance();
            }
            SolanaToken::Spl { mint, .. } => *mint,
        };

        let ata = spl_associated_token_account::get_associated_token_address(
            &self.keypair.pubkey(),
            &mint,
        );

        match self.rpc_client.get_token_account_balance(&ata) {
            Ok(balance) => balance
                .amount
                .parse::<u64>()
                .map_err(|e| ElisymError::Payment(format!("Failed to parse token balance: {}", e))),
            Err(_) => Ok(0), // No token account → zero balance
        }
    }

    /// Set a fee configuration for protocol fees.
    /// When set, `create_payment_request()` embeds fee info in the request JSON,
    /// and `pay()` splits the payment into provider + fee transfers.
    pub fn set_fee_config(&mut self, config: FeeConfig) {
        self.fee_config = Some(config);
    }

    /// Request an airdrop of SOL (devnet/testnet only).
    pub fn request_airdrop(&self, lamports: u64) -> Result<String> {
        let sig = self
            .rpc_client
            .request_airdrop(&self.keypair.pubkey(), lamports)
            .map_err(|e| ElisymError::Payment(format!("Airdrop failed: {}", e)))?;
        Ok(sig.to_string())
    }

    /// Check if a fee transfer is viable: the destination account must either
    /// already exist or the transfer amount must meet rent-exempt minimum.
    /// Returns true if the fee should be included, false if it should be skipped.
    fn should_include_fee(&self, fee_address: &Pubkey, fee_amount: u64) -> bool {
        // If account already has balance, any transfer amount is fine
        match self.rpc_client.get_balance(fee_address) {
            Ok(balance) if balance > 0 => return true,
            Ok(_) => {} // zero balance — need rent check
            Err(_) => {} // can't reach RPC — be conservative, check rent
        }

        // Account doesn't exist: fee must meet rent-exempt minimum for a 0-data account
        let rent_exempt_min = self
            .rpc_client
            .get_minimum_balance_for_rent_exemption(0)
            .unwrap_or(890_880); // fallback to known constant

        if fee_amount < rent_exempt_min {
            tracing::warn!(
                fee_amount,
                rent_exempt_min,
                fee_address = %fee_address,
                "fee below rent-exempt minimum and account doesn't exist — skipping fee transfer"
            );
            return false;
        }
        true
    }

    /// Build a native SOL transfer instruction with reference key.
    /// If `fee_params` is provided, adds a second transfer for the fee amount
    /// and sends `(amount - fee)` to the recipient.
    fn build_sol_transfer(
        &self,
        recipient: &Pubkey,
        amount: u64,
        reference: &Pubkey,
        fee_params: Option<&(Pubkey, u64)>,
    ) -> Result<Transaction> {
        let mut instructions: Vec<Instruction> = Vec::new();

        // Check if fee transfer is viable (rent-exempt check)
        let effective_fee = fee_params.filter(|(addr, amt)| self.should_include_fee(addr, *amt));

        let provider_amount = if let Some((_, fee_amount)) = effective_fee {
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

        // Fee transfer (if viable)
        if let Some((fee_address, fee_amount)) = effective_fee {
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

    /// Build an SPL token transfer instruction with reference key.
    /// If `fee_params` is provided, adds a second SPL transfer for the fee amount
    /// and sends `(amount - fee)` to the recipient.
    fn build_spl_transfer(
        &self,
        recipient: &Pubkey,
        amount: u64,
        mint: &Pubkey,
        reference: &Pubkey,
        fee_params: Option<&(Pubkey, u64)>,
    ) -> Result<Transaction> {
        let sender_ata = spl_associated_token_account::get_associated_token_address(
            &self.keypair.pubkey(),
            mint,
        );
        let recipient_ata =
            spl_associated_token_account::get_associated_token_address(recipient, mint);

        let mut instructions: Vec<Instruction> = Vec::new();

        // Create recipient ATA if it doesn't exist (idempotent)
        instructions.push(
            spl_associated_token_account::instruction::create_associated_token_account_idempotent(
                &self.keypair.pubkey(),
                recipient,
                mint,
                &spl_token::id(),
            ),
        );

        let provider_amount = if let Some((_, fee_amount)) = fee_params {
            amount.saturating_sub(*fee_amount)
        } else {
            amount
        };

        // SPL token transfer to provider
        let mut transfer_ix = spl_token::instruction::transfer(
            &spl_token::id(),
            &sender_ata,
            &recipient_ata,
            &self.keypair.pubkey(),
            &[],
            provider_amount,
        )
        .map_err(|e| ElisymError::Payment(format!("Failed to create SPL transfer: {}", e)))?;

        // Add reference pubkey as read-only non-signer for payment detection
        transfer_ix
            .accounts
            .push(AccountMeta::new_readonly(*reference, false));
        instructions.push(transfer_ix);

        // Fee transfer (if configured)
        if let Some((fee_address, fee_amount)) = fee_params {
            let fee_ata =
                spl_associated_token_account::get_associated_token_address(fee_address, mint);

            // Create fee ATA idempotently
            instructions.push(
                spl_associated_token_account::instruction::create_associated_token_account_idempotent(
                    &self.keypair.pubkey(),
                    fee_address,
                    mint,
                    &spl_token::id(),
                ),
            );

            let fee_ix = spl_token::instruction::transfer(
                &spl_token::id(),
                &sender_ata,
                &fee_ata,
                &self.keypair.pubkey(),
                &[],
                *fee_amount,
            )
            .map_err(|e| ElisymError::Payment(format!("Failed to create SPL fee transfer: {}", e)))?;
            instructions.push(fee_ix);
        }

        let recent_blockhash = self
            .rpc_client
            .get_latest_blockhash()
            .map_err(|e| ElisymError::Payment(format!("Failed to get blockhash: {}", e)))?;

        let message = Message::new_with_blockhash(
            &instructions,
            Some(&self.keypair.pubkey()),
            &recent_blockhash,
        );
        let tx = Transaction::new(&[&self.keypair], message, recent_blockhash);
        Ok(tx)
    }
}

impl PaymentProvider for SolanaPaymentProvider {
    fn chain(&self) -> PaymentChain {
        PaymentChain::Solana
    }

    fn create_payment_request(
        &self,
        amount: u64,
        _description: &str,
        _expiry_secs: u32,
    ) -> Result<PaymentRequest> {
        if amount == 0 {
            return Err(ElisymError::Payment(
                "Payment amount must be greater than 0".into(),
            ));
        }

        // Generate ephemeral reference keypair for payment detection
        let reference_keypair = Keypair::new();
        let reference = reference_keypair.pubkey();

        let (mint, decimals) = match &self.config.token {
            SolanaToken::Sol => (None, None),
            SolanaToken::Spl { mint, decimals } => {
                (Some(mint.to_string()), Some(*decimals))
            }
        };

        let currency_unit = match &self.config.token {
            SolanaToken::Sol => "lamport".to_string(),
            SolanaToken::Spl { decimals, .. } => format!("token({}dp)", decimals),
        };

        let (fee_address, fee_amount) = if let Some(ref fc) = self.fee_config {
            let (_provider_amount, fee) = fc.calculate(amount);
            if fee > 0 {
                (Some(fc.app_fee_address.clone()), Some(fee))
            } else {
                (None, None)
            }
        } else {
            (None, None)
        };

        let data = SolanaPaymentRequestData {
            recipient: self.keypair.pubkey().to_string(),
            amount,
            reference: reference.to_string(),
            mint,
            decimals,
            fee_address,
            fee_amount,
        };

        let request = serde_json::to_string(&data)
            .map_err(|e| ElisymError::Payment(format!("Failed to serialize request: {}", e)))?;

        // Track this pending payment
        {
            let mut pending = self.pending.lock().unwrap();
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
            currency_unit,
            request,
        })
    }

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

        // Parse optional fee parameters
        let fee_params = match (data.fee_address, data.fee_amount) {
            (Some(addr), Some(amt)) if amt > 0 => {
                let fee_pubkey: Pubkey = addr.parse().map_err(|e| {
                    ElisymError::Payment(format!("Invalid fee address: {:?}", e))
                })?;
                Some((fee_pubkey, amt))
            }
            _ => None,
        };

        let tx = match data.mint {
            None => self.build_sol_transfer(&recipient, data.amount, &reference, fee_params.as_ref())?,
            Some(mint_str) => {
                let mint: Pubkey = mint_str
                    .parse()
                    .map_err(|e| ElisymError::Payment(format!("Invalid mint address: {:?}", e)))?;
                self.build_spl_transfer(&recipient, data.amount, &mint, &reference, fee_params.as_ref())?
            }
        };

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
            let pending = self.pending.lock().unwrap();
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

        // Found a signature — mark as settled in cache
        {
            let mut pending = self.pending.lock().unwrap();
            if let Some(p) = pending.get_mut(request) {
                p.settled = true;
            }
        }

        Ok(PaymentStatus {
            settled: true,
            amount: Some(data.amount),
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
        assert_eq!(config.token, SolanaToken::Sol);
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
            token: SolanaToken::Sol,
        };
        assert_eq!(config.effective_rpc_url(), "http://my-rpc:8899");
    }

    #[test]
    fn test_request_serialization_roundtrip_sol() {
        let data = SolanaPaymentRequestData {
            recipient: "11111111111111111111111111111111".to_string(),
            amount: 10_000_000,
            reference: "22222222222222222222222222222222".to_string(),
            mint: None,
            decimals: None,
            fee_address: None,
            fee_amount: None,
        };
        let json = serde_json::to_string(&data).unwrap();
        assert!(!json.contains("mint"));
        assert!(!json.contains("decimals"));
        assert!(!json.contains("fee_address"));
        assert!(!json.contains("fee_amount"));
        let parsed: SolanaPaymentRequestData = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.recipient, data.recipient);
        assert_eq!(parsed.amount, data.amount);
        assert_eq!(parsed.reference, data.reference);
        assert!(parsed.mint.is_none());
        assert!(parsed.decimals.is_none());
        assert!(parsed.fee_address.is_none());
        assert!(parsed.fee_amount.is_none());
    }

    #[test]
    fn test_request_serialization_roundtrip_spl() {
        let data = SolanaPaymentRequestData {
            recipient: "11111111111111111111111111111111".to_string(),
            amount: 1_000_000,
            reference: "22222222222222222222222222222222".to_string(),
            mint: Some(USDC_MINT_DEVNET.to_string()),
            decimals: Some(6),
            fee_address: None,
            fee_amount: None,
        };
        let json = serde_json::to_string(&data).unwrap();
        assert!(json.contains("mint"));
        assert!(json.contains("decimals"));
        let parsed: SolanaPaymentRequestData = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.mint, data.mint);
        assert_eq!(parsed.decimals, Some(6));
    }

    #[test]
    fn test_fee_calculation_inclusive() {
        let fc = FeeConfig {
            app_fee_bps: 300,
            app_fee_address: "11111111111111111111111111111111".to_string(),
            app_fee_chain: PaymentChain::Solana,
        };
        let (provider, fee) = fc.calculate(100_000);
        assert_eq!(fee, 3_000);
        assert_eq!(provider, 97_000);
        assert_eq!(provider + fee, 100_000);

        // Edge: small amount where ceil matters
        let (provider, fee) = fc.calculate(100);
        assert_eq!(fee, 3); // ceil(3.0) = 3
        assert_eq!(provider, 97);

        // Edge: zero
        let (provider, fee) = fc.calculate(0);
        assert_eq!(fee, 0);
        assert_eq!(provider, 0);
    }

    #[test]
    fn test_request_serialization_with_fee() {
        let data = SolanaPaymentRequestData {
            recipient: "11111111111111111111111111111111".to_string(),
            amount: 100_000,
            reference: "22222222222222222222222222222222".to_string(),
            mint: None,
            decimals: None,
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
        assert!(parsed.mint.is_none());
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
        assert!(data.mint.is_none());
    }

    #[test]
    fn test_create_payment_request_spl() {
        let mint: Pubkey = USDC_MINT_DEVNET.parse().unwrap();
        let config = SolanaPaymentConfig {
            token: SolanaToken::Spl { mint, decimals: 6 },
            ..Default::default()
        };
        let keypair = Keypair::new();
        let provider = SolanaPaymentProvider::new(config, keypair);
        let req = provider
            .create_payment_request(1_000_000, "USDC payment", 3600)
            .unwrap();
        assert_eq!(req.currency_unit, "token(6dp)");

        let data: SolanaPaymentRequestData = serde_json::from_str(&req.request).unwrap();
        assert_eq!(data.mint.as_deref(), Some(USDC_MINT_DEVNET));
        assert_eq!(data.decimals, Some(6));
    }
}
