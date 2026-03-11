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
    card.set_payment(PaymentInfo {
        chain: "solana".into(),
        network: "devnet".into(),
        address: "test@wallet.example.com".into(),
        job_price: None,
    });

    let json = card.to_json().unwrap();
    let restored = CapabilityCard::from_json(&json).unwrap();

    assert_eq!(restored.name, "test-agent");
    assert_eq!(restored.description, "A test agent for integration testing");
    assert_eq!(restored.capabilities.len(), 2);
    let payment = restored.payment.unwrap();
    assert_eq!(payment.address, "test@wallet.example.com");
    assert_eq!(payment.chain, "solana");
}

#[test]
fn test_capability_card_without_optional_fields() {
    let card = CapabilityCard::new("minimal", "Minimal agent", vec![]);
    let json = card.to_json().unwrap();

    // payment should not be in JSON when None
    assert!(!json.contains("payment"));

    let restored = CapabilityCard::from_json(&json).unwrap();
    assert_eq!(restored.name, "minimal");
    assert!(restored.payment.is_none());
}

#[test]
fn test_job_status_display() {
    assert_eq!(JobStatus::PaymentRequired.as_str(), "payment-required");
    assert_eq!(JobStatus::Processing.as_str(), "processing");
    assert_eq!(JobStatus::Error.as_str(), "error");
    assert_eq!(JobStatus::Success.as_str(), "success");
    assert_eq!(JobStatus::Partial.as_str(), "partial");
    assert_eq!(JobStatus::PaymentCompleted.as_str(), "payment-completed");
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
fn test_capability_card_with_payment() {
    let mut card = CapabilityCard::new("ln-agent", "Agent with payment", vec![]);
    card.set_payment(PaymentInfo {
        chain: "lightning".into(),
        network: "mainnet".into(),
        address: "agent@wallet.com".into(),
        job_price: Some(1000),
    });

    let json = card.to_json().unwrap();
    assert!(json.contains("payment"));

    let restored = CapabilityCard::from_json(&json).unwrap();
    let payment = restored.payment.unwrap();
    assert_eq!(payment.address, "agent@wallet.com");
    assert_eq!(payment.chain, "lightning");
    assert_eq!(payment.job_price, Some(1000));
}
