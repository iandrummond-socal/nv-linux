//! OAuth PKCE with a pasted authorization code (no redirect URI).

use base64::Engine;
use serde::Deserialize;
use sha2::{Digest, Sha256};

pub use super::Endpoints;
use super::{transport, Response};
use crate::error::{Error, Result};
use crate::remote::{RemoteError, RemoteResult};
use crate::secret::Secret;

#[derive(Debug)]
pub struct Pkce {
    pub verifier: Secret,
    pub challenge: String,
}

impl Pkce {
    pub fn generate() -> Result<Self> {
        const ALPHABET: &[u8] =
            b"0123456789abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQRSTUVWXYZ-._~";
        let mut verifier = String::with_capacity(64);
        while verifier.len() < 64 {
            let mut random = [0; 64];
            getrandom::fill(&mut random)
                .map_err(|_| Error::Auth("Unable to generate PKCE randomness".into()))?;
            for byte in random {
                // 198 is the largest multiple of 66 below 256.
                if byte < 198 {
                    verifier.push(char::from(ALPHABET[usize::from(byte) % ALPHABET.len()]));
                    if verifier.len() == 64 {
                        break;
                    }
                }
            }
        }
        Ok(Self::from_verifier(Secret::new(verifier)))
    }

    pub fn from_verifier(verifier: Secret) -> Self {
        let digest: [u8; 32] = Sha256::digest(verifier.expose().as_bytes()).into();
        let challenge = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(digest);
        Self {
            verifier,
            challenge,
        }
    }
}

fn percent_encode(value: &str) -> String {
    let mut result = String::new();
    for byte in value.bytes() {
        if byte.is_ascii_alphanumeric() || b"-._~".contains(&byte) {
            result.push(char::from(byte));
        } else {
            use std::fmt::Write;
            write!(result, "%{byte:02X}").expect("writing to String");
        }
    }
    result
}

pub fn authorize_url(ep: &Endpoints, app_key: &str, challenge: &str) -> String {
    format!(
        "{}?client_id={}&response_type=code&code_challenge={}&code_challenge_method=S256&token_access_type=offline",
        ep.authorize, percent_encode(app_key), percent_encode(challenge)
    )
}

#[derive(Debug)]
pub struct TokenResponse {
    pub access_token: Secret,
    pub refresh_token: Option<Secret>,
    pub expires_in: u64,
    pub account_id: Option<String>,
}

#[derive(Deserialize)]
struct RawTokenResponse {
    access_token: String,
    refresh_token: Option<String>,
    expires_in: u64,
    account_id: Option<String>,
}

fn token_response(mut response: Response, refreshing: bool) -> RemoteResult<TokenResponse> {
    let status = response.status().as_u16();
    if !(200..300).contains(&status) {
        let value = response.body_mut().read_json::<serde_json::Value>().ok();
        let error = value.as_ref().and_then(|v| v["error"].as_str());
        if status == 401 || (status == 400 && (refreshing || error == Some("invalid_grant"))) {
            return Err(RemoteError::Unauthorized);
        }
        // Only the protocol error identifier is included, never descriptions or bodies.
        let error = error.filter(|s| {
            !s.is_empty() && s.len() <= 64 && s.bytes().all(|b| b.is_ascii_lowercase() || b == b'_')
        });
        return Err(RemoteError::Other(match error {
            Some(error) => format!("HTTP {status}: {error}"),
            None => format!("HTTP {status}"),
        }));
    }
    let body = response.body_mut().read_to_string().map_err(transport)?;
    let raw: RawTokenResponse = serde_json::from_str(&body)
        .map_err(|_| RemoteError::Other("Invalid token response".into()))?;
    Ok(TokenResponse {
        access_token: Secret::new(raw.access_token),
        refresh_token: raw.refresh_token.map(Secret::new),
        expires_in: raw.expires_in,
        account_id: raw.account_id,
    })
}

pub fn exchange_code(
    agent: &ureq::Agent,
    ep: &Endpoints,
    app_key: &str,
    code: &Secret,
    verifier: &Secret,
) -> RemoteResult<TokenResponse> {
    let response = agent
        .post(format!("{}/oauth2/token", ep.api))
        .send_form([
            ("code", code.expose().trim()),
            ("grant_type", "authorization_code"),
            ("code_verifier", verifier.expose()),
            ("client_id", app_key),
        ])
        .map_err(transport)?;
    token_response(response, false)
}

pub fn refresh(
    agent: &ureq::Agent,
    ep: &Endpoints,
    app_key: &str,
    refresh_token: &Secret,
) -> RemoteResult<TokenResponse> {
    let response = agent
        .post(format!("{}/oauth2/token", ep.api))
        .send_form([
            ("grant_type", "refresh_token"),
            ("refresh_token", refresh_token.expose()),
            ("client_id", app_key),
        ])
        .map_err(transport)?;
    token_response(response, true)
}

pub fn revoke(agent: &ureq::Agent, ep: &Endpoints, access: &Secret) -> RemoteResult<()> {
    let response = agent
        .post(format!("{}/2/auth/token/revoke", ep.api))
        .header("Authorization", &format!("Bearer {}", access.expose()))
        .send_empty()
        .map_err(transport)?;
    super::api::check_response(response).map(|_| ())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pkce_vector_and_random_verifier() {
        let pkce = Pkce::from_verifier(Secret::new("dBjftJeZ4CVP-mJ0kHkfYlxqKyLpo4iM_I9ihu-fgBR8"));
        // Computed independently with openssl (sha256 -> base64url, no padding).
        assert_eq!(
            pkce.challenge,
            "5v4WL6bCKYn2BhQ3kZ4HAphwi0kRr-eXZv4YZRyu_44"
        );
        let generated = Pkce::generate().unwrap();
        assert_eq!(generated.verifier.expose().len(), 64);
        assert!(generated
            .verifier
            .expose()
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"-._~".contains(&b)));
    }

    #[test]
    fn authorization_url() {
        let url = authorize_url(&Endpoints::dropbox(), "key +", "challenge/");
        assert!(url.contains("client_id=key%20%2B"));
        assert!(url.contains("code_challenge=challenge%2F"));
        assert!(url.contains("token_access_type=offline"));
        assert!(url.contains("code_challenge_method=S256"));
        assert!(!url.contains("redirect_uri"));
    }
}
