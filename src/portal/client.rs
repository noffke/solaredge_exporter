use std::sync::Arc;

use reqwest::cookie::{CookieStore, Jar};
use reqwest::{StatusCode, Url};
use thiserror::Error;
use tracing::{debug, info};

use crate::portal::models::{EnergyResponse, LayoutResponse, OptimizerData};

const USER_AGENT: &str = "Mozilla/5.0 (X11; Linux x86_64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/120.0.0.0 Safari/537.36";
const ORIGIN: &str = "https://monitoring.solaredge.com";

pub struct Secret(String);

impl Secret {
    pub fn new(s: String) -> Self {
        Self(s)
    }
    pub fn expose(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Debug for Secret {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("<redacted>")
    }
}

#[derive(Debug)]
pub struct Credentials {
    pub username: String,
    pub password: Secret,
}

#[derive(Debug, Error)]
pub enum PortalError {
    #[error("HTTP request failed: {0}")]
    Http(#[from] reqwest::Error),
    #[error("unexpected HTTP {status} from {endpoint}: {body}")]
    Status {
        endpoint: &'static str,
        status: StatusCode,
        body: String,
    },
    #[error("JSON decode error from {endpoint}: {source}")]
    Json {
        endpoint: &'static str,
        #[source]
        source: serde_json::Error,
    },
    #[error("missing CSRF token cookie after login")]
    MissingCsrf,
    #[error("failed to build HTTP client: {0}")]
    BuildClient(String),
    #[error("failed to parse response body: {0}")]
    Parse(String),
}

pub struct PortalClient {
    site_id: u64,
    creds: Credentials,
    http: reqwest::Client,
    jar: Arc<Jar>,
}

impl PortalClient {
    pub fn new(site_id: u64, creds: Credentials) -> Result<Self, PortalError> {
        let jar = Arc::new(Jar::default());
        let http = reqwest::Client::builder()
            .user_agent(USER_AGENT)
            .cookie_provider(jar.clone())
            .build()
            .map_err(|e| PortalError::BuildClient(e.to_string()))?;
        Ok(Self {
            site_id,
            creds,
            http,
            jar,
        })
    }

    /// Warm the cookie jar with JSESSIONID + CSRF-TOKEN. Required before calling
    /// `fetch_energy`, optional otherwise — the layout and publicSystemData
    /// endpoints accept HTTP Basic auth without cookies.
    pub async fn login(&self) -> Result<(), PortalError> {
        let url = format!("{ORIGIN}/solaredge-web/p/login");
        let resp = self
            .http
            .get(&url)
            .basic_auth(&self.creds.username, Some(self.creds.password.expose()))
            .send()
            .await?;
        let status = resp.status();
        let text = resp.text().await?;
        debug!(
            endpoint = "login",
            status = %status,
            body = text.as_str(),
            "portal response"
        );
        if !status.is_success() && !status.is_redirection() {
            return Err(PortalError::Status {
                endpoint: "login",
                status,
                body: truncate(&text),
            });
        }
        info!("logged in to SolarEdge portal");
        Ok(())
    }

    pub async fn fetch_layout(&self) -> Result<LayoutResponse, PortalError> {
        let url = format!(
            "{ORIGIN}/solaredge-apigw/api/sites/{}/layout/logical",
            self.site_id
        );
        let resp = self
            .http
            .get(&url)
            .basic_auth(&self.creds.username, Some(self.creds.password.expose()))
            .send()
            .await?;
        let status = resp.status();
        let text = resp.text().await?;
        debug!(
            endpoint = "layout/logical",
            status = %status,
            body = text.as_str(),
            "portal response"
        );
        if !status.is_success() {
            return Err(PortalError::Status {
                endpoint: "layout/logical",
                status,
                body: truncate(&text),
            });
        }
        serde_json::from_str(&text).map_err(|e| PortalError::Json {
            endpoint: "layout/logical",
            source: e,
        })
    }

    /// Returns `Ok(None)` when the optimizer has no measurements yet (fresh install
    /// or night-time with no historical data), matching the Python reference's
    /// "skip empty `lastMeasurementDate`" behaviour.
    ///
    /// Endpoint matches upstream PR #13 (moved from `monitoringpublic.solaredge.com/publicSystemData`
    /// to `monitoring.solaredge.com/systemData` with a millis cache-buster).
    pub async fn fetch_optimizer(
        &self,
        reporter_id: i64,
    ) -> Result<Option<OptimizerData>, PortalError> {
        let v = jiff::Timestamp::now().as_millisecond();
        // locale=en_US: force English measurement keys (Power/Voltage/Current)
        // and dot-decimal numeric strings; otherwise the portal honours the
        // user's account locale (e.g. German "Leistung"/"252,19").
        let url = format!(
            "{ORIGIN}/solaredge-web/p/systemData?reporterId={reporter_id}&type=panel&activeTab=0&fieldId={}&isPublic=false&locale=en_US&v={v}",
            self.site_id
        );
        let resp = self
            .http
            .get(&url)
            .basic_auth(&self.creds.username, Some(self.creds.password.expose()))
            .send()
            .await?;
        let status = resp.status();
        let text = resp.text().await?;
        debug!(
            endpoint = "systemData",
            reporter_id,
            status = %status,
            body = text.as_str(),
            "portal response"
        );
        if !status.is_success() {
            return Err(PortalError::Status {
                endpoint: "publicSystemData",
                status,
                body: truncate(&text),
            });
        }
        let data: OptimizerData = extract_json(&text).map_err(|e| match e {
            PortalError::Json { source, .. } => PortalError::Json {
                endpoint: "publicSystemData",
                source,
            },
            other => other,
        })?;
        if data.last_measurement_date.trim().is_empty() {
            debug!(reporter_id, "optimizer has no measurements yet; skipping");
            return Ok(None);
        }
        Ok(Some(data))
    }

    pub async fn fetch_energy(&self) -> Result<EnergyResponse, PortalError> {
        // Energy endpoint needs cookies + CSRF header. Login warms the jar.
        self.login().await?;
        let csrf = self.csrf_token().ok_or(PortalError::MissingCsrf)?;
        let url = format!(
            "{ORIGIN}/solaredge-apigw/api/sites/{}/layout/energy?timeUnit=ALL",
            self.site_id
        );
        let resp = self
            .http
            .post(&url)
            .basic_auth(&self.creds.username, Some(self.creds.password.expose()))
            .header(reqwest::header::CONTENT_TYPE, "application/json")
            .header("X-CSRF-Token", csrf)
            .header("X-Requested-With", "XMLHttpRequest")
            .header(
                reqwest::header::REFERER,
                format!("{ORIGIN}/solaredge-web/p/site/{}/", self.site_id),
            )
            .header(reqwest::header::ORIGIN, ORIGIN)
            .send()
            .await?;
        let status = resp.status();
        let text = resp.text().await?;
        debug!(
            endpoint = "layout/energy",
            status = %status,
            body = text.as_str(),
            "portal response"
        );
        if !status.is_success() {
            return Err(PortalError::Status {
                endpoint: "layout/energy",
                status,
                body: truncate(&text),
            });
        }
        serde_json::from_str(&text).map_err(|e| PortalError::Json {
            endpoint: "layout/energy",
            source: e,
        })
    }

    fn csrf_token(&self) -> Option<String> {
        let url = Url::parse(ORIGIN).ok()?;
        let header = CookieStore::cookies(self.jar.as_ref(), &url)?;
        let s = header.to_str().ok()?;
        for kv in s.split(';') {
            let kv = kv.trim();
            if let Some(v) = kv.strip_prefix("CSRF-TOKEN=") {
                return Some(v.to_string());
            }
        }
        None
    }
}

fn truncate(s: &str) -> String {
    const MAX: usize = 500;
    if s.len() <= MAX {
        return s.to_string();
    }
    let mut end = MAX;
    while end > 0 && !s.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}…", &s[..end])
}

/// Parse JSON tolerating leading non-JSON junk (some portal endpoints wrap the
/// payload). Equivalent to the Python reference's `jsonfinder` helper.
fn extract_json<T: serde::de::DeserializeOwned>(text: &str) -> Result<T, PortalError> {
    if let Ok(v) = serde_json::from_str::<T>(text) {
        return Ok(v);
    }
    let bytes = text.as_bytes();
    let start = bytes
        .iter()
        .position(|&b| b == b'{')
        .ok_or_else(|| PortalError::Parse("no JSON object found in response".into()))?;
    let mut depth: i32 = 0;
    let mut in_string = false;
    let mut escape = false;
    for (i, &b) in bytes[start..].iter().enumerate() {
        if in_string {
            if escape {
                escape = false;
            } else if b == b'\\' {
                escape = true;
            } else if b == b'"' {
                in_string = false;
            }
            continue;
        }
        match b {
            b'"' => in_string = true,
            b'{' => depth += 1,
            b'}' => {
                depth -= 1;
                if depth == 0 {
                    let slice = &text[start..start + i + 1];
                    return serde_json::from_str(slice).map_err(|e| PortalError::Json {
                        endpoint: "publicSystemData",
                        source: e,
                    });
                }
            }
            _ => {}
        }
    }
    Err(PortalError::Parse(
        "unbalanced braces while scanning for JSON object".into(),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn truncate_keeps_short_strings() {
        assert_eq!(truncate("hello"), "hello");
    }

    #[test]
    fn truncate_respects_char_boundaries() {
        let s = "a".repeat(600);
        let t = truncate(&s);
        assert!(t.len() > 500);
        assert!(t.ends_with('…'));
    }

    #[test]
    fn extract_json_strips_prefix() {
        #[derive(Debug, serde::Deserialize)]
        struct Foo {
            x: i32,
        }
        let v: Foo = extract_json("garbage{\"x\": 42}").expect("extracts object");
        assert_eq!(v.x, 42);
    }

    #[test]
    fn secret_debug_redacts() {
        let s = Secret::new("hunter2".to_string());
        assert_eq!(format!("{s:?}"), "<redacted>");
    }
}
