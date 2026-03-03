use ldk_node::bitcoin::hashes::Hash as _;

use crate::error::{ElisymError, Result};

/// Configuration for the Lightning payment service.
#[derive(Debug, Clone)]
pub struct PaymentConfig {
    /// Directory for LDK-node storage.
    pub storage_dir: String,
    /// Bitcoin network (testnet by default).
    pub network: ldk_node::bitcoin::Network,
    /// Esplora server URL.
    pub esplora_url: String,
    /// Listening address for LDK-node (e.g., "0.0.0.0:9735").
    pub listening_address: Option<String>,
    /// LSPS2 LSP node public key (hex).
    pub lsp_node_id: Option<String>,
    /// LSPS2 LSP address (host:port).
    pub lsp_address: Option<String>,
    /// Optional LSPS2 auth token.
    pub lsp_token: Option<String>,
}

impl Default for PaymentConfig {
    fn default() -> Self {
        Self {
            storage_dir: "/tmp/elisym-ldk".to_string(),
            network: ldk_node::bitcoin::Network::Testnet,
            esplora_url: crate::types::DEFAULT_ESPLORA_URL.to_string(),
            listening_address: None,
            lsp_node_id: None,
            lsp_address: None,
            lsp_token: None,
        }
    }
}

impl PaymentConfig {
    /// Returns true if LSPS2 JIT channel configuration is present.
    pub fn has_lsps2(&self) -> bool {
        self.lsp_node_id.is_some() && self.lsp_address.is_some()
    }
}

/// Result of a payment.
#[derive(Debug, Clone)]
pub struct PaymentResult {
    pub payment_id: String,
    pub status: String,
}

/// Status of an invoice looked up via LDK-node.
#[derive(Debug, Clone)]
pub struct LdkInvoiceStatus {
    pub settled: bool,
    pub amount_msat: Option<u64>,
}

/// Information about an open Lightning channel.
#[derive(Debug, Clone)]
pub struct ChannelInfo {
    pub channel_id: String,
    pub counterparty_node_id: String,
    pub channel_value_sats: u64,
    pub is_channel_ready: bool,
    pub is_usable: bool,
    pub outbound_capacity_msat: u64,
    pub inbound_capacity_msat: u64,
    /// Funding transaction outpoint (txid:vout). Present once the funding tx is broadcast.
    pub funding_txo: Option<String>,
}

/// Service wrapping LDK-node for Lightning payments (BOLT11 + on-chain).
pub struct PaymentService {
    config: PaymentConfig,
    node: Option<ldk_node::Node>,
    lsps2_enabled: bool,
}

impl std::fmt::Debug for PaymentService {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PaymentService")
            .field("config", &self.config)
            .field("lsps2_enabled", &self.lsps2_enabled)
            .finish()
    }
}

impl PaymentService {
    pub fn new(config: PaymentConfig) -> Self {
        let lsps2_enabled = config.has_lsps2();
        Self {
            config,
            node: None,
            lsps2_enabled,
        }
    }

    /// Returns whether LSPS2 JIT channels are configured.
    pub fn lsps2_enabled(&self) -> bool {
        self.lsps2_enabled
    }

    /// Start the LDK-node.
    ///
    /// LDK-node internally manages its own tokio runtime, so the build/start
    /// steps are run inside `spawn_blocking` to avoid runtime nesting conflicts.
    pub async fn start(&mut self) -> Result<()> {
        let config = self.config.clone();

        // Ensure storage directory exists with restrictive permissions (owner-only).
        // LDK storage contains private keys — world-readable would be a fund-theft risk.
        {
            use std::fs;
            let path = std::path::Path::new(&config.storage_dir);
            if !path.exists() {
                fs::create_dir_all(path).map_err(|e| {
                    ElisymError::Config(format!(
                        "Failed to create storage dir {}: {}",
                        config.storage_dir, e
                    ))
                })?;
            }

            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                let perms = fs::metadata(path)
                    .map_err(|e| ElisymError::Config(format!("Cannot read storage dir metadata: {}", e)))?
                    .permissions();
                let mode = perms.mode() & 0o777;
                if mode & 0o077 != 0 {
                    // Directory is accessible by group/others — fix it
                    fs::set_permissions(path, fs::Permissions::from_mode(0o700)).map_err(|e| {
                        ElisymError::Config(format!("Failed to set storage dir permissions: {}", e))
                    })?;
                    tracing::warn!(
                        dir = %config.storage_dir,
                        old_mode = format!("{:o}", mode),
                        "Fixed insecure storage directory permissions to 0700"
                    );
                }
            }
        }

        let node = tokio::task::spawn_blocking(move || {
            let mut builder = ldk_node::Builder::new();
            builder.set_storage_dir_path(config.storage_dir.clone());
            builder.set_network(config.network);
            builder.set_chain_source_esplora(config.esplora_url.clone(), None);

            if let Some(ref addr) = config.listening_address {
                let socket_addr: std::net::SocketAddr = addr
                    .parse()
                    .map_err(|e| ElisymError::Config(format!("Invalid listening address: {}", e)))?;
                builder.set_listening_addresses(vec![socket_addr.into()])
                    .map_err(|e| ElisymError::Config(format!("Failed to set listening address: {}", e)))?;
            }

            if let (Some(ref node_id_hex), Some(ref addr_str)) =
                (&config.lsp_node_id, &config.lsp_address)
            {
                let lsp_pubkey: ldk_node::bitcoin::secp256k1::PublicKey = node_id_hex
                    .parse()
                    .map_err(|e| ElisymError::Config(format!("Invalid LSP node ID: {:?}", e)))?;
                let lsp_addr: ldk_node::lightning::ln::msgs::SocketAddress = addr_str
                    .parse()
                    .map_err(|_| ElisymError::Config(format!("Invalid LSP address: {}", addr_str)))?;
                builder.set_liquidity_source_lsps2(lsp_pubkey, lsp_addr, config.lsp_token.clone());
            }

            let node = builder
                .build()
                .map_err(|e| ElisymError::Payment(format!("Failed to build LDK node: {}", e)))?;

            node.start()
                .map_err(|e| ElisymError::Payment(format!("Failed to start LDK node: {}", e)))?;

            Ok::<_, ElisymError>(node)
        })
        .await
        .map_err(|e| ElisymError::Payment(format!("LDK start task panicked: {}", e)))??;

        self.node = Some(node);

        tracing::info!("Payment service started");

        Ok(())
    }

    // ── BOLT11 methods ──

    /// Generate a BOLT11 invoice for the given amount.
    pub fn make_invoice(
        &self,
        amount_msat: u64,
        description: &str,
        expiry_secs: u32,
    ) -> Result<String> {
        if amount_msat == 0 {
            return Err(ElisymError::Payment("Invoice amount must be greater than 0".into()));
        }
        let node = self
            .node
            .as_ref()
            .ok_or_else(|| ElisymError::Payment("Node not started".into()))?;

        let desc = ldk_node::lightning_invoice::Bolt11InvoiceDescription::Direct(
            ldk_node::lightning_invoice::Description::new(description.to_string())
                .map_err(|e| ElisymError::Payment(format!("Invalid description: {:?}", e)))?,
        );

        let invoice = if self.lsps2_enabled {
            node.bolt11_payment()
                .receive_via_jit_channel(amount_msat, &desc, expiry_secs, None)
                .map_err(|e| ElisymError::Payment(format!("Failed to create JIT invoice: {}", e)))?
        } else {
            node.bolt11_payment()
                .receive(amount_msat, &desc, expiry_secs)
                .map_err(|e| ElisymError::Payment(format!("Failed to create BOLT11 invoice: {}", e)))?
        };

        Ok(invoice.to_string())
    }

    /// Pay a BOLT11 invoice.
    ///
    /// Returns an error if there is insufficient outbound channel capacity
    /// or if no channels are available.
    pub fn pay_invoice(&self, bolt11: &str) -> Result<PaymentResult> {
        let node = self
            .node
            .as_ref()
            .ok_or_else(|| ElisymError::Payment("Node not started".into()))?;

        let invoice: ldk_node::lightning_invoice::Bolt11Invoice = bolt11
            .parse()
            .map_err(|e| ElisymError::Payment(format!("Invalid BOLT11 invoice: {:?}", e)))?;

        // Check outbound liquidity before attempting payment
        if let Some(amount_msat) = invoice.amount_milli_satoshis() {
            let total_outbound: u64 = node
                .list_channels()
                .iter()
                .filter(|ch| ch.is_usable)
                .map(|ch| ch.outbound_capacity_msat)
                .sum();
            if total_outbound < amount_msat {
                return Err(ElisymError::Payment(format!(
                    "Insufficient outbound capacity: need {} msat, have {} msat across usable channels",
                    amount_msat, total_outbound
                )));
            }
        }

        let payment_id = node
            .bolt11_payment()
            .send(&invoice, None)
            .map_err(|e| ElisymError::Payment(format!("BOLT11 payment failed: {}", e)))?;

        Ok(PaymentResult {
            payment_id: format!("{:?}", payment_id),
            status: "pending".to_string(),
        })
    }

    /// Check the status of a payment by looking up the BOLT11 invoice string.
    /// Extracts the payment hash from the invoice to find the payment in the store.
    pub fn lookup_invoice(&self, bolt11: &str) -> Result<LdkInvoiceStatus> {
        let node = self
            .node
            .as_ref()
            .ok_or_else(|| ElisymError::Payment("Node not started".into()))?;

        let invoice: ldk_node::lightning_invoice::Bolt11Invoice = bolt11
            .parse()
            .map_err(|e| ElisymError::Payment(format!("Invalid BOLT11 invoice: {:?}", e)))?;

        let payment_id = ldk_node::lightning::ln::channelmanager::PaymentId(
            *invoice.payment_hash().as_byte_array(),
        );

        match node.payment(&payment_id) {
            Some(details) => Ok(LdkInvoiceStatus {
                settled: details.status == ldk_node::payment::PaymentStatus::Succeeded,
                amount_msat: details.amount_msat,
            }),
            None => Err(ElisymError::Payment("Payment not found".into())),
        }
    }

    /// Check whether a BOLT11 invoice has been paid. Useful for crash recovery:
    /// if a provider crashes after receiving payment but before sending the result,
    /// it can call this on restart with the saved invoice string to decide whether
    /// to re-deliver the result.
    ///
    /// LDK-node persists payment state to disk (SQLite), so this works across restarts.
    pub fn is_invoice_paid(&self, bolt11: &str) -> Result<bool> {
        Ok(self.lookup_invoice(bolt11)?.settled)
    }

    /// Get the on-chain balance in satoshis.
    pub fn onchain_balance(&self) -> Result<u64> {
        let node = self
            .node
            .as_ref()
            .ok_or_else(|| ElisymError::Payment("Node not started".into()))?;

        let balance = node.list_balances();
        Ok(balance.total_onchain_balance_sats)
    }

    /// Get a new on-chain address for funding.
    pub fn new_onchain_address(&self) -> Result<String> {
        let node = self
            .node
            .as_ref()
            .ok_or_else(|| ElisymError::Payment("Node not started".into()))?;

        let addr = node
            .onchain_payment()
            .new_address()
            .map_err(|e| ElisymError::Payment(format!("Failed to get address: {}", e)))?;

        Ok(addr.to_string())
    }

    /// Get the node's public key.
    pub fn node_id(&self) -> Result<String> {
        let node = self
            .node
            .as_ref()
            .ok_or_else(|| ElisymError::Payment("Node not started".into()))?;

        Ok(node.node_id().to_string())
    }

    /// Open a channel to a peer (connects first if needed).
    pub fn open_channel(
        &self,
        node_id: &str,
        address: &str,
        amount_sats: u64,
    ) -> Result<String> {
        if amount_sats == 0 {
            return Err(ElisymError::Payment("Channel amount must be greater than 0".into()));
        }
        let node = self
            .node
            .as_ref()
            .ok_or_else(|| ElisymError::Payment("Node not started".into()))?;

        let pubkey: ldk_node::bitcoin::secp256k1::PublicKey = node_id
            .parse()
            .map_err(|e| ElisymError::Payment(format!("Invalid node ID: {:?}", e)))?;

        let addr: std::net::SocketAddr = address
            .parse()
            .map_err(|e| ElisymError::Payment(format!("Invalid address: {}", e)))?;

        let user_channel_id = node
            .open_channel(pubkey, addr.into(), amount_sats, None, None)
            .map_err(|e| ElisymError::Payment(format!("Failed to open channel: {}", e)))?;

        Ok(format!("{:?}", user_channel_id))
    }

    /// Close a channel cooperatively. Funds return to on-chain wallets.
    ///
    /// If multiple channels exist with this peer, only the first one is closed.
    /// Use `list_channels()` to find specific channel IDs and `close_channel_by_id()`
    /// to close a specific one.
    ///
    /// If the counterparty is unresponsive, LDK will automatically force-close
    /// the channel after its internal timeout. Force-closed channels have a
    /// timelock delay before funds become spendable (typically 144 blocks on mainnet).
    pub fn close_channel(&self, counterparty_node_id: &str) -> Result<()> {
        let node = self
            .node
            .as_ref()
            .ok_or_else(|| ElisymError::Payment("Node not started".into()))?;

        let pubkey: ldk_node::bitcoin::secp256k1::PublicKey = counterparty_node_id
            .parse()
            .map_err(|e| ElisymError::Payment(format!("Invalid node ID: {:?}", e)))?;

        // Find the channel with this counterparty
        let channel = node
            .list_channels()
            .into_iter()
            .find(|ch| ch.counterparty_node_id == pubkey)
            .ok_or_else(|| ElisymError::Payment("No channel found with this peer".into()))?;

        node.close_channel(&channel.user_channel_id, pubkey)
            .map_err(|e| ElisymError::Payment(format!("Failed to close channel: {}", e)))?;

        Ok(())
    }

    /// Close a specific channel by its channel ID string (from `ChannelInfo::channel_id`).
    pub fn close_channel_by_id(&self, channel_id: &str, counterparty_node_id: &str) -> Result<()> {
        let node = self
            .node
            .as_ref()
            .ok_or_else(|| ElisymError::Payment("Node not started".into()))?;

        let pubkey: ldk_node::bitcoin::secp256k1::PublicKey = counterparty_node_id
            .parse()
            .map_err(|e| ElisymError::Payment(format!("Invalid node ID: {:?}", e)))?;

        let channel = node
            .list_channels()
            .into_iter()
            .find(|ch| format!("{}", ch.channel_id) == channel_id && ch.counterparty_node_id == pubkey)
            .ok_or_else(|| ElisymError::Payment("Channel not found".into()))?;

        node.close_channel(&channel.user_channel_id, pubkey)
            .map_err(|e| ElisymError::Payment(format!("Failed to close channel: {}", e)))?;

        Ok(())
    }

    /// Send on-chain BTC to an external address.
    pub fn send_onchain(&self, address: &str, amount_sats: u64) -> Result<String> {
        if amount_sats == 0 {
            return Err(ElisymError::Payment("Send amount must be greater than 0".into()));
        }
        let node = self
            .node
            .as_ref()
            .ok_or_else(|| ElisymError::Payment("Node not started".into()))?;

        let addr: ldk_node::bitcoin::Address<ldk_node::bitcoin::address::NetworkUnchecked> =
            address
                .parse()
                .map_err(|e| ElisymError::Payment(format!("Invalid address: {:?}", e)))?;

        let checked_addr = addr.require_network(self.config.network).map_err(|e| {
            ElisymError::Payment(format!(
                "Address network mismatch (expected {:?}): {}",
                self.config.network, e
            ))
        })?;

        let txid = node
            .onchain_payment()
            .send_to_address(&checked_addr, amount_sats, None)
            .map_err(|e| ElisymError::Payment(format!("Failed to send on-chain: {}", e)))?;

        Ok(txid.to_string())
    }

    /// Send all on-chain funds to an address, draining the wallet.
    pub fn send_all_onchain(&self, address: &str) -> Result<String> {
        let node = self
            .node
            .as_ref()
            .ok_or_else(|| ElisymError::Payment("Node not started".into()))?;

        let addr: ldk_node::bitcoin::Address<ldk_node::bitcoin::address::NetworkUnchecked> =
            address
                .parse()
                .map_err(|e| ElisymError::Payment(format!("Invalid address: {:?}", e)))?;

        let checked_addr = addr.require_network(self.config.network).map_err(|e| {
            ElisymError::Payment(format!(
                "Address network mismatch (expected {:?}): {}",
                self.config.network, e
            ))
        })?;

        let txid = node
            .onchain_payment()
            .send_all_to_address(&checked_addr, false, None)
            .map_err(|e| ElisymError::Payment(format!("Failed to send all on-chain: {}", e)))?;

        Ok(txid.to_string())
    }

    /// List open channels.
    pub fn list_channels(&self) -> Result<Vec<ChannelInfo>> {
        let node = self
            .node
            .as_ref()
            .ok_or_else(|| ElisymError::Payment("Node not started".into()))?;

        Ok(node
            .list_channels()
            .iter()
            .map(|ch| ChannelInfo {
                channel_id: format!("{}", ch.channel_id),
                counterparty_node_id: ch.counterparty_node_id.to_string(),
                channel_value_sats: ch.channel_value_sats,
                is_channel_ready: ch.is_channel_ready,
                is_usable: ch.is_usable,
                outbound_capacity_msat: ch.outbound_capacity_msat,
                inbound_capacity_msat: ch.inbound_capacity_msat,
                funding_txo: ch.funding_txo.map(|o| format!("{}", o)),
            })
            .collect())
    }
}

impl PaymentService {
    /// Gracefully stop the LDK node. Call this before dropping in async context.
    pub fn stop(&mut self) {
        if let Some(node) = self.node.take() {
            let _ = node.stop();
        }
    }
}

impl Drop for PaymentService {
    fn drop(&mut self) {
        // Use take() so we don't try to stop twice if stop() was already called
        if let Some(node) = self.node.take() {
            let _ = node.stop();
        }
    }
}
