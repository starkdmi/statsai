use statsai_core::ModelInfo;

use crate::table::{active_pricing, PricingTable};

/// Whether `lower` contains `version` not followed by another digit.
///
/// Anthropic snapshot ids end in an eight-digit date, and every date so far
/// starts with `2`. A plain `contains("claude-fable-5-2")` therefore also
/// matches the Fable 5 snapshot `claude-fable-5-20260610`.
fn contains_version(lower: &str, version: &str) -> bool {
    lower.match_indices(version).any(|(start, _)| {
        !lower[start + version.len()..]
            .bytes()
            .next()
            .is_some_and(|next| next.is_ascii_digit())
    })
}

/// Recomputes a model's `normalized_name` from its observed name, using the
/// active pricing table.
///
/// See [`PricingTable::model_with_refreshed_normalization`].
#[must_use]
pub fn model_with_refreshed_normalization(model: &ModelInfo) -> ModelInfo {
    active_pricing().model_with_refreshed_normalization(model)
}

/// Normalizes a possibly provider-qualified label, using the active pricing
/// table.
///
/// See [`PricingTable::normalize_qualified_model_name`].
#[must_use]
pub fn normalize_qualified_model_name(label: &str) -> String {
    active_pricing().normalize_qualified_model_name(label)
}

/// Maps an observed model name onto its catalog name, using the active
/// pricing table.
#[must_use]
pub fn normalize_model_name(name: &str) -> String {
    active_pricing().normalize_model_name(name)
}

impl PricingTable {
    /// Recomputes a model's `normalized_name` from its observed name.
    ///
    /// `normalized_name` is a cache of a derivation, written once when an event is
    /// scanned. It goes stale whenever the normalizer learns a name it previously
    /// folded into a coarser one - `claude-fable-5-1-thinking-max` resolved to
    /// `claude-fable-5` before Fable 5.1 had its own entry - which then misprices
    /// and misgroups every already-stored event. Refresh it when repricing.
    ///
    /// Returns the model unchanged when it carries no observed name to derive from.
    #[must_use]
    pub fn model_with_refreshed_normalization(&self, model: &ModelInfo) -> ModelInfo {
        let Some(observed) = model.name.as_deref().or(model.provider_model_id.as_deref()) else {
            return model.clone();
        };
        ModelInfo {
            normalized_name: Some(self.normalize_qualified_model_name(observed)),
            ..model.clone()
        }
    }

    /// Normalizes a possibly provider-qualified label such as `google/gemini-3.1-pro`.
    ///
    /// The provider segment is dropped unconditionally, including for models with
    /// no catalog price. Gating this on pricing would make the same model normalize
    /// two different ways depending on whether it happened to be priced, splitting
    /// its history into separate groups the moment either side changed.
    ///
    /// This is the definition adapters use when they first record a model, so that
    /// refreshing a stored `normalized_name` cannot drift from a fresh scan.
    #[must_use]
    pub fn normalize_qualified_model_name(&self, label: &str) -> String {
        label.rsplit_once('/').map_or_else(
            || self.normalize_model_name(label),
            |(_, model)| self.normalize_model_name(model),
        )
    }

    /// Maps an observed model name onto its catalog name.
    ///
    /// Strips an `anthropic/` or `openai/` prefix and lowercases, then tries
    /// the exact `names` table, the ordered substring `patterns`, and Cursor's
    /// reversed Claude names, in that order. A name none of them knows is
    /// returned lowercased.
    #[must_use]
    pub fn normalize_model_name(&self, name: &str) -> String {
        let name = name.trim();
        let name = name
            .strip_prefix("anthropic/")
            .or_else(|| name.strip_prefix("openai/"))
            .unwrap_or(name);

        let lower = name.to_ascii_lowercase();
        if let Some(canonical) = self.names.get(&lower) {
            return canonical.clone();
        }
        match self
            .normalize_proxy_wrapped_model_name(&lower)
            .or_else(|| self.normalize_reversed_claude_model_name(&lower))
        {
            Some(canonical) => canonical.to_string(),
            None => lower,
        }
    }

    /// The first `patterns` rule with a matching substring wins, so a rule for
    /// a longer, more specific name must precede the rules it contains.
    fn normalize_proxy_wrapped_model_name(&self, lower: &str) -> Option<&str> {
        self.patterns
            .iter()
            .find(|pattern| {
                pattern.contains.iter().any(|needle| {
                    if pattern.whole_version {
                        contains_version(lower, needle)
                    } else {
                        lower.contains(needle.as_str())
                    }
                })
            })
            .map(|pattern| pattern.model.as_str())
    }

    /// Maps Cursor's reversed `claude-<version>-<family>` names onto catalog names.
    ///
    /// Cursor writes `claude-4.5-sonnet` where every other surface writes
    /// `claude-sonnet-4-5`, so the substring patterns never match it.
    fn normalize_reversed_claude_model_name(&self, lower: &str) -> Option<&str> {
        let rest = lower.strip_prefix("claude-")?;
        let (version, family) = rest.split_once('-')?;
        let family = family.split('-').next()?;
        self.reversed
            .get(&format!("claude-{version}-{family}"))
            .map(String::as_str)
    }
}
