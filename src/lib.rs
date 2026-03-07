//! # elisym-core
//!
//! Rust SDK for AI agents to discover each other via [Nostr](https://github.com/nostr-protocol/nips)
//! and pay for task execution via pluggable payment backends.
//!
//! ## Overview
//!
//! `elisym-core` provides everything an AI agent needs to participate in a
//! decentralized marketplace:
//!
//! - **Discovery** — publish and search agent capabilities using NIP-89 (kind:31990)
//! - **Marketplace** — submit and receive jobs using NIP-90 Data Vending Machines
//! - **Messaging** — encrypted private messages via NIP-17 (NIP-44 + NIP-59 gift wrap)
//! - **Payments** — pluggable payment providers via the [`PaymentProvider`] trait.
//!   Built-in: Lightning via LDK-node (`payments-ldk`), Solana (`payments-solana`)
//!
//! ## Quick Start
//!
//! ```rust,no_run
//! use elisym_core::*;
//!
//! #[tokio::main]
//! async fn main() -> Result<()> {
//!     let agent = AgentNodeBuilder::new("my-agent", "Summarizes text")
//!         .capabilities(vec!["summarization".into()])
//!         .build().await?;
//!
//!     // Publish capabilities, subscribe to jobs, send results — see examples/
//!     agent.discovery.search_agents(&AgentFilter::default()).await?;
//!     Ok(())
//! }
//! ```

pub mod error;
pub mod types;
pub mod identity;
pub mod discovery;
pub mod messaging;
pub mod marketplace;
pub mod payment;
pub(crate) mod dedup;

pub use error::{ElisymError, Result};
pub use types::*;
pub use identity::{AgentIdentity, CapabilityCard};
pub use discovery::{DiscoveryService, DiscoveredAgent, AgentFilter};
pub use messaging::MessagingService;
pub use marketplace::MarketplaceService;
pub use payment::{PaymentProvider, PaymentRequest, PaymentResult, PaymentStatus, PaymentChain};

#[cfg(feature = "payments-ldk")]
pub use payment::ldk::{LdkPaymentProvider, LdkPaymentConfig, ChannelInfo};

#[cfg(feature = "payments-solana")]
pub use payment::solana::{SolanaPaymentProvider, SolanaPaymentConfig, SolanaNetwork};

use nostr_sdk::Client;

/// Main orchestrator that ties all services together.
pub struct AgentNode {
    pub identity: AgentIdentity,
    pub client: Client,
    pub discovery: DiscoveryService,
    pub messaging: MessagingService,
    pub marketplace: MarketplaceService,
    /// Pluggable payment provider (Lightning, Solana, etc.).
    pub payments: Option<Box<dyn PaymentProvider>>,
    pub capability_card: CapabilityCard,
}

impl AgentNode {
    /// Gracefully shut down the agent: disconnect from relays and drop the
    /// payment provider (if running). Disconnecting the client causes all spawned
    /// subscription tasks to terminate.
    ///
    /// **Note:** Callers should drain any active `mpsc::Receiver` channels
    /// before calling shutdown to process remaining buffered events.
    pub async fn shutdown(&mut self) {
        let _ = self.client.disconnect().await;
        self.payments = None;
        tracing::info!("AgentNode shut down");
    }

    /// Downcast the payment provider to [`LdkPaymentProvider`] for LDK-specific operations
    /// (channel management, on-chain, etc.).
    #[cfg(feature = "payments-ldk")]
    pub fn ldk_payments(&self) -> Option<&payment::ldk::LdkPaymentProvider> {
        self.payments.as_ref()?.as_any().downcast_ref()
    }

    /// Downcast the payment provider to [`SolanaPaymentProvider`] for Solana-specific operations
    /// (balance, airdrop, etc.).
    #[cfg(feature = "payments-solana")]
    pub fn solana_payments(&self) -> Option<&payment::solana::SolanaPaymentProvider> {
        self.payments.as_ref()?.as_any().downcast_ref()
    }

    /// Process a job with payment enforcement: generate payment request, send
    /// payment-required feedback, wait for payment, only then deliver result.
    ///
    /// This is the **recommended** way for providers to deliver paid results.
    /// Calling `submit_job_result()` directly skips payment verification.
    ///
    /// This method creates a payment request without fees. For fee-aware
    /// payments, use the provider-specific method (e.g.,
    /// `SolanaPaymentProvider::create_payment_request_with_fee()`) directly.
    pub async fn process_job_with_payment(
        &self,
        job: &marketplace::JobRequest,
        result_content: &str,
        amount: u64,
        invoice_description: &str,
        invoice_expiry_secs: u32,
        payment_timeout: std::time::Duration,
    ) -> Result<nostr_sdk::EventId> {
        let payments = self
            .payments
            .as_ref()
            .ok_or_else(|| ElisymError::Payment("Payments not configured".into()))?;

        // 1. Generate payment request
        let payment_request = payments.create_payment_request(
            amount,
            invoice_description,
            invoice_expiry_secs,
        )?;

        let chain_str = payment_request.chain.to_string();
        tracing::info!(amount, chain = %chain_str, "Generated payment request for job");

        // 2. Send payment-required feedback
        self.marketplace
            .submit_job_feedback(
                &job.raw_event,
                JobStatus::PaymentRequired,
                None,
                Some(amount),
                Some(&payment_request.request),
                Some(&chain_str),
            )
            .await?;

        // 3. Poll for payment until confirmed or timeout (with backoff: 1s, 2s, 4s, 8s, 8s, ...)
        let deadline = tokio::time::Instant::now() + payment_timeout;
        let mut poll_interval = std::time::Duration::from_secs(1);
        let max_interval = std::time::Duration::from_secs(8);
        loop {
            if tokio::time::Instant::now() >= deadline {
                // Send error feedback so customer knows
                let _ = self
                    .marketplace
                    .submit_job_feedback(
                        &job.raw_event,
                        JobStatus::Error,
                        Some("payment-timeout"),
                        None,
                        None,
                        None,
                    )
                    .await;
                return Err(ElisymError::Payment(
                    "Payment timeout — result not delivered".into(),
                ));
            }

            match payments.lookup_payment(&payment_request.request) {
                Ok(status) if status.settled => {
                    tracing::info!(amount, "Payment confirmed");
                    break;
                }
                _ => {
                    tokio::time::sleep(poll_interval).await;
                    poll_interval = (poll_interval * 2).min(max_interval);
                }
            }
        }

        // 4. Payment confirmed — deliver result with retries.
        let mut last_err = None;
        for attempt in 0..3u32 {
            match self
                .marketplace
                .submit_job_result(&job.raw_event, result_content, Some(amount))
                .await
            {
                Ok(event_id) => return Ok(event_id),
                Err(e) => {
                    tracing::warn!(attempt, error = %e, "Failed to deliver result after payment, retrying");
                    last_err = Some(e);
                    tokio::time::sleep(std::time::Duration::from_secs(2u64.pow(attempt))).await;
                }
            }
        }

        Err(last_err.unwrap_or_else(|| {
            ElisymError::Payment("Failed to deliver result after payment".into())
        }))
    }
}

/// Builder for constructing an AgentNode with all services configured.
pub struct AgentNodeBuilder {
    name: String,
    description: String,
    capabilities: Vec<String>,
    relays: Vec<String>,
    supported_job_kinds: Vec<u16>,
    secret_key: Option<String>,
    #[cfg(feature = "payments-ldk")]
    ldk_payment_config: Option<payment::ldk::LdkPaymentConfig>,
    #[cfg(feature = "payments-solana")]
    solana_payment_provider: Option<payment::solana::SolanaPaymentProvider>,
}

impl AgentNodeBuilder {
    pub fn new(name: impl Into<String>, description: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            description: description.into(),
            capabilities: Vec::new(),
            relays: DEFAULT_RELAYS.iter().map(|s| s.to_string()).collect(),
            supported_job_kinds: vec![KIND_JOB_REQUEST_BASE + DEFAULT_KIND_OFFSET],
            secret_key: None,
            #[cfg(feature = "payments-ldk")]
            ldk_payment_config: None,
            #[cfg(feature = "payments-solana")]
            solana_payment_provider: None,
        }
    }

    pub fn capabilities(mut self, capabilities: Vec<String>) -> Self {
        self.capabilities = capabilities;
        self
    }

    pub fn relays(mut self, relays: Vec<String>) -> Self {
        self.relays = relays;
        self
    }

    pub fn supported_job_kinds(mut self, kinds: Vec<u16>) -> Self {
        self.supported_job_kinds = kinds;
        self
    }

    pub fn secret_key(mut self, secret_key: impl Into<String>) -> Self {
        self.secret_key = Some(secret_key.into());
        self
    }

    #[cfg(feature = "payments-ldk")]
    pub fn ldk_payment_config(mut self, config: payment::ldk::LdkPaymentConfig) -> Self {
        self.ldk_payment_config = Some(config);
        self
    }

    /// Set a pre-constructed Solana payment provider.
    #[cfg(feature = "payments-solana")]
    pub fn solana_payment_provider(mut self, provider: payment::solana::SolanaPaymentProvider) -> Self {
        self.solana_payment_provider = Some(provider);
        self
    }

    pub async fn build(self) -> Result<AgentNode> {
        // Ensure rustls has a crypto provider for TLS (wss:// relay connections).
        // LDK-node pulls in rustls 0.23 which requires an explicit provider.
        #[cfg(feature = "payments-ldk")]
        let _ = rustls::crypto::aws_lc_rs::default_provider().install_default();

        let identity = match self.secret_key {
            Some(key) => AgentIdentity::from_secret_key(&key)?,
            None => AgentIdentity::generate(),
        };

        // Initialize payment provider (only one active at a time)
        let payments: Option<Box<dyn PaymentProvider>> = {
            // Try LDK first
            #[cfg(feature = "payments-ldk")]
            {
                if let Some(config) = self.ldk_payment_config {
                    let mut provider = payment::ldk::LdkPaymentProvider::new(config);
                    provider.start().await?;
                    Some(Box::new(provider) as Box<dyn PaymentProvider>)
                } else {
                    None
                }
            }
            #[cfg(not(feature = "payments-ldk"))]
            { None }
        }
        // Then try Solana if no LDK provider was configured
        .or_else(|| {
            #[cfg(feature = "payments-solana")]
            {
                self.solana_payment_provider
                    .map(|p| Box::new(p) as Box<dyn PaymentProvider>)
            }
            #[cfg(not(feature = "payments-solana"))]
            { None }
        });

        // Create capability card
        if self.capabilities.is_empty() {
            tracing::warn!(
                "No capabilities set — this agent will not be discoverable via search_agents()"
            );
        }
        let card = CapabilityCard::new(
            &self.name,
            &self.description,
            self.capabilities.clone(),
        );

        // Create nostr client and connect to relays
        let client = Client::builder().signer(identity.keys().clone()).build();
        for relay in &self.relays {
            client.add_relay(relay.as_str()).await.map_err(|e| {
                ElisymError::Config(format!("Failed to add relay {}: {}", relay, e))
            })?;
        }
        client.connect().await;

        // Wait for at least one relay to connect, with increasing backoff
        let mut connected_count = 0;
        for wait_secs in [2, 3, 5] {
            tokio::time::sleep(std::time::Duration::from_secs(wait_secs)).await;
            let relays = client.relays().await;
            connected_count = relays
                .values()
                .filter(|r| r.status() == nostr_sdk::RelayStatus::Connected)
                .count();
            if connected_count > 0 {
                break;
            }
            tracing::warn!(
                attempt_wait = wait_secs,
                "No relays connected yet, retrying..."
            );
        }
        if connected_count == 0 {
            return Err(ElisymError::Config(
                "No relays connected — cannot operate without at least one relay".into(),
            ));
        } else {
            tracing::info!(
                connected = connected_count,
                total = self.relays.len(),
                "Connected to relays"
            );
        }

        // Create services
        let discovery = DiscoveryService::new(client.clone(), identity.clone());
        let messaging = MessagingService::new(client.clone(), identity.clone());
        let marketplace = MarketplaceService::new(client.clone(), identity.clone());

        // Publish capability card
        discovery
            .publish_capability(&card, &self.supported_job_kinds)
            .await?;

        tracing::info!(
            name = %self.name,
            pubkey = %identity.npub(),
            "AgentNode started"
        );

        Ok(AgentNode {
            identity,
            client,
            discovery,
            messaging,
            marketplace,
            payments,
            capability_card: card,
        })
    }
}
