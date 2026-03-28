use std::collections::{HashMap, HashSet};
use std::collections::hash_map::Entry;
use std::time::Duration;

use nostr_sdk::prelude::*;
use serde::{Deserialize, Serialize};

use crate::error::Result;
use crate::identity::{AgentIdentity, CapabilityCard};
use crate::types::{kind, KIND_APP_HANDLER};

/// Convert a capability name to its Nostr d-tag form (lowercase, spaces → hyphens).
pub fn to_d_tag(name: &str) -> String {
    name.to_lowercase()
        .split_whitespace()
        .collect::<Vec<_>>()
        .join("-")
}

/// A discovered agent with its capability cards and event metadata.
#[derive(Debug, Clone)]
pub struct DiscoveredAgent {
    pub pubkey: PublicKey,
    pub cards: Vec<CapabilityCard>,
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
    /// Filter by a specific agent's public key. When set, the relay query
    /// is scoped to events authored by this key (via `Filter::authors`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub pubkey: Option<PublicKey>,
}

/// Minimum token length for fuzzy prefix matching (both direct and reverse).
/// Tokens shorter than this only match via exact equality.
const MIN_FUZZY_PREFIX_LEN: usize = 4;

/// Check if two lowercase tokens fuzzy-match: exact equality, or one is a prefix
/// of the other (both tokens must be at least [`MIN_FUZZY_PREFIX_LEN`] chars).
fn tokens_match(a: &str, b: &str) -> bool {
    a == b
        || (a.len() >= MIN_FUZZY_PREFIX_LEN && b.starts_with(a))
        || (b.len() >= MIN_FUZZY_PREFIX_LEN && a.starts_with(b))
}

/// Check if a single query capability matches any of the event's tags.
///
/// Matching is fuzzy: both the query and tags are split on delimiters (`-`, `_`, ` `)
/// into tokens. A query token matches a tag token if they are equal or one is a prefix
/// of the other (min [`MIN_FUZZY_PREFIX_LEN`] chars).
fn matches_capability(query_cap: &str, event_tags: &HashSet<&str>) -> bool {
    // Exact match first (fast path)
    if event_tags.contains(query_cap) {
        return true;
    }
    // Fuzzy: split query into tokens, check if every token
    // appears in at least one tag's tokens
    let query_tokens: Vec<&str> = query_cap
        .split(['-', '_', ' '])
        .filter(|t| !t.is_empty())
        .collect();
    query_tokens.iter().all(|qt| {
        let qt_lower = qt.to_lowercase();
        event_tags.iter().any(|tag| {
            tag.split(['-', '_', ' ']).any(|tt| {
                tokens_match(&qt_lower, &tt.to_lowercase())
            })
        })
    })
}

/// Check if a query capability fuzzy-matches tokens in a free-text field (name or description).
/// Uses the same tokenization and prefix logic as `matches_capability`.
fn matches_capability_in_text(query_cap: &str, text: &str) -> bool {
    let query_tokens: Vec<&str> = query_cap
        .split(['-', '_', ' '])
        .filter(|t| !t.is_empty())
        .collect();
    if query_tokens.is_empty() {
        return false;
    }
    query_tokens.iter().all(|qt| {
        let qt_lower = qt.to_lowercase();
        text.split(|c: char| !c.is_alphanumeric())
            .filter(|t| !t.is_empty())
            .any(|tt| tokens_match(&qt_lower, &tt.to_lowercase()))
    })
}

/// Check if a free-text query matches a capability card (name, description, or capabilities).
fn matches_query(query: &str, card: &CapabilityCard) -> bool {
    let q = query.to_lowercase();
    let name_lower = card.name.to_lowercase();
    let desc_lower = card.description.to_lowercase();
    name_lower.contains(&q)
        || desc_lower.contains(&q)
        || card
            .capabilities
            .iter()
            .any(|c| c.to_lowercase().contains(&q))
}

/// Handle returned by [`DiscoveryService::start_heartbeat`].
///
/// Use `.stop()` for graceful shutdown (waits for the current tick to finish)
/// or `.abort()` for immediate cancellation.
pub struct HeartbeatHandle {
    shutdown_tx: tokio::sync::oneshot::Sender<()>,
    join_handle: tokio::task::JoinHandle<()>,
}

impl HeartbeatHandle {
    /// Signal the heartbeat to stop and wait for the task to finish.
    pub async fn stop(self) {
        let _ = self.shutdown_tx.send(());
        let _ = self.join_handle.await;
    }

    /// Immediately cancel the heartbeat task.
    pub fn abort(self) {
        self.join_handle.abort();
    }
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
        if card.payment.is_none() {
            return Err(crate::error::ElisymError::InvalidCapabilityCard(
                "payment info is required to publish a capability card".into(),
            ));
        }
        let content = card.to_json()?;
        let d_tag = to_d_tag(&card.name);

        let mut tags: Vec<Tag> = vec![
            Tag::identifier(d_tag),
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

    /// Update the Nostr kind:0 profile metadata from the capability card.
    /// Call once at agent startup to set name, description, picture, and website.
    pub async fn update_profile(&self, card: &CapabilityCard) -> Result<EventId> {
        let pubkey_hex = self.identity.public_key().to_hex();
        let picture_url = format!("https://robohash.org/{pubkey_hex}");
        let about = format!("{} | Powered by Elisym", card.description);
        let metadata = Metadata::new()
            .name(&card.name)
            .about(about)
            .picture(Url::parse(&picture_url).expect("valid robohash URL"))
            .website(Url::parse("https://elisym.network").expect("valid elisym URL"));
        let output = self.client.set_metadata(&metadata).await?;
        tracing::info!(event_id = %output.val, "Updated Nostr profile (kind:0)");
        Ok(output.val)
    }

    /// Start a background heartbeat that republishes the capability card at `interval`
    /// to keep `created_at` fresh on relays (NIP-89 replaceable event).
    ///
    /// When `skip_first_tick` is `true`, the first publish happens after `interval`
    /// (use this when `publish_capability` was already called before starting the
    /// heartbeat). When `false`, the card is published immediately on start.
    ///
    /// Returns a [`HeartbeatHandle`] — call `.stop()` for graceful shutdown or
    /// `.abort()` for immediate cancellation.
    pub fn start_heartbeat(
        &self,
        card: CapabilityCard,
        supported_job_kinds: Vec<u16>,
        interval: Duration,
        skip_first_tick: bool,
    ) -> HeartbeatHandle {
        let discovery = self.clone();
        let (shutdown_tx, mut shutdown_rx) = tokio::sync::oneshot::channel();
        let join_handle = tokio::spawn(async move {
            let mut ticker = tokio::time::interval(interval);
            if skip_first_tick {
                ticker.tick().await; // consume the immediate first tick
            }
            loop {
                tokio::select! {
                    _ = ticker.tick() => {
                        match discovery
                            .publish_capability(&card, &supported_job_kinds)
                            .await
                        {
                            Ok(id) => {
                                tracing::debug!(event_id = %id, "Heartbeat: republished capability card");
                            }
                            Err(e) => {
                                tracing::warn!(error = %e, "Heartbeat: failed to republish capability card");
                            }
                        }
                    }
                    _ = &mut shutdown_rx => break,
                }
            }
        });
        HeartbeatHandle { shutdown_tx, join_handle }
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

        if let Some(pubkey) = filter.pubkey {
            f = f.author(pubkey);
        }

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

        // Dedup by (pubkey, d-tag) — same card may arrive from multiple relays.
        // Keep only the newest event per (pubkey, d-tag) pair.
        let mut latest_by_key: HashMap<(PublicKey, String), Event> = HashMap::new();
        for event in events {
            let d_tag = event
                .tags
                .iter()
                .find_map(|t| {
                    let s = t.as_slice();
                    if s.first().map(|v| v.as_str()) == Some("d") {
                        s.get(1).map(|v| v.to_string())
                    } else {
                        None
                    }
                })
                .unwrap_or_default();
            let key = (event.pubkey, d_tag);
            match latest_by_key.entry(key) {
                Entry::Vacant(e) => { e.insert(event); }
                Entry::Occupied(mut e) => {
                    if event.created_at > e.get().created_at {
                        e.insert(event);
                    }
                }
            }
        }

        // Group parsed cards and tags by pubkey, then compute match_count once
        // over the union of all capability tags per agent.
        struct CardEntry {
            card: CapabilityCard,
            created_at: Timestamp,
            tags: Vec<String>,
            kinds: Vec<u16>,
        }

        struct AgentAccum {
            entries: Vec<CardEntry>,
            event_id: EventId,
            latest_created_at: Timestamp,
        }

        let mut agent_map: HashMap<PublicKey, AgentAccum> = HashMap::new();

        for event in latest_by_key.into_values() {
            match CapabilityCard::from_json(&event.content) {
                Ok(card) => {
                    let mut event_tags = Vec::new();
                    let mut supported_kinds: Vec<u16> = Vec::new();
                    for tag in event.tags.iter() {
                        let s = tag.as_slice();
                        match s.first().map(|v| v.as_str()) {
                            Some("t") => {
                                if let Some(v) = s.get(1) {
                                    event_tags.push(v.to_string());
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

                    let created_at = event.created_at;
                    let entry = CardEntry {
                        card,
                        created_at,
                        tags: event_tags,
                        kinds: supported_kinds,
                    };

                    match agent_map.entry(event.pubkey) {
                        Entry::Occupied(mut e) => {
                            let acc = e.get_mut();
                            // Deduplicate by card name — keep the newer version
                            if let Some(pos) = acc.entries.iter().position(|e| e.card.name == entry.card.name) {
                                if created_at > acc.entries[pos].created_at {
                                    acc.entries[pos] = entry;
                                }
                            } else {
                                acc.entries.push(entry);
                            }
                            if created_at > acc.latest_created_at {
                                acc.event_id = event.id;
                                acc.latest_created_at = created_at;
                            }
                        }
                        Entry::Vacant(e) => {
                            e.insert(AgentAccum {
                                entries: vec![entry],
                                event_id: event.id,
                                latest_created_at: created_at,
                            });
                        }
                    }
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

        // Recompute supported_kinds from surviving card entries,
        // then apply filters per-agent.
        let mut agents: Vec<DiscoveredAgent> = Vec::new();
        for (pubkey, mut acc) in agent_map {
            // Post-filter: free-text query against any of the agent's cards.
            if let Some(ref query) = filter.query {
                if !acc.entries.iter().any(|e| matches_query(query, &e.card)) {
                    continue;
                }
            }

            // Collect supported kinds from the final (deduplicated) entries.
            let mut supported_kinds: Vec<u16> = Vec::new();
            for e in &acc.entries {
                for &k in &e.kinds {
                    if !supported_kinds.contains(&k) {
                        supported_kinds.push(k);
                    }
                }
            }

            let match_count = if filter.capabilities.is_empty() {
                0
            } else {
                // Per-card matching: for each card, count how many filter capabilities
                // match via its tags OR its name/description. Use the best card's count.
                let best = acc.entries.iter().map(|e| {
                    let entry_tags: HashSet<&str> = e.tags.iter().map(|s| s.as_str()).collect();
                    filter.capabilities.iter()
                        .filter(|cap| {
                            matches_capability(cap, &entry_tags)
                                || matches_capability_in_text(cap, &e.card.name)
                                || matches_capability_in_text(cap, &e.card.description)
                        })
                        .count()
                }).max().unwrap_or(0);
                if best == 0 {
                    continue;
                }
                best
            };

            // Sort cards by created_at descending (newest first) for deterministic order.
            acc.entries.sort_by(|a, b| b.created_at.cmp(&a.created_at));
            let cards = acc.entries.into_iter().map(|e| e.card).collect();

            agents.push(DiscoveredAgent {
                pubkey,
                cards,
                event_id: acc.event_id,
                supported_kinds,
                match_count,
            });
        }

        if !filter.capabilities.is_empty() {
            agents.sort_by(|a, b| b.match_count.cmp(&a.match_count));
        }

        if let Some(limit) = filter.limit {
            agents.truncate(limit);
        }

        Ok(agents)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── to_d_tag ──

    #[test]
    fn test_d_tag_basic() {
        assert_eq!(to_d_tag("My Agent"), "my-agent");
    }

    #[test]
    fn test_d_tag_multiple_spaces() {
        assert_eq!(to_d_tag("Stock  Price   Analyzer"), "stock-price-analyzer");
    }

    #[test]
    fn test_d_tag_already_lowercase() {
        assert_eq!(to_d_tag("summarizer"), "summarizer");
    }

    #[test]
    fn test_d_tag_leading_trailing_spaces() {
        assert_eq!(to_d_tag("  hello world  "), "hello-world");
    }

    #[test]
    fn test_d_tag_empty() {
        assert_eq!(to_d_tag(""), "");
    }

    // ── matches_capability ──

    #[test]
    fn test_exact_match() {
        let tags: HashSet<&str> = ["translation", "elisym"].into();
        assert!(matches_capability("translation", &tags));
    }

    #[test]
    fn test_no_match() {
        let tags: HashSet<&str> = ["summarization"].into();
        assert!(!matches_capability("translation", &tags));
    }

    #[test]
    fn test_fuzzy_prefix_query_shorter() {
        // "stock" is a prefix of "stocks" (len >= 3)
        let tags: HashSet<&str> = ["stocks"].into();
        assert!(matches_capability("stock", &tags));
    }

    #[test]
    fn test_fuzzy_prefix_tag_shorter() {
        // "summarization" starts with "summar" (tag token len >= 3)
        let tags: HashSet<&str> = ["summar"].into();
        assert!(matches_capability("summarization", &tags));
    }

    #[test]
    fn test_compound_tag_token_split() {
        // "text-summarization" splits into ["text", "summarization"]
        // Both must match some tag token
        let tags: HashSet<&str> = ["text", "summarization"].into();
        assert!(matches_capability("text-summarization", &tags));
    }

    #[test]
    fn test_case_insensitive() {
        let tags: HashSet<&str> = ["translation"].into();
        assert!(matches_capability("Translation", &tags));
    }

    #[test]
    fn test_short_token_no_direct_prefix() {
        // "ai" (2 chars < 4) won't fuzzy-prefix-match "aim"
        let tags: HashSet<&str> = ["aim"].into();
        assert!(!matches_capability("ai", &tags));
    }

    #[test]
    fn test_short_token_no_reverse_prefix() {
        // "cat" (3 chars < 4) in tags won't reverse-prefix-match "category" query
        let tags: HashSet<&str> = ["cat"].into();
        assert!(!matches_capability("category", &tags));
    }

    #[test]
    fn test_direct_prefix_at_threshold() {
        // "summ" (4 chars >= 4) direct-prefix-matches "summarization"
        let tags: HashSet<&str> = ["summarization"].into();
        assert!(matches_capability("summ", &tags));
    }

    #[test]
    fn test_reverse_prefix_at_threshold() {
        // "stock" (5 chars >= 4) in tags reverse-prefix-matches "stocks"
        let tags: HashSet<&str> = ["stock"].into();
        assert!(matches_capability("stocks", &tags));
    }

    #[test]
    fn test_compound_all_tokens_must_match() {
        // "stock-analysis" requires both "stock" and "analysis"
        let tags: HashSet<&str> = ["stocks"].into();
        assert!(!matches_capability("stock-analysis", &tags));
    }

    #[test]
    fn test_empty_tags_no_match() {
        let tags: HashSet<&str> = HashSet::new();
        assert!(!matches_capability("translation", &tags));
    }

    // ── matches_capability_in_text ──

    #[test]
    fn test_capability_in_text_name_match() {
        assert!(matches_capability_in_text("stock", "Stock Price Analyzer"));
    }

    #[test]
    fn test_capability_in_text_compound_match() {
        assert!(matches_capability_in_text(
            "stock-analysis",
            "Performs stock market analysis"
        ));
    }

    #[test]
    fn test_capability_in_text_no_match() {
        assert!(!matches_capability_in_text("translation", "Stock Price Analyzer"));
    }

    #[test]
    fn test_capability_in_text_prefix_match() {
        assert!(matches_capability_in_text("summar", "Text Summarization Service"));
    }

    #[test]
    fn test_capability_in_text_short_token_no_direct_prefix() {
        // "ai" (2 chars < 4) won't fuzzy-prefix-match "aim"
        assert!(!matches_capability_in_text("ai", "aim helper"));
    }

    #[test]
    fn test_capability_in_text_no_reverse_prefix_short() {
        // "cat" (3 chars < 4) in text won't reverse-prefix-match "category" query
        assert!(!matches_capability_in_text("category", "cat image generator"));
    }

    #[test]
    fn test_capability_in_text_direct_prefix_at_threshold() {
        // "summ" (4 chars >= 4) direct-prefix-matches "summarization"
        assert!(matches_capability_in_text("summ", "summarization service"));
    }

    #[test]
    fn test_capability_in_text_reverse_prefix_at_threshold() {
        // "stock" (5 chars >= 4) in text reverse-prefix-matches "stocks" query
        assert!(matches_capability_in_text("stocks", "stock price analyzer"));
    }

    #[test]
    fn test_capability_in_text_empty_query() {
        assert!(!matches_capability_in_text("", "Some text"));
    }

    // ── matches_query ──

    #[test]
    fn test_query_matches_name() {
        let card = CapabilityCard::new("Stock Analyzer", "Analyzes things", vec![]);
        assert!(matches_query("stock", &card));
    }

    #[test]
    fn test_query_matches_description() {
        let card = CapabilityCard::new("Agent", "Translates text between languages", vec![]);
        assert!(matches_query("translates", &card));
    }

    #[test]
    fn test_query_matches_capability() {
        let card = CapabilityCard::new("Agent", "Does stuff", vec!["summarization".into()]);
        assert!(matches_query("summar", &card));
    }

    #[test]
    fn test_query_no_match() {
        let card = CapabilityCard::new("Agent", "Does stuff", vec!["coding".into()]);
        assert!(!matches_query("translation", &card));
    }

    // ── search filter integration (simulates search_agents filtering logic) ──

    /// Helper that replicates the combined tag + text matching from search_agents.
    fn search_filter_matches(cap: &str, tags: &HashSet<&str>, card: &CapabilityCard) -> bool {
        matches_capability(cap, tags)
            || matches_capability_in_text(cap, &card.name)
            || matches_capability_in_text(cap, &card.description)
    }

    #[test]
    fn test_search_filter_text_fallback_name() {
        // Tags don't contain "stock", but card name does.
        let tags: HashSet<&str> = ["elisym", "finance"].into();
        let card = CapabilityCard::new(
            "Stock Price Analyzer",
            "Provides market data",
            vec!["finance".into()],
        );
        assert!(search_filter_matches("stock", &tags, &card));
    }

    #[test]
    fn test_search_filter_text_fallback_description() {
        // Tags don't match, but description does.
        let tags: HashSet<&str> = ["elisym"].into();
        let card = CapabilityCard::new(
            "Market Agent",
            "Performs deep analysis of market trends",
            vec![],
        );
        assert!(search_filter_matches("analysis", &tags, &card));
    }

    #[test]
    fn test_search_filter_no_match_anywhere() {
        let tags: HashSet<&str> = ["elisym", "finance"].into();
        let card = CapabilityCard::new(
            "Stock Market Agent",
            "Performs deep analysis of market trends",
            vec!["finance".into()],
        );
        assert!(!search_filter_matches("translation", &tags, &card));
    }

    #[test]
    fn test_search_filter_tag_match_sufficient() {
        // Tag match alone is enough — text content is irrelevant.
        let tags: HashSet<&str> = ["elisym", "stocks"].into();
        let card = CapabilityCard::new("Unrelated Agent", "Does other things", vec![]);
        assert!(search_filter_matches("stock", &tags, &card));
    }
}
