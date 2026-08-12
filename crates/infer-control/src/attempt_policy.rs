//! Retry and fallback eligibility. Execution state remains owned by Runtime.

use infer_provider::ProviderFailureKind;
use std::time::Duration;

pub const MAX_ATTEMPTS: usize = 3;
pub const MAX_RETRIES_PER_CANDIDATE: usize = 1;
pub const MAX_RETRY_DELAY: Duration = Duration::from_secs(30);

pub fn retryable(kind: ProviderFailureKind) -> bool {
    matches!(
        kind,
        ProviderFailureKind::RateLimited
            | ProviderFailureKind::Timeout
            | ProviderFailureKind::Unavailable
    )
}

pub fn fallback_eligible(kind: ProviderFailureKind) -> bool {
    kind != ProviderFailureKind::InvalidRequest
}

pub fn kind_code(kind: ProviderFailureKind) -> &'static str {
    match kind {
        ProviderFailureKind::Authentication => "authentication",
        ProviderFailureKind::RateLimited => "rate_limited",
        ProviderFailureKind::Timeout => "timeout",
        ProviderFailureKind::Unavailable => "unavailable",
        ProviderFailureKind::InvalidRequest => "invalid_request",
        ProviderFailureKind::Protocol => "protocol",
    }
}

pub fn retry_delay(
    kind: ProviderFailureKind,
    retry_index: usize,
    retry_after: Option<Duration>,
    jitter_key: &str,
) -> Duration {
    let base_ms = match kind {
        ProviderFailureKind::RateLimited => 500_u64,
        ProviderFailureKind::Timeout | ProviderFailureKind::Unavailable => 200,
        _ => return Duration::ZERO,
    };
    let exponent = u32::try_from(retry_index.min(5)).unwrap_or(5);
    let exponential = Duration::from_millis(base_ms.saturating_mul(2_u64.pow(exponent)));
    let jitter = Duration::from_millis(stable_jitter(jitter_key, retry_index, base_ms / 2 + 1));
    retry_after
        .unwrap_or(exponential.saturating_add(jitter))
        .min(MAX_RETRY_DELAY)
}

fn stable_jitter(key: &str, retry_index: usize, modulus: u64) -> u64 {
    let mut hash = 0xcbf29ce484222325_u64;
    for byte in key.bytes().chain(retry_index.to_le_bytes()) {
        hash ^= u64::from(byte);
        hash = hash.wrapping_mul(0x100000001b3);
    }
    hash % modulus
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn invalid_request_neither_retries_nor_falls_back() {
        assert!(!retryable(ProviderFailureKind::InvalidRequest));
        assert!(!fallback_eligible(ProviderFailureKind::InvalidRequest));
        assert!(retryable(ProviderFailureKind::Timeout));
        assert!(fallback_eligible(ProviderFailureKind::Protocol));
    }

    #[test]
    fn retry_delay_is_bounded_jittered_and_honors_retry_after() {
        let first = retry_delay(ProviderFailureKind::Unavailable, 0, None, "job-a");
        let other = retry_delay(ProviderFailureKind::Unavailable, 0, None, "job-b");
        assert!(first >= Duration::from_millis(200));
        assert_ne!(first, other);
        assert_eq!(
            retry_delay(
                ProviderFailureKind::RateLimited,
                0,
                Some(Duration::from_secs(90)),
                "job-a"
            ),
            MAX_RETRY_DELAY
        );
        assert_eq!(
            retry_delay(ProviderFailureKind::InvalidRequest, 0, None, "job-a"),
            Duration::ZERO
        );
    }
}
