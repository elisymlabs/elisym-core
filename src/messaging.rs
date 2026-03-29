use nostr_sdk::prelude::*;
use serde::Serialize;
use tokio::sync::mpsc;

use crate::Subscription;
use crate::dedup::{BoundedDedup, recv_notification, DEDUP_CAPACITY};
use crate::error::Result;
use crate::identity::AgentIdentity;
use crate::types::{KIND_PING, KIND_PONG};
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

    // ── Ephemeral ping/pong (kind 20200/20201) ──

    /// Send an ephemeral ping to an agent.
    /// Best-effort: ephemeral events may not get relay OK acknowledgment.
    pub async fn send_ping(
        &self,
        agent_pubkey: &PublicKey,
        nonce: &str,
    ) -> Result<()> {
        let content = serde_json::json!({"type": "elisym_ping", "nonce": nonce}).to_string();
        let builder = EventBuilder::new(Kind::from(KIND_PING), &content)
            .tag(Tag::public_key(*agent_pubkey));
        let _ = self.client.send_event_builder(builder).await;
        Ok(())
    }

    /// Send an ephemeral pong response.
    /// Best-effort: logs but does not fail on relay rejection (common for ephemeral events).
    pub async fn send_pong(
        &self,
        recipient_pubkey: &PublicKey,
        nonce: &str,
    ) -> Result<()> {
        let content = serde_json::json!({"type": "elisym_pong", "nonce": nonce}).to_string();
        let builder = EventBuilder::new(Kind::from(KIND_PONG), &content)
            .tag(Tag::public_key(*recipient_pubkey));
        if let Err(e) = self.client.send_event_builder(builder).await {
            tracing::trace!(error = %e, "Pong relay rejection (expected for ephemeral events)");
        }
        Ok(())
    }

    /// Ping an agent and wait for pong. Returns true if online.
    /// Uses ephemeral events - no `since` filter needed (nothing stored).
    ///
    /// Spawns a dedicated listener task that immediately starts draining the
    /// broadcast channel, matching the TS SDK approach of callback-based
    /// subscriptions. This prevents pong events from being lost due to
    /// broadcast channel lag when other subscriptions are active.
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

        let my_pk_hex = self.identity.public_key().to_hex();
        let target_pk = *agent_pubkey;
        let nonce_for_listener = nonce.clone();

        // Create broadcast receiver and immediately spawn a task to drain it.
        // This avoids losing the pong to broadcast lag when the relay pool is busy.
        let mut notifications = self.client.notifications();
        let (found_tx, found_rx) = tokio::sync::oneshot::channel::<()>();

        let listener = tokio::spawn(async move {
            loop {
                match notifications.recv().await {
                    Ok(RelayPoolNotification::Event { event, .. }) => {
                        if event.kind != Kind::from(KIND_PONG) { continue; }
                        let targeted = event.tags.iter().any(|t| {
                            let s = t.as_slice();
                            s.first().map(|v| v.as_str()) == Some("p")
                                && s.get(1).map(|v| v.as_str()) == Some(my_pk_hex.as_str())
                        });
                        if !targeted { continue; }
                        if event.pubkey != target_pk { continue; }
                        if let Ok(parsed) = serde_json::from_str::<serde_json::Value>(&event.content) {
                            if parsed.get("type").and_then(|v| v.as_str()) == Some("elisym_pong")
                                && parsed.get("nonce").and_then(|v| v.as_str()) == Some(&nonce_for_listener)
                            {
                                let _ = found_tx.send(());
                                return;
                            }
                        }
                    }
                    // Lag means the pong could be among skipped events — potential false negative
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(n)) => {
                        tracing::warn!(skipped = n, "Ping listener lagged — pong may have been missed (false negative possible)");
                        continue;
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Closed) => return,
                    _ => continue,
                }
            }
        });

        // Subscribe to ephemeral pongs (helps relays route events to us)
        let filter = Filter::new()
            .kind(Kind::from(KIND_PONG))
            .custom_tag(
                SingleLetterTag::lowercase(Alphabet::P),
                vec![self.identity.public_key().to_hex()],
            );
        let sub_output = match self.client.subscribe(vec![filter], None).await {
            Ok(v) => v,
            Err(e) => {
                listener.abort();
                return Err(e.into());
            }
        };

        // Send ephemeral ping
        if let Err(e) = self.send_ping(agent_pubkey, &nonce).await {
            listener.abort();
            self.client.unsubscribe(sub_output.val).await;
            return Err(e);
        }

        // Wait for pong or timeout
        let timeout = tokio::time::Duration::from_secs(timeout_secs);
        let result = tokio::time::timeout(timeout, found_rx).await;

        listener.abort();
        self.client.unsubscribe(sub_output.val).await;

        Ok(result.is_ok_and(|r| r.is_ok()))
    }

    /// Subscribe to incoming ephemeral pings addressed to this agent.
    /// Returns a subscription yielding `(sender_pubkey, nonce)` pairs.
    /// No `since` filter - ephemeral events are never stored by relays.
    pub async fn subscribe_to_pings(&self) -> Result<Subscription<(PublicKey, String)>> {
        let (tx, rx) = mpsc::channel(256);

        let filter = Filter::new()
            .kind(Kind::from(KIND_PING))
            .custom_tag(
                SingleLetterTag::lowercase(Alphabet::P),
                vec![self.identity.public_key().to_hex()],
            );

        let mut notifications = self.client.notifications();
        self.client.subscribe(vec![filter], None).await?;

        let my_pk_hex = self.identity.public_key().to_hex();
        let handle = tokio::spawn(async move {
            let mut seen = BoundedDedup::new(DEDUP_CAPACITY);
            while let Some(notification) = recv_notification(&mut notifications).await {
                if let RelayPoolNotification::Event { event, .. } = notification {
                    if event.kind != Kind::from(KIND_PING) { continue; }
                    if !seen.insert(event.id) { continue; }

                    let targeted = event.tags.iter().any(|t| {
                        let s = t.as_slice();
                        s.first().map(|v| v.as_str()) == Some("p")
                            && s.get(1).map(|v| v.as_str()) == Some(my_pk_hex.as_str())
                    });
                    if !targeted { continue; }

                    if let Ok(parsed) = serde_json::from_str::<serde_json::Value>(&event.content) {
                        if parsed.get("type").and_then(|v| v.as_str()) == Some("elisym_ping") {
                            if let Some(nonce) = parsed.get("nonce").and_then(|v| v.as_str()) {
                                if tx.send((event.pubkey, nonce.to_string())).await.is_err() {
                                    break;
                                }
                            }
                        }
                    }
                }
            }
        });

        Ok(Subscription::new(rx, handle))
    }
}
