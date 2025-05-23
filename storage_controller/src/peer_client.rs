use std::collections::HashMap;
use std::error::Error as _;
use std::time::Duration;

use http_utils::error::HttpErrorBody;
use hyper::Uri;
use pageserver_api::shard::TenantShardId;
use reqwest::{StatusCode, Url};
use serde::{Deserialize, Serialize};
use tokio_util::sync::CancellationToken;
use utils::backoff;

use crate::tenant_shard::ObservedState;

#[derive(Debug, Clone)]
pub(crate) struct PeerClient {
    uri: Uri,
    jwt: Option<String>,
    client: reqwest::Client,
}

#[derive(thiserror::Error, Debug)]
pub(crate) enum StorageControllerPeerError {
    #[error(
        "failed to deserialize error response with status code {0} at {1}: {2}{}",
        .2.source().map(|e| format!(": {e}")).unwrap_or_default()
    )]
    DeserializationError(StatusCode, Url, reqwest::Error),
    #[error("storage controller peer API error ({0}): {1}")]
    ApiError(StatusCode, String),
    #[error("failed to send HTTP request: {0}{}", .0.source().map(|e| format!(": {e}")).unwrap_or_default())]
    SendError(reqwest::Error),
    #[error("Cancelled")]
    Cancelled,
}

pub(crate) type Result<T> = std::result::Result<T, StorageControllerPeerError>;

pub(crate) trait ResponseErrorMessageExt: Sized {
    fn error_from_body(self) -> impl std::future::Future<Output = Result<Self>> + Send;
}

impl ResponseErrorMessageExt for reqwest::Response {
    async fn error_from_body(self) -> Result<Self> {
        let status = self.status();
        if !(status.is_client_error() || status.is_server_error()) {
            return Ok(self);
        }

        let url = self.url().to_owned();
        Err(match self.json::<HttpErrorBody>().await {
            Ok(HttpErrorBody { msg }) => StorageControllerPeerError::ApiError(status, msg),
            Err(err) => StorageControllerPeerError::DeserializationError(status, url, err),
        })
    }
}

#[derive(Serialize, Deserialize, Debug, Default, Clone)]
pub(crate) struct GlobalObservedState(pub(crate) HashMap<TenantShardId, ObservedState>);

const STEP_DOWN_RETRIES: u32 = 8;
const STEP_DOWN_TIMEOUT: Duration = Duration::from_secs(1);

impl PeerClient {
    pub(crate) fn new(http_client: reqwest::Client, uri: Uri, jwt: Option<String>) -> Self {
        Self {
            uri,
            jwt,
            client: http_client,
        }
    }

    async fn request_step_down(&self) -> Result<GlobalObservedState> {
        let step_down_path = format!("{}control/v1/step_down", self.uri);
        let req = self.client.put(step_down_path);
        let req = if let Some(jwt) = &self.jwt {
            req.header(reqwest::header::AUTHORIZATION, format!("Bearer {jwt}"))
        } else {
            req
        };

        let req = req.timeout(STEP_DOWN_TIMEOUT);

        let res = req
            .send()
            .await
            .map_err(StorageControllerPeerError::SendError)?;
        let response = res.error_from_body().await?;

        let status = response.status();
        let url = response.url().to_owned();

        response
            .json()
            .await
            .map_err(|err| StorageControllerPeerError::DeserializationError(status, url, err))
    }

    /// Request the peer to step down and return its current observed state
    /// All errors are re-tried
    pub(crate) async fn step_down(
        &self,
        cancel: &CancellationToken,
    ) -> Result<GlobalObservedState> {
        backoff::retry(
            || self.request_step_down(),
            |_e| false,
            2,
            STEP_DOWN_RETRIES,
            "Send step down request",
            cancel,
        )
        .await
        .ok_or_else(|| StorageControllerPeerError::Cancelled)
        .and_then(|x| x)
    }
}
