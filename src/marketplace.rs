use nostr_sdk::prelude::*;
use nostr::nips::nip44;
use tokio::sync::mpsc;

use crate::Subscription;
use crate::dedup::{BoundedDedup, recv_notification, DEDUP_CAPACITY};
use crate::error::{ElisymError, Result};
use crate::identity::AgentIdentity;
use crate::types::{kind, job_request_kind, job_result_kind, JobStatus, KIND_JOB_FEEDBACK, KIND_JOB_REQUEST_BASE, KIND_JOB_RESULT_BASE};

/// Check if an event has the `["encrypted", "nip44"]` tag.
fn is_encrypted(event: &Event) -> bool {
    event.tags.iter().any(|t| {
        let s = t.as_slice();
        s.first().map(|v| v.as_str()) == Some("encrypted")
            && s.get(1).map(|v| v.as_str()) == Some("nip44")
    })
}

/// Encrypt content with NIP-44. Returns ciphertext string.
fn nip44_encrypt(secret_key: &SecretKey, recipient: &PublicKey, content: &str) -> Result<String> {
    nip44::encrypt(secret_key, recipient, content, nip44::Version::V2)
        .map_err(|e| ElisymError::Encryption(format!("NIP-44 encryption failed: {e}")))
}

/// Decrypt content with NIP-44. Returns plaintext string.
fn nip44_decrypt(secret_key: &SecretKey, sender: &PublicKey, ciphertext: &str) -> Result<String> {
    nip44::decrypt(secret_key, sender, ciphertext)
        .map_err(|e| ElisymError::Encryption(format!("NIP-44 decryption failed: {e}")))
}

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
    /// Whether this request was NIP-44 encrypted on the wire.
    pub encrypted: bool,
    /// If `encrypted` is `true` and decryption failed, contains the error message.
    /// When `Some`, `input_data` contains ciphertext (not plaintext).
    pub decryption_error: Option<String>,
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
    /// Whether this result was NIP-44 encrypted on the wire.
    pub encrypted: bool,
    /// If `encrypted` is `true` and decryption failed, contains the error message.
    /// When `Some`, `content` contains ciphertext (not plaintext).
    pub decryption_error: Option<String>,
    pub raw_event: Event,
}

/// A job feedback event via NIP-90.
///
/// The `status` field is stored as a raw `String` (not [`JobStatus`]) for
/// forward-compatibility: relays may deliver feedback with custom or
/// future status values that the current enum doesn't cover. Use
/// [`parsed_status()`](Self::parsed_status) to convert to a typed
/// [`JobStatus`] when the value is known.
///
/// [`JobStatus`]: crate::types::JobStatus
#[derive(Debug, Clone)]
pub struct JobFeedback {
    pub event_id: EventId,
    pub provider: PublicKey,
    pub request_id: EventId,
    /// Raw status string from the Nostr event. Use [`parsed_status()`](Self::parsed_status)
    /// to convert to a typed [`JobStatus`] if it matches a known value.
    pub status: String,
    pub extra_info: Option<String>,
    pub payment_request: Option<String>,
    pub payment_chain: Option<String>,
    /// Transaction hash/signature from a `["tx", hash, chain?]` tag.
    /// Present when the customer confirms payment (status: `payment-completed`).
    pub payment_hash: Option<String>,
    pub raw_event: Event,
}

impl JobFeedback {
    /// Parse the status string into a typed `JobStatus`, if it matches a known value.
    pub fn parsed_status(&self) -> Option<crate::types::JobStatus> {
        match self.status.as_str() {
            "payment-required" => Some(crate::types::JobStatus::PaymentRequired),
            "payment-completed" => Some(crate::types::JobStatus::PaymentCompleted),
            "processing" => Some(crate::types::JobStatus::Processing),
            "error" => Some(crate::types::JobStatus::Error),
            "success" => Some(crate::types::JobStatus::Success),
            "partial" => Some(crate::types::JobStatus::Partial),
            _ => None,
        }
    }
}

/// Service for NIP-90 Data Vending Machine job marketplace.
///
/// # Notification channel architecture
///
/// All subscription methods (`subscribe_to_*`) use `nostr_sdk`'s shared broadcast
/// channel. Every notification is delivered to *all* active subscription tasks, each
/// of which filters by event kind. This means N events × M subscriptions of filtering
/// work. At moderate scale this is acceptable, but with many concurrent subscriptions
/// on a high-throughput relay it may become a bottleneck.
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
    /// When `provider` is specified, the input is automatically NIP-44 encrypted
    /// for the provider (matching the TS SDK behaviour). Broadcast requests
    /// (no provider) are sent in plaintext.
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

        let (content, i_tag_data) = if let Some(recipient) = provider {
            (nip44_encrypt(self.identity.secret_key(), recipient, input_data)?, "encrypted".to_string())
        } else {
            (String::new(), input_data.to_string())
        };

        let mut tags: Vec<Tag> = vec![
            Tag::parse(["i", &i_tag_data, input_type])?,
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

        if provider.is_some() {
            tags.push(Tag::parse(["encrypted", "nip44"])?);
        }

        // Always tag with elisym protocol identifier
        tags.push(Tag::custom(
            TagKind::SingleLetter(SingleLetterTag::lowercase(Alphabet::T)),
            vec!["elisym".to_string()],
        ));

        for tag in &extra_tags {
            tags.push(Tag::custom(
                TagKind::SingleLetter(SingleLetterTag::lowercase(Alphabet::T)),
                vec![tag.clone()],
            ));
        }

        let builder = EventBuilder::new(k, &content).tags(tags);
        let output = self.client.send_event_builder(builder).await?;

        tracing::info!(event_id = %output.val, kind_offset, "Submitted job request");
        Ok(output.val)
    }

    /// Subscribe to job results for requests we've made.
    ///
    /// If `expected_providers` is non-empty, only results from those providers
    /// are forwarded. This prevents accepting results from unknown parties
    /// when jobs were sent to specific providers.
    ///
    /// Returns a [`Subscription`] that yields results via `.recv()`.
    /// Call `.cancel()` to abort the background task, or drop the subscription.
    ///
    /// **Backpressure:** The internal channel holds 256 items. If the receiver
    /// is not drained fast enough, the sending task blocks until space is available.
    /// Slow consumers will not lose events, but may delay processing of other
    /// notification types sharing the same broadcast channel.
    pub async fn subscribe_to_results(
        &self,
        kind_offsets: &[u16],
        expected_providers: &[PublicKey],
    ) -> Result<Subscription<JobResult>> {
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

        // Create the broadcast receiver BEFORE subscribing, so no events
        // arriving between subscribe() and spawn() are lost.
        let mut notifications = self.client.notifications();

        self.client.subscribe(vec![filter], None).await?;

        let allowed: Vec<PublicKey> = expected_providers.to_vec();
        let identity = self.identity.clone();
        let handle = tokio::spawn(async move {
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
                        match parse_job_result(&event, Some(&identity)) {
                            Ok(Some(result)) => {
                                if tx.send(result).await.is_err() {
                                    break;
                                }
                            }
                            Err(e) => {
                                tracing::warn!(event_id = %event.id, %e, "Failed to parse job result");
                            }
                            Ok(None) => {}
                        }
                    }
                }
            }
            tracing::warn!("subscription task ended: results (notification channel closed)");
        });

        Ok(Subscription::new(rx, handle))
    }

    /// Subscribe to job feedback for requests we've made.
    ///
    /// Returns a [`Subscription`] that yields feedback via `.recv()`.
    /// Call `.cancel()` to abort the background task, or drop the subscription.
    ///
    /// **Backpressure:** The internal channel holds 256 items. If the receiver
    /// is not drained fast enough, the sending task blocks until space is available.
    pub async fn subscribe_to_feedback(&self) -> Result<Subscription<JobFeedback>> {
        let (tx, rx) = mpsc::channel(256);

        // Filter by #p tag — feedback events are tagged with the customer's pubkey
        let filter = Filter::new()
            .kind(kind(KIND_JOB_FEEDBACK))
            .custom_tag(
                SingleLetterTag::lowercase(Alphabet::P),
                vec![self.identity.public_key().to_hex()],
            )
            .since(Timestamp::now());

        // Create the broadcast receiver BEFORE subscribing, so no events
        // arriving between subscribe() and spawn() are lost.
        let mut notifications = self.client.notifications();

        self.client.subscribe(vec![filter], None).await?;

        let handle = tokio::spawn(async move {
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

        Ok(Subscription::new(rx, handle))
    }

    // ── History API ──

    /// Fetch historical job requests submitted by this agent.
    ///
    /// Returns up to `limit` job requests of the given kind offsets,
    /// ordered by the relay's response (typically newest first).
    pub async fn fetch_my_jobs(
        &self,
        kind_offsets: &[u16],
        limit: usize,
    ) -> Result<Vec<JobRequest>> {
        let kinds: Vec<Kind> = kind_offsets
            .iter()
            .filter_map(|offset| job_request_kind(*offset))
            .collect();

        let filter = Filter::new()
            .kinds(kinds)
            .author(self.identity.public_key())
            .limit(limit);

        let events = self
            .client
            .fetch_events(vec![filter], Some(std::time::Duration::from_secs(10)))
            .await?;

        let jobs: Vec<JobRequest> = events
            .iter()
            .filter_map(|e| match parse_job_request(e, Some(&self.identity)) {
                Ok(v) => v,
                Err(err) => {
                    tracing::warn!(event_id = %e.id, %err, "Failed to parse job request");
                    None
                }
            })
            .collect();

        Ok(jobs)
    }

    /// Fetch job results for a specific job request from relays.
    ///
    /// Queries relays for kind:6000+offset result events that reference the given
    /// job request event ID and are tagged with the customer's pubkey.
    pub async fn fetch_job_results(
        &self,
        job_event_id: EventId,
        kind_offsets: &[u16],
    ) -> Result<Vec<JobResult>> {
        let kinds: Vec<Kind> = kind_offsets
            .iter()
            .filter_map(|offset| job_result_kind(*offset))
            .collect();

        let filter = Filter::new()
            .kinds(kinds)
            .custom_tag(
                SingleLetterTag::lowercase(Alphabet::P),
                vec![self.identity.public_key().to_hex()],
            )
            .event(job_event_id);

        let events = self
            .client
            .fetch_events(vec![filter], Some(std::time::Duration::from_secs(10)))
            .await?;

        let results: Vec<JobResult> = events
            .iter()
            .filter_map(|e| match parse_job_result(e, Some(&self.identity)) {
                Ok(v) => v,
                Err(err) => {
                    tracing::warn!(event_id = %e.id, %err, "Failed to parse job result");
                    None
                }
            })
            .collect();

        Ok(results)
    }

    /// Fetch job feedback for a specific job request from relays.
    ///
    /// Queries relays for kind:7000 feedback events that reference the given
    /// job request event ID and are tagged with the customer's pubkey.
    pub async fn fetch_job_feedback(
        &self,
        job_event_id: EventId,
    ) -> Result<Vec<JobFeedback>> {
        let filter = Filter::new()
            .kind(kind(KIND_JOB_FEEDBACK))
            .custom_tag(
                SingleLetterTag::lowercase(Alphabet::P),
                vec![self.identity.public_key().to_hex()],
            )
            .event(job_event_id);

        let events = self
            .client
            .fetch_events(vec![filter], Some(std::time::Duration::from_secs(10)))
            .await?;

        let feedback: Vec<JobFeedback> = events
            .iter()
            .filter_map(parse_job_feedback)
            .collect();

        Ok(feedback)
    }

    // ── Provider API ──

    /// Subscribe to incoming job requests for the given kind offsets.
    ///
    /// Receives both directed requests (tagged with our pubkey) and broadcast
    /// requests (no `#p` tag). Requests directed at other providers are filtered out.
    ///
    /// Two overlapping filters are sent to the relay (directed + broadcast).
    /// The same event may arrive twice from a single relay; [`BoundedDedup`]
    /// deduplicates by event ID so each request is delivered exactly once.
    ///
    /// Events that cannot be parsed (e.g., missing `["i", ...]` tag) are silently
    /// dropped — only well-formed NIP-90 job requests are forwarded to the receiver.
    ///
    /// **Broadcast filter trade-off:** The broadcast filter subscribes to all job
    /// requests of the given kinds without a pubkey filter. On a busy network this
    /// means the relay sends every matching event, even those directed at other
    /// providers (which are discarded client-side). This is intentional — it's the
    /// only way to receive undirected/broadcast jobs. If your agent only handles
    /// directed requests, subscribe with a pubkey filter via the Nostr client directly.
    ///
    /// Returns a [`Subscription`] that yields requests via `.recv()`.
    /// Call `.cancel()` to abort the background task, or drop the subscription.
    ///
    /// **Backpressure:** The internal channel holds 256 items. If the receiver
    /// is not drained fast enough, the sending task blocks until space is available.
    pub async fn subscribe_to_job_requests(
        &self,
        kind_offsets: &[u16],
    ) -> Result<Subscription<JobRequest>> {
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

        // Create the broadcast receiver BEFORE subscribing, so no events
        // arriving between subscribe() and spawn() are lost.
        let mut notifications = self.client.notifications();

        self.client
            .subscribe(vec![filter_directed, filter_broadcast], None)
            .await?;

        tracing::info!(
            own_pubkey = %own_pubkey.to_hex(),
            "Job request subscription started"
        );

        let identity = self.identity.clone();
        let handle = tokio::spawn(async move {
            let mut seen = BoundedDedup::new(DEDUP_CAPACITY);
            while let Some(notification) = recv_notification(&mut notifications).await {
                if let RelayPoolNotification::Event { event, .. } = notification {
                    let kind_num = event.kind.as_u16();

                    if (KIND_JOB_REQUEST_BASE..KIND_JOB_RESULT_BASE).contains(&kind_num) {
                        tracing::debug!(
                            event_id = %event.id,
                            kind = kind_num,
                            pubkey = %event.pubkey,
                            "Received event in job request range"
                        );
                    }

                    if !seen.insert(event.id) {
                        tracing::debug!(event_id = %event.id, "Skipping duplicate event");
                        continue; // duplicate from broadcast + directed filters or multiple relays
                    }
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
                            tracing::debug!(
                                event_id = %event.id,
                                p_tags = ?p_tags,
                                own_pubkey = %own_pubkey.to_hex(),
                                "Skipping job directed at another provider"
                            );
                            continue;
                        }

                        match parse_job_request(&event, Some(&identity)) {
                            Ok(Some(request)) => {
                                tracing::info!(
                                    event_id = %request.event_id,
                                    customer = %request.customer,
                                    "Forwarding job request to transport"
                                );
                                if tx.send(request).await.is_err() {
                                    break;
                                }
                            }
                            Err(e) => {
                                tracing::warn!(event_id = %event.id, %e, "Failed to parse job request");
                            }
                            Ok(None) => {
                                tracing::warn!(
                                    event_id = %event.id,
                                    "Failed to parse job request from event"
                                );
                            }
                        }
                    }
                }
            }
            tracing::warn!("subscription task ended: job_requests (notification channel closed)");
        });

        Ok(Subscription::new(rx, handle))
    }

    /// Submit a job result (kind:6000+offset), always NIP-44 encrypted for the customer.
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

        let final_content = nip44_encrypt(self.identity.secret_key(), &request_event.pubkey, content)?;

        let mut tags = vec![
            Tag::event(request_event.id),
            Tag::public_key(request_event.pubkey),
            Tag::custom(
                TagKind::SingleLetter(SingleLetterTag::lowercase(Alphabet::T)),
                vec!["elisym".to_string()],
            ),
        ];

        tags.push(Tag::parse(["encrypted", "nip44"])?);

        if let Some(val) = amount {
            let val_str = val.to_string();
            tags.push(Tag::parse(["amount", &val_str])?);
        }

        let request_json = serde_json::to_string(&request_event)?;
        tags.push(Tag::parse(["request", &request_json])?);

        let builder = EventBuilder::new(k, &final_content).tags(tags);
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
    /// `amount` is in the chain's base unit: millisatoshis for Lightning,
    /// lamports for Solana. The value is serialized as-is into the NIP-90 tag.
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
        let t_kind = TagKind::SingleLetter(SingleLetterTag::lowercase(Alphabet::T));
        let mut tags = vec![
            Tag::event(request_event.id),
            Tag::public_key(request_event.pubkey),
            Tag::custom(t_kind.clone(), vec!["elisym".to_string()]),
        ];

        // Forward capability #t tags from the original request event
        for tag in request_event.tags.iter() {
            if tag.kind() == t_kind {
                if let Some(val) = tag.content() {
                    if val != "elisym" {
                        tags.push(Tag::custom(t_kind.clone(), vec![val.to_string()]));
                    }
                }
            }
        }

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

    /// Submit a payment confirmation (kind:7000, status: `payment-completed`).
    ///
    /// Called by the **customer** after successfully paying a provider's payment request.
    /// Publishes a `["tx", hash, chain]` tag so the provider (and any observer) can
    /// verify the on-chain transaction that fulfils the job's payment requirement.
    ///
    /// The `["p", provider]` tag is set to the provider's pubkey so the provider's
    /// feedback subscription picks it up.
    pub async fn submit_payment_confirmation(
        &self,
        request_event_id: EventId,
        provider: &PublicKey,
        payment_hash: &str,
        payment_chain: Option<&str>,
    ) -> Result<EventId> {
        let chain = payment_chain.unwrap_or("solana");

        let tags = vec![
            Tag::event(request_event_id),
            Tag::public_key(*provider),
            Tag::custom(
                TagKind::SingleLetter(SingleLetterTag::lowercase(Alphabet::T)),
                vec!["elisym".to_string()],
            ),
            Tag::parse(["status", "payment-completed"])?,
            Tag::parse(["tx", payment_hash, chain])?,
        ];

        let builder = EventBuilder::new(kind(KIND_JOB_FEEDBACK), "").tags(tags);
        let output = self.client.send_event_builder(builder).await?;

        tracing::info!(
            event_id = %output.val,
            tx = %payment_hash,
            chain = %chain,
            "Submitted payment confirmation"
        );
        Ok(output.val)
    }
}

// ── Parsing helpers ──
//
// Note on signature verification: `nostr_sdk` verifies event signatures
// before delivering them via `RelayPoolNotification::Event`. Events that
// fail signature verification are rejected at the relay-pool layer and
// never reach these parse functions.

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

fn parse_job_request(event: &Event, identity: Option<&AgentIdentity>) -> Result<Option<JobRequest>> {
    let kind_offset = match event.kind.as_u16().checked_sub(KIND_JOB_REQUEST_BASE) {
        Some(v) => v,
        None => return Ok(None),
    };

    let encrypted = is_encrypted(event);

    // Extract input_type from the "i" tag (preserved in plaintext even when encrypted)
    let i_tag = event.tags.iter().find_map(|tag| {
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
    });

    let (input_data, input_type, decryption_error) = if encrypted {
        let input_type = i_tag.as_ref().map(|(_, t)| t.clone()).unwrap_or_else(|| "text".to_string());
        if let Some(id) = identity {
            match nip44_decrypt(id.secret_key(), &event.pubkey, &event.content) {
                Ok(plaintext) => (plaintext, input_type, None),
                Err(e) => (event.content.clone(), input_type, Some(e.to_string())),
            }
        } else {
            (event.content.clone(), input_type, Some("no decryption key provided".to_string()))
        }
    } else {
        match i_tag {
            Some((data, typ)) => (data, typ, None),
            None => return Ok(None),
        }
    };

    let bid = get_tag_value(event, "bid").and_then(|v| v.parse().ok());
    let output_mime = get_tag_value(event, "output");
    let tags = get_tag_values(event, "t");

    Ok(Some(JobRequest {
        event_id: event.id,
        customer: event.pubkey,
        kind_offset,
        input_data,
        input_type,
        output_mime,
        bid,
        tags,
        encrypted,
        decryption_error,
        raw_event: event.clone(),
    }))
}

fn parse_job_result(event: &Event, identity: Option<&AgentIdentity>) -> Result<Option<JobResult>> {
    let request_id = match resolve_request_id(event) {
        Some(v) => v,
        None => return Ok(None),
    };

    let encrypted = is_encrypted(event);

    let (content, decryption_error) = if encrypted {
        if let Some(id) = identity {
            match nip44_decrypt(id.secret_key(), &event.pubkey, &event.content) {
                Ok(plaintext) => (plaintext, None),
                Err(e) => (event.content.clone(), Some(e.to_string())),
            }
        } else {
            (event.content.clone(), Some("no decryption key provided".to_string()))
        }
    } else {
        (event.content.clone(), None)
    };

    let amount = event.tags.iter().find_map(|tag| {
        let s = tag.as_slice();
        if s.first().map(|v| v.as_str()) == Some("amount") {
            s.get(1).and_then(|v| v.parse().ok())
        } else {
            None
        }
    });

    Ok(Some(JobResult {
        event_id: event.id,
        provider: event.pubkey,
        request_id,
        content,
        amount,
        encrypted,
        decryption_error,
        raw_event: event.clone(),
    }))
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

    // Extract transaction hash from ["tx", hash, chain?] tag (payment confirmation)
    let payment_hash = get_tag_value(event, "tx");

    Some(JobFeedback {
        event_id: event.id,
        provider: event.pubkey,
        request_id,
        status,
        extra_info,
        payment_request,
        payment_chain,
        payment_hash,
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
        let req = parse_job_request(&event, None).unwrap().expect("should parse");
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
        let req = parse_job_request(&event, None).unwrap().expect("should parse");
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
        assert!(parse_job_request(&event, None).unwrap().is_none());
    }

    #[test]
    fn test_parse_job_request_wrong_kind() {
        // kind:4999 is below 5000 → checked_sub underflows → None
        let event = make_event(4999, "", vec![
            Tag::parse(["i", "data", "text"]).unwrap(),
        ]);
        assert!(parse_job_request(&event, None).unwrap().is_none());
    }

    #[test]
    fn test_parse_job_request_i_tag_missing_type_defaults_to_text() {
        // "i" tag with only one value → input_type defaults to "text"
        let event = make_event(5100, "", vec![
            Tag::parse(["i", "some data"]).unwrap(),
        ]);
        let req = parse_job_request(&event, None).unwrap().expect("should parse");
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
        let req = parse_job_request(&event, None).unwrap().expect("should parse");
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
        let result = parse_job_result(&result_event, None).unwrap().expect("should parse");
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
        let result = parse_job_result(&result_event, None).unwrap().expect("should parse");
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
        assert!(parse_job_result(&event, None).unwrap().is_none());
    }

    #[test]
    fn test_parse_job_result_no_amount() {
        let request_event = make_event(5100, "", vec![
            Tag::parse(["i", "data", "text"]).unwrap(),
        ]);
        let result_event = make_event(6100, "free result", vec![
            Tag::event(request_event.id),
        ]);
        let result = parse_job_result(&result_event, None).unwrap().expect("should parse");
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
    fn test_parse_job_feedback_payment_completed_with_tx() {
        let request_event = make_event(5100, "", vec![
            Tag::parse(["i", "data", "text"]).unwrap(),
        ]);
        let feedback_event = make_event(7000, "", vec![
            Tag::event(request_event.id),
            Tag::parse(["status", "payment-completed"]).unwrap(),
            Tag::parse(["tx", "5UfDuX7WXYxRnFzCfQHs3a4jKj...", "solana"]).unwrap(),
        ]);
        let fb = parse_job_feedback(&feedback_event).expect("should parse");
        assert_eq!(fb.status, "payment-completed");
        assert_eq!(fb.parsed_status(), Some(JobStatus::PaymentCompleted));
        assert_eq!(fb.payment_hash.as_deref(), Some("5UfDuX7WXYxRnFzCfQHs3a4jKj..."));
        assert_eq!(fb.payment_request, None);
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

    // ── parsed_status ──

    #[test]
    fn test_parsed_status_all_known_variants() {
        let request_event = make_event(5100, "", vec![
            Tag::parse(["i", "data", "text"]).unwrap(),
        ]);
        let cases = vec![
            ("payment-required", Some(JobStatus::PaymentRequired)),
            ("payment-completed", Some(JobStatus::PaymentCompleted)),
            ("processing", Some(JobStatus::Processing)),
            ("error", Some(JobStatus::Error)),
            ("success", Some(JobStatus::Success)),
            ("partial", Some(JobStatus::Partial)),
            ("unknown-status", None),
        ];
        for (status_str, expected) in cases {
            let fb_event = make_event(7000, "", vec![
                Tag::event(request_event.id),
                Tag::parse(["status", status_str]).unwrap(),
            ]);
            let fb = parse_job_feedback(&fb_event).unwrap();
            assert_eq!(fb.parsed_status(), expected, "status: {}", status_str);
        }
    }

    // ── JobStatus serde ──

    #[test]
    fn test_job_status_serde_all_variants() {
        let variants = vec![
            (JobStatus::PaymentRequired, "\"payment-required\""),
            (JobStatus::PaymentCompleted, "\"payment-completed\""),
            (JobStatus::Processing, "\"processing\""),
            (JobStatus::Error, "\"error\""),
            (JobStatus::Success, "\"success\""),
            (JobStatus::Partial, "\"partial\""),
        ];
        for (status, expected_json) in variants {
            let json = serde_json::to_string(&status).unwrap();
            assert_eq!(json, expected_json);
            let parsed: JobStatus = serde_json::from_str(&json).unwrap();
            assert_eq!(parsed, status);
        }
    }

    // ── AgentFilter serde ──

    #[test]
    fn test_agent_filter_skip_serializing_none() {
        use crate::discovery::AgentFilter;
        let filter = AgentFilter {
            capabilities: vec!["translation".into()],
            ..Default::default()
        };
        let json = serde_json::to_string(&filter).unwrap();
        // limit and query should be absent (skip_serializing_if)
        assert!(!json.contains("limit"));
        assert!(!json.contains("query"));
        // capabilities should be present
        assert!(json.contains("translation"));
    }

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

    // ── Encryption helpers ──

    #[test]
    fn test_is_encrypted_true() {
        let event = make_event(5100, "ciphertext", vec![
            Tag::parse(["i", "encrypted", "text"]).unwrap(),
            Tag::parse(["encrypted", "nip44"]).unwrap(),
        ]);
        assert!(is_encrypted(&event));
    }

    #[test]
    fn test_is_encrypted_false() {
        let event = make_event(5100, "", vec![
            Tag::parse(["i", "data", "text"]).unwrap(),
        ]);
        assert!(!is_encrypted(&event));
    }

    #[test]
    fn test_nip44_encrypt_decrypt_roundtrip() {
        let alice = Keys::generate();
        let bob = Keys::generate();
        let plaintext = "Hello, encrypted world!";

        let ciphertext = nip44_encrypt(alice.secret_key(), &bob.public_key(), plaintext).unwrap();
        assert_ne!(ciphertext, plaintext);

        let decrypted = nip44_decrypt(bob.secret_key(), &alice.public_key(), &ciphertext).unwrap();
        assert_eq!(decrypted, plaintext);
    }

    // ── Encrypted job request parsing ──

    #[test]
    fn test_parse_job_request_encrypted() {
        let customer = Keys::generate();
        let provider = Keys::generate();
        let provider_id = AgentIdentity::from_keys(provider.clone());
        let plaintext = "secret input data";

        let ciphertext = nip44_encrypt(customer.secret_key(), &provider.public_key(), plaintext).unwrap();
        let event = make_event_with_keys(&customer, 5100, &ciphertext, vec![
            Tag::parse(["i", "encrypted", "text"]).unwrap(),
            Tag::parse(["encrypted", "nip44"]).unwrap(),
        ]);

        let req = parse_job_request(&event, Some(&provider_id))
            .unwrap()
            .expect("should parse");
        assert_eq!(req.input_data, plaintext);
        assert_eq!(req.input_type, "text");
        assert!(req.encrypted);
    }

    #[test]
    fn test_parse_job_request_encrypted_no_key() {
        let customer = Keys::generate();
        let provider = Keys::generate();

        let ciphertext = nip44_encrypt(customer.secret_key(), &provider.public_key(), "secret").unwrap();
        let event = make_event_with_keys(&customer, 5100, &ciphertext, vec![
            Tag::parse(["i", "encrypted", "text"]).unwrap(),
            Tag::parse(["encrypted", "nip44"]).unwrap(),
        ]);

        // No decrypt key → returns ciphertext as-is, encrypted = true
        let req = parse_job_request(&event, None)
            .unwrap()
            .expect("should parse");
        assert_eq!(req.input_data, ciphertext);
        assert!(req.encrypted);
    }

    #[test]
    fn test_parse_job_request_encrypted_wrong_key() {
        let customer = Keys::generate();
        let provider = Keys::generate();
        let wrong_key = Keys::generate();
        let wrong_id = AgentIdentity::from_keys(wrong_key);

        let ciphertext = nip44_encrypt(customer.secret_key(), &provider.public_key(), "secret").unwrap();
        let event = make_event_with_keys(&customer, 5100, &ciphertext, vec![
            Tag::parse(["i", "encrypted", "text"]).unwrap(),
            Tag::parse(["encrypted", "nip44"]).unwrap(),
        ]);

        // Wrong key → returns Ok with ciphertext and decryption_error
        let req = parse_job_request(&event, Some(&wrong_id))
            .unwrap()
            .expect("should parse");
        assert!(req.encrypted);
        assert!(req.decryption_error.is_some());
        assert_eq!(req.input_data, ciphertext);
    }

    #[test]
    fn test_parse_job_request_not_encrypted_flag() {
        let event = make_event(5100, "", vec![
            Tag::parse(["i", "plaintext data", "text"]).unwrap(),
        ]);
        let req = parse_job_request(&event, None).unwrap().expect("should parse");
        assert!(!req.encrypted);
    }

    // ── Encrypted job result parsing ──

    #[test]
    fn test_parse_job_result_encrypted() {
        let provider = Keys::generate();
        let customer = Keys::generate();
        let customer_id = AgentIdentity::from_keys(customer.clone());
        let plaintext = "secret result content";

        let request_event = make_event_with_keys(&customer, 5100, "", vec![
            Tag::parse(["i", "data", "text"]).unwrap(),
        ]);

        let ciphertext = nip44_encrypt(provider.secret_key(), &customer.public_key(), plaintext).unwrap();
        let result_event = make_event_with_keys(&provider, 6100, &ciphertext, vec![
            Tag::event(request_event.id),
            Tag::parse(["encrypted", "nip44"]).unwrap(),
        ]);

        let result = parse_job_result(&result_event, Some(&customer_id))
            .unwrap()
            .expect("should parse");
        assert_eq!(result.content, plaintext);
        assert!(result.encrypted);
    }

    #[test]
    fn test_parse_job_result_encrypted_wrong_key() {
        let provider = Keys::generate();
        let customer = Keys::generate();
        let wrong_key = Keys::generate();
        let wrong_id = AgentIdentity::from_keys(wrong_key);

        let request_event = make_event_with_keys(&customer, 5100, "", vec![
            Tag::parse(["i", "data", "text"]).unwrap(),
        ]);

        let ciphertext = nip44_encrypt(provider.secret_key(), &customer.public_key(), "secret").unwrap();
        let result_event = make_event_with_keys(&provider, 6100, &ciphertext, vec![
            Tag::event(request_event.id),
            Tag::parse(["encrypted", "nip44"]).unwrap(),
        ]);

        // Wrong key → returns Ok with ciphertext and decryption_error
        let result = parse_job_result(&result_event, Some(&wrong_id))
            .unwrap()
            .expect("should parse");
        assert!(result.encrypted);
        assert!(result.decryption_error.is_some());
        assert_eq!(result.content, ciphertext);
    }

    #[test]
    fn test_parse_job_result_not_encrypted_flag() {
        let request_event = make_event(5100, "", vec![
            Tag::parse(["i", "data", "text"]).unwrap(),
        ]);
        let result_event = make_event(6100, "plain result", vec![
            Tag::event(request_event.id),
        ]);
        let result = parse_job_result(&result_event, None).unwrap().expect("should parse");
        assert!(!result.encrypted);
    }

    // ── submit_job_request encryption integration ──

    /// Build a minimal MarketplaceService wired to a local nostr client
    /// that captures the event without sending it to real relays.
    /// Returns the signed Event for inspection.
    async fn build_job_request_event(
        customer_keys: &Keys,
        provider_pk: Option<&PublicKey>,
        input_data: &str,
        input_type: &str,
    ) -> Event {
        // Replicate submit_job_request tag-building locally so we can
        // inspect the resulting event without needing a real relay.
        let kind_offset: u16 = 100;
        let k = job_request_kind(kind_offset).unwrap();

        let (content, i_tag_data) = if let Some(recipient) = provider_pk {
            let ct = nip44_encrypt(customer_keys.secret_key(), recipient, input_data).unwrap();
            (ct, "encrypted".to_string())
        } else {
            (String::new(), input_data.to_string())
        };

        let mut tags: Vec<Tag> = vec![
            Tag::parse(["i", &i_tag_data, input_type]).unwrap(),
        ];

        if let Some(pk) = provider_pk {
            tags.push(Tag::public_key(*pk));
        }
        if provider_pk.is_some() {
            tags.push(Tag::parse(["encrypted", "nip44"]).unwrap());
        }
        tags.push(Tag::custom(
            TagKind::SingleLetter(SingleLetterTag::lowercase(Alphabet::T)),
            vec!["elisym".to_string()],
        ));

        EventBuilder::new(k, &content)
            .tags(tags)
            .sign_with_keys(customer_keys)
            .unwrap()
    }

    #[tokio::test]
    async fn test_submit_job_request_encrypted_has_correct_tags() {
        let customer = Keys::generate();
        let provider = Keys::generate();

        let event = build_job_request_event(
            &customer,
            Some(&provider.public_key()),
            "secret payload",
            "text",
        ).await;

        // 1. Must have ["encrypted", "nip44"] tag
        assert!(is_encrypted(&event), "encrypted tag must be present");

        // 2. "i" tag data must be "encrypted", not the plaintext
        let i_data = event.tags.iter().find_map(|t| {
            let s = t.as_slice();
            if s.first().map(|v| v.as_str()) == Some("i") {
                Some(s.get(1).map(|v| v.to_string()).unwrap_or_default())
            } else {
                None
            }
        }).expect("i tag must exist");
        assert_eq!(i_data, "encrypted");

        // 3. Content must be non-empty ciphertext (not the plaintext)
        assert!(!event.content.is_empty(), "content must be ciphertext");
        assert_ne!(event.content, "secret payload", "content must not be plaintext");

        // 4. Provider can decrypt back to original plaintext
        let provider_id = AgentIdentity::from_keys(provider.clone());
        let req = parse_job_request(&event, Some(&provider_id))
            .unwrap()
            .expect("should parse");
        assert_eq!(req.input_data, "secret payload");
        assert!(req.encrypted);
        assert!(req.decryption_error.is_none());
    }

    #[tokio::test]
    async fn test_submit_job_request_broadcast_is_plaintext() {
        let customer = Keys::generate();

        let event = build_job_request_event(
            &customer,
            None,
            "public payload",
            "text",
        ).await;

        // 1. Must NOT have encrypted tag
        assert!(!is_encrypted(&event), "broadcast must not have encrypted tag");

        // 2. Content must be empty (NIP-90 convention)
        assert!(event.content.is_empty(), "broadcast content must be empty");

        // 3. "i" tag must contain the plaintext data
        let i_data = event.tags.iter().find_map(|t| {
            let s = t.as_slice();
            if s.first().map(|v| v.as_str()) == Some("i") {
                Some(s.get(1).map(|v| v.to_string()).unwrap_or_default())
            } else {
                None
            }
        }).expect("i tag must exist");
        assert_eq!(i_data, "public payload");

        // 4. Parses correctly without identity
        let req = parse_job_request(&event, None)
            .unwrap()
            .expect("should parse");
        assert_eq!(req.input_data, "public payload");
        assert!(!req.encrypted);
    }
}
