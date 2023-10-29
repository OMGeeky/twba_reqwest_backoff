use std::ops::Deref;

use chrono::{DateTime, NaiveDateTime, Utc};
use reqwest::{Error, Request, Response};
use url::Host;

use prelude::*;

const MAX_BACKOFF_ATTEMPTS: u32 = 50;
const MAX_BACKOFF_ATTEMPTS_GOOGLE: u32 = 50;
const MAX_BACKOFF_ATTEMPTS_TWITCH: u32 = 50;

const GOOGLE_BASE_BACKOFF_TIME_S: u64 = 2;
const GOOGLE_MAX_BACKOFF_TIME_S: u64 = 3600;

pub mod prelude;

#[derive(Debug, thiserror::Error)]
pub enum ReqwestBackoffError {
    #[error("Reqwest error")]
    Reqwest(#[from] Error),
    #[error("Other error")]
    Other(#[from] Box<dyn StdError + Send + Sync>),
    #[error("Backoff error after {backoff_attempts} attempts")]
    BackoffExceeded { backoff_attempts: u32 },
}

#[derive(Debug, Clone)]
pub struct ReqwestClient {
    client: reqwest::Client,
}

impl Deref for ReqwestClient {
    type Target = reqwest::Client;

    fn deref(&self) -> &Self::Target {
        &self.client
    }
}

impl From<reqwest::Client> for ReqwestClient {
    fn from(client: reqwest::Client) -> Self {
        Self { client }
    }
}

impl Default for ReqwestClient {
    fn default() -> Self {
        Self::new()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum HostType {
    Twitch,
    Google,
    Youtube,
    Other,
}

impl ReqwestClient {
    pub fn new() -> Self {
        Self {
            client: reqwest::Client::new(),
        }
    }
    #[tracing::instrument]
    pub async fn execute_with_backoff(&self, request: Request) -> Result<Response> {
        let host: HostType = get_host_from_request(&request);

        let request_clone = request.try_clone();
        if let Some(request_clone) = request_clone {
            self.execute_with_backoff_inner(request_clone, host).await
        } else {
            warn!("Failed to clone request. No backoff possible.");
            Ok(self
                .client
                .execute(request)
                .await
                .map_err(ReqwestBackoffError::Reqwest)?)
        }
    }

    /// Execute a request with backoff if the response indicates that it should.
    ///
    /// # Arguments  
    ///
    /// * `self` - The client to use for the request.
    /// * `request` - The request to execute. This needs to be cloneable otherwise the function will panic. (not cloneable requests can't be retried)
    /// * `host` - The host of the request. This is used to determine the backoff time.
    async fn execute_with_backoff_inner(
        &self,
        request: Request,
        host: HostType,
    ) -> Result<Response> {
        let mut attempt: u32 = 1;
        let mut response = self
            .execute(request.try_clone().unwrap())
            .await
            .map_err(ReqwestBackoffError::Reqwest)?;
        while check_response_is_backoff(&response, host) {
            if is_backoff_limit_reached(attempt, host) {
                return Err(ReqwestBackoffError::BackoffExceeded {
                    backoff_attempts: attempt,
                });
            }
            let sleep_duration = get_backoff_time(&response, host, attempt)?;
            info!("Sleeping for {} seconds", sleep_duration);
            tokio::time::sleep(std::time::Duration::from_secs(sleep_duration)).await;
            attempt += 1;
            info!("Backoff attempt #{}", attempt);
            response = self
                .client
                .execute(request.try_clone().unwrap())
                .await
                .map_err(ReqwestBackoffError::Reqwest)?;
        }
        Ok(response)
    }
}

#[tracing::instrument]
fn get_host_from_request(request: &Request) -> HostType {
    if let Some(Host::Domain(domain)) = request.url().host() {
        match domain {
            "twitch.tv" => HostType::Twitch,
            "google.com" => HostType::Google,
            "youtube.com" => HostType::Youtube,
            _ => HostType::Other,
        }
    } else {
        HostType::Other
    }
}

#[tracing::instrument]
fn is_backoff_limit_reached(attempt: u32, host: HostType) -> bool {
    match host {
        HostType::Twitch => attempt > MAX_BACKOFF_ATTEMPTS_TWITCH,
        HostType::Google | HostType::Youtube => attempt > MAX_BACKOFF_ATTEMPTS_GOOGLE,
        HostType::Other => attempt > MAX_BACKOFF_ATTEMPTS,
    }
}

#[tracing::instrument]
fn check_response_is_backoff(response: &Response, host: HostType) -> bool {
    // dbg!(response, host);
    let code = response.status();
    if code.is_success() {
        return false;
    }
    let code = code.as_u16();
    match host {
        HostType::Twitch => code == 429,
        HostType::Google | HostType::Youtube => {
            if !(code == 403 || code == 400) {
                return false;
            }
            warn!("check_response_is_backoff->code: {}", code);
            warn!("check_response_is_backoff->response: {:?}", response);
            true
        }
        HostType::Other => false,
    }
}

#[tracing::instrument]
fn get_backoff_time(response: &Response, host: HostType, attempt: u32) -> Result<u64> {
    // dbg!(response, host);
    Ok(match host {
        HostType::Twitch => {
            let timestamp = get_twitch_rate_limit_value(response)?;
            let duration = chrono::Local::now().naive_utc().and_utc() - timestamp;
            let duration = duration.num_seconds() as u64;
            if duration > 0 {
                duration
            } else {
                1
            }
        }
        HostType::Google | HostType::Youtube => {
            let backoff_time = GOOGLE_BASE_BACKOFF_TIME_S.pow(attempt);
            if backoff_time > GOOGLE_MAX_BACKOFF_TIME_S {
                GOOGLE_MAX_BACKOFF_TIME_S
            } else {
                backoff_time
            }
        }
        HostType::Other => 5,
    })
}

#[tracing::instrument]
fn get_twitch_rate_limit_value(response: &Response) -> Result<DateTime<Utc>> {
    let timestamp = response
        .headers()
        .get("Ratelimit-Reset")
        .unwrap()
        .to_str()
        .map_err(|e| ReqwestBackoffError::Other(e.into()))?
        .to_string()
        .parse::<i64>()
        .map_err(|e| ReqwestBackoffError::Other(e.into()))?;
    let timestamp = NaiveDateTime::from_timestamp_opt(timestamp, 0).ok_or(
        ReqwestBackoffError::Other("Could not convert the provided timestamp".into()),
    )?;
    Ok(timestamp.and_utc())
}
