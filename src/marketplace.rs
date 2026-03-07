use nostr_sdk::prelude::*;
use tokio::sync::mpsc;

use crate::dedup::BoundedDedup;
use crate::error::{ElisymError, Result};
use crate::identity::AgentIdentity;
use crate::types::{kind, job_request_kind, job_result_kind, JobStatus, KIND_JOB_FEEDBACK, KIND_JOB_REQUEST_BASE, KIND_JOB_RESULT_BASE};

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

/// Max number of event IDs to keep for deduplication in subscription handlers.
const DEDUP_CAPACITY: usize = 10_000;

/// A job request received or sent via NIP-90.
#[derive(Debug, Clone)]
pub struct JobRequest {
    pub event_id: EventId,
    pub customer: PublicKey,
    pub kind_offset: u16,
    pub input_data: String,
    pub input_type: String,
    pub output_mime: Option<String>,
    pub bid: Option<u64>,
    pub tags: Vec<String>,
    pub raw_event: Event,
}

/// A job result received or sent via NIP-90.
#[derive(Debug, Clone)]
pub struct JobResult {
    pub event_id: EventId,
    pub provider: PublicKey,
    pub request_id: EventId,
    pub content: String,
    pub amount: Option<u64>,
    pub raw_event: Event,
}

/// A job feedback event via NIP-90.
#[derive(Debug, Clone)]
pub struct JobFeedback {
    pub event_id: EventId,
    pub provider: PublicKey,
    pub request_id: EventId,
    pub status: String,
    pub extra_info: Option<String>,
    pub payment_request: Option<String>,
    pub payment_chain: Option<String>,
    pub raw_event: Event,
}

impl JobFeedback {
    /// Parse the status string into a typed `JobStatus`, if it matches a known value.
    pub fn parsed_status(&self) -> Option<crate::types::JobStatus> {
        match self.status.as_str() {
            "payment-required" => Some(crate::types::JobStatus::PaymentRequired),
            "processing" => Some(crate::types::JobStatus::Processing),
            "error" => Some(crate::types::JobStatus::Error),
            "success" => Some(crate::types::JobStatus::Success),
            "partial" => Some(crate::types::JobStatus::Partial),
            _ => None,
        }
    }
}

/// Service for NIP-90 Data Vending Machine job marketplace.
#[derive(Debug, Clone)]
pub struct MarketplaceService {
    client: Client,
    identity: AgentIdentity,
}

impl MarketplaceService {
    pub fn new(client: Client, identity: AgentIdentity) -> Self {
        Self { client, identity }
    }

    // ── Customer API ──

    /// Submit a job request (kind:5000+offset).
    #[allow(clippy::too_many_arguments)]
    pub async fn submit_job_request(
        &self,
        kind_offset: u16,
        input_data: &str,
        input_type: &str,
        output_mime: Option<&str>,
        bid: Option<u64>,
        provider: Option<&PublicKey>,
        extra_tags: Vec<String>,
    ) -> Result<EventId> {
        let k = job_request_kind(kind_offset).ok_or_else(|| {
            ElisymError::Config(format!("Invalid job request kind offset: {}", kind_offset))
        })?;

        let mut tags: Vec<Tag> = vec![
            Tag::parse(["i", input_data, input_type])?,
        ];

        if let Some(mime) = output_mime {
            tags.push(Tag::parse(["output", mime])?);
        }

        if let Some(val) = bid {
            let val_str = val.to_string();
            tags.push(Tag::parse(["bid", &val_str])?);
        }

        if let Some(pk) = provider {
            tags.push(Tag::public_key(*pk));
        }

        for tag in &extra_tags {
            tags.push(Tag::custom(
                TagKind::SingleLetter(SingleLetterTag::lowercase(Alphabet::T)),
                vec![tag.clone()],
            ));
        }

        // NIP-90: job input goes in the "i" tag, content is intentionally empty.
        let builder = EventBuilder::new(k, "").tags(tags);
        let output = self.client.send_event_builder(builder).await?;

        tracing::info!(event_id = %output.val, kind_offset, "Submitted job request");
        Ok(output.val)
    }

    /// Subscribe to job results for requests we've made.
    ///
    /// If `expected_providers` is non-empty, only results from those providers
    /// are forwarded. This prevents accepting results from unknown parties
    /// when jobs were sent to specific providers.
    pub async fn subscribe_to_results(
        &self,
        kind_offsets: &[u16],
        expected_providers: &[PublicKey],
    ) -> Result<mpsc::Receiver<JobResult>> {
        let (tx, rx) = mpsc::channel(256);

        let kinds: Vec<Kind> = kind_offsets
            .iter()
            .filter_map(|offset| job_result_kind(*offset))
            .collect();

        // Filter by #p tag — results are tagged with the customer's pubkey
        // Use `since` to avoid receiving stale events from previous runs
        let filter = Filter::new()
            .kinds(kinds)
            .custom_tag(
                SingleLetterTag::lowercase(Alphabet::P),
                vec![self.identity.public_key().to_hex()],
            )
            .since(Timestamp::now());

        self.client.subscribe(vec![filter], None).await?;

        let client = self.client.clone();
        let allowed: Vec<PublicKey> = expected_providers.to_vec();
        tokio::spawn(async move {
            let mut notifications = client.notifications();
            let mut seen = BoundedDedup::new(DEDUP_CAPACITY);
            while let Some(notification) = recv_notification(&mut notifications).await {
                if let RelayPoolNotification::Event { event, .. } = notification {
                    if !seen.insert(event.id) {
                        continue; // duplicate from another relay
                    }
                    let kind_num = event.kind.as_u16();
                    if (KIND_JOB_RESULT_BASE..KIND_JOB_FEEDBACK).contains(&kind_num) {
                        // Skip results from unexpected providers
                        if !allowed.is_empty() && !allowed.contains(&event.pubkey) {
                            tracing::warn!(
                                provider = %event.pubkey,
                                "Ignoring result from unexpected provider"
                            );
                            continue;
                        }
                        if let Some(result) = parse_job_result(&event) {
                            if tx.send(result).await.is_err() {
                                break;
                            }
                        }
                    }
                }
            }
            tracing::warn!("subscription task ended: results (notification channel closed)");
        });

        Ok(rx)
    }

    /// Subscribe to job feedback for requests we've made.
    pub async fn subscribe_to_feedback(&self) -> Result<mpsc::Receiver<JobFeedback>> {
        let (tx, rx) = mpsc::channel(256);

        // Filter by #p tag — feedback events are tagged with the customer's pubkey
        let filter = Filter::new()
            .kind(kind(KIND_JOB_FEEDBACK))
            .custom_tag(
                SingleLetterTag::lowercase(Alphabet::P),
                vec![self.identity.public_key().to_hex()],
            )
            .since(Timestamp::now());

        self.client.subscribe(vec![filter], None).await?;

        let client = self.client.clone();
        tokio::spawn(async move {
            let mut notifications = client.notifications();
            let mut seen = BoundedDedup::new(DEDUP_CAPACITY);
            while let Some(notification) = recv_notification(&mut notifications).await {
                if let RelayPoolNotification::Event { event, .. } = notification {
                    if !seen.insert(event.id) {
                        continue;
                    }
                    if event.kind.as_u16() == KIND_JOB_FEEDBACK {
                        if let Some(feedback) = parse_job_feedback(&event) {
                            if tx.send(feedback).await.is_err() {
                                break;
                            }
                        }
                    }
                }
            }
            tracing::warn!("subscription task ended: feedback (notification channel closed)");
        });

        Ok(rx)
    }

    // ── Provider API ──

    /// Subscribe to incoming job requests for the given kind offsets.
    ///
    /// Receives both directed requests (tagged with our pubkey) and broadcast
    /// requests (no `#p` tag). Requests directed at other providers are filtered out.
    ///
    /// Events that cannot be parsed (e.g., missing `["i", ...]` tag) are silently
    /// dropped — only well-formed NIP-90 job requests are forwarded to the receiver.
    pub async fn subscribe_to_job_requests(
        &self,
        kind_offsets: &[u16],
    ) -> Result<mpsc::Receiver<JobRequest>> {
        let (tx, rx) = mpsc::channel(256);

        let kinds: Vec<Kind> = kind_offsets
            .iter()
            .filter_map(|offset| job_request_kind(*offset))
            .collect();

        let own_pubkey = self.identity.public_key();

        // Two filters: one for jobs directed at us (#p = our pubkey),
        // one for all jobs of these kinds (to catch broadcasts).
        // Post-filter in the task discards jobs directed at other providers.
        let filter_directed = Filter::new()
            .kinds(kinds.clone())
            .custom_tag(
                SingleLetterTag::lowercase(Alphabet::P),
                vec![own_pubkey.to_hex()],
            )
            .since(Timestamp::now());

        let filter_broadcast = Filter::new()
            .kinds(kinds)
            .since(Timestamp::now());

        self.client
            .subscribe(vec![filter_directed, filter_broadcast], None)
            .await?;

        let client = self.client.clone();
        tokio::spawn(async move {
            let mut notifications = client.notifications();
            let mut seen = BoundedDedup::new(DEDUP_CAPACITY);
            while let Some(notification) = recv_notification(&mut notifications).await {
                if let RelayPoolNotification::Event { event, .. } = notification {
                    if !seen.insert(event.id) {
                        continue; // duplicate from broadcast + directed filters or multiple relays
                    }
                    let kind_num = event.kind.as_u16();
                    if (KIND_JOB_REQUEST_BASE..KIND_JOB_RESULT_BASE).contains(&kind_num) {
                        // Accept if: no #p tag (broadcast) or #p matches our pubkey.
                        // Reject if #p points to a different provider.
                        let p_tags: Vec<String> = event
                            .tags
                            .iter()
                            .filter_map(|tag| {
                                let s = tag.as_slice();
                                if s.first().map(|v| v.as_str()) == Some("p") {
                                    s.get(1).map(|v| v.to_string())
                                } else {
                                    None
                                }
                            })
                            .collect();

                        let dominated = !p_tags.is_empty()
                            && !p_tags.contains(&own_pubkey.to_hex());

                        if dominated {
                            continue;
                        }

                        if let Some(request) = parse_job_request(&event) {
                            if tx.send(request).await.is_err() {
                                break;
                            }
                        }
                    }
                }
            }
            tracing::warn!("subscription task ended: job_requests (notification channel closed)");
        });

        Ok(rx)
    }

    /// Submit a job result (kind:6000+offset).
    pub async fn submit_job_result(
        &self,
        request_event: &Event,
        content: &str,
        amount: Option<u64>,
    ) -> Result<EventId> {
        let kind_offset = request_event
            .kind
            .as_u16()
            .checked_sub(KIND_JOB_REQUEST_BASE)
            .ok_or_else(|| ElisymError::Config("Request event kind is below job request base".into()))?;
        let k = job_result_kind(kind_offset).ok_or_else(|| {
            ElisymError::Config(format!("Invalid job result kind offset: {}", kind_offset))
        })?;

        let mut tags = vec![
            Tag::event(request_event.id),
            Tag::public_key(request_event.pubkey),
        ];

        if let Some(val) = amount {
            let val_str = val.to_string();
            tags.push(Tag::parse(["amount", &val_str])?);
        }

        let request_json = serde_json::to_string(&request_event)?;
        tags.push(Tag::parse(["request", &request_json])?);

        let builder = EventBuilder::new(k, content).tags(tags);
        let output = self.client.send_event_builder(builder).await?;

        tracing::info!(event_id = %output.val, "Submitted job result");
        Ok(output.val)
    }

    /// Submit job feedback (kind:7000).
    ///
    /// When `status` is `PaymentRequired`, pass the payment amount in
    /// `amount` and the payment request string in `payment_request` to produce
    /// a correct `["amount", amount, request]` or `["amount", amount, request, chain]` tag per NIP-90.
    ///
    /// The optional `payment_chain` identifies the payment network (e.g., "lightning", "solana").
    /// If omitted, "lightning" is assumed for backward compatibility.
    pub async fn submit_job_feedback(
        &self,
        request_event: &Event,
        status: JobStatus,
        extra_info: Option<&str>,
        amount: Option<u64>,
        payment_request: Option<&str>,
        payment_chain: Option<&str>,
    ) -> Result<EventId> {
        let mut tags = vec![
            Tag::event(request_event.id),
            Tag::public_key(request_event.pubkey),
        ];

        let status_str = status.to_string();
        if let Some(info) = extra_info {
            tags.push(Tag::parse(["status", &status_str, info])?);
        } else {
            tags.push(Tag::parse(["status", &status_str])?);
        }

        if let Some(request) = payment_request {
            let amt = amount.ok_or_else(|| {
                ElisymError::Config("amount is required when payment_request is provided".into())
            })?;
            let amt_str = amt.to_string();
            if let Some(chain) = payment_chain {
                tags.push(Tag::parse(["amount", &amt_str, request, chain])?);
            } else {
                tags.push(Tag::parse(["amount", &amt_str, request])?);
            }
        }

        let builder = EventBuilder::new(kind(KIND_JOB_FEEDBACK), "").tags(tags);
        let output = self.client.send_event_builder(builder).await?;

        tracing::info!(event_id = %output.val, status = %status, "Submitted job feedback");
        Ok(output.val)
    }
}

// ── Parsing helpers ──

fn get_tag_value(event: &Event, tag_name: &str) -> Option<String> {
    event.tags.iter().find_map(|tag| {
        let s = tag.as_slice();
        if s.first().map(|v| v.as_str()) == Some(tag_name) {
            s.get(1).map(|v| v.to_string())
        } else {
            None
        }
    })
}

fn get_tag_values(event: &Event, tag_name: &str) -> Vec<String> {
    event
        .tags
        .iter()
        .filter_map(|tag| {
            let s = tag.as_slice();
            if s.first().map(|v| v.as_str()) == Some(tag_name) {
                s.get(1).map(|v| v.to_string())
            } else {
                None
            }
        })
        .collect()
}

/// Resolve the job-request EventId from a result or feedback event.
///
/// Prefers the "request" tag (contains the stringified original event) for
/// robustness when multiple "e" tags are present. Falls back to the first "e" tag.
fn resolve_request_id(event: &Event) -> Option<EventId> {
    // Try "request" tag first: it contains the full stringified request event
    if let Some(request_json) = get_tag_value(event, "request") {
        if let Ok(request_event) = serde_json::from_str::<Event>(&request_json) {
            return Some(request_event.id);
        }
    }

    // Fallback: first "e" tag
    let request_id_str = get_tag_value(event, "e")?;
    EventId::parse(&request_id_str).ok()
}

fn parse_job_request(event: &Event) -> Option<JobRequest> {
    let kind_offset = event.kind.as_u16().checked_sub(KIND_JOB_REQUEST_BASE)?;

    let (input_data, input_type) = event.tags.iter().find_map(|tag| {
        let s = tag.as_slice();
        if s.first().map(|v| v.as_str()) == Some("i") {
            Some((
                s.get(1).map(|v| v.to_string()).unwrap_or_default(),
                s.get(2)
                    .map(|v| v.to_string())
                    .unwrap_or_else(|| "text".to_string()),
            ))
        } else {
            None
        }
    })?;

    let bid = get_tag_value(event, "bid").and_then(|v| v.parse().ok());
    let output_mime = get_tag_value(event, "output");
    let tags = get_tag_values(event, "t");

    Some(JobRequest {
        event_id: event.id,
        customer: event.pubkey,
        kind_offset,
        input_data,
        input_type,
        output_mime,
        bid,
        tags,
        raw_event: event.clone(),
    })
}

fn parse_job_result(event: &Event) -> Option<JobResult> {
    // Determine the request ID robustly:
    // 1. If a "request" tag exists, parse the stringified event to extract its id
    // 2. Fall back to the first "e" tag
    let request_id = resolve_request_id(event)?;

    let amount = event.tags.iter().find_map(|tag| {
        let s = tag.as_slice();
        if s.first().map(|v| v.as_str()) == Some("amount") {
            s.get(1).and_then(|v| v.parse().ok())
        } else {
            None
        }
    });

    Some(JobResult {
        event_id: event.id,
        provider: event.pubkey,
        request_id,
        content: event.content.clone(),
        amount,
        raw_event: event.clone(),
    })
}

fn parse_job_feedback(event: &Event) -> Option<JobFeedback> {
    let request_id = resolve_request_id(event)?;

    let (status, extra_info) = event.tags.iter().find_map(|tag| {
        let s = tag.as_slice();
        if s.first().map(|v| v.as_str()) == Some("status") {
            Some((
                s.get(1).map(|v| v.to_string()).unwrap_or_default(),
                s.get(2).map(|v| v.to_string()),
            ))
        } else {
            None
        }
    })?;

    // Extract payment request and chain from ["amount", msat, request, chain?] tag
    let (payment_request, payment_chain) = event
        .tags
        .iter()
        .find_map(|tag| {
            let s = tag.as_slice();
            if s.first().map(|v| v.as_str()) == Some("amount") {
                let request = s.get(2).map(|v| v.to_string());
                let chain = s.get(3).map(|v| v.to_string());
                Some((request, chain))
            } else {
                None
            }
        })
        .unwrap_or((None, None));

    Some(JobFeedback {
        event_id: event.id,
        provider: event.pubkey,
        request_id,
        status,
        extra_info,
        payment_request,
        payment_chain,
        raw_event: event.clone(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Helper: build a signed Event with given kind, content, and tags.
    fn make_event(kind_num: u16, content: &str, tags: Vec<Tag>) -> Event {
        let keys = Keys::generate();
        EventBuilder::new(kind(kind_num), content)
            .tags(tags)
            .sign_with_keys(&keys)
            .unwrap()
    }

    /// Helper: build a signed Event with a specific keypair.
    fn make_event_with_keys(keys: &Keys, kind_num: u16, content: &str, tags: Vec<Tag>) -> Event {
        EventBuilder::new(kind(kind_num), content)
            .tags(tags)
            .sign_with_keys(keys)
            .unwrap()
    }

    // ── get_tag_value / get_tag_values ──

    #[test]
    fn test_get_tag_value_found() {
        let event = make_event(1, "", vec![
            Tag::parse(["i", "hello", "text"]).unwrap(),
            Tag::parse(["bid", "1000"]).unwrap(),
        ]);
        assert_eq!(get_tag_value(&event, "bid"), Some("1000".to_string()));
    }

    #[test]
    fn test_get_tag_value_missing() {
        let event = make_event(1, "", vec![
            Tag::parse(["i", "hello", "text"]).unwrap(),
        ]);
        assert_eq!(get_tag_value(&event, "bid"), None);
    }

    #[test]
    fn test_get_tag_values_multiple() {
        let event = make_event(1, "", vec![
            Tag::custom(TagKind::SingleLetter(SingleLetterTag::lowercase(Alphabet::T)), vec!["summarization".to_string()]),
            Tag::custom(TagKind::SingleLetter(SingleLetterTag::lowercase(Alphabet::T)), vec!["translation".to_string()]),
        ]);
        let vals = get_tag_values(&event, "t");
        assert_eq!(vals, vec!["summarization", "translation"]);
    }

    // ── parse_job_request ──

    #[test]
    fn test_parse_job_request_valid() {
        let event = make_event(5100, "", vec![
            Tag::parse(["i", "Summarize this text", "text"]).unwrap(),
            Tag::parse(["output", "text/plain"]).unwrap(),
            Tag::parse(["bid", "1000000"]).unwrap(),
            Tag::custom(TagKind::SingleLetter(SingleLetterTag::lowercase(Alphabet::T)), vec!["summarization".to_string()]),
        ]);
        let req = parse_job_request(&event).expect("should parse");
        assert_eq!(req.input_data, "Summarize this text");
        assert_eq!(req.input_type, "text");
        assert_eq!(req.output_mime.as_deref(), Some("text/plain"));
        assert_eq!(req.bid, Some(1_000_000));
        assert_eq!(req.kind_offset, 100);
        assert_eq!(req.tags, vec!["summarization"]);
        assert_eq!(req.customer, event.pubkey);
    }

    #[test]
    fn test_parse_job_request_minimal() {
        // Only "i" tag, no bid/output/t tags
        let event = make_event(5100, "", vec![
            Tag::parse(["i", "data", "url"]).unwrap(),
        ]);
        let req = parse_job_request(&event).expect("should parse");
        assert_eq!(req.input_data, "data");
        assert_eq!(req.input_type, "url");
        assert_eq!(req.output_mime, None);
        assert_eq!(req.bid, None);
        assert!(req.tags.is_empty());
    }

    #[test]
    fn test_parse_job_request_missing_i_tag() {
        // No "i" tag → should return None
        let event = make_event(5100, "", vec![
            Tag::parse(["bid", "1000"]).unwrap(),
        ]);
        assert!(parse_job_request(&event).is_none());
    }

    #[test]
    fn test_parse_job_request_wrong_kind() {
        // kind:4999 is below 5000 → checked_sub underflows → None
        let event = make_event(4999, "", vec![
            Tag::parse(["i", "data", "text"]).unwrap(),
        ]);
        assert!(parse_job_request(&event).is_none());
    }

    #[test]
    fn test_parse_job_request_i_tag_missing_type_defaults_to_text() {
        // "i" tag with only one value → input_type defaults to "text"
        let event = make_event(5100, "", vec![
            Tag::parse(["i", "some data"]).unwrap(),
        ]);
        let req = parse_job_request(&event).expect("should parse");
        assert_eq!(req.input_data, "some data");
        assert_eq!(req.input_type, "text");
    }

    #[test]
    fn test_parse_job_request_invalid_bid_ignored() {
        // Non-numeric bid → parsed as None
        let event = make_event(5100, "", vec![
            Tag::parse(["i", "data", "text"]).unwrap(),
            Tag::parse(["bid", "not-a-number"]).unwrap(),
        ]);
        let req = parse_job_request(&event).expect("should parse");
        assert_eq!(req.bid, None);
    }

    // ── parse_job_result ──

    #[test]
    fn test_parse_job_result_with_e_tag() {
        let request_event = make_event(5100, "", vec![
            Tag::parse(["i", "data", "text"]).unwrap(),
        ]);
        let result_event = make_event(6100, "Summary: this is a summary", vec![
            Tag::event(request_event.id),
            Tag::parse(["amount", "1000000"]).unwrap(),
        ]);
        let result = parse_job_result(&result_event).expect("should parse");
        assert_eq!(result.request_id, request_event.id);
        assert_eq!(result.content, "Summary: this is a summary");
        assert_eq!(result.amount, Some(1_000_000));
        assert_eq!(result.provider, result_event.pubkey);
    }

    #[test]
    fn test_parse_job_result_with_request_tag() {
        let request_event = make_event(5100, "", vec![
            Tag::parse(["i", "data", "text"]).unwrap(),
        ]);
        let request_json = serde_json::to_string(&request_event).unwrap();
        let result_event = make_event(6100, "result content", vec![
            Tag::event(request_event.id),
            Tag::parse(["request", &request_json]).unwrap(),
            Tag::parse(["amount", "500000"]).unwrap(),
        ]);
        let result = parse_job_result(&result_event).expect("should parse");
        // Should prefer "request" tag for request_id
        assert_eq!(result.request_id, request_event.id);
        assert_eq!(result.amount, Some(500_000));
    }

    #[test]
    fn test_parse_job_result_no_e_tag() {
        // No "e" tag and no "request" tag → None
        let event = make_event(6100, "content", vec![
            Tag::parse(["amount", "1000"]).unwrap(),
        ]);
        assert!(parse_job_result(&event).is_none());
    }

    #[test]
    fn test_parse_job_result_no_amount() {
        let request_event = make_event(5100, "", vec![
            Tag::parse(["i", "data", "text"]).unwrap(),
        ]);
        let result_event = make_event(6100, "free result", vec![
            Tag::event(request_event.id),
        ]);
        let result = parse_job_result(&result_event).expect("should parse");
        assert_eq!(result.amount, None);
        assert_eq!(result.content, "free result");
    }

    // ── parse_job_feedback ──

    #[test]
    fn test_parse_job_feedback_payment_required() {
        let request_event = make_event(5100, "", vec![
            Tag::parse(["i", "data", "text"]).unwrap(),
        ]);
        let feedback_event = make_event(7000, "", vec![
            Tag::event(request_event.id),
            Tag::parse(["status", "payment-required"]).unwrap(),
            Tag::parse(["amount", "1000000", "lnbc10u1..."]).unwrap(),
        ]);
        let fb = parse_job_feedback(&feedback_event).expect("should parse");
        assert_eq!(fb.request_id, request_event.id);
        assert_eq!(fb.status, "payment-required");
        assert_eq!(fb.extra_info, None);
        assert_eq!(fb.payment_request.as_deref(), Some("lnbc10u1..."));
        assert_eq!(fb.parsed_status(), Some(JobStatus::PaymentRequired));
    }

    #[test]
    fn test_parse_job_feedback_error_with_extra_info() {
        let request_event = make_event(5100, "", vec![
            Tag::parse(["i", "data", "text"]).unwrap(),
        ]);
        let feedback_event = make_event(7000, "", vec![
            Tag::event(request_event.id),
            Tag::parse(["status", "error", "payment-timeout"]).unwrap(),
        ]);
        let fb = parse_job_feedback(&feedback_event).expect("should parse");
        assert_eq!(fb.status, "error");
        assert_eq!(fb.extra_info.as_deref(), Some("payment-timeout"));
        assert_eq!(fb.payment_request, None);
        assert_eq!(fb.parsed_status(), Some(JobStatus::Error));
    }

    #[test]
    fn test_parse_job_feedback_processing() {
        let request_event = make_event(5100, "", vec![
            Tag::parse(["i", "data", "text"]).unwrap(),
        ]);
        let feedback_event = make_event(7000, "", vec![
            Tag::event(request_event.id),
            Tag::parse(["status", "processing"]).unwrap(),
        ]);
        let fb = parse_job_feedback(&feedback_event).expect("should parse");
        assert_eq!(fb.parsed_status(), Some(JobStatus::Processing));
    }

    #[test]
    fn test_parse_job_feedback_no_status_tag() {
        let request_event = make_event(5100, "", vec![
            Tag::parse(["i", "data", "text"]).unwrap(),
        ]);
        let feedback_event = make_event(7000, "", vec![
            Tag::event(request_event.id),
            // no "status" tag
        ]);
        assert!(parse_job_feedback(&feedback_event).is_none());
    }

    #[test]
    fn test_parse_job_feedback_no_e_tag() {
        // No reference to request → None
        let event = make_event(7000, "", vec![
            Tag::parse(["status", "processing"]).unwrap(),
        ]);
        assert!(parse_job_feedback(&event).is_none());
    }

    #[test]
    fn test_parse_job_feedback_unknown_status() {
        let request_event = make_event(5100, "", vec![
            Tag::parse(["i", "data", "text"]).unwrap(),
        ]);
        let feedback_event = make_event(7000, "", vec![
            Tag::event(request_event.id),
            Tag::parse(["status", "custom-status"]).unwrap(),
        ]);
        let fb = parse_job_feedback(&feedback_event).expect("should parse");
        assert_eq!(fb.status, "custom-status");
        assert_eq!(fb.parsed_status(), None); // unknown → None
    }

    // ── resolve_request_id ──

    #[test]
    fn test_resolve_request_id_prefers_request_tag() {
        let request_event = make_event(5100, "", vec![
            Tag::parse(["i", "data", "text"]).unwrap(),
        ]);
        let request_json = serde_json::to_string(&request_event).unwrap();
        // Put a different "e" tag to verify "request" tag is preferred
        let dummy_keys = Keys::generate();
        let dummy_event = make_event_with_keys(&dummy_keys, 5100, "", vec![
            Tag::parse(["i", "other", "text"]).unwrap(),
        ]);
        let event = make_event(6100, "", vec![
            Tag::event(dummy_event.id),
            Tag::parse(["request", &request_json]).unwrap(),
        ]);
        let resolved = resolve_request_id(&event);
        assert_eq!(resolved, Some(request_event.id));
    }

    #[test]
    fn test_resolve_request_id_falls_back_to_e_tag() {
        let request_event = make_event(5100, "", vec![
            Tag::parse(["i", "data", "text"]).unwrap(),
        ]);
        let event = make_event(6100, "", vec![
            Tag::event(request_event.id),
        ]);
        let resolved = resolve_request_id(&event);
        assert_eq!(resolved, Some(request_event.id));
    }

    // ── job_request_kind / job_result_kind ──

    #[test]
    fn test_job_kind_helpers() {
        use crate::types::{job_request_kind, job_result_kind};

        assert_eq!(job_request_kind(100).unwrap().as_u16(), 5100);
        assert_eq!(job_result_kind(100).unwrap().as_u16(), 6100);
        assert_eq!(job_request_kind(0).unwrap().as_u16(), 5000);
        // Offset 999 → kind 5999, still valid (< 6000)
        assert_eq!(job_request_kind(999).unwrap().as_u16(), 5999);
        // Offset 1000 → kind 6000 → invalid (>= KIND_JOB_RESULT_BASE)
        assert!(job_request_kind(1000).is_none());
    }
}
