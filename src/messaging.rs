use nostr_sdk::prelude::*;
use serde::Serialize;
use tokio::sync::mpsc;

use crate::dedup::BoundedDedup;
use crate::error::Result;
use crate::identity::AgentIdentity;

/// Receive the next notification, handling lagged broadcast receivers by
/// logging the skip and continuing. Returns `None` when the channel is closed.
async fn recv_notification(
    notifications: &mut tokio::sync::broadcast::Receiver<RelayPoolNotification>,
) -> Option<RelayPoolNotification> {
    loop {
        match notifications.recv().await {
            Ok(n) => return Some(n),
            Err(tokio::sync::broadcast::error::RecvError::Lagged(n)) => {
                tracing::warn!(skipped = n, "Subscription receiver lagged, some events may have been missed");
                continue;
            }
            Err(tokio::sync::broadcast::error::RecvError::Closed) => return None,
        }
    }
}

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
    /// Returns a receiver channel that yields messages as they arrive.
    pub async fn subscribe_to_messages(&self) -> Result<mpsc::Receiver<PrivateMessage>> {
        let (tx, rx) = mpsc::channel(256);

        // NIP-59 gift wraps use a randomized created_at (±2 days) for privacy.
        // Use a wide window so relays don't filter out messages with past timestamps.
        let since = Timestamp::from(Timestamp::now().as_u64().saturating_sub(2 * 24 * 60 * 60));
        let filter = Filter::new()
            .kind(Kind::GiftWrap)
            .pubkey(self.identity.public_key())
            .since(since);
        self.client.subscribe(vec![filter], None).await?;

        let client = self.client.clone();
        tokio::spawn(async move {
            let mut notifications = client.notifications();
            let mut seen = BoundedDedup::new(10_000);
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

        Ok(rx)
    }
}
