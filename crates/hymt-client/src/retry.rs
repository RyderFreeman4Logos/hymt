//! One retry budget covers connection establishment and response-body consumption.

use reqwest::{header::HeaderMap, RequestBuilder, Response};
use serde::de::DeserializeOwned;

use super::{backoff_duration, cold_generation, http_error, is_retryable_status, ClientError};

pub(super) struct RetryBudget {
    maximum: u32,
    used: u32,
}

impl RetryBudget {
    pub(super) fn new(maximum: u32) -> Self {
        Self { maximum, used: 0 }
    }

    pub(super) fn retried(&self) -> bool {
        self.used != 0
    }

    pub(super) async fn wait(&mut self, error: ClientError) -> Result<(), ClientError> {
        if self.used == self.maximum {
            return Err(error);
        }
        let delay = backoff_duration(self.used);
        self.used += 1;
        tokio::time::sleep(delay).await;
        Ok(())
    }
}

fn retryable_send(error: &reqwest::Error) -> bool {
    // TLS verification and invalid connector configuration may be wrapped as
    // connection errors. They cannot heal by replaying the request unchanged.
    let mut cause = std::error::Error::source(error);
    while let Some(source) = cause {
        if let Some(io) = source.downcast_ref::<std::io::Error>() {
            if matches!(
                io.kind(),
                std::io::ErrorKind::InvalidData
                    | std::io::ErrorKind::InvalidInput
                    | std::io::ErrorKind::PermissionDenied
            ) {
                return false;
            }
        }
        cause = source.source();
    }
    error.is_connect() || error.is_timeout() || error.is_request()
}

pub(super) async fn connect(
    request: impl Fn() -> RequestBuilder,
    budget: &mut RetryBudget,
) -> Result<Response, ClientError> {
    loop {
        match request().send().await {
            Ok(response) => {
                let status = response.status().as_u16();
                if status < 400 {
                    return Ok(response);
                }
                // Preserve a permanent HTTP status even if its error body is cut short.
                let body = response.bytes().await.unwrap_or_default();
                let error = http_error(status, &body);
                if !is_retryable_status(status, &body) {
                    return Err(error);
                }
                budget.wait(error).await?;
            }
            Err(error) => {
                if !retryable_send(&error) {
                    return Err(error.into());
                }
                budget.wait(error.into()).await?;
            }
        }
    }
}

pub(super) async fn json<T: DeserializeOwned>(
    request: impl Fn() -> RequestBuilder,
    budget: &mut RetryBudget,
) -> Result<(T, HeaderMap, bool), ClientError> {
    loop {
        let response = connect(&request, budget).await?;
        let headers = response.headers().clone();
        match response.bytes().await {
            Ok(body) => {
                // Complete but invalid JSON is a protocol/configuration error,
                // not evidence that the HTTP body was interrupted.
                let decoded = serde_json::from_slice(&body)?;
                let cold = !budget.retried() && cold_generation(&headers);
                return Ok((decoded, headers, cold));
            }
            Err(error) => budget.wait(error.into()).await?,
        }
    }
}
