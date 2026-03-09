use std::collections::HashSet;
use std::time::Duration;

use nostr_sdk::prelude::*;
use serde::{Deserialize, Serialize};

use crate::error::Result;
use crate::identity::{AgentIdentity, CapabilityCard};
use crate::types::{kind, KIND_APP_HANDLER};

/// A discovered agent with its capability card and event metadata.
#[derive(Debug, Clone)]
pub struct DiscoveredAgent {
    pub pubkey: PublicKey,
    pub card: CapabilityCard,
    pub event_id: EventId,
    pub supported_kinds: Vec<u16>,
}

/// Filter for searching agents.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct AgentFilter {
    pub capabilities: Vec<String>,
    pub job_kind: Option<u16>,
    pub since: Option<Timestamp>,
    /// Maximum number of agents to return. `None` means no limit.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub limit: Option<usize>,
}

/// Service for publishing and discovering agent capabilities via NIP-89.
#[derive(Debug, Clone)]
pub struct DiscoveryService {
    client: Client,
    identity: AgentIdentity,
}

impl DiscoveryService {
    pub fn new(client: Client, identity: AgentIdentity) -> Self {
        Self { client, identity }
    }

    /// Publish a capability card as a NIP-89 kind:31990 parameterized replaceable event.
    pub async fn publish_capability(
        &self,
        card: &CapabilityCard,
        supported_job_kinds: &[u16],
    ) -> Result<EventId> {
        let content = card.to_json()?;
        let pubkey_hex = self.identity.public_key().to_hex();

        let mut tags: Vec<Tag> = vec![
            Tag::identifier(pubkey_hex),
            Tag::custom(
                TagKind::SingleLetter(SingleLetterTag::lowercase(Alphabet::T)),
                vec!["elisym".to_string()],
            ),
        ];

        for k in supported_job_kinds {
            tags.push(Tag::custom(
                TagKind::SingleLetter(SingleLetterTag::lowercase(Alphabet::K)),
                vec![k.to_string()],
            ));
        }

        for cap in &card.capabilities {
            tags.push(Tag::custom(
                TagKind::SingleLetter(SingleLetterTag::lowercase(Alphabet::T)),
                vec![cap.clone()],
            ));
        }

        let builder = EventBuilder::new(kind(KIND_APP_HANDLER), content).tags(tags);
        let output = self.client.send_event_builder(builder).await?;

        tracing::info!(event_id = %output.val, "Published capability card");
        Ok(output.val)
    }

    /// Search for agents matching the given filter.
    ///
    /// NIP-01 `custom_tag` uses OR semantics for multiple values, so we fetch
    /// all elisym agents from relays and apply capability filtering locally
    /// to get correct AND semantics (agent must have ALL requested capabilities).
    ///
    /// **Scalability note:** This fetches all elisym capability cards from relays
    /// and filters client-side. At small scale this is fine, but on a busy network
    /// with thousands of agents it will become expensive. The `limit` field
    /// truncates results *after* fetching. Relay-side pagination or caching may
    /// be needed for production at scale.
    pub async fn search_agents(&self, filter: &AgentFilter) -> Result<Vec<DiscoveredAgent>> {
        let mut f = Filter::new().kind(kind(KIND_APP_HANDLER));

        // Only filter by "elisym" tag on the relay side — adding capabilities
        // here would use OR semantics (NIP-01), returning agents matching ANY tag.
        f = f.custom_tag(
            SingleLetterTag::lowercase(Alphabet::T),
            vec!["elisym".to_string()],
        );

        if let Some(job_kind) = filter.job_kind {
            f = f.custom_tag(
                SingleLetterTag::lowercase(Alphabet::K),
                vec![job_kind.to_string()],
            );
        }

        if let Some(since) = filter.since {
            f = f.since(since);
        }

        let events = self
            .client
            .fetch_events(vec![f], Some(Duration::from_secs(10)))
            .await?;

        let mut agents = Vec::new();
        let mut seen_pubkeys = HashSet::new();
        for event in events {
            // Dedup by pubkey — same agent's card may arrive from multiple relays
            if !seen_pubkeys.insert(event.pubkey) {
                continue;
            }
            match CapabilityCard::from_json(&event.content) {
                Ok(card) => {
                    // Single pass: collect "t" tags into a set and "k" tags into kinds
                    let mut event_tags: HashSet<&str> = HashSet::new();
                    let mut supported_kinds: Vec<u16> = Vec::new();
                    for tag in event.tags.iter() {
                        let s = tag.as_slice();
                        match s.first().map(|v| v.as_str()) {
                            Some("t") => {
                                if let Some(v) = s.get(1) {
                                    event_tags.insert(v.as_str());
                                }
                            }
                            Some("k") => {
                                if let Some(v) = s.get(1).and_then(|v| v.parse().ok()) {
                                    supported_kinds.push(v);
                                }
                            }
                            _ => {}
                        }
                    }

                    // Post-filter: agent must match ALL requested capabilities (AND semantics).
                    // Matching is fuzzy: both query terms and agent tags are split on
                    // delimiters ('-', '_', ' ') into tokens. A query capability matches
                    // if it equals a tag exactly OR any of its tokens appears in the
                    // tag's token set (e.g. query "youtube" matches tag "youtube-summarization").
                    if !filter.capabilities.is_empty() {
                        let has_all = filter.capabilities.iter().all(|cap| {
                            // Exact match first (fast path)
                            if event_tags.contains(cap.as_str()) {
                                return true;
                            }
                            // Fuzzy: split query into tokens, check if every token
                            // appears in at least one tag's tokens
                            let query_tokens: Vec<&str> =
                                cap.split(['-', '_', ' '])
                                    .filter(|t| !t.is_empty())
                                    .collect();
                            query_tokens.iter().all(|qt| {
                                let qt_lower = qt.to_lowercase();
                                event_tags.iter().any(|tag| {
                                    tag.split(['-', '_', ' '])
                                        .any(|tt| tt.to_lowercase() == qt_lower)
                                })
                            })
                        });

                        if !has_all {
                            continue;
                        }
                    }

                    agents.push(DiscoveredAgent {
                        pubkey: event.pubkey,
                        card,
                        event_id: event.id,
                        supported_kinds,
                    });
                }
                Err(e) => {
                    tracing::warn!(
                        event_id = %event.id,
                        error = %e,
                        "Failed to parse capability card, skipping"
                    );
                }
            }
        }

        if let Some(limit) = filter.limit {
            agents.truncate(limit);
        }

        Ok(agents)
    }
}
