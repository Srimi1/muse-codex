//! Upstream selection for one gateway process.
//!
//! Exactly one provider is chosen per launch and never changes, so this is an
//! enum rather than a trait object: adding a route or a provider makes the
//! compiler demand an explicit decision for every combination, and a defaulted
//! trait method is precisely the shape that produces a silent fallback between
//! providers.

use crate::ModelInfo;
use crate::RawResponse;
use crate::Result;
use crate::Transport;
use crate::ZaiTransport;
use crate::error::Error;
use http::HeaderMap;
use serde_json::Value;

#[derive(Debug)]
pub enum Backend {
    Codex(Transport),
    Zai(ZaiTransport),
}

impl Backend {
    /// The public provider label this process serves.
    pub fn provider(&self) -> &'static str {
        match self {
            Self::Codex(_) => "codex",
            Self::Zai(_) => "zai",
        }
    }

    pub async fn list_models(&self) -> Result<Vec<ModelInfo>> {
        match self {
            Self::Codex(transport) => transport.list_models().await,
            Self::Zai(transport) => transport.list_models().await,
        }
    }

    pub async fn stream_responses(&self, body: Value, headers: HeaderMap) -> Result<RawResponse> {
        match self {
            Self::Codex(transport) => transport.stream_responses(body, headers).await,
            Self::Zai(transport) => transport.stream_responses(body, headers).await,
        }
    }

    pub async fn search_muse_request(
        &self,
        request: Value,
        headers: HeaderMap,
    ) -> Result<RawResponse> {
        match self {
            Self::Codex(transport) => transport.search_muse_request(request, headers).await,
            // Z.ai exposes no search service. Returning an empty success would
            // let the model reason from "the web returned nothing", which is a
            // false premise rather than a missing capability.
            Self::Zai(_) => Err(Error::ProviderUnsupported("web search")),
        }
    }

    pub async fn browser_open_muse_request(
        &self,
        request: Value,
        headers: HeaderMap,
    ) -> Result<RawResponse> {
        match self {
            Self::Codex(transport) => transport.browser_open_muse_request(request, headers).await,
            Self::Zai(_) => Err(Error::ProviderUnsupported("browser open")),
        }
    }
}
