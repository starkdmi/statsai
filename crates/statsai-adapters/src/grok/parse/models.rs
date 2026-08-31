use super::*;

pub(crate) fn grok_prompt_model_observation(value: &Value) -> Option<GrokModelObservation> {
    let meta = value.pointer("/params/update/_meta")?;
    // User-prompt rows carry both modelId and promptIndex. Later stream/tool
    // chunks share promptId but omit modelId, so only this pair is a stable
    // per-prompt identity.
    let model_id = grok_nonempty_model_id(meta.get("modelId"))?;
    meta.get("promptIndex").and_then(value_as_u64)?;
    Some(GrokModelObservation {
        model_id,
        observed_at: grok_update_timestamp(value),
    })
}

pub(crate) fn grok_turn_model_observation(value: &Value) -> Option<GrokModelObservation> {
    Some(GrokModelObservation {
        model_id: grok_nonempty_model_id(value.get("model_id"))?,
        observed_at: value.get("ts").and_then(timestamp_from_scalar),
    })
}

pub(crate) fn grok_update_timestamp(value: &Value) -> Option<DateTime<Utc>> {
    value
        .pointer("/params/_meta/agentTimestampMs")
        .and_then(timestamp_from_scalar)
        .or_else(|| value.get("timestamp").and_then(timestamp_from_scalar))
}

pub(crate) fn grok_nonempty_model_id(value: Option<&Value>) -> Option<String> {
    value
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|model_id| !model_id.is_empty())
        .map(ToOwned::to_owned)
}

pub(crate) fn grok_signals_models_used(signals: Option<&Value>) -> Vec<String> {
    signals
        .and_then(|signals| signals.get("modelsUsed"))
        .and_then(Value::as_array)
        .map(|models| {
            models
                .iter()
                .filter_map(|value| grok_nonempty_model_id(Some(value)))
                .collect()
        })
        .unwrap_or_default()
}

pub(crate) fn grok_normalized_model_id(model_id: &str) -> String {
    normalize_model_name(model_id)
}

pub(crate) fn grok_models_equivalent(left: &str, right: &str) -> bool {
    grok_normalized_model_id(left) == grok_normalized_model_id(right)
}

pub(crate) fn unique_grok_normalized_models<'a>(
    ids: impl IntoIterator<Item = &'a str>,
) -> HashSet<String> {
    ids.into_iter()
        .map(str::trim)
        .filter(|model_id| !model_id.is_empty())
        .map(grok_normalized_model_id)
        .collect()
}

pub(crate) fn grok_current_model_id(model: Option<&ModelInfo>) -> Option<&str> {
    model.and_then(|model| {
        model
            .name
            .as_deref()
            .or(model.provider_model_id.as_deref())
            .map(str::trim)
            .filter(|model_id| !model_id.is_empty())
    })
}

pub(crate) fn last_grok_model_at_or_before(
    observations: &[GrokModelObservation],
    at: DateTime<Utc>,
) -> Option<&str> {
    observations
        .iter()
        .enumerate()
        .filter(|(_, observation)| {
            observation
                .observed_at
                .is_some_and(|observed_at| observed_at <= at)
        })
        .max_by_key(|(index, observation)| (observation.observed_at, *index))
        .map(|(_, observation)| observation.model_id.as_str())
}

pub(crate) fn resolve_grok_inference_sample_model(
    sample: &GrokInferenceSample,
    prompt_models: &[GrokModelObservation],
    turn_models: &[GrokModelObservation],
    session_models_used: &[String],
    current_model: Option<&ModelInfo>,
) -> Option<ModelInfo> {
    let assignable_ids = prompt_models
        .iter()
        .map(|observation| observation.model_id.as_str())
        .chain(
            turn_models
                .iter()
                .map(|observation| observation.model_id.as_str()),
        );
    let assignable = unique_grok_normalized_models(assignable_ids);
    if assignable.len() == 1 {
        let models_used =
            unique_grok_normalized_models(session_models_used.iter().map(String::as_str));
        // A lone prompt/turn observation cannot cover every inference when
        // modelsUsed reports another model: request-level attribution is
        // incomplete, so do not silently price the missing model as this one.
        if !models_used.is_empty() && models_used != assignable {
            return None;
        }
        let model_id = prompt_models
            .iter()
            .map(|observation| observation.model_id.as_str())
            .chain(
                turn_models
                    .iter()
                    .map(|observation| observation.model_id.as_str()),
            )
            .next()?;
        return Some(model_info(model_id));
    }
    if assignable.len() >= 2 {
        let observed_at = sample.observed_at?;
        let from_prompt = last_grok_model_at_or_before(prompt_models, observed_at);
        let from_turn = last_grok_model_at_or_before(turn_models, observed_at);
        return match (from_prompt, from_turn) {
            (Some(prompt), Some(turn)) if grok_models_equivalent(prompt, turn) => {
                Some(model_info(prompt))
            }
            (Some(_prompt), Some(_turn)) => None,
            (Some(prompt), None) => Some(model_info(prompt)),
            (None, Some(turn)) => Some(model_info(turn)),
            (None, None) => None,
        };
    }

    let session_ids = session_models_used
        .iter()
        .map(String::as_str)
        .chain(grok_current_model_id(current_model));
    if unique_grok_normalized_models(session_ids).len() == 1 {
        return current_model.cloned().or_else(|| {
            session_models_used
                .first()
                .map(|model_id| model_info(model_id))
        });
    }
    None
}
