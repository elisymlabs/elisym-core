//! Solana payment provider — native SOL transfers.
//!
//! Uses a reference-based payment detection approach: each payment request includes
//! a unique ephemeral reference pubkey added as a read-only non-signer to the transfer
//! instruction. The provider detects payment via `getSignaturesForAddress(reference)`.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

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

/// Solana address of the protocol treasury that receives the protocol fee.
pub const PROTOCOL_TREASURY: &str = "GY7vnWMkKpftU4nQ16C2ATkj1JwrQpHhknkaBUn67VTy";

/// Validate fee fields on an already-parsed payment request.
/// Checks treasury address, fee amount matches protocol fee, consistency of fee_address/fee_amount.
/// Free jobs (amount=0, no fee fields) pass.
fn validate_fee_fields_parsed(data: &SolanaPaymentRequestData) -> Result<()> {
    // Free job — fee fields must be absent
    if data.amount == 0 {
        if data.fee_address.is_some() || data.fee_amount.is_some() {
            return Err(ElisymError::Payment(
                "Invalid fee params: fee fields must be absent for free jobs (amount=0).".into(),
            ));
        }
        return Ok(());
    }

    let expected_fee = crate::types::calculate_protocol_fee(data.amount)
        .ok_or_else(|| ElisymError::Payment(
            "Fee calculation overflow for the given amount".into()
        ))?;

    match (data.fee_address.as_deref(), data.fee_amount) {
        (Some(addr), Some(amt)) => {
            if amt == 0 {
                return Err(ElisymError::Payment(
                    "Invalid fee params: fee_amount is zero but fee_address present.".into()
                ));
            }
            if addr != PROTOCOL_TREASURY {
                return Err(ElisymError::Payment(format!(
                    "Fee address mismatch: expected {PROTOCOL_TREASURY}, got {addr}. \
                     Provider may be attempting to redirect fees."
                )));
            }
            if amt != expected_fee {
                return Err(ElisymError::Payment(format!(
                    "Fee amount mismatch: expected {expected_fee} lamports ({}bps of {}), got {amt}. \
                     Provider may be tampering with fee.",
                    crate::types::PROTOCOL_FEE_BPS, data.amount
                )));
            }
            Ok(())
        }
        (Some(_), None) => {
            Err(ElisymError::Payment(
                "Invalid fee params: fee_address present but fee_amount missing.".into()
            ))
        }
        (None, Some(_)) => {
            Err(ElisymError::Payment(
                "Invalid fee params: fee_amount present but fee_address missing.".into()
            ))
        }
        (None, None) => {
            Err(ElisymError::Payment(format!(
                "Payment request missing protocol fee ({}bps). \
                 Expected fee: {expected_fee} lamports to {PROTOCOL_TREASURY}.",
                crate::types::PROTOCOL_FEE_BPS
            )))
        }
    }
}

/// Check that the actual recipient matches the expected one.
fn check_recipient(actual: &str, expected: &str) -> Result<()> {
    if actual != expected {
        return Err(ElisymError::Payment(format!(
            "Recipient mismatch: expected {expected}, got {actual}. \
             Provider may be attempting to redirect payment.",
        )));
    }
    Ok(())
}

/// Validate that a payment request has the correct recipient and protocol fee params.
/// `expected_recipient` is the provider's Solana address from their capability card.
pub fn validate_protocol_fee(request: &str, expected_recipient: &str) -> Result<()> {
    let data: SolanaPaymentRequestData = serde_json::from_str(request)
        .map_err(|e| ElisymError::Payment(format!("Invalid payment request JSON: {e}")))?;

    check_recipient(&data.recipient, expected_recipient)?;
    validate_fee_fields_parsed(&data)
}

/// Solana rent-exempt minimum for a 0-data account (lamports).
/// Formula: (128 + data_len) × rent_rate × exemption_years
///        = (128 + 0) × 3.48e-3 SOL/byte/year × 2 years
///        = 128 × 0.00348 × 2 × 1_000_000_000 = 890_880 lamports
/// This value has never changed since Solana genesis. If rent parameters
/// are ever updated via governance, this constant must be revised.
pub const RENT_EXEMPT_MINIMUM: u64 = 890_880;

/// Validate that provider's net amount (price − protocol fee) ≥ rent-exempt minimum.
///
/// Free mode (0) is always valid. If `account_funded` is `true`, the rent-exempt
/// check is skipped (the recipient account already exists on-chain).
pub fn validate_job_price(lamports: u64, account_funded: bool) -> Result<()> {
    if lamports == 0 {
        return Ok(());
    }
    let fee = crate::types::calculate_protocol_fee(lamports)
        .ok_or_else(|| ElisymError::Payment("Fee calculation overflow".into()))?;
    let provider_net = lamports.saturating_sub(fee);
    if !account_funded && provider_net < RENT_EXEMPT_MINIMUM {
        return Err(ElisymError::Payment(format!(
            "Price too low: after {} protocol fee the provider receives {} lamports, \
             below rent-exempt minimum ({} lamports).",
            crate::types::format_bps_percent(crate::types::PROTOCOL_FEE_BPS),
            provider_net,
            RENT_EXEMPT_MINIMUM,
        )));
    }
    Ok(())
}

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
            SolanaNetwork::Mainnet => "https://api.mainnet.solana.com".to_string(),
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

/// Returns the current Unix timestamp in seconds.
fn now_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("system clock before UNIX epoch")
        .as_secs()
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
    /// Human-readable description (for audit/debugging; not used on-chain).
    #[serde(skip_serializing_if = "Option::is_none")]
    #[serde(default)]
    description: Option<String>,
    /// Fee recipient address (omitted when no fee configured).
    #[serde(skip_serializing_if = "Option::is_none")]
    fee_address: Option<String>,
    /// Fee amount in lamports (omitted when no fee configured).
    #[serde(skip_serializing_if = "Option::is_none")]
    fee_amount: Option<u64>,
    /// Creation timestamp (Unix seconds). 0 means unset (old format).
    #[serde(default)]
    created_at: u64,
    /// Expiry duration in seconds. 0 means no expiry (old format).
    #[serde(default)]
    expiry_secs: u32,
}

impl SolanaPaymentRequestData {
    /// Returns `true` if this request has expired.
    fn is_expired(&self) -> bool {
        self.created_at > 0 && self.expiry_secs > 0 && {
            let elapsed = now_secs().saturating_sub(self.created_at);
            elapsed > self.expiry_secs as u64
        }
    }
}

/// Tracks a pending payment request.
#[derive(Debug)]
struct PendingPayment {
    amount: u64,
    created_at: u64,
    settled: bool,
    tx_signature: Option<String>,
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
    /// Expected recipient address for trait `pay()` validation. If set, `pay()` will
    /// reject requests whose recipient doesn't match.
    expected_recipient: Option<Arc<str>>,
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
            expected_recipient: None,
        }
    }

    /// Create a new Solana payment provider with a pre-configured expected recipient.
    ///
    /// Preferred over calling [`Self::new`] + [`Self::set_expected_recipient`] separately,
    /// as it ensures recipient validation is active from the first `pay()` call.
    pub fn new_with_recipient(config: SolanaPaymentConfig, keypair: Keypair, expected_recipient: &str) -> Self {
        let mut provider = Self::new(config, keypair);
        provider.expected_recipient = Some(Arc::from(expected_recipient));
        provider
    }

    /// Set the expected recipient address for trait `pay()` validation.
    /// When set, `pay()` will reject requests whose recipient doesn't match.
    pub fn set_expected_recipient(&mut self, addr: &str) {
        self.expected_recipient = Some(Arc::from(addr));
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

    /// Get the configured Solana network name (e.g. "devnet", "mainnet").
    pub fn network_name(&self) -> &str {
        match &self.config.network {
            SolanaNetwork::Mainnet => "mainnet",
            SolanaNetwork::Devnet => "devnet",
            SolanaNetwork::Testnet => "testnet",
            SolanaNetwork::Custom(_) => "custom",
        }
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

    /// Create a payment request with the protocol fee automatically applied.
    /// Internally calculates fee via `calculate_protocol_fee()` and uses `PROTOCOL_TREASURY`.
    pub fn create_payment_request_with_protocol_fee(
        &self,
        amount: u64,
        description: &str,
        expiry_secs: u32,
    ) -> Result<PaymentRequest> {
        if amount == 0 {
            return Err(ElisymError::Payment("Payment amount must be greater than 0".into()));
        }
        let fee_amount = crate::types::calculate_protocol_fee(amount)
            .ok_or_else(|| ElisymError::Payment("Fee calculation overflow".into()))?;
        self.create_payment_request_with_fee(amount, description, expiry_secs, PROTOCOL_TREASURY, fee_amount)
    }

    /// Send a direct SOL transfer (no protocol fee, no reference key).
    ///
    /// This is a simple wallet-to-wallet transfer for user-initiated sends,
    /// bypassing the marketplace payment request flow.
    pub fn send_transfer(&self, recipient: &str, lamports: u64) -> Result<String> {
        if lamports == 0 {
            return Err(ElisymError::Payment("Send amount must be greater than 0".into()));
        }

        if lamports < RENT_EXEMPT_MINIMUM {
            return Err(ElisymError::Payment(format!(
                "Amount {} lamports is below rent-exempt minimum ({} lamports). \
                 Sending less would create a non-viable account.",
                lamports, RENT_EXEMPT_MINIMUM,
            )));
        }

        let recipient_pubkey: Pubkey = recipient
            .parse()
            .map_err(|e| ElisymError::Payment(format!("Invalid Solana address: {e:?}")))?;

        let sender = self.keypair.pubkey();
        if sender == recipient_pubkey {
            return Err(ElisymError::Payment("Cannot send SOL to your own address".into()));
        }

        let recent_blockhash = self
            .rpc_client
            .get_latest_blockhash()
            .map_err(|e| ElisymError::Payment(format!("Failed to get blockhash: {e}")))?;

        // system_instruction::transfer is deprecated in favor of transfer_with_seed / newer API,
        // but remains correct for simple SOL transfers without derived addresses.
        #[allow(deprecated)]
        let ix = solana_sdk::system_instruction::transfer(&sender, &recipient_pubkey, lamports);
        let message = Message::new_with_blockhash(&[ix], Some(&sender), &recent_blockhash);
        let tx = Transaction::new(&[&self.keypair], message, recent_blockhash);

        let sig = self
            .rpc_client
            .send_and_confirm_transaction(&tx)
            .map_err(|e| ElisymError::Payment(format!("Transaction failed: {e}")))?;

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
        description: &str,
        expiry_secs: u32,
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

        let now = now_secs();

        let desc = if description.is_empty() { None } else { Some(description.to_string()) };

        let data = SolanaPaymentRequestData {
            recipient: self.keypair.pubkey().to_string(),
            amount,
            reference: reference.to_string(),
            description: desc,
            fee_address,
            fee_amount,
            created_at: now,
            expiry_secs,
        };

        let request = serde_json::to_string(&data)
            .map_err(|e| ElisymError::Payment(format!("Failed to serialize request: {}", e)))?;

        // Track this pending payment
        {
            let mut pending = self.pending.lock().unwrap_or_else(|e| e.into_inner());

            // Cap the pending map size by clearing settled and stale entries
            if pending.len() >= PENDING_MAP_CAP {
                const STALE_SECS: u64 = 3600;
                pending.retain(|_, v| {
                    let stale = v.created_at > 0 && now.saturating_sub(v.created_at) >= STALE_SECS;
                    !v.settled && !stale
                });
            }

            pending.insert(
                request.clone(),
                PendingPayment {
                    amount,
                    created_at: now,
                    settled: false,
                    tx_signature: None,
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

impl SolanaPaymentProvider {
    /// Internal payment execution from pre-parsed request data — no re-parsing.
    fn pay_internal_parsed(&self, data: SolanaPaymentRequestData) -> Result<PaymentResult> {
        // Check expiry before sending funds
        if data.is_expired() {
            return Err(ElisymError::Payment(
                "Payment request has expired".into(),
            ));
        }

        let recipient: Pubkey = data
            .recipient
            .parse()
            .map_err(|e| ElisymError::Payment(format!("Invalid recipient address: {:?}", e)))?;

        let reference: Pubkey = data
            .reference
            .parse()
            .map_err(|e| ElisymError::Payment(format!("Invalid reference pubkey: {:?}", e)))?;

        // Parse optional fee parameters (validation already done by validate_fee_fields_parsed).
        let fee_params = match (data.fee_address, data.fee_amount) {
            (Some(addr), Some(amt)) if amt > 0 => {
                // Defense-in-depth: fee must be strictly less than total amount.
                // This should never trigger if validate_fee_fields_parsed ran, but guards
                // against a provider_amount=0 transfer if the check is somehow bypassed.
                if amt >= data.amount {
                    return Err(ElisymError::Payment(format!(
                        "fee_amount ({amt}) must be less than total amount ({})",
                        data.amount
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

    /// Pay a payment request with full validation (fee fields + recipient).
    ///
    /// Convenience method when the expected recipient is known at call time rather
    /// than at provider construction time. Equivalent to `pay()` but takes the
    /// expected recipient as a parameter instead of requiring [`set_expected_recipient`].
    ///
    /// | Check                  | `pay()` (trait)        | `pay_validated()` |
    /// |------------------------|:----------------------:|:-----------------:|
    /// | Fee treasury address   | Yes                    | Yes               |
    /// | Fee amount (3% exact)  | Yes                    | Yes               |
    /// | Recipient address      | Yes (via constructor)  | Yes (via param)   |
    /// | Expiry                 | Yes                    | Yes               |
    ///
    /// - `expected_recipient`: the provider's Solana address from their capability card
    pub fn pay_validated(&self, request: &str, expected_recipient: &str) -> Result<PaymentResult> {
        let data: SolanaPaymentRequestData = serde_json::from_str(request)
            .map_err(|e| ElisymError::Payment(format!("Invalid payment request JSON: {e}")))?;
        validate_fee_fields_parsed(&data)?;
        check_recipient(&data.recipient, expected_recipient)?;
        self.pay_internal_parsed(data)
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
        self.create_payment_request_with_protocol_fee(amount, description, expiry_secs)
    }

    /// Pay a Solana payment request by sending a SOL transfer on-chain.
    ///
    /// Automatically validates protocol fee fields (treasury address, fee amount)
    /// and recipient address. Free jobs (amount=0, no fee fields) are allowed.
    ///
    /// **Requires** [`SolanaPaymentProvider::set_expected_recipient`] or
    /// [`SolanaPaymentProvider::new_with_recipient`] to be called first.
    /// Returns an error if no expected recipient is configured — this prevents
    /// a malicious provider from redirecting funds to an arbitrary address.
    ///
    /// For one-off payments where the recipient is known at call time, use
    /// [`SolanaPaymentProvider::pay_validated`] instead.
    fn pay(&self, request: &str) -> Result<PaymentResult> {
        let expected = self.expected_recipient.as_deref().ok_or_else(|| {
            ElisymError::Payment(
                "pay() requires expected_recipient to be set for recipient validation. \
                 Call set_expected_recipient() / new_with_recipient(), or use pay_validated(). \
                 Without recipient validation, a malicious provider can redirect funds."
                    .into(),
            )
        })?;
        let data: SolanaPaymentRequestData = serde_json::from_str(request)
            .map_err(|e| ElisymError::Payment(format!("Invalid payment request JSON: {e}")))?;
        validate_fee_fields_parsed(&data)?;
        check_recipient(&data.recipient, expected)?;
        self.pay_internal_parsed(data)
    }

    /// Look up the status of a Solana payment by its request string.
    ///
    /// First checks a local in-memory cache of settled payments, then queries
    /// the chain via `getSignaturesForAddress(reference)`.
    ///
    /// **Cache race note:** The cache lock is released between the initial check
    /// and the on-chain query. In rare cases, concurrent cleanup (triggered by
    /// `PENDING_MAP_CAP`) could evict the entry before the settled status is
    /// written back. This is harmless — correctness is preserved, but the next
    /// lookup will repeat the on-chain query instead of hitting the cache.
    fn lookup_payment(&self, request: &str) -> Result<PaymentStatus> {
        // Check local cache first
        {
            let pending = self.pending.lock().unwrap_or_else(|e| e.into_inner());
            if let Some(p) = pending.get(request) {
                if p.settled {
                    return Ok(PaymentStatus {
                        settled: true,
                        amount: Some(p.amount),
                        tx_signature: p.tx_signature.clone(),
                    });
                }
            }
        }

        // Parse the request to get the reference pubkey
        let data: SolanaPaymentRequestData = serde_json::from_str(request)
            .map_err(|e| ElisymError::Payment(format!("Invalid payment request: {}", e)))?;

        // Check expiry — don't query chain for expired requests
        if data.is_expired() {
            return Ok(PaymentStatus {
                settled: false,
                amount: None,
                tx_signature: None,
            });
        }

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
                tx_signature: None,
            });
        }

        // Verify the on-chain transfer amount (check at most 10 signatures
        // to bound RPC calls for references that accumulate many transactions)
        const MAX_SIG_CHECK: usize = 10;
        for sig_info in sigs.iter().take(MAX_SIG_CHECK) {
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
                    // Verify protocol fee was paid to treasury
                    if let (Some(ref fee_addr), Some(fee_amt)) =
                        (&data.fee_address, data.fee_amount)
                    {
                        if fee_amt > 0 {
                            let fee_ok = account_keys
                                .iter()
                                .position(|k| k == fee_addr)
                                .map(|fi| {
                                    meta.post_balances[fi]
                                        .saturating_sub(meta.pre_balances[fi])
                                        >= fee_amt
                                })
                                .unwrap_or(false);
                            if !fee_ok {
                                continue;
                            }
                        }
                    }

                    // Payment verified — mark as settled in cache so future lookups
                    // skip the on-chain query.
                    let sig_str = sig.to_string();
                    let mut pending = self.pending.lock().unwrap_or_else(|e| e.into_inner());
                    if let Some(p) = pending.get_mut(request) {
                        p.settled = true;
                        p.tx_signature = Some(sig_str.clone());
                    }
                    return Ok(PaymentStatus {
                        settled: true,
                        amount: Some(received),
                        tx_signature: Some(sig_str),
                    });
                }
            }
        }

        // Signature found but amount insufficient
        Ok(PaymentStatus {
            settled: false,
            amount: None,
            tx_signature: None,
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
            "https://api.mainnet.solana.com"
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
            description: None,
            fee_address: None,
            fee_amount: None,
            created_at: 1700000000,
            expiry_secs: 3600,
        };
        let json = serde_json::to_string(&data).unwrap();
        assert!(!json.contains("fee_address"));
        assert!(!json.contains("fee_amount"));
        assert!(json.contains("created_at"));
        assert!(json.contains("expiry_secs"));
        let parsed: SolanaPaymentRequestData = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.recipient, data.recipient);
        assert_eq!(parsed.amount, data.amount);
        assert_eq!(parsed.reference, data.reference);
        assert!(parsed.fee_address.is_none());
        assert!(parsed.fee_amount.is_none());
        assert_eq!(parsed.created_at, 1700000000);
        assert_eq!(parsed.expiry_secs, 3600);
    }

    #[test]
    fn test_request_serialization_with_fee() {
        let data = SolanaPaymentRequestData {
            recipient: "11111111111111111111111111111111".to_string(),
            amount: 100_000,
            reference: "22222222222222222222222222222222".to_string(),
            description: None,
            fee_address: Some("33333333333333333333333333333333".to_string()),
            fee_amount: Some(3_000),
            created_at: 1700000000,
            expiry_secs: 3600,
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
        // create_payment_request now auto-applies protocol fee
        assert_eq!(data.fee_address.as_deref(), Some(PROTOCOL_TREASURY));
        assert_eq!(data.fee_amount, Some(300_000)); // 3% of 10M
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
                        created_at: now_secs(),
                        settled: true,
                        tx_signature: None,
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

    #[test]
    fn test_expiry_check() {
        let data = SolanaPaymentRequestData {
            recipient: "11111111111111111111111111111111".to_string(),
            amount: 100_000,
            reference: "22222222222222222222222222222222".to_string(),
            description: None,
            fee_address: None,
            fee_amount: None,
            created_at: now_secs() - 7200, // 2 hours ago
            expiry_secs: 3600,             // 1 hour expiry
        };
        assert!(data.is_expired());

        let fresh = SolanaPaymentRequestData {
            created_at: now_secs(),
            expiry_secs: 3600,
            ..data.clone()
        };
        assert!(!fresh.is_expired());

        // Old format (no expiry) should never be considered expired
        let old_format = SolanaPaymentRequestData {
            created_at: 0,
            expiry_secs: 0,
            ..data
        };
        assert!(!old_format.is_expired());
    }

    #[test]
    fn test_pay_rejects_excessive_fee() {
        let keypair = Keypair::new();
        let recipient = Keypair::new().pubkey().to_string();
        let mut provider = SolanaPaymentProvider::new(SolanaPaymentConfig::default(), keypair);
        provider.set_expected_recipient(&recipient);
        let reference = Keypair::new().pubkey().to_string();
        // Construct a request with 50% fee (5000 bps) — exceeds protocol fee
        // Use PROTOCOL_TREASURY so it passes fee address validation but fails on amount mismatch
        let data = SolanaPaymentRequestData {
            recipient,
            amount: 100_000,
            reference,
            description: None,
            fee_address: Some(PROTOCOL_TREASURY.to_string()),
            fee_amount: Some(50_000), // 50% — wrong amount for protocol fee
            created_at: now_secs(),
            expiry_secs: 3600,
        };
        let json = serde_json::to_string(&data).unwrap();
        let result = provider.pay(&json);
        assert!(result.is_err());
        let err_msg = result.unwrap_err().to_string();
        assert!(
            err_msg.contains("Fee amount mismatch"),
            "unexpected error: {}", err_msg
        );
    }

    #[test]
    fn test_pay_allows_fee_within_limit() {
        let keypair = Keypair::new();
        let recipient = Keypair::new().pubkey().to_string();
        let mut provider = SolanaPaymentProvider::new(SolanaPaymentConfig::default(), keypair);
        provider.set_expected_recipient(&recipient);
        let reference = Keypair::new().pubkey().to_string();
        let amount = 100_000u64;
        let fee = crate::types::calculate_protocol_fee(amount).unwrap(); // correct protocol fee
        let data = SolanaPaymentRequestData {
            recipient,
            amount,
            reference,
            description: None,
            fee_address: Some(PROTOCOL_TREASURY.to_string()),
            fee_amount: Some(fee),
            created_at: now_secs(),
            expiry_secs: 3600,
        };
        let json = serde_json::to_string(&data).unwrap();
        let result = provider.pay(&json);
        // Should pass fee + recipient validation — must fail on RPC (no network)
        assert!(result.is_err(), "expected RPC error in test env");
        let err_msg = result.unwrap_err().to_string();
        assert!(!err_msg.contains("Fee"), "fee should be accepted: {}", err_msg);
        assert!(!err_msg.contains("Recipient"), "recipient should pass: {}", err_msg);
    }

    #[test]
    fn test_pay_rejects_without_expected_recipient() {
        let keypair = Keypair::new();
        let provider = SolanaPaymentProvider::new(SolanaPaymentConfig::default(), keypair);
        // No set_expected_recipient — pay() must reject
        let recipient = Keypair::new().pubkey().to_string();
        let reference = Keypair::new().pubkey().to_string();
        let amount = 100_000u64;
        let fee = crate::types::calculate_protocol_fee(amount).unwrap();
        let data = SolanaPaymentRequestData {
            recipient,
            amount,
            reference,
            description: None,
            fee_address: Some(PROTOCOL_TREASURY.to_string()),
            fee_amount: Some(fee),
            created_at: now_secs(),
            expiry_secs: 3600,
        };
        let json = serde_json::to_string(&data).unwrap();
        let result = provider.pay(&json);
        assert!(result.is_err());
        let err_msg = result.unwrap_err().to_string();
        assert!(
            err_msg.contains("expected_recipient"),
            "should require expected_recipient: {}", err_msg
        );
    }

    #[test]
    fn test_create_payment_request_sets_expiry() {
        let keypair = Keypair::new();
        let provider = SolanaPaymentProvider::new(SolanaPaymentConfig::default(), keypair);
        let req = provider.create_payment_request(10_000, "test", 3600).unwrap();
        let data: SolanaPaymentRequestData = serde_json::from_str(&req.request).unwrap();
        assert!(data.created_at > 0);
        assert_eq!(data.expiry_secs, 3600);
    }

    // ── calculate_protocol_fee ─────────────────────────────────────

    #[test]
    fn test_calculate_protocol_fee_standard() {
        assert_eq!(crate::types::calculate_protocol_fee(10_000_000), Some(300_000));
    }

    #[test]
    fn test_calculate_protocol_fee_rounds_up() {
        // 10_000_001 * 300 = 3_000_000_300, div_ceil(10_000) = 300_001
        assert_eq!(crate::types::calculate_protocol_fee(10_000_001), Some(300_001));
    }

    #[test]
    fn test_calculate_protocol_fee_one_lamport() {
        // 1 * 300 = 300, div_ceil(10_000) = 1
        assert_eq!(crate::types::calculate_protocol_fee(1), Some(1));
    }

    #[test]
    fn test_calculate_protocol_fee_zero() {
        assert_eq!(crate::types::calculate_protocol_fee(0), Some(0));
    }

    #[test]
    fn test_calculate_protocol_fee_overflow() {
        // Very large value that overflows checked_mul
        assert_eq!(crate::types::calculate_protocol_fee(u64::MAX), None);
    }

    // ── format_bps_percent ─────────────────────────────────────────

    #[test]
    fn test_format_bps_percent() {
        assert_eq!(crate::types::format_bps_percent(300), "3.00%");
        assert_eq!(crate::types::format_bps_percent(0), "0.00%");
        assert_eq!(crate::types::format_bps_percent(50), "0.50%");
        assert_eq!(crate::types::format_bps_percent(10000), "100.00%");
    }

    // ── validate_protocol_fee ──────────────────────────────────────

    fn make_payment_json(amount: u64, fee_address: Option<&str>, fee_amount: Option<u64>) -> String {
        let mut obj = serde_json::json!({
            "recipient": "SomeAddress",
            "amount": amount,
            "reference": "ref123",
            "created_at": now_secs(),
            "expiry_secs": 3600,
        });
        if let Some(addr) = fee_address {
            obj["fee_address"] = serde_json::json!(addr);
        }
        if let Some(amt) = fee_amount {
            obj["fee_amount"] = serde_json::json!(amt);
        }
        serde_json::to_string(&obj).unwrap()
    }

    #[test]
    fn test_validate_protocol_fee_valid() {
        let amount = 10_000_000u64;
        let fee = crate::types::calculate_protocol_fee(amount).unwrap();
        let json = make_payment_json(amount, Some(PROTOCOL_TREASURY), Some(fee));
        assert!(validate_protocol_fee(&json, "SomeAddress").is_ok());
    }

    #[test]
    fn test_validate_protocol_fee_wrong_treasury() {
        let amount = 10_000_000u64;
        let fee = crate::types::calculate_protocol_fee(amount).unwrap();
        let json = make_payment_json(amount, Some("WrongAddress"), Some(fee));
        let err = validate_protocol_fee(&json, "SomeAddress").unwrap_err();
        assert!(err.to_string().contains("Fee address mismatch"));
    }

    #[test]
    fn test_validate_protocol_fee_wrong_amount() {
        let amount = 10_000_000u64;
        let json = make_payment_json(amount, Some(PROTOCOL_TREASURY), Some(1));
        let err = validate_protocol_fee(&json, "SomeAddress").unwrap_err();
        assert!(err.to_string().contains("Fee amount mismatch"));
    }

    #[test]
    fn test_validate_protocol_fee_missing() {
        let json = make_payment_json(10_000_000, None, None);
        let err = validate_protocol_fee(&json, "SomeAddress").unwrap_err();
        assert!(err.to_string().contains("missing protocol fee"));
    }

    #[test]
    fn test_validate_protocol_fee_invalid_json() {
        assert!(validate_protocol_fee("not json", "SomeAddress").is_err());
    }

    #[test]
    fn test_validate_protocol_fee_recipient_mismatch() {
        let amount = 10_000_000u64;
        let fee = crate::types::calculate_protocol_fee(amount).unwrap();
        let json = make_payment_json(amount, Some(PROTOCOL_TREASURY), Some(fee));
        let err = validate_protocol_fee(&json, "DifferentAddress").unwrap_err();
        assert!(err.to_string().contains("Recipient mismatch"));
    }

    #[test]
    fn test_validate_protocol_fee_fee_address_without_amount() {
        let json = make_payment_json(10_000_000, Some(PROTOCOL_TREASURY), None);
        let err = validate_protocol_fee(&json, "SomeAddress").unwrap_err();
        assert!(err.to_string().contains("fee_address present but fee_amount missing"));
    }

    #[test]
    fn test_validate_protocol_fee_fee_amount_without_address() {
        let json = make_payment_json(10_000_000, None, Some(300_000));
        let err = validate_protocol_fee(&json, "SomeAddress").unwrap_err();
        assert!(err.to_string().contains("fee_amount present but fee_address missing"));
    }

    #[test]
    fn test_validate_protocol_fee_zero_fee_amount() {
        let json = make_payment_json(10_000_000, Some(PROTOCOL_TREASURY), Some(0));
        let err = validate_protocol_fee(&json, "SomeAddress").unwrap_err();
        assert!(err.to_string().contains("fee_amount is zero but fee_address present"));
    }

    #[test]
    fn test_validate_protocol_fee_free_job() {
        // amount=0, no fee fields — should be Ok
        let json = make_payment_json(0, None, None);
        assert!(validate_protocol_fee(&json, "SomeAddress").is_ok());
    }

    #[test]
    fn test_pay_rejects_wrong_treasury() {
        let keypair = Keypair::new();
        let recipient = Keypair::new().pubkey().to_string();
        let mut provider = SolanaPaymentProvider::new(SolanaPaymentConfig::default(), keypair);
        provider.set_expected_recipient(&recipient);
        let reference = Keypair::new().pubkey().to_string();
        let wrong_treasury = Keypair::new().pubkey().to_string();
        let amount = 100_000u64;
        let fee = crate::types::calculate_protocol_fee(amount).unwrap();
        let data = SolanaPaymentRequestData {
            recipient,
            amount,
            reference,
            description: None,
            fee_address: Some(wrong_treasury),
            fee_amount: Some(fee),
            created_at: now_secs(),
            expiry_secs: 3600,
        };
        let json = serde_json::to_string(&data).unwrap();
        let result = provider.pay(&json);
        assert!(result.is_err());
        let err_msg = result.unwrap_err().to_string();
        assert!(err_msg.contains("Fee address mismatch"), "unexpected error: {}", err_msg);
    }

    #[test]
    fn test_pay_rejects_missing_fee() {
        let keypair = Keypair::new();
        let recipient = Keypair::new().pubkey().to_string();
        let mut provider = SolanaPaymentProvider::new(SolanaPaymentConfig::default(), keypair);
        provider.set_expected_recipient(&recipient);
        let reference = Keypair::new().pubkey().to_string();
        // Paid job (amount > 0) without fee fields
        let data = SolanaPaymentRequestData {
            recipient,
            amount: 100_000,
            reference,
            description: None,
            fee_address: None,
            fee_amount: None,
            created_at: now_secs(),
            expiry_secs: 3600,
        };
        let json = serde_json::to_string(&data).unwrap();
        let result = provider.pay(&json);
        assert!(result.is_err());
        let err_msg = result.unwrap_err().to_string();
        assert!(err_msg.contains("missing protocol fee"), "unexpected error: {}", err_msg);
    }

    #[test]
    fn test_pay_allows_free_job() {
        let keypair = Keypair::new();
        let recipient = Keypair::new().pubkey().to_string();
        let mut provider = SolanaPaymentProvider::new(SolanaPaymentConfig::default(), keypair);
        provider.set_expected_recipient(&recipient);
        let reference = Keypair::new().pubkey().to_string();
        // Free job: amount=0, no fee
        let data = SolanaPaymentRequestData {
            recipient,
            amount: 0,
            reference,
            description: None,
            fee_address: None,
            fee_amount: None,
            created_at: now_secs(),
            expiry_secs: 3600,
        };
        let json = serde_json::to_string(&data).unwrap();
        let result = provider.pay(&json);
        // Should pass fee + recipient validation — must fail on RPC (no network), not on fee
        assert!(result.is_err(), "expected RPC error in test env");
        let err_msg = result.unwrap_err().to_string();
        assert!(!err_msg.contains("fee"), "free job should pass fee validation: {}", err_msg);
        assert!(!err_msg.contains("protocol fee"), "free job should pass: {}", err_msg);
    }

    // ── create_payment_request_with_protocol_fee ───────────────────

    #[test]
    fn test_create_payment_request_with_protocol_fee() {
        let keypair = Keypair::new();
        let provider = SolanaPaymentProvider::new(SolanaPaymentConfig::default(), keypair);
        let req = provider.create_payment_request_with_protocol_fee(10_000_000, "test", 3600).unwrap();
        let data: SolanaPaymentRequestData = serde_json::from_str(&req.request).unwrap();
        assert_eq!(data.amount, 10_000_000);
        assert_eq!(data.fee_address.as_deref(), Some(PROTOCOL_TREASURY));
        assert_eq!(data.fee_amount, Some(300_000)); // 3% of 10M
    }

    #[test]
    fn test_create_payment_request_with_protocol_fee_overflow() {
        let keypair = Keypair::new();
        let provider = SolanaPaymentProvider::new(SolanaPaymentConfig::default(), keypair);
        let result = provider.create_payment_request_with_protocol_fee(u64::MAX, "test", 3600);
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("overflow"));
    }

    // ── recipient validation in pay() ─────────────────────────────

    #[test]
    fn test_pay_validates_recipient_when_set() {
        let keypair = Keypair::new();
        let mut provider = SolanaPaymentProvider::new(SolanaPaymentConfig::default(), keypair);
        provider.set_expected_recipient("ExpectedAddress");

        let recipient = Keypair::new().pubkey().to_string(); // different from expected
        let reference = Keypair::new().pubkey().to_string();
        let amount = 100_000u64;
        let fee = crate::types::calculate_protocol_fee(amount).unwrap();
        let data = SolanaPaymentRequestData {
            recipient,
            amount,
            reference,
            description: None,
            fee_address: Some(PROTOCOL_TREASURY.to_string()),
            fee_amount: Some(fee),
            created_at: now_secs(),
            expiry_secs: 3600,
        };
        let json = serde_json::to_string(&data).unwrap();
        let result = provider.pay(&json);
        assert!(result.is_err());
        let err_msg = result.unwrap_err().to_string();
        assert!(err_msg.contains("Recipient mismatch"), "unexpected error: {}", err_msg);
    }

    #[test]
    fn test_pay_validated_checks_recipient() {
        let keypair = Keypair::new();
        let provider = SolanaPaymentProvider::new(SolanaPaymentConfig::default(), keypair);
        let recipient = Keypair::new().pubkey().to_string();
        let reference = Keypair::new().pubkey().to_string();
        let amount = 100_000u64;
        let fee = crate::types::calculate_protocol_fee(amount).unwrap();
        let data = SolanaPaymentRequestData {
            recipient,
            amount,
            reference,
            description: None,
            fee_address: Some(PROTOCOL_TREASURY.to_string()),
            fee_amount: Some(fee),
            created_at: now_secs(),
            expiry_secs: 3600,
        };
        let json = serde_json::to_string(&data).unwrap();
        let result = provider.pay_validated(&json, "WrongRecipient");
        assert!(result.is_err());
        let err_msg = result.unwrap_err().to_string();
        assert!(err_msg.contains("Recipient mismatch"),
            "should fail on recipient: {}", err_msg);
    }

    // ── network_name ──────────────────────────────────────────────

    #[test]
    fn test_network_name_mainnet() {
        let keypair = Keypair::new();
        let provider = SolanaPaymentProvider::new(
            SolanaPaymentConfig { network: SolanaNetwork::Mainnet, rpc_url: None },
            keypair,
        );
        assert_eq!(provider.network_name(), "mainnet");
    }

    #[test]
    fn test_network_name_devnet() {
        let keypair = Keypair::new();
        let provider = SolanaPaymentProvider::new(SolanaPaymentConfig::default(), keypair);
        assert_eq!(provider.network_name(), "devnet");
    }

    #[test]
    fn test_network_name_testnet() {
        let keypair = Keypair::new();
        let provider = SolanaPaymentProvider::new(
            SolanaPaymentConfig { network: SolanaNetwork::Testnet, rpc_url: None },
            keypair,
        );
        assert_eq!(provider.network_name(), "testnet");
    }

    #[test]
    fn test_network_name_custom() {
        let keypair = Keypair::new();
        let provider = SolanaPaymentProvider::new(
            SolanaPaymentConfig {
                network: SolanaNetwork::Custom("http://localhost:8899".to_string()),
                rpc_url: None,
            },
            keypair,
        );
        assert_eq!(provider.network_name(), "custom");
    }

    // ── from_secret_key / from_bytes constructors ────────────────

    #[test]
    fn test_from_secret_key_invalid_base58() {
        let result = SolanaPaymentProvider::from_secret_key(
            SolanaPaymentConfig::default(),
            "not-valid-base58!!!",
        );
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("Invalid base58"));
    }

    #[test]
    fn test_from_bytes_wrong_length() {
        let result = SolanaPaymentProvider::from_bytes(
            SolanaPaymentConfig::default(),
            &[1, 2, 3, 4], // too short
        );
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("Invalid keypair"));
    }

    #[test]
    fn test_from_secret_key_valid() {
        let keypair = Keypair::new();
        let expected_address = keypair.pubkey().to_string();
        let base58_secret = bs58::encode(keypair.to_bytes()).into_string();
        let provider = SolanaPaymentProvider::from_secret_key(
            SolanaPaymentConfig::default(),
            &base58_secret,
        )
        .unwrap();
        assert_eq!(provider.address(), expected_address);
    }

    // ── validate_job_price ───────────────────────────────────────

    #[test]
    fn test_validate_job_price_free() {
        assert!(validate_job_price(0, false).is_ok());
    }

    #[test]
    fn test_validate_job_price_above_threshold() {
        assert!(validate_job_price(RENT_EXEMPT_MINIMUM * 2, false).is_ok());
    }

    #[test]
    fn test_validate_job_price_below_rent_exempt() {
        let result = validate_job_price(1000, false);
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("Price too low"));
    }

    #[test]
    fn test_validate_job_price_account_funded_skips_rent_check() {
        assert!(validate_job_price(1000, true).is_ok());
    }

    // ── new_with_recipient ───────────────────────────────────────

    #[test]
    fn test_new_with_recipient_rejects_wrong_recipient() {
        let keypair = Keypair::new();
        let expected = Keypair::new().pubkey().to_string();
        let provider = SolanaPaymentProvider::new_with_recipient(
            SolanaPaymentConfig::default(),
            keypair,
            &expected,
        );
        // Build a request with a different recipient
        let wrong_recipient = Keypair::new().pubkey().to_string();
        let reference = Keypair::new().pubkey().to_string();
        let amount = 100_000u64;
        let fee = crate::types::calculate_protocol_fee(amount).unwrap();
        let data = SolanaPaymentRequestData {
            recipient: wrong_recipient,
            amount,
            reference,
            description: None,
            fee_address: Some(PROTOCOL_TREASURY.to_string()),
            fee_amount: Some(fee),
            created_at: now_secs(),
            expiry_secs: 3600,
        };
        let json = serde_json::to_string(&data).unwrap();
        let result = provider.pay(&json);
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("Recipient mismatch"));
    }

    // ── create_payment_request_with_fee edge case ────────────────

    #[test]
    fn test_create_payment_request_with_fee_amount_exceeds() {
        let keypair = Keypair::new();
        let provider = SolanaPaymentProvider::new(SolanaPaymentConfig::default(), keypair);
        let result = provider.create_payment_request_with_fee(
            100_000, "test", 3600, PROTOCOL_TREASURY, 100_000,
        );
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("fee_amount must be less than amount"));
    }

    // ── validate_fee_fields_parsed: free job with fee fields ─────

    #[test]
    fn test_validate_fee_fields_free_job_with_fee() {
        let data = SolanaPaymentRequestData {
            recipient: "11111111111111111111111111111111".to_string(),
            amount: 0,
            reference: "22222222222222222222222222222222".to_string(),
            description: None,
            fee_address: Some(PROTOCOL_TREASURY.to_string()),
            fee_amount: Some(1000),
            created_at: now_secs(),
            expiry_secs: 3600,
        };
        let result = validate_fee_fields_parsed(&data);
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("fee fields must be absent for free jobs"));
    }

    #[test]
    fn test_pay_validated_passes_with_correct_recipient() {
        let keypair = Keypair::new();
        let provider = SolanaPaymentProvider::new(SolanaPaymentConfig::default(), keypair);
        let recipient = Keypair::new().pubkey().to_string();
        let reference = Keypair::new().pubkey().to_string();
        let amount = 100_000u64;
        let fee = crate::types::calculate_protocol_fee(amount).unwrap();
        let data = SolanaPaymentRequestData {
            recipient: recipient.clone(),
            amount,
            reference,
            description: None,
            fee_address: Some(PROTOCOL_TREASURY.to_string()),
            fee_amount: Some(fee),
            created_at: now_secs(),
            expiry_secs: 3600,
        };
        let json = serde_json::to_string(&data).unwrap();
        let result = provider.pay_validated(&json, &recipient);
        // Should pass all validation — must fail on RPC (no network)
        assert!(result.is_err(), "expected RPC error in test env");
        let err_msg = result.unwrap_err().to_string();
        assert!(!err_msg.contains("Recipient"), "should pass recipient: {}", err_msg);
        assert!(!err_msg.contains("Fee"), "should pass fee: {}", err_msg);
    }

    // ── send_transfer validation tests ──────────────────────────────

    #[test]
    fn test_send_transfer_zero_amount() {
        let keypair = Keypair::new();
        let provider = SolanaPaymentProvider::new(SolanaPaymentConfig::default(), keypair);
        let recipient = Keypair::new().pubkey().to_string();
        let result = provider.send_transfer(&recipient, 0);
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("greater than 0"));
    }

    #[test]
    fn test_send_transfer_below_rent_exempt() {
        let keypair = Keypair::new();
        let provider = SolanaPaymentProvider::new(SolanaPaymentConfig::default(), keypair);
        let recipient = Keypair::new().pubkey().to_string();
        let result = provider.send_transfer(&recipient, 100);
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("rent-exempt minimum"));
    }

    #[test]
    fn test_send_transfer_invalid_recipient() {
        let keypair = Keypair::new();
        let provider = SolanaPaymentProvider::new(SolanaPaymentConfig::default(), keypair);
        let result = provider.send_transfer("not-a-pubkey", 1_000_000_000);
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("Invalid Solana address"));
    }

    #[test]
    fn test_send_transfer_self_send() {
        let keypair = Keypair::new();
        let own_address = keypair.pubkey().to_string();
        let provider = SolanaPaymentProvider::new(SolanaPaymentConfig::default(), keypair);
        let result = provider.send_transfer(&own_address, 1_000_000_000);
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("Cannot send SOL to your own address"));
    }
}
