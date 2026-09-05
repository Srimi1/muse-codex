//! Pinned Z.ai GLM Coding Plan catalog.
//!
//! Z.ai publishes no model-listing endpoint, so the catalog is pinned here
//! rather than discovered. Anything this file cannot state from Z.ai's own
//! documentation is left unknown: the launcher seeds Muse's normalized cache
//! from these rows, and a fabricated context limit would be indistinguishable
//! from a measured one.

use crate::ModelInfo;

/// Z.ai documents `max_tokens` as 1..131072 for every chat model.
pub(crate) const MAX_OUTPUT_TOKENS: u64 = 131_072;

/// Muse's effort vocabulary maps onto Z.ai's `reasoning_effort` almost
/// exactly. `ultra` is Muse's own delegation mode and is translated to Z.ai's
/// deepest documented level when the request is built.
const REASONING_EFFORTS: &[&str] = &["minimal", "low", "medium", "high", "xhigh", "ultra"];

struct PinnedModel {
    id: &'static str,
    display_name: &'static str,
    description: &'static str,
    context_window: Option<u64>,
    accepts_images: bool,
}

const PINNED_MODELS: &[PinnedModel] = &[
    PinnedModel {
        id: "glm-5.3",
        display_name: "GLM-5.3",
        description: "Z.ai GLM Coding Plan default coding model",
        context_window: Some(1_048_576),
        accepts_images: false,
    },
    PinnedModel {
        id: "glm-5.3-flash",
        display_name: "GLM-5.3 Flash",
        description: "Faster multimodal GLM Coding Plan model",
        context_window: None,
        accepts_images: true,
    },
    PinnedModel {
        id: "glm-4.7",
        display_name: "GLM-4.7",
        description: "Previous-generation GLM coding model",
        context_window: None,
        accepts_images: false,
    },
    PinnedModel {
        id: "glm-4.6",
        display_name: "GLM-4.6",
        description: "Previous-generation GLM coding model",
        context_window: None,
        accepts_images: false,
    },
];

pub(crate) fn pinned_models() -> Vec<ModelInfo> {
    PINNED_MODELS
        .iter()
        .enumerate()
        .map(|(index, model)| ModelInfo {
            id: model.id.to_string(),
            display_name: Some(model.display_name.to_string()),
            description: Some(model.description.to_string()),
            context_window: model.context_window,
            max_output_tokens: Some(MAX_OUTPUT_TOKENS),
            supported_reasoning_efforts: REASONING_EFFORTS
                .iter()
                .map(|effort| (*effort).to_string())
                .collect(),
            default_reasoning_effort: Some("medium".to_string()),
            is_visible: true,
            is_default: index == 0,
            use_responses_lite: false,
            tool_mode: Some("direct".to_string()),
            input_modalities: if model.accepts_images {
                vec!["text".to_string(), "image".to_string()]
            } else {
                vec!["text".to_string()]
            },
            supported_in_api: true,
            // Fast is an OpenAI service tier. Z.ai has no equivalent.
            supports_fast_mode: false,
        })
        .collect()
}

/// Returns the pinned entry for a model id, if the catalog contains one.
pub(crate) fn find(id: &str) -> Option<ModelInfo> {
    pinned_models().into_iter().find(|model| model.id == id)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeSet;

    #[test]
    fn exactly_one_visible_default_exists() {
        let models = pinned_models();
        let defaults: Vec<&ModelInfo> = models.iter().filter(|model| model.is_default).collect();
        assert_eq!(defaults.len(), 1);
        assert!(defaults[0].is_visible);
        assert_eq!(defaults[0].id, "glm-5.3");
    }

    #[test]
    fn ids_are_unique_and_limits_stay_within_documented_bounds() {
        let models = pinned_models();
        let unique: BTreeSet<&str> = models.iter().map(|model| model.id.as_str()).collect();
        assert_eq!(unique.len(), models.len());
        for model in &models {
            assert!(model.context_window.is_none_or(|window| window > 0));
            assert_eq!(model.max_output_tokens, Some(MAX_OUTPUT_TOKENS));
            assert!(model.input_modalities.contains(&"text".to_string()));
            assert!(!model.supports_fast_mode);
        }
    }

    #[test]
    fn every_model_offers_a_recognized_default_effort() {
        for model in pinned_models() {
            let default = model.default_reasoning_effort.expect("a default effort");
            assert!(model.supported_reasoning_efforts.contains(&default));
        }
    }
}
