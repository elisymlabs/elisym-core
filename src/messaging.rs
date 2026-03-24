use nostr_sdk::prelude::*;
use serde::Serialize;
use tokio::sync::mpsc;

use crate::Subscription;
use crate::dedup::{BoundedDedup, recv_notification, DEDUP_CAPACITY};
use crate::error::Result;
use crate::identity::AgentIdentity;
/// A received private message.
#[derive(Debug, Clone)]
pub struct PrivateMessage {
    pub sender: PublicKey,
    pub content: String,
    pub timestamp: Timestamp,
}

/// Service for NIP-17 private messaging between agents.
#[derive(Debug, Clone)]
pub struct MessagingService {
    client: Client,
    identity: AgentIdentity,
}

impl MessagingService {
    pub fn new(client: Client, identity: AgentIdentity) -> Self {
        Self { client, identity }
    }

    /// Send a plaintext private message to a recipient using NIP-17 gift wrap.
    pub async fn send_message(
        &self,
        recipient: &PublicKey,
        content: impl Into<String>,
    ) -> Result<()> {
        self.client
            .send_private_msg(*recipient, content, [])
            .await?;

        tracing::debug!(recipient = %recipient, "Sent private message");
        Ok(())
    }

    /// Send a structured JSON message to a recipient.
    pub async fn send_structured_message<T: Serialize>(
        &self,
        recipient: &PublicKey,
        message: &T,
    ) -> Result<()> {
        let json = serde_json::to_string(message)?;
        self.send_message(recipient, json).await
    }

    /// Subscribe to incoming private messages.
    ///
    /// Returns a [`Subscription`] that yields messages via `.recv()`.
    /// Call `.cancel()` to abort the background task, or drop the subscription.
    ///
    /// **Backpressure:** The internal channel holds 256 items. If the receiver
    /// is not drained fast enough, the sending task blocks until space is available.
    pub async fn subscribe_to_messages(&self) -> Result<Subscription<PrivateMessage>> {
        let (tx, rx) = mpsc::channel(256);

        // NIP-59 gift wraps use a randomized created_at (±2 days) for privacy.
        // Use a wide window so relays don't filter out messages with past timestamps.
        let since = Timestamp::from(Timestamp::now().as_u64().saturating_sub(2 * 24 * 60 * 60));
        let filter = Filter::new()
            .kind(Kind::GiftWrap)
            .pubkey(self.identity.public_key())
            .since(since);
        // Create the broadcast receiver BEFORE subscribing, so no events
        // arriving between subscribe() and spawn() are lost.
        let mut notifications = self.client.notifications();

        self.client.subscribe(vec![filter], None).await?;

        let client = self.client.clone();
        let handle = tokio::spawn(async move {
            let mut seen = BoundedDedup::new(DEDUP_CAPACITY);
            while let Some(notification) = recv_notification(&mut notifications).await {
                if let RelayPoolNotification::Event { event, .. } = notification {
                    if !seen.insert(event.id) {
                        continue;
                    }
                    if event.kind == Kind::GiftWrap {
                        match client.unwrap_gift_wrap(&event).await {
                            Ok(unwrapped) => {
                                let msg = PrivateMessage {
                                    sender: unwrapped.sender,
                                    content: unwrapped.rumor.content.clone(),
                                    timestamp: unwrapped.rumor.created_at,
                                };
                                if tx.send(msg).await.is_err() {
                                    break;
                                }
                            }
                            Err(e) => {
                                // Expected for gift wraps not addressed to us
                                tracing::trace!(error = %e, "Could not unwrap gift wrap (not for us)");
                            }
                        }
                    }
                }
            }
            tracing::warn!("subscription task ended: messages (notification channel closed)");
        });

        Ok(Subscription::new(rx, handle))
    }

    // ── NIP-17 gift-wrapped ping/pong ──

    /// Send a gift-wrapped ping to an agent.
    pub async fn send_ping(
        &self,
        agent_pubkey: &PublicKey,
        nonce: &str,
    ) -> Result<()> {
        let content = serde_json::json!({"type": "elisym_ping", "nonce": nonce}).to_string();
        self.client.send_private_msg(*agent_pubkey, content, []).await?;
        Ok(())
    }

    /// Send a gift-wrapped pong response.
    pub async fn send_pong(
        &self,
        recipient_pubkey: &PublicKey,
        nonce: &str,
    ) -> Result<()> {
        let content = serde_json::json!({"type": "elisym_pong", "nonce": nonce}).to_string();
        self.client.send_private_msg(*recipient_pubkey, content, []).await?;
        Ok(())
    }

    /// Ping an agent and wait for pong. Returns true if online.
    /// Uses NIP-17 gift wraps. `since: now - 2 days` for NIP-59 timestamp randomization.
    pub async fn ping_agent(&self, agent_pubkey: &PublicKey, timeout_secs: u64) -> Result<bool> {
        use std::sync::atomic::{AtomicU64, Ordering};
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let nonce = format!(
            "{:x}{:x}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_nanos(),
            COUNTER.fetch_add(1, Ordering::Relaxed)
        );

        // Subscribe to gift wraps BEFORE sending ping
        let since = Timestamp::from(Timestamp::now().as_u64().saturating_sub(2 * 24 * 60 * 60));
        let filter = Filter::new()
            .kind(Kind::GiftWrap)
            .pubkey(self.identity.public_key())
            .since(since);
        let mut notifications = self.client.notifications();
        let sub_output = self.client.subscribe(vec![filter], None).await?;

        // Send gift-wrapped ping
        self.send_ping(agent_pubkey, &nonce).await?;

        let client = self.client.clone();
        let timeout = tokio::time::Duration::from_secs(timeout_secs);
        let result = tokio::time::timeout(timeout, async {
            while let Some(notification) = recv_notification(&mut notifications).await {
                if let RelayPoolNotification::Event { event, .. } = notification {
                    if event.kind != Kind::GiftWrap { continue; }
                    if let Ok(unwrapped) = client.unwrap_gift_wrap(&event).await {
                        if let Ok(parsed) = serde_json::from_str::<serde_json::Value>(&unwrapped.rumor.content) {
                            if parsed.get("type").and_then(|v| v.as_str()) == Some("elisym_pong")
                                && parsed.get("nonce").and_then(|v| v.as_str()) == Some(&nonce)
                            {
                                return true;
                            }
                        }
                    }
                }
            }
            false
        })
        .await
        .unwrap_or(false);

        self.client.unsubscribe(sub_output.val).await;

        Ok(result)
    }

    /// Subscribe to incoming gift-wrapped pings addressed to this agent.
    /// Returns a subscription yielding `(sender_pubkey, nonce)` pairs.
    /// Uses `since: now - 2 days` for NIP-59 timestamp randomization.
    pub async fn subscribe_to_pings(&self) -> Result<Subscription<(PublicKey, String)>> {
        let (tx, rx) = mpsc::channel(256);

        let since = Timestamp::from(Timestamp::now().as_u64().saturating_sub(2 * 24 * 60 * 60));
        let filter = Filter::new()
            .kind(Kind::GiftWrap)
            .pubkey(self.identity.public_key())
            .since(since);

        let mut notifications = self.client.notifications();
        self.client.subscribe(vec![filter], None).await?;

        let client = self.client.clone();
        let handle = tokio::spawn(async move {
            let mut seen = BoundedDedup::new(DEDUP_CAPACITY);
            while let Some(notification) = recv_notification(&mut notifications).await {
                if let RelayPoolNotification::Event { event, .. } = notification {
                    if event.kind != Kind::GiftWrap { continue; }
                    if !seen.insert(event.id) { continue; }

                    match client.unwrap_gift_wrap(&event).await {
                        Ok(unwrapped) => {
                            if let Ok(parsed) = serde_json::from_str::<serde_json::Value>(&unwrapped.rumor.content) {
                                if parsed.get("type").and_then(|v| v.as_str()) == Some("elisym_ping") {
                                    if let Some(nonce) = parsed.get("nonce").and_then(|v| v.as_str()) {
                                        if tx.send((unwrapped.sender, nonce.to_string())).await.is_err() {
                                            break;
                                        }
                                    }
                                }
                            }
                        }
                        Err(_) => { /* not for us */ }
                    }
                }
            }
        });

        Ok(Subscription::new(rx, handle))
    }
}
