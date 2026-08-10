//! Retry and fallback eligibility. Execution state remains owned by Runtime.

use infer_provider::ProviderFailureKind;

pub const MAX_ATTEMPTS: usize = 3;
pub const MAX_RETRIES_PER_CANDIDATE: usize = 1;

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
}
