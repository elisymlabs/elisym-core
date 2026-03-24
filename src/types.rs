use nostr_sdk::Kind;
use serde::{Deserialize, Serialize};

/// NIP-89 Application Handler (parameterized replaceable event)
pub const KIND_APP_HANDLER: u16 = 31990;

/// NIP-90 Data Vending Machine job request base kind
pub const KIND_JOB_REQUEST_BASE: u16 = 5000;

/// NIP-90 Data Vending Machine job result base kind
pub const KIND_JOB_RESULT_BASE: u16 = 6000;

/// NIP-90 Data Vending Machine job feedback kind
pub const KIND_JOB_FEEDBACK: u16 = 7000;

/// NIP-17 DM kind used for unencrypted ping/pong (no gift-wrap).
pub const KIND_DM: u16 = 14;

/// Default NIP-90 job kind offset (request kind = 5000 + offset, result kind = 6000 + offset)
pub const DEFAULT_KIND_OFFSET: u16 = 100;

/// Default relays for the network
pub const DEFAULT_RELAYS: &[&str] = &[
    "wss://relay.damus.io",
    "wss://nos.lol",
    "wss://relay.nostr.band",
];

/// Protocol fee in basis points (300 = 3%).
pub const PROTOCOL_FEE_BPS: u64 = 300;

/// Calculate protocol fee for a given amount (integer-only, rounds up).
/// Returns `None` on overflow.
pub fn calculate_protocol_fee(amount: u64) -> Option<u64> {
    amount.checked_mul(PROTOCOL_FEE_BPS).map(|v| v.div_ceil(10_000))
}

/// Format basis points as percentage string (300 → "3.00%"). Integer-only.
pub fn format_bps_percent(bps: u64) -> String {
    let whole = bps / 100;
    let frac = bps % 100;
    format!("{}.{:02}%", whole, frac)
}

/// Helper to create a Kind from a u16
pub fn kind(k: u16) -> Kind {
    Kind::from(k)
}

/// Compute job request kind (5000 + offset) with overflow check.
pub fn job_request_kind(offset: u16) -> Option<Kind> {
    KIND_JOB_REQUEST_BASE
        .checked_add(offset)
        .filter(|&k| k < KIND_JOB_RESULT_BASE)
        .map(kind)
}

/// Compute job result kind (6000 + offset) with overflow check.
pub fn job_result_kind(offset: u16) -> Option<Kind> {
    KIND_JOB_RESULT_BASE
        .checked_add(offset)
        .filter(|&k| k < KIND_JOB_FEEDBACK)
        .map(kind)
}

/// Job status for NIP-90 feedback events
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum JobStatus {
    PaymentRequired,
    PaymentCompleted,
    Processing,
    Error,
    Success,
    Partial,
}

impl JobStatus {
    pub fn as_str(&self) -> &'static str {
        match self {
            JobStatus::PaymentRequired => "payment-required",
            JobStatus::PaymentCompleted => "payment-completed",
            JobStatus::Processing => "processing",
            JobStatus::Error => "error",
            JobStatus::Success => "success",
            JobStatus::Partial => "partial",
        }
    }
}

impl std::fmt::Display for JobStatus {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── calculate_protocol_fee ──

    #[test]
    fn test_calculate_protocol_fee_normal() {
        // 1_000_000 * 300 / 10_000 = 30_000
        assert_eq!(calculate_protocol_fee(1_000_000), Some(30_000));
    }

    #[test]
    fn test_calculate_protocol_fee_zero() {
        assert_eq!(calculate_protocol_fee(0), Some(0));
    }

    #[test]
    fn test_calculate_protocol_fee_rounds_up() {
        // 1 * 300 / 10_000 = 0.03 → ceil = 1
        assert_eq!(calculate_protocol_fee(1), Some(1));
    }

    #[test]
    fn test_calculate_protocol_fee_small_amount() {
        // 100 * 300 / 10_000 = 3
        assert_eq!(calculate_protocol_fee(100), Some(3));
    }

    #[test]
    fn test_calculate_protocol_fee_overflow_returns_none() {
        // u64::MAX * 300 overflows → None
        assert_eq!(calculate_protocol_fee(u64::MAX), None);
    }

    #[test]
    fn test_calculate_protocol_fee_large_no_overflow() {
        // u64::MAX / 300 won't overflow when multiplied by 300
        let amount = u64::MAX / PROTOCOL_FEE_BPS;
        assert!(calculate_protocol_fee(amount).is_some());
    }

    // ── format_bps_percent ──

    #[test]
    fn test_format_bps_percent_300() {
        assert_eq!(format_bps_percent(300), "3.00%");
    }

    #[test]
    fn test_format_bps_percent_zero() {
        assert_eq!(format_bps_percent(0), "0.00%");
    }

    #[test]
    fn test_format_bps_percent_one() {
        assert_eq!(format_bps_percent(1), "0.01%");
    }

    #[test]
    fn test_format_bps_percent_fifty() {
        assert_eq!(format_bps_percent(50), "0.50%");
    }

    #[test]
    fn test_format_bps_percent_10000() {
        assert_eq!(format_bps_percent(10_000), "100.00%");
    }

    #[test]
    fn test_format_bps_percent_12345() {
        assert_eq!(format_bps_percent(12_345), "123.45%");
    }

    // ── job_request_kind ──

    #[test]
    fn test_job_request_kind_offset_0() {
        assert_eq!(job_request_kind(0).unwrap().as_u16(), 5000);
    }

    #[test]
    fn test_job_request_kind_offset_100() {
        assert_eq!(job_request_kind(100).unwrap().as_u16(), 5100);
    }

    #[test]
    fn test_job_request_kind_offset_999() {
        assert_eq!(job_request_kind(999).unwrap().as_u16(), 5999);
    }

    #[test]
    fn test_job_request_kind_offset_1000_returns_none() {
        // 5000 + 1000 = 6000 which is >= KIND_JOB_RESULT_BASE
        assert!(job_request_kind(1000).is_none());
    }

    #[test]
    fn test_job_request_kind_max_offset_returns_none() {
        // 5000 + 65535 overflows u16 or >= 6000
        assert!(job_request_kind(u16::MAX).is_none());
    }

    // ── job_result_kind ──

    #[test]
    fn test_job_result_kind_offset_0() {
        assert_eq!(job_result_kind(0).unwrap().as_u16(), 6000);
    }

    #[test]
    fn test_job_result_kind_offset_100() {
        assert_eq!(job_result_kind(100).unwrap().as_u16(), 6100);
    }

    #[test]
    fn test_job_result_kind_offset_999() {
        assert_eq!(job_result_kind(999).unwrap().as_u16(), 6999);
    }

    #[test]
    fn test_job_result_kind_offset_1000_returns_none() {
        // 6000 + 1000 = 7000 which is >= KIND_JOB_FEEDBACK
        assert!(job_result_kind(1000).is_none());
    }

    // ── JobStatus ──

    #[test]
    fn test_job_status_display() {
        assert_eq!(JobStatus::PaymentRequired.to_string(), "payment-required");
        assert_eq!(JobStatus::Processing.to_string(), "processing");
        assert_eq!(JobStatus::Success.to_string(), "success");
    }

    #[test]
    fn test_job_status_serde_roundtrip() {
        let statuses = vec![
            JobStatus::PaymentRequired,
            JobStatus::PaymentCompleted,
            JobStatus::Processing,
            JobStatus::Error,
            JobStatus::Success,
            JobStatus::Partial,
        ];
        for status in statuses {
            let json = serde_json::to_string(&status).unwrap();
            let parsed: JobStatus = serde_json::from_str(&json).unwrap();
            assert_eq!(parsed, status);
        }
    }
}
