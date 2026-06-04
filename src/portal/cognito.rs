//! AWS Cognito `USER_SRP_AUTH` login for the new
//! `monitoring.solaredge.com/services/...` platform.
//!
//! The dashboard energy endpoint (see [`super::client::PortalClient::fetch_battery_energy`])
//! is gated by an AWS Cognito JWT carried in the `se_monitoring_auth` cookie —
//! the old HTTP Basic / Spring session does **not** authenticate it. This module
//! performs the SRP handshake (the crypto is done by the `aws-cognito-srp`
//! crate; we only do the two HTTP calls) and returns the access token to use as
//! that cookie.

use aws_cognito_srp::{SrpClient, User};
use serde::Deserialize;

use super::client::{PortalError, truncate};

const COGNITO_IDP_URL: &str = "https://cognito-idp.eu-central-1.amazonaws.com/";
/// SolarEdge's Cognito user pool + SPA app client backing the new
/// `monitoring.solaredge.com/services/...` platform. Captured from the web
/// app's `se_monitoring_auth` JWT (2026-06). If SolarEdge rotates these, the
/// dashboard energy fetch starts returning Cognito errors — re-capture from a
/// browser login and update here.
const POOL_ID: &str = "eu-central-1_fVUTz39em";
const CLIENT_ID: &str = "ugfnsujd3384sshcjehaphlh3";

/// Access token plus its lifetime (seconds) as reported by Cognito.
pub struct CognitoTokens {
    pub access_token: String,
    pub expires_in: i64,
}

#[derive(Debug, Deserialize)]
struct InitiateAuthResponse {
    #[serde(rename = "ChallengeName", default)]
    challenge_name: String,
    #[serde(rename = "ChallengeParameters", default)]
    challenge_parameters: ChallengeParameters,
}

#[derive(Debug, Default, Deserialize)]
struct ChallengeParameters {
    #[serde(rename = "SALT", default)]
    salt: String,
    #[serde(rename = "SECRET_BLOCK", default)]
    secret_block: String,
    #[serde(rename = "SRP_B", default)]
    srp_b: String,
    #[serde(rename = "USER_ID_FOR_SRP", default)]
    user_id_for_srp: String,
}

#[derive(Debug, Deserialize)]
struct RespondResponse {
    #[serde(rename = "AuthenticationResult")]
    authentication_result: Option<AuthenticationResult>,
    #[serde(rename = "ChallengeName", default)]
    challenge_name: String,
}

#[derive(Debug, Deserialize)]
struct AuthenticationResult {
    #[serde(rename = "AccessToken", default)]
    access_token: String,
    #[serde(rename = "ExpiresIn", default)]
    expires_in: i64,
}

/// Perform the full SRP login and return the `se_monitoring_auth` access token.
/// Reuses the supplied reqwest client (its cookie jar is irrelevant here — the
/// Cognito IDP endpoint is token-based and lives on `amazonaws.com`).
pub async fn login(
    http: &reqwest::Client,
    username: &str,
    password: &str,
) -> Result<CognitoTokens, PortalError> {
    let srp = SrpClient::new(User::new(POOL_ID, username, password), CLIENT_ID, None);
    let params = srp.get_auth_parameters();

    // Step 1 — InitiateAuth: send SRP_A, receive the PASSWORD_VERIFIER challenge.
    let init: InitiateAuthResponse = cognito_call(
        http,
        "AWSCognitoIdentityProviderService.InitiateAuth",
        serde_json::json!({
            "AuthFlow": "USER_SRP_AUTH",
            "ClientId": CLIENT_ID,
            "AuthParameters": { "USERNAME": params.username, "SRP_A": params.a },
        }),
    )
    .await?;
    if init.challenge_name != "PASSWORD_VERIFIER" {
        return Err(PortalError::CognitoAuth(format!(
            "expected PASSWORD_VERIFIER challenge, got {:?}",
            init.challenge_name
        )));
    }
    let cp = init.challenge_parameters;

    // Step 2 — compute the password proof from the server's salt/B/secret block.
    let v = srp
        .verify(&cp.secret_block, &cp.user_id_for_srp, &cp.salt, &cp.srp_b)
        .map_err(|e| PortalError::CognitoAuth(format!("SRP verify failed: {e}")))?;

    // Step 3 — RespondToAuthChallenge: submit the proof, receive the tokens.
    let resp: RespondResponse = cognito_call(
        http,
        "AWSCognitoIdentityProviderService.RespondToAuthChallenge",
        serde_json::json!({
            "ChallengeName": "PASSWORD_VERIFIER",
            "ClientId": CLIENT_ID,
            "ChallengeResponses": {
                "USERNAME": cp.user_id_for_srp,
                "PASSWORD_CLAIM_SECRET_BLOCK": v.password_claim_secret_block,
                "PASSWORD_CLAIM_SIGNATURE": v.password_claim_signature,
                "TIMESTAMP": v.timestamp,
            },
        }),
    )
    .await?;

    let result = resp.authentication_result.ok_or_else(|| {
        PortalError::CognitoAuth(format!(
            "no AuthenticationResult (further challenge: {:?})",
            resp.challenge_name
        ))
    })?;
    if result.access_token.is_empty() {
        return Err(PortalError::CognitoAuth("empty AccessToken".into()));
    }
    Ok(CognitoTokens {
        access_token: result.access_token,
        expires_in: result.expires_in,
    })
}

async fn cognito_call<T: serde::de::DeserializeOwned>(
    http: &reqwest::Client,
    target: &'static str,
    body: serde_json::Value,
) -> Result<T, PortalError> {
    let resp = http
        .post(COGNITO_IDP_URL)
        .header(reqwest::header::CONTENT_TYPE, "application/x-amz-json-1.1")
        .header("X-Amz-Target", target)
        .json(&body)
        .send()
        .await?;
    let status = resp.status();
    let text = resp.text().await?;
    if !status.is_success() {
        // Cognito returns its error type/message in the body (e.g.
        // NotAuthorizedException). Don't log it at higher levels — it can
        // echo request material — but surface a truncated form for diagnosis.
        return Err(PortalError::CognitoAuth(format!(
            "HTTP {status} from {target}: {}",
            truncate(&text)
        )));
    }
    serde_json::from_str(&text).map_err(|e| PortalError::Json {
        endpoint: "cognito-idp",
        source: e,
    })
}
