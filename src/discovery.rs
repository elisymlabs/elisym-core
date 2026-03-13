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
    /// Number of requested capabilities that matched (for relevance sorting).
    pub match_count: usize,
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
    /// Free-text query to match against agent name and description (case-insensitive substring).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub query: Option<String>,
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
    /// Also updates the kind:0 profile metadata (name, description) to stay in sync.
    pub async fn publish_capability(
        &self,
        card: &CapabilityCard,
        supported_job_kinds: &[u16],
    ) -> Result<EventId> {
        let content = card.to_json()?;
        let pubkey_hex = self.identity.public_key().to_hex();

        let mut tags: Vec<Tag> = vec![
            Tag::identifier(pubkey_hex.clone()),
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

        // Update kind:0 profile so name/description stay in sync with the capability card
        let picture_url = format!("https://robohash.org/{pubkey_hex}");
        let metadata = Metadata::new()
            .name(&card.name)
            .about(&card.description)
            .picture(Url::parse(&picture_url).expect("valid robohash URL"));
        match self.client.set_metadata(&metadata).await {
            Ok(meta_output) => {
                tracing::info!(event_id = %meta_output.val, "Updated Nostr profile (kind:0)");
            }
            Err(e) => {
                tracing::warn!(error = %e, "Failed to update kind:0 profile, continuing");
            }
        }

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

                    // Post-filter: count how many requested capabilities match (OR with ranking).
                    // Agents matching at least 1 capability are included; more matches = higher rank.
                    // Matching is fuzzy: both query terms and agent tags are split on
                    // delimiters ('-', '_', ' ') into tokens. A query token matches a
                    // tag token if they are equal OR one is a prefix of the other
                    // (min 3 chars), so "stock" matches "stocks", "summarize" matches
                    // "summarization", etc.
                    let match_count = if filter.capabilities.is_empty() {
                        0
                    } else {
                        let count = filter.capabilities.iter().filter(|cap| {
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
                                        .any(|tt| {
                                            let tt_lower = tt.to_lowercase();
                                            tt_lower == qt_lower
                                                || (qt_lower.len() >= 3 && tt_lower.starts_with(&qt_lower))
                                                || (tt_lower.len() >= 3 && qt_lower.starts_with(&tt_lower))
                                        })
                                })
                            })
                        }).count();

                        if count == 0 {
                            continue;
                        }
                        count
                    };

                    // Post-filter: free-text query against name and description.
                    if let Some(ref query) = filter.query {
                        let q = query.to_lowercase();
                        let name_lower = card.name.to_lowercase();
                        let desc_lower = card.description.to_lowercase();
                        let caps_lower: Vec<String> = card.capabilities.iter().map(|c| c.to_lowercase()).collect();
                        let matches = name_lower.contains(&q)
                            || desc_lower.contains(&q)
                            || caps_lower.iter().any(|c| c.contains(&q));
                        if !matches {
                            continue;
                        }
                    }

                    agents.push(DiscoveredAgent {
                        pubkey: event.pubkey,
                        card,
                        event_id: event.id,
                        supported_kinds,
                        match_count,
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

        // Sort by relevance: more capability matches first
        if !filter.capabilities.is_empty() {
            agents.sort_by(|a, b| b.match_count.cmp(&a.match_count));
        }

        if let Some(limit) = filter.limit {
            agents.truncate(limit);
        }

        Ok(agents)
    }
}
