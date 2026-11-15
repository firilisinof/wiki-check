//! OAuth 2.1 resource-server checks, adapted from the remote MCP shell that runs
//! on the same host: github.com/stepbrobd/inc, pkgs/mcp, MIT License,
//! Copyright (c) 2026 Yifei Sun.
//!
//! Every request carries an RFC 9068 access token from Kanidm. It is validated
//! locally against the issuer's keys. This process never issues tokens.

use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use axum::body::Body;
use axum::extract::State;
use axum::http::{HeaderValue, Request, StatusCode, header};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use jsonwebtoken::jwk::JwkSet;
use jsonwebtoken::{Algorithm, DecodingKey, Validation, decode, decode_header};
use serde::Deserialize;
use serde_json::json;
use tokio::sync::RwLock;

const ACCESS_TOKEN_TYPE: &str = "at+jwt";
const KEY_FETCH_TIMEOUT: Duration = Duration::from_secs(10);
const KEY_REFRESH_INTERVAL: Duration = Duration::from_secs(3600);
const CLOCK_LEEWAY_SECONDS: u64 = 60;

#[derive(Debug, Clone)]
pub struct AuthSettings {
    pub issuer: String,
    pub jwks_uri: String,
    pub audience: String,
    pub required_scope: String,
    /// the public URL of the MCP endpoint
    pub resource: String,
    pub resource_metadata_url: String,
}

#[derive(Clone)]
pub struct Authenticator {
    settings: Arc<AuthSettings>,
    keys: Arc<RwLock<JwkSet>>,
}

#[derive(Debug, Deserialize)]
struct Claims {
    #[serde(default)]
    scope: String,
}

/// A missing credential starts the OAuth handshake. It is reported apart from
/// a token that was presented and failed.
#[derive(Debug, PartialEq, Eq)]
enum Denial {
    Missing,
    Invalid,
    InsufficientScope,
}

fn fetch_keys(uri: &str) -> Result<JwkSet> {
    let tls = ureq::tls::TlsConfig::builder()
        .root_certs(ureq::tls::RootCerts::PlatformVerifier)
        .unversioned_rustls_crypto_provider(Arc::new(rustls::crypto::ring::default_provider()))
        .build();
    let agent: ureq::Agent = ureq::Agent::config_builder()
        .timeout_global(Some(KEY_FETCH_TIMEOUT))
        .tls_config(tls)
        .build()
        .into();
    let body = agent
        .get(uri)
        .call()
        .with_context(|| format!("fetching signing keys from {uri}"))?
        .body_mut()
        .read_to_string()
        .context("reading signing keys")?;
    serde_json::from_str(&body).context("parsing signing keys")
}

impl Authenticator {
    /// Fetch the issuer's keys and keep them fresh across key rotations.
    pub async fn start(settings: AuthSettings) -> Result<Self> {
        let uri = settings.jwks_uri.clone();
        let keys = tokio::task::spawn_blocking(move || fetch_keys(&uri)).await??;
        let authenticator = Self {
            settings: Arc::new(settings),
            keys: Arc::new(RwLock::new(keys)),
        };
        let refreshed = authenticator.clone();
        tokio::spawn(async move {
            loop {
                tokio::time::sleep(KEY_REFRESH_INTERVAL).await;
                let uri = refreshed.settings.jwks_uri.clone();
                match tokio::task::spawn_blocking(move || fetch_keys(&uri)).await {
                    Ok(Ok(keys)) => *refreshed.keys.write().await = keys,
                    Ok(Err(error)) => {
                        tracing::warn!(error = %format!("{error:#}"), "signing key refresh failed")
                    }
                    Err(error) => tracing::warn!(error = %error, "signing key refresh task failed"),
                }
            }
        });
        Ok(authenticator)
    }

    #[cfg(test)]
    fn with_keys(settings: AuthSettings, keys: JwkSet) -> Self {
        Self {
            settings: Arc::new(settings),
            keys: Arc::new(RwLock::new(keys)),
        }
    }

    async fn validate(&self, authorization: Option<&HeaderValue>) -> Result<(), Denial> {
        let raw = authorization
            .and_then(|value| value.to_str().ok())
            .ok_or(Denial::Missing)?;
        let token = raw.strip_prefix("Bearer ").ok_or(Denial::Invalid)?;
        let header = decode_header(token).map_err(|_| Denial::Invalid)?;
        if header.alg != Algorithm::ES256 || header.typ.as_deref() != Some(ACCESS_TOKEN_TYPE) {
            return Err(Denial::Invalid);
        }
        let key_id = header.kid.ok_or(Denial::Invalid)?;
        let key = {
            let keys = self.keys.read().await;
            let key = keys.find(&key_id).ok_or(Denial::Invalid)?;
            DecodingKey::from_jwk(key).map_err(|_| Denial::Invalid)?
        };
        let mut validation = Validation::new(Algorithm::ES256);
        validation.set_issuer(&[self.settings.issuer.as_str()]);
        validation.set_audience(&[self.settings.audience.as_str()]);
        validation.set_required_spec_claims(&["exp", "iss", "aud", "sub"]);
        validation.leeway = CLOCK_LEEWAY_SECONDS;
        let claims = decode::<Claims>(token, &key, &validation)
            .map_err(|_| Denial::Invalid)?
            .claims;
        if !claims
            .scope
            .split_whitespace()
            .any(|scope| scope == self.settings.required_scope)
        {
            return Err(Denial::InsufficientScope);
        }
        Ok(())
    }

    fn challenge(&self, error: Option<&str>) -> HeaderValue {
        let mut value = format!(
            "Bearer resource_metadata=\"{}\", scope=\"{}\"",
            self.settings.resource_metadata_url, self.settings.required_scope
        );
        if let Some(error) = error {
            value.push_str(&format!(", error=\"{error}\""));
        }
        HeaderValue::from_str(&value).expect("challenge is ASCII")
    }

    pub fn resource_metadata(&self) -> serde_json::Value {
        json!({
            "resource": self.settings.resource,
            "authorization_servers": [self.settings.issuer],
            "scopes_supported": [self.settings.required_scope],
            "bearer_methods_supported": ["header"],
        })
    }
}

pub async fn require_token(
    State(authenticator): State<Authenticator>,
    request: Request<Body>,
    next: Next,
) -> Response {
    match authenticator
        .validate(request.headers().get(header::AUTHORIZATION))
        .await
    {
        Ok(()) => next.run(request).await,
        Err(denial) => {
            let (status, error) = match denial {
                Denial::Missing => (StatusCode::UNAUTHORIZED, None),
                Denial::Invalid => (StatusCode::UNAUTHORIZED, Some("invalid_token")),
                Denial::InsufficientScope => (StatusCode::FORBIDDEN, Some("insufficient_scope")),
            };
            if denial != Denial::Missing {
                tracing::warn!(
                    operation = "authenticate",
                    reason = error.unwrap_or(""),
                    path = request.uri().path(),
                    "request denied"
                );
            }
            (
                status,
                [(header::WWW_AUTHENTICATE, authenticator.challenge(error))],
            )
                .into_response()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn authenticator() -> Authenticator {
        Authenticator::with_keys(
            AuthSettings {
                issuer: "https://auth.example/oauth2/openid/wiki".to_string(),
                jwks_uri: String::new(),
                audience: "wiki".to_string(),
                required_scope: "wiki".to_string(),
                resource: "https://wiki.example/mcp".to_string(),
                resource_metadata_url:
                    "https://wiki.example/.well-known/oauth-protected-resource/mcp".to_string(),
            },
            JwkSet { keys: Vec::new() },
        )
    }

    #[tokio::test]
    async fn requests_without_a_valid_token_are_denied() {
        let authenticator = authenticator();
        assert_eq!(authenticator.validate(None).await, Err(Denial::Missing));
        for value in ["Basic abc", "Bearer not-a-token", "Bearer a.b.c"] {
            let header = HeaderValue::from_static(value);
            assert_eq!(
                authenticator.validate(Some(&header)).await,
                Err(Denial::Invalid),
                "{value}"
            );
        }
        // an unsigned token must not pass, whatever its claims say
        let unsigned =
            "Bearer eyJhbGciOiJub25lIiwidHlwIjoiYXQrand0In0.eyJzdWIiOiJ4Iiwic2NvcGUiOiJ3aWtpIn0.";
        let header = HeaderValue::from_static(unsigned);
        assert_eq!(
            authenticator.validate(Some(&header)).await,
            Err(Denial::Invalid)
        );
    }

    /// a throwaway P-256 key generated for these tests and used nowhere else
    const TEST_KEY: &str = "-----BEGIN PRIVATE KEY-----
MIGHAgEAMBMGByqGSM49AgEGCCqGSM49AwEHBG0wawIBAQQgGG+SwjveqRmMyjMT
Wptbi1RWIpHe/eQ7GIgj9kdZqGShRANCAAQBpIO0vu1UQfULIQ861a0WYlaUcJzv
g1ZsvnD8nmIkiXBO+2YQNnY3259f1Cr+g/Kn8INJxyZTgIyPUND5/9I2
-----END PRIVATE KEY-----
";

    fn signed(claims: serde_json::Value) -> (Authenticator, HeaderValue) {
        use jsonwebtoken::{EncodingKey, Header, encode, jwk::Jwk};
        let key = EncodingKey::from_ec_pem(TEST_KEY.as_bytes()).unwrap();
        let mut jwk = Jwk::from_encoding_key(&key, Algorithm::ES256).unwrap();
        jwk.common.key_id = Some("test-key".to_string());
        let mut header = Header::new(Algorithm::ES256);
        header.typ = Some(ACCESS_TOKEN_TYPE.to_string());
        header.kid = Some("test-key".to_string());
        let token = encode(&header, &claims, &key).unwrap();
        let mut authenticator = authenticator();
        authenticator.keys = Arc::new(RwLock::new(JwkSet { keys: vec![jwk] }));
        (
            authenticator,
            HeaderValue::from_str(&format!("Bearer {token}")).unwrap(),
        )
    }

    fn claims() -> serde_json::Value {
        let expiry = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs()
            + 600;
        json!({
            "iss": "https://auth.example/oauth2/openid/wiki",
            "aud": "wiki",
            "sub": "user",
            "exp": expiry,
            "scope": "openid wiki",
        })
    }

    #[tokio::test]
    async fn a_signed_token_passes_only_with_matching_claims() {
        let (authenticator, header) = signed(claims());
        assert_eq!(authenticator.validate(Some(&header)).await, Ok(()));

        let mut cases = Vec::new();
        for (field, value) in [
            ("aud", json!("another-client")),
            ("iss", json!("https://auth.example/oauth2/openid/other")),
            ("exp", json!(1_000_000)),
        ] {
            let mut altered = claims();
            altered[field] = value;
            cases.push((field, altered, Denial::Invalid));
        }
        let mut unscoped = claims();
        unscoped["scope"] = json!("openid mcp:shell");
        cases.push(("scope", unscoped, Denial::InsufficientScope));
        for (field, altered, denial) in cases {
            let (authenticator, header) = signed(altered);
            assert_eq!(
                authenticator.validate(Some(&header)).await,
                Err(denial),
                "{field}"
            );
        }

        // a token for an unknown key is rejected even when its claims are right
        let (_, header) = signed(claims());
        assert_eq!(authenticator.validate(Some(&header)).await, Ok(()));
        let empty = self::authenticator();
        assert_eq!(empty.validate(Some(&header)).await, Err(Denial::Invalid));
    }

    #[test]
    fn the_challenge_points_at_the_resource_metadata() {
        let authenticator = authenticator();
        let challenge = authenticator.challenge(Some("invalid_token"));
        let text = challenge.to_str().unwrap();
        assert!(text.contains("resource_metadata=\"https://wiki.example/.well-known/"));
        assert!(text.contains("error=\"invalid_token\""));
        assert_eq!(
            authenticator.resource_metadata()["resource"],
            "https://wiki.example/mcp"
        );
    }
}
