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
pub use identity::{AgentIdentity, CapabilityCard, PaymentInfo};
pub use discovery::{DiscoveryService, DiscoveredAgent, AgentFilter};
pub use messaging::MessagingService;
pub use marketplace::{MarketplaceService, JobRequest, JobResult, JobFeedback};
pub use payment::{PaymentProvider, PaymentRequest, PaymentResult, PaymentStatus, PaymentChain};

/// A subscription handle wrapping an `mpsc::Receiver<T>` and the spawned task.
///
/// `.recv()` works transparently via `DerefMut`. Call [`cancel()`](Self::cancel)
/// to abort the background task, or simply drop the `Subscription` (the task
/// will end naturally once the receiver is dropped and `tx.send()` fails).
///
/// # Usage
///
/// Use `sub.recv().await` directly (via `DerefMut` to `mpsc::Receiver<T>`).
/// The `rx` field is public for advanced use cases (e.g., `tokio::select!`),
/// but in most cases the `Deref`-based access is sufficient.
pub struct Subscription<T> {
    /// The underlying channel receiver. Exposed for advanced use cases like
    /// `tokio::select!`. For simple consumption, use `sub.recv().await` directly
    /// (provided by the `DerefMut` impl).
    pub rx: mpsc::Receiver<T>,
    handle: JoinHandle<()>,
}

impl<T> Subscription<T> {
    pub(crate) fn new(rx: mpsc::Receiver<T>, handle: JoinHandle<()>) -> Self {
        Self { rx, handle }
    }

    /// Abort the background subscription task immediately.
    pub fn cancel(self) {
        self.handle.abort();
    }
}

impl<T> Drop for Subscription<T> {
    fn drop(&mut self) {
        self.handle.abort();
    }
}

impl<T> std::ops::Deref for Subscription<T> {
    type Target = mpsc::Receiver<T>;
    fn deref(&self) -> &Self::Target {
        &self.rx
    }
}

impl<T> std::ops::DerefMut for Subscription<T> {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.rx
    }
}

#[cfg(feature = "payments-ldk")]
pub use payment::ldk::{LdkPaymentProvider, LdkPaymentConfig, ChannelInfo};

#[cfg(feature = "payments-solana")]
pub use payment::solana::{
    SolanaPaymentProvider, SolanaPaymentConfig, SolanaNetwork,
    PROTOCOL_TREASURY, validate_protocol_fee,
};

use std::sync::Arc;
use nostr_sdk::Client;
use tokio::sync::mpsc;
use tokio::task::JoinHandle;

/// Main orchestrator that ties all services together.
///
/// Fields are `pub` for SDK ergonomics — direct access to `discovery`,
/// `marketplace`, `messaging`, and `payments` lets callers compose
/// workflows freely. Use [`shutdown()`](Self::shutdown) for clean teardown
/// (disconnects relays, which terminates subscription tasks).
pub struct AgentNode {
    pub identity: AgentIdentity,
    pub client: Client,
    pub discovery: DiscoveryService,
    pub messaging: MessagingService,
    pub marketplace: MarketplaceService,
    /// Pluggable payment provider (Lightning, Solana, etc.).
    ///
    /// Stored as `Arc` so that blocking payment calls (e.g., Solana RPC) can be
    /// offloaded to `spawn_blocking` without lifetime issues.
    pub payments: Option<Arc<dyn PaymentProvider>>,
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
    /// For Solana, the protocol fee (3%) is automatically included via
    /// `create_payment_request()` (which delegates to
    /// `create_payment_request_with_protocol_fee()`). For custom fee
    /// setups, use `SolanaPaymentProvider::create_payment_request_with_fee()`
    /// directly instead of this method.
    ///
    /// # Cancellation safety
    ///
    /// Once payment is confirmed (step 3), the result delivery (step 4) is
    /// spawned as an independent `tokio::spawn` task. This means dropping the
    /// returned future after payment confirmation will **not** prevent result
    /// delivery — the spawned task runs to completion independently.
    ///
    /// Steps 1–3 (invoice creation, feedback, payment polling) are safe to
    /// cancel — no funds have been transferred yet.
    ///
    /// # Known limitation: paid-but-undelivered failure
    ///
    /// If the payment is confirmed but all result delivery attempts fail
    /// (e.g., relay outage), the customer has paid but will **not** receive
    /// the result. Currently, an error feedback event is sent to notify the
    /// customer, but there is no automatic retry or refund mechanism.
    ///
    /// Callers should handle the returned `Err` and implement their own
    /// recovery strategy (e.g., persist the result for later re-delivery,
    /// initiate a refund, or alert an operator).
    ///
    /// A built-in recovery mechanism is planned for a future release.
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

        // 1. Generate payment request (offloaded to blocking thread for Solana RPC safety)
        let p = Arc::clone(payments);
        let desc = invoice_description.to_string();
        let payment_request = tokio::task::spawn_blocking(move || {
            p.create_payment_request(amount, &desc, invoice_expiry_secs)
        })
        .await
        .map_err(|e| ElisymError::Payment(format!("Payment task panicked: {}", e)))??;

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
        //
        // Lookup runs *before* the deadline check so that a payment confirmed
        // during the last sleep interval is not missed when the deadline expires.
        let deadline = tokio::time::Instant::now() + payment_timeout;
        let mut poll_interval = std::time::Duration::from_secs(1);
        let max_interval = std::time::Duration::from_secs(8);
        loop {
            // Offload blocking payment lookup to a separate thread
            let p = Arc::clone(payments);
            let req_str = payment_request.request.clone();
            let lookup_result = tokio::task::spawn_blocking(move || {
                p.lookup_payment(&req_str)
            })
            .await
            .map_err(|e| ElisymError::Payment(format!("Payment task panicked: {}", e)))?;

            match lookup_result {
                Ok(status) if status.settled => {
                    tracing::info!(amount, "Payment confirmed");
                    break;
                }
                _ => {}
            }

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

            tokio::time::sleep(poll_interval).await;
            poll_interval = (poll_interval * 2).min(max_interval);
        }

        // 4. Payment confirmed — deliver result with retries.
        //
        // Spawned as an independent task so that dropping the parent future
        // (e.g., via tokio::select!) does not abort result delivery after
        // the customer has already paid.
        let marketplace = self.marketplace.clone();
        let raw_event = job.raw_event.clone();
        let content = result_content.to_string();
        let delivery_handle = tokio::spawn(async move {
            let mut last_err = None;
            for attempt in 0..3u32 {
                match marketplace
                    .submit_job_result(&raw_event, &content, Some(amount))
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

            // Notify customer that payment was received but delivery failed
            let _ = marketplace
                .submit_job_feedback(
                    &raw_event,
                    JobStatus::Error,
                    Some("payment-received-delivery-failed"),
                    None,
                    None,
                    None,
                )
                .await;

            Err(last_err.unwrap_or_else(|| {
                ElisymError::Payment("Failed to deliver result after payment".into())
            }))
        });

        delivery_handle
            .await
            .map_err(|e| ElisymError::Payment(format!("Delivery task panicked: {}", e)))?
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
    picture_url: Option<String>,
    /// When `true`, `build()` publishes the Nostr profile (kind:0) and
    /// capability card (NIP-89) automatically. Default: `false`.
    publish_on_build: bool,
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
            picture_url: None,
            publish_on_build: false,
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

    /// Set the NIP-90 job kind offsets this agent handles.
    ///
    /// Each offset produces a request kind `5000 + offset` and result kind `6000 + offset`.
    /// Default: `[5100]` (offset 100, the elisym default).
    pub fn supported_job_kinds(mut self, kinds: Vec<u16>) -> Self {
        self.supported_job_kinds = kinds;
        self
    }

    pub fn secret_key(mut self, secret_key: impl Into<String>) -> Self {
        self.secret_key = Some(secret_key.into());
        self
    }

    pub fn picture_url(mut self, url: impl Into<String>) -> Self {
        self.picture_url = Some(url.into());
        self
    }

    #[cfg(feature = "payments-ldk")]
    pub fn ldk_payment_config(mut self, config: payment::ldk::LdkPaymentConfig) -> Self {
        self.ldk_payment_config = Some(config);
        self
    }

    /// Publish the Nostr profile and capability card automatically when
    /// `build()` completes. Default: `false` (no auto-publish).
    pub fn publish_on_build(mut self, yes: bool) -> Self {
        self.publish_on_build = yes;
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
        let payments: Option<Arc<dyn PaymentProvider>> = {
            // Try LDK first
            #[cfg(feature = "payments-ldk")]
            {
                if let Some(config) = self.ldk_payment_config {
                    let mut provider = payment::ldk::LdkPaymentProvider::new(config);
                    provider.start().await?;
                    Some(Arc::new(provider) as Arc<dyn PaymentProvider>)
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
                    .map(|p| Arc::new(p) as Arc<dyn PaymentProvider>)
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
        let mut card = CapabilityCard::new(
            &self.name,
            &self.description,
            self.capabilities.clone(),
        );
        card.set_version(env!("CARGO_PKG_VERSION"));

        // Create nostr client and connect to relays
        let client = Client::builder().signer(identity.keys().clone()).build();
        for relay in &self.relays {
            client.add_relay(relay.as_str()).await.map_err(|e| {
                ElisymError::Config(format!("Failed to add relay {}: {}", relay, e))
            })?;
        }
        client.connect().await;

        // Wait for at least one relay to connect (100ms polling, 15s max)
        let start = tokio::time::Instant::now();
        let connected_count;
        loop {
            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
            let relays = client.relays().await;
            let count = relays
                .values()
                .filter(|r| r.status() == nostr_sdk::RelayStatus::Connected)
                .count();
            if count > 0 || start.elapsed() >= std::time::Duration::from_secs(15) {
                connected_count = count;
                break;
            }
        }
        if connected_count == 0 {
            return Err(ElisymError::Config(
                "No relays connected — cannot operate without at least one relay".into(),
            ));
        } else {
            let connected_urls: Vec<String> = client
                .relays()
                .await
                .iter()
                .filter(|(_, r)| r.status() == nostr_sdk::RelayStatus::Connected)
                .map(|(url, _)| url.to_string())
                .collect();
            tracing::info!(
                connected = connected_count,
                total = self.relays.len(),
                relays = ?connected_urls,
                "Connected to relays"
            );
        }

        // Create services
        let discovery = DiscoveryService::new(client.clone(), identity.clone());
        let messaging = MessagingService::new(client.clone(), identity.clone());
        let marketplace = MarketplaceService::new(client.clone(), identity.clone());

        // Optionally publish profile and capability card in the background.
        // Only runs when the caller explicitly opted in via `.publish_on_build(true)`.
        if self.publish_on_build {
            let discovery_bg = discovery.clone();
            let card_bg = card.clone();
            let capabilities_bg = self.capabilities.clone();
            let job_kinds_bg = self.supported_job_kinds.clone();
            tokio::spawn(async move {
                // Publish capability card + kind:0 profile (skip for customer-only agents with no capabilities)
                if !capabilities_bg.is_empty() {
                    if let Err(e) = discovery_bg
                        .publish_capability(&card_bg, &job_kinds_bg)
                        .await
                    {
                        tracing::warn!(error = %e, "Failed to publish capability card, continuing");
                    }
                }
            });
        }

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
