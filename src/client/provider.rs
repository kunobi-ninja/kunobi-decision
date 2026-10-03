//! Provider-specific paths and response envelopes.

use serde::Deserialize;
use serde::de::DeserializeOwned;

use super::call::DecodeFailure;
use crate::{Error, Result, SystemOneRequest};

/// Service used to access a decision model. Model IDs are selected separately.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub enum Provider {
    /// TypeSafe's System One API, with `TYPESAFE_*` environment defaults.
    #[default]
    TypeSafe,
    /// Cloudflare Workers AI. Uses `CLOUDFLARE_API_TOKEN`.
    Cloudflare {
        /// Cloudflare account ID, containing only letters, digits, `_` or `-`.
        account_id: String,
    },
    /// Liquid AI's decisions API. Uses `LIQUID_API_KEY`.
    Liquid,
    /// Vercel's TypeSafe-compatible API. Uses `AI_GATEWAY_API_KEY`.
    Vercel,
    /// OpenRouter's System One API. Uses `OPENROUTER_API_KEY`.
    OpenRouter,
    /// A custom System One API. Requires an explicit base URL.
    /// Uses explicit credentials or `KUNOBI_DECISION_API_KEY`.
    Compatible,
}

impl Provider {
    /// Default model ID for this access provider.
    pub fn default_model(&self) -> &'static str {
        match self {
            Self::Cloudflare { .. } => "clef",
            Self::Liquid => "d1:free",
            Self::Vercel => "typesafe-ai/jev",
            Self::OpenRouter => "~typesafe/jev-latest",
            Self::TypeSafe | Self::Compatible => "jev-latest",
        }
    }

    /// Default API root. Compatible providers require an explicit root.
    pub fn base_url(&self) -> &'static str {
        match self {
            Self::Cloudflare { .. } => "https://api.cloudflare.com",
            Self::Liquid => "https://api.liquid.ai/decisions",
            Self::Vercel => "https://ai-gateway.vercel.sh/typesafe",
            Self::OpenRouter => "https://openrouter.ai/api",
            Self::TypeSafe => "https://api.typesafe.ai",
            Self::Compatible => "",
        }
    }

    pub(super) fn api_key_env(&self) -> &'static str {
        match self {
            Self::TypeSafe => "TYPESAFE_API_KEY",
            Self::Cloudflare { .. } => "CLOUDFLARE_API_TOKEN",
            Self::Liquid => "LIQUID_API_KEY",
            Self::Vercel => "AI_GATEWAY_API_KEY",
            Self::OpenRouter => "OPENROUTER_API_KEY",
            Self::Compatible => "KUNOBI_DECISION_API_KEY",
        }
    }

    pub(super) fn base_url_env(&self) -> Option<&'static str> {
        match self {
            Self::TypeSafe => Some(super::ENV_BASE_URL),
            Self::OpenRouter => Some("OPENROUTER_BASE_URL"),
            _ => None,
        }
    }

    pub(super) fn default_model_env(&self) -> Option<&'static str> {
        match self {
            Self::TypeSafe => Some(super::ENV_DEFAULT_MODEL),
            Self::OpenRouter => Some("OPENROUTER_DEFAULT_MODEL"),
            _ => None,
        }
    }

    pub(super) fn validate(&self) -> Result<()> {
        if let Self::Cloudflare { account_id } = self
            && (account_id.is_empty()
                || !account_id
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'-')))
        {
            return Err(Error::Config("Invalid Cloudflare account ID".into()));
        }
        Ok(())
    }

    pub(super) fn prepare(
        &self,
        mut request: SystemOneRequest,
        default_model: &str,
    ) -> Result<(String, Vec<u8>)> {
        let path = match self {
            Self::Cloudflare { account_id } => {
                if request.questions.len() > 64 {
                    return Err(Error::InvalidRequest(
                        "Clef accepts at most 64 questions per request".into(),
                    ));
                }
                let model = request.model.as_deref().unwrap_or(default_model);
                let model = model.strip_prefix("@cf/cloudflare/").unwrap_or(model);
                if !matches!(model, "clef" | "clef-flash") {
                    return Err(Error::InvalidRequest(
                        "Cloudflare decision endpoints serve clef and clef-flash".into(),
                    ));
                }
                let path =
                    format!("/client/v4/accounts/{account_id}/ai/run/@cf/cloudflare/{model}");
                request.model = Some(model.to_owned());
                path
            }
            _ => "/v1/systemone".into(),
        };
        Ok((path, request.to_body(default_model)?))
    }
}

pub(super) fn unwrap_cloudflare<T: DeserializeOwned>(
    body: &[u8],
) -> std::result::Result<T, DecodeFailure> {
    #[derive(Deserialize)]
    struct Envelope<T> {
        success: bool,
        result: Option<T>,
    }
    let envelope: Envelope<T> = serde_json::from_slice(body).map_err(|err| DecodeFailure {
        message: "Unexpected Cloudflare response shape".into(),
        source: Some(err),
    })?;
    if !envelope.success {
        return Err(DecodeFailure {
            message: "Cloudflare reported an unsuccessful request".into(),
            source: None,
        });
    }
    envelope.result.ok_or_else(|| DecodeFailure {
        message: "Cloudflare response is missing its result".into(),
        source: None,
    })
}
