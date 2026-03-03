use elisym_core::*;

#[test]
fn test_identity_generate_and_restore() {
    let id = AgentIdentity::generate();
    let hex = id.keys().secret_key().to_secret_hex();
    let restored = AgentIdentity::from_secret_key(&hex).unwrap();
    assert_eq!(id.public_key(), restored.public_key());
}

#[test]
fn test_capability_card_roundtrip() {
    let mut card = CapabilityCard::new(
        "test-agent",
        "A test agent for integration testing",
        vec!["capability-a".into(), "capability-b".into()],
    );
    card.set_payment_address("test@wallet.example.com");

    let json = card.to_json().unwrap();
    let restored = CapabilityCard::from_json(&json).unwrap();

    assert_eq!(restored.name, "test-agent");
    assert_eq!(restored.description, "A test agent for integration testing");
    assert_eq!(restored.capabilities.len(), 2);
    assert_eq!(restored.payment_address.as_deref(), Some("test@wallet.example.com"));
    assert_eq!(restored.protocol_version, PROTOCOL_VERSION);
}

#[test]
fn test_capability_card_without_optional_fields() {
    let card = CapabilityCard::new("minimal", "Minimal agent", vec![]);
    let json = card.to_json().unwrap();

    // payment_address and metadata should not be in JSON when None
    assert!(!json.contains("payment_address"));
    assert!(!json.contains("metadata"));

    let restored = CapabilityCard::from_json(&json).unwrap();
    assert_eq!(restored.name, "minimal");
    assert!(restored.payment_address.is_none());
}

#[test]
fn test_job_status_display() {
    assert_eq!(JobStatus::PaymentRequired.as_str(), "payment-required");
    assert_eq!(JobStatus::Processing.as_str(), "processing");
    assert_eq!(JobStatus::Error.as_str(), "error");
    assert_eq!(JobStatus::Success.as_str(), "success");
    assert_eq!(JobStatus::Partial.as_str(), "partial");
}

#[test]
fn test_job_status_serde() {
    let status = JobStatus::PaymentRequired;
    let json = serde_json::to_string(&status).unwrap();
    assert_eq!(json, "\"payment-required\"");

    let restored: JobStatus = serde_json::from_str(&json).unwrap();
    assert_eq!(restored, JobStatus::PaymentRequired);
}

#[test]
fn test_agent_filter_default() {
    let filter = AgentFilter::default();
    assert!(filter.capabilities.is_empty());
    assert!(filter.job_kind.is_none());
    assert!(filter.since.is_none());
}

#[test]
fn test_kind_constants() {
    assert_eq!(KIND_APP_HANDLER, 31990);
    assert_eq!(KIND_JOB_REQUEST_BASE, 5000);
    assert_eq!(KIND_JOB_RESULT_BASE, 6000);
    assert_eq!(KIND_JOB_FEEDBACK, 7000);
}

#[test]
fn test_protocol_version() {
    assert_eq!(PROTOCOL_VERSION, "elisym/0.1");
}

#[test]
fn test_capability_card_with_payment_address_only() {
    let mut card = CapabilityCard::new("ln-agent", "Agent with payment address", vec![]);
    card.set_payment_address("agent@wallet.com");

    let json = card.to_json().unwrap();
    assert!(json.contains("payment_address"));

    let restored = CapabilityCard::from_json(&json).unwrap();
    assert_eq!(restored.payment_address.as_deref(), Some("agent@wallet.com"));
}
