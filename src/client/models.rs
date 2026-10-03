//! The Models API resource.

use reqwest::Method;
use serde::Deserialize;

use super::call::{Call, DecodeFailure};
use super::{Client, Provider};
use crate::types::ModelCard;
use crate::{Questions, SystemOneRequest, SystemOneResult, noul};

/// Access to the Models API resource.
#[derive(Debug, Clone)]
pub struct Models {
    client: Client,
}

impl Models {
    pub(crate) fn new(client: Client) -> Self {
        Self { client }
    }

    /// List the models available to the account.
    ///
    /// Liquid has no documented catalog endpoint: this sends a small inference
    /// probe and returns the responding model. It consumes input tokens.
    /// OpenRouter returns decision models from its public catalog; use
    /// [`Client::check`] to verify credentials too.
    pub fn list(&self) -> Call<Vec<ModelCard>> {
        match self.client.provider() {
            Provider::OpenRouter => Call::new(
                self.client.clone(),
                Method::GET,
                "/v1/models?output_modalities=decisions",
                Ok(None),
                unwrap_openrouter_models,
            ),
            Provider::Cloudflare { account_id } => Call::new(
                self.client.clone(),
                Method::GET,
                format!(
                    "/client/v4/accounts/{account_id}/ai/models/search?search=%40cf%2Fcloudflare%2Fclef&per_page=100"
                ),
                Ok(None),
                unwrap_clef_models,
            ),
            // Liquid documents inference but no model catalog. Authenticate by
            // evaluating a small probe with the documented default model.
            Provider::Liquid => {
                let mut questions = Questions::new();
                questions.add("available", noul("Does the state say ready?"));
                let body =
                    SystemOneRequest::new("ready", questions).to_body(self.client.default_model());
                Call::new(
                    self.client.clone(),
                    Method::POST,
                    "/v1/systemone",
                    body.map(Some),
                    unwrap_liquid_model,
                )
            }
            _ => Call::new(
                self.client.clone(),
                Method::GET,
                "/v1/models",
                Ok(None),
                unwrap_models,
            ),
        }
    }
}

fn unwrap_models(body: &[u8]) -> Result<Vec<ModelCard>, DecodeFailure> {
    #[derive(Deserialize)]
    struct Wire {
        models: Vec<ModelCard>,
    }

    serde_json::from_slice::<Wire>(body)
        .map(|wire| wire.models)
        .map_err(|err| DecodeFailure {
            message: "Unexpected response shape from GET /v1/models; expected { models: [...] }."
                .into(),
            source: Some(err),
        })
}

fn unwrap_clef_models(body: &[u8]) -> Result<Vec<ModelCard>, DecodeFailure> {
    let models: Vec<ModelCard> = super::provider::unwrap_cloudflare(body)?;
    let models: Vec<_> = models
        .into_iter()
        .filter_map(|mut model| {
            let name = model.name.strip_prefix("@cf/cloudflare/")?;
            if !matches!(name, "clef" | "clef-flash") {
                return None;
            }
            model.name = name.to_owned();
            Some(model)
        })
        .collect();
    if models.is_empty() {
        return Err(DecodeFailure {
            message: "Cloudflare did not list any Clef models for this account".into(),
            source: None,
        });
    }
    Ok(models)
}

fn unwrap_liquid_model(body: &[u8]) -> Result<Vec<ModelCard>, DecodeFailure> {
    let result: SystemOneResult = super::call::parse_json(body)?;
    Ok(vec![ModelCard {
        name: result.model,
        description: "Liquid decision model (inference probe)".into(),
        release_date: String::new(),
    }])
}

fn unwrap_openrouter_models(body: &[u8]) -> Result<Vec<ModelCard>, DecodeFailure> {
    #[derive(Deserialize)]
    struct Catalog {
        data: Vec<Model>,
    }
    #[derive(Deserialize)]
    struct Model {
        id: String,
        #[serde(default)]
        description: String,
        architecture: Architecture,
    }
    #[derive(Deserialize)]
    struct Architecture {
        output_modalities: Vec<String>,
    }
    let catalog: Catalog = super::call::parse_json(body)?;
    Ok(catalog
        .data
        .into_iter()
        .filter(|m| {
            m.architecture
                .output_modalities
                .iter()
                .any(|kind| kind == "decisions")
        })
        .map(|m| ModelCard {
            name: m.id,
            description: m.description,
            release_date: String::new(),
        })
        .collect())
}
