//! Infra Discovery offer for Infer Runtime's authenticated Consumer API.
//!
//! Discovery locates the already-bound HTTP data plane. It never carries an
//! App id, bearer credential, permission, or request payload.

use std::net::SocketAddr;

use thiserror::Error;

use crate::DiscoveryOffer;

pub const CONSUMER_PROTOCOL: &str = "infer-runtime.consumer";
pub const CONSUMER_HTTP_LOOPBACK_BINDING: &str = "infer-runtime.http-loopback";

#[derive(Debug, Error, PartialEq, Eq)]
pub enum ConsumerOfferError {
    #[error("consumer protocol version `{0}` is invalid")]
    InvalidProtocolVersion(String),
    #[error("consumer HTTP endpoint `{0}` must be a canonical numeric loopback address")]
    InvalidEndpoint(String),
}

pub fn consumer_http_offer(
    address: SocketAddr,
    protocol_version: &str,
) -> Result<DiscoveryOffer, ConsumerOfferError> {
    validate_protocol_version(protocol_version)?;
    let endpoint = format!("http://{address}");
    validate_consumer_http_endpoint(&endpoint)?;
    Ok(DiscoveryOffer {
        protocol: CONSUMER_PROTOCOL.to_owned(),
        protocol_versions: vec![protocol_version.to_owned()],
        binding: CONSUMER_HTTP_LOOPBACK_BINDING.to_owned(),
        endpoint,
    })
}

pub fn validate_consumer_http_endpoint(endpoint: &str) -> Result<SocketAddr, ConsumerOfferError> {
    let invalid = || ConsumerOfferError::InvalidEndpoint(endpoint.to_owned());
    let address = endpoint
        .strip_prefix("http://")
        .ok_or_else(invalid)?
        .parse::<SocketAddr>()
        .map_err(|_| invalid())?;
    if !address.ip().is_loopback() || address.port() == 0 {
        return Err(invalid());
    }
    if format!("http://{address}") != endpoint {
        return Err(invalid());
    }
    Ok(address)
}

fn validate_protocol_version(protocol_version: &str) -> Result<(), ConsumerOfferError> {
    let valid = !protocol_version.is_empty()
        && protocol_version.len() <= 64
        && protocol_version
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'+' | b'-'));
    if valid {
        Ok(())
    } else {
        Err(ConsumerOfferError::InvalidProtocolVersion(
            protocol_version.to_owned(),
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn builds_exact_ipv4_consumer_offer() {
        let offer =
            consumer_http_offer("127.0.0.1:8787".parse().unwrap(), "0.1.0-candidate.3").unwrap();

        assert_eq!(offer.protocol, CONSUMER_PROTOCOL);
        assert_eq!(
            offer.protocol_versions,
            vec!["0.1.0-candidate.3".to_owned()]
        );
        assert_eq!(offer.binding, CONSUMER_HTTP_LOOPBACK_BINDING);
        assert_eq!(offer.endpoint, "http://127.0.0.1:8787");
    }

    #[test]
    fn preserves_canonical_ipv6_loopback_endpoint() {
        let offer = consumer_http_offer("[::1]:8787".parse().unwrap(), "20260811.1").unwrap();
        assert_eq!(offer.endpoint, "http://[::1]:8787");
        assert_eq!(
            validate_consumer_http_endpoint(&offer.endpoint).unwrap(),
            "[::1]:8787".parse::<SocketAddr>().unwrap()
        );
    }

    #[test]
    fn rejects_non_canonical_or_non_loopback_endpoints() {
        for endpoint in [
            "https://127.0.0.1:8787",
            "http://localhost:8787",
            "http://127.0.0.1:8787/",
            "http://127.0.0.1:8787/v1",
            "http://127.0.0.1:0",
            "http://0.0.0.0:8787",
            "http://192.168.1.10:8787",
            "http://user@127.0.0.1:8787",
        ] {
            assert_eq!(
                validate_consumer_http_endpoint(endpoint),
                Err(ConsumerOfferError::InvalidEndpoint(endpoint.to_owned()))
            );
        }
    }

    #[test]
    fn rejects_invalid_protocol_versions() {
        for version in ["", "invalid version"] {
            assert!(matches!(
                consumer_http_offer("127.0.0.1:8787".parse().unwrap(), version),
                Err(ConsumerOfferError::InvalidProtocolVersion(_))
            ));
        }
        let too_long = "x".repeat(65);
        assert!(matches!(
            consumer_http_offer("127.0.0.1:8787".parse().unwrap(), &too_long),
            Err(ConsumerOfferError::InvalidProtocolVersion(_))
        ));
    }
}
