use std::sync::Arc;

use reqwest::cookie::Jar;
use reqwest::{StatusCode, Url};
use thiserror::Error;
use tracing::{debug, info};

use crate::portal::cognito;
use crate::portal::models::{
    DashboardEnergyResponse, EnergyGraphResponse, LayoutNodeV2, OptimizersInfoResponse,
};

const USER_AGENT: &str = "Mozilla/5.0 (X11; Linux x86_64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/120.0.0.0 Safari/537.36";
const ORIGIN: &str = "https://monitoring.solaredge.com";
/// Fixed lower bound for the battery charge/discharge query window. Any date
/// before the battery's commissioning works (PV-only years contribute zero
/// battery flow); keeping it fixed makes the resulting cumulative totals
/// monotonic. 2015 predates the SolarEdge Home Battery line.
const BATTERY_ENERGY_START_DATE: &str = "2015-01-01";
/// Fixed lower bound for the per-optimizer lifetime energy window, same
/// rationale as `BATTERY_ENERGY_START_DATE`: a wide fixed window keeps the
/// returned cumulative total monotonic across refreshes.
const OPTIMIZER_ENERGY_START_DATE: &str = "2010-01-01";

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
    #[error("Cognito auth failed: {0}")]
    CognitoAuth(String),
    #[error(
        "{endpoint} returned HTTP 200 but no usable data — schema may have changed; body: {body}"
    )]
    EmptyResponse {
        endpoint: &'static str,
        body: String,
    },
    #[error("failed to build HTTP client")]
    BuildClient(#[source] reqwest::Error),
    #[error("failed to parse response body: {0}")]
    Parse(String),
}

pub struct PortalClient {
    site_id: u64,
    creds: Credentials,
    http: reqwest::Client,
    jar: Arc<Jar>,
    /// Cached `se_monitoring_auth` Cognito token for the `/services/` API,
    /// refreshed via SRP when missing or expired.
    se_monitoring_auth: tokio::sync::Mutex<Option<SeMonitoringAuth>>,
}

struct SeMonitoringAuth {
    access_token: String,
    expires_at: jiff::Timestamp,
}

impl PortalClient {
    pub fn new(site_id: u64, creds: Credentials) -> Result<Self, PortalError> {
        let jar = Arc::new(Jar::default());
        let http = reqwest::Client::builder()
            .user_agent(USER_AGENT)
            .cookie_provider(jar.clone())
            .build()
            .map_err(PortalError::BuildClient)?;
        Ok(Self {
            site_id,
            creds,
            http,
            jar,
            se_monitoring_auth: tokio::sync::Mutex::new(None),
        })
    }

    /// Ensure a valid `se_monitoring_auth` cookie is in the jar, performing the
    /// Cognito SRP login when the cached token is missing or about to expire.
    /// The access token is long-lived (~24 h), so this hits Cognito at most
    /// once a day; every other refresh cycle is a no-op.
    ///
    /// Returns the access token so callers can also send it as a bearer header —
    /// the `/services/layout/` endpoints authenticate on `Authorization`, while
    /// `/services/dashboard/` accepts the cookie. We send both everywhere.
    ///
    /// `fresh_login` reports whether this call actually performed an SRP
    /// handshake, so the caller can count real logins.
    async fn ensure_se_monitoring_auth(&self) -> Result<ServicesAuth, PortalError> {
        let mut guard = self.se_monitoring_auth.lock().await;
        let now = jiff::Timestamp::now();
        if let Some(s) = guard.as_ref()
            && s.expires_at > now
        {
            return Ok(ServicesAuth {
                access_token: s.access_token.clone(),
                fresh_login: false,
            });
        }
        let tokens = cognito::login(
            &self.http,
            &self.creds.username,
            self.creds.password.expose(),
        )
        .await?;
        // Renew a touch early; clamp pathologically short lifetimes.
        let ttl = (tokens.expires_in - 60).max(60);
        let expires_at = now
            .checked_add(jiff::SignedDuration::from_secs(ttl))
            .unwrap_or(now);
        let url = Url::parse(ORIGIN).map_err(|e| PortalError::Parse(e.to_string()))?;
        self.jar
            .add_cookie_str(&format!("se_monitoring_auth={}", tokens.access_token), &url);
        *guard = Some(SeMonitoringAuth {
            access_token: tokens.access_token.clone(),
            expires_at,
        });
        info!("obtained se_monitoring_auth via Cognito SRP");
        Ok(ServicesAuth {
            access_token: tokens.access_token,
            fresh_login: true,
        })
    }

    /// Common header set for the `/services/` platform. The new API is stricter
    /// than the retired one: it wants the bearer token, JSON content
    /// negotiation, and a same-origin `Origin`/`Referer` pair.
    fn services_request(
        &self,
        builder: reqwest::RequestBuilder,
        token: &str,
    ) -> reqwest::RequestBuilder {
        builder
            .bearer_auth(token)
            .header(reqwest::header::ACCEPT, "application/json")
            .header(reqwest::header::ORIGIN, ORIGIN)
            .header(reqwest::header::REFERER, format!("{ORIGIN}/"))
    }

    /// Read a `/services/` JSON response, mapping non-2xx and decode failures
    /// onto `PortalError` with a stable endpoint label.
    async fn services_json<T: serde::de::DeserializeOwned>(
        resp: reqwest::Response,
        endpoint: &'static str,
    ) -> Result<T, PortalError> {
        let status = resp.status();
        let text = resp.text().await?;
        debug!(
            endpoint,
            status = %status,
            body = text.as_str(),
            "portal response"
        );
        if !status.is_success() {
            return Err(PortalError::Status {
                endpoint,
                status,
                body: truncate(&text),
            });
        }
        // `/services/` returns clean JSON — no need for the junk-tolerant
        // scanner the retired `systemData` endpoint required.
        serde_json::from_str(&text).map_err(|e| PortalError::Json {
            endpoint,
            source: e,
        })
    }

    /// The site's logical layout (inverter → string → optimizer) from the ONE
    /// platform. Replaces the retired
    /// `GET /solaredge-apigw/api/sites/{id}/layout/logical`, which has returned
    /// HTTP 410 Gone since ~2026-07-21.
    pub async fn fetch_site_structure(&self) -> Result<LayoutNodeV2, PortalError> {
        let auth = self.ensure_se_monitoring_auth().await?;
        let url = format!(
            "{ORIGIN}/services/layout/logical/generic/v2/site/{}?include-optimizers=true",
            self.site_id
        );
        let resp = self
            .services_request(self.http.get(&url), &auth.access_token)
            .send()
            .await?;
        Self::services_json(resp, "layout/v2").await
    }

    /// Live telemetry for every optimizer in **one** request, keyed by serial.
    /// Replaces the retired per-optimizer `systemData` GET (one HTTP call per
    /// optimizer, 18 on this site).
    ///
    /// Retries once on timeout: this is now a single point of failure for the
    /// whole optimizer fleet, so one cheap retry is worth more than it was when
    /// each optimizer had its own request.
    pub async fn fetch_optimizers_live(
        &self,
        serials: &[String],
    ) -> Result<OptimizersInfoResponse, PortalError> {
        let auth = self.ensure_se_monitoring_auth().await?;
        let url = format!("{ORIGIN}/services/layout/information/optimizers");
        let post = || async {
            self.services_request(self.http.post(&url), &auth.access_token)
                .json(&serials)
                .send()
                .await
        };
        let resp = match post().await {
            Err(e) if e.is_timeout() => {
                debug!(error = %e, "optimizer batch timed out; retrying once");
                post().await?
            }
            other => other?,
        };
        Self::services_json(resp, "layout/optimizers").await
    }

    /// Lifetime energy for a **single** optimizer.
    ///
    /// The endpoint's `totalEnergy` is one scalar covering all serials passed in
    /// `optimizer-serials`, so batching would return the site sum with no
    /// per-optimizer attribution. Hence one call per optimizer — the caller
    /// fans these out concurrently.
    pub async fn fetch_optimizer_energy(&self, serial: &str) -> Result<Option<f64>, PortalError> {
        let auth = self.ensure_se_monitoring_auth().await?;
        let today = jiff::Zoned::now().date();
        let url = format!(
            "{ORIGIN}/services/layout/energy-graph/site/{}/optimizers?chart-time-unit=years&start-date={}&end-date={}&optimizer-serials={}",
            self.site_id, OPTIMIZER_ENERGY_START_DATE, today, serial
        );
        let resp = self
            .services_request(self.http.get(&url), &auth.access_token)
            .send()
            .await?;
        let parsed: EnergyGraphResponse = Self::services_json(resp, "layout/energy-graph").await?;
        Ok(parsed.total_energy)
    }

    /// Site-level battery charge/discharge energy from the monitoring
    /// dashboard's energy service. This plugs the gap left by the public
    /// `storageData` API, which reports `lifeTimeEnergyCharged`/`Discharged`
    /// as 0 for the SolarEdge Home Battery 48V.
    ///
    /// We query a wide, fixed window so `summary` reflects the battery's whole
    /// lifetime and the totals stay monotonic across refreshes.
    pub async fn fetch_battery_energy(&self) -> Result<DashboardEnergyResponse, PortalError> {
        // Authenticated by the Cognito token — Basic auth is rejected here.
        let auth = self.ensure_se_monitoring_auth().await?;
        let today = jiff::Zoned::now().date();
        let url = format!(
            "{ORIGIN}/services/dashboard/energy/sites/{}?chart-time-unit=years&start-date={}&end-date={}&measurement-types=production-distribution-with-storage%2Cconsumption-distribution-with-storage&isCniViewer=true",
            self.site_id, BATTERY_ENERGY_START_DATE, today
        );
        let resp = self
            .services_request(self.http.get(&url), &auth.access_token)
            .header("X-Requested-With", "XMLHttpRequest")
            .send()
            .await?;
        let status = resp.status();
        let text = resp.text().await?;
        debug!(
            endpoint = "dashboard/energy",
            status = %status,
            body = text.as_str(),
            "portal response"
        );
        if !status.is_success() {
            return Err(PortalError::Status {
                endpoint: "dashboard/energy",
                status,
                body: truncate(&text),
            });
        }
        let resp: DashboardEnergyResponse =
            serde_json::from_str(&text).map_err(|e| PortalError::Json {
                endpoint: "dashboard/energy",
                source: e,
            })?;
        // HTTP 200 that parses but carries neither charge nor discharge means
        // the unofficial dashboard schema drifted. Surface it (with the body)
        // as an error so the caller logs it and the staleness alert fires,
        // rather than silently freezing the gauges.
        if resp.charged_watt_hours().is_none() && resp.discharged_watt_hours().is_none() {
            return Err(PortalError::EmptyResponse {
                endpoint: "dashboard/energy",
                body: truncate(&text),
            });
        }
        Ok(resp)
    }

    /// Force a Cognito login if none is cached, reporting whether a handshake
    /// actually happened. Used at startup so the `login_count` counter reflects
    /// real SRP logins.
    pub async fn warm_services_auth(&self) -> Result<bool, PortalError> {
        Ok(self.ensure_se_monitoring_auth().await?.fresh_login)
    }
}

/// Outcome of `ensure_se_monitoring_auth`.
struct ServicesAuth {
    access_token: String,
    /// `true` when this call performed an SRP handshake rather than reusing the
    /// cached token.
    fresh_login: bool,
}

pub(crate) fn truncate(s: &str) -> String {
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
    fn secret_debug_redacts() {
        let s = Secret::new("hunter2".to_string());
        assert_eq!(format!("{s:?}"), "<redacted>");
    }
}
