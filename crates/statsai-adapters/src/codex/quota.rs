use super::*;
use crate::*;

pub(crate) fn link_quota_observations(
    scan: &mut AdapterScan,
    quota_observation_indices: &HashMap<usize, usize>,
    line_numbers: &[usize],
    event_id: &EventId,
    link_kind: QuotaUsageLinkKind,
) {
    if line_numbers.is_empty() {
        return;
    }
    for line_number in line_numbers {
        let Some(record) = quota_observation_indices
            .get(line_number)
            .and_then(|index| scan.quota_observations.get_mut(*index))
        else {
            continue;
        };
        if record
            .observation
            .usage_sample
            .as_ref()
            .is_some_and(|usage| usage.computed_total() > 0)
        {
            record.observation.usage_event_id = Some(event_id.clone());
            record.observation.usage_link_kind = link_kind;
        }
    }
}

pub(crate) fn codex_quota_observation(
    source: &SourceLocation,
    path: &Path,
    line_number: usize,
    observed_at: DateTime<Utc>,
    usage_sample: Option<UsageCounts>,
    value: &Value,
) -> Option<QuotaObservationRecordV1> {
    if !is_codex_token_count(value) {
        return None;
    }
    let rate_limits = value.pointer("/payload/rate_limits")?.as_object()?;
    let raw_rate_limits = Value::Object(rate_limits.clone());
    let raw_json = serde_json::to_string(&raw_rate_limits).ok()?;
    let payload_hash = hash_text(&raw_json);
    let source_file_path_hash = hash_text(&canonical_display(path));
    let source_record_id = format!("quota:{source_file_path_hash}:{line_number}");
    let observation_id = format!(
        "quota_observation_{}",
        &hash_text(&format!("{}:{source_record_id}", source.source_id.0))[..32]
    );
    let semantic_fingerprint = hash_text(&format!(
        "quota_semantic.v1:{}:{}:{payload_hash}",
        source.provider,
        observed_at.to_rfc3339()
    ));
    let global_limit_id = quota_string_at_any(
        &raw_rate_limits,
        &["limit_id", "limitId", "limit_name", "limitName", "id"],
    );
    let mut windows = Vec::new();
    for (slot, candidate) in rate_limits {
        let Some(candidate) = candidate.as_object() else {
            continue;
        };
        let Some(window_minutes) = quota_u64_at_any(
            candidate,
            &["window_minutes", "windowMinutes", "duration_minutes"],
        ) else {
            continue;
        };
        let Some(used_percent) = quota_f64_at_any(
            candidate,
            &["used_percent", "usedPercent", "percentage", "percent_used"],
        ) else {
            continue;
        };
        let Some(resets_at_epoch_seconds) =
            quota_i64_at_any(candidate, &["resets_at", "resetsAt", "reset_at", "resetAt"])
        else {
            continue;
        };
        let Some(resets_at) = Utc.timestamp_opt(resets_at_epoch_seconds, 0).single() else {
            continue;
        };
        let limit_id = quota_string_at_any_from_map(
            candidate,
            &["limit_id", "limitId", "limit_name", "limitName", "id"],
        )
        .or_else(|| global_limit_id.clone());
        let window_observation_id = format!(
            "quota_window_observation_{}",
            &hash_text(&format!("{observation_id}:{slot}"))[..32]
        );
        windows.push(QuotaWindowObservationV1 {
            schema_version: QUOTA_WINDOW_OBSERVATION_SCHEMA_VERSION.to_string(),
            window_observation_id,
            observation_id: observation_id.clone(),
            provider_slot: slot.clone(),
            limit_id,
            window_minutes,
            used_percent,
            resets_at,
            resets_at_epoch_seconds,
        });
    }

    let credits_value = rate_limits.get("credits").and_then(Value::as_object);
    let balance_raw = credits_value
        .and_then(|credits| credits.get("balance"))
        .cloned();
    let balance = balance_raw.as_ref().and_then(normalize_quota_decimal);
    let status = QuotaStatusV1 {
        plan_type: quota_string_at_any(
            &raw_rate_limits,
            &["plan_type", "planType", "plan", "subscription_type"],
        ),
        individual_limit: rate_limits
            .get("individual_limit")
            .or_else(|| rate_limits.get("individualLimit"))
            .cloned(),
        spend_control_state: quota_string_at_any(
            &raw_rate_limits,
            &["spend_control_state", "spendControlState"],
        )
        .or_else(|| {
            raw_rate_limits
                .pointer("/spend_control/state")
                .and_then(Value::as_str)
                .map(ToOwned::to_owned)
        }),
        reached_type: quota_string_at_any(
            &raw_rate_limits,
            &["reached_type", "reachedType", "limit_reached_type"],
        ),
        credits: QuotaCreditsV1 {
            has_credits: credits_value
                .and_then(|credits| credits.get("has_credits"))
                .and_then(Value::as_bool),
            unlimited: credits_value
                .and_then(|credits| credits.get("unlimited"))
                .and_then(Value::as_bool),
            balance,
            balance_raw,
        },
    };
    Some(QuotaObservationRecordV1 {
        observation: QuotaObservationV1 {
            schema_version: QUOTA_OBSERVATION_SCHEMA_VERSION.to_string(),
            observation_id,
            semantic_fingerprint,
            provider: source.provider.clone(),
            source_id: source.source_id.clone(),
            provider_account_id: None,
            observed_at,
            source_file_path_hash,
            source_record_id,
            source_line_number: line_number as u64,
            payload_hash,
            usage_sample,
            usage_event_id: None,
            usage_link_kind: QuotaUsageLinkKind::None,
            status,
        },
        windows,
        raw_rate_limits,
    })
}

pub(crate) fn quota_string_at_any(value: &Value, keys: &[&str]) -> Option<String> {
    value
        .as_object()
        .and_then(|map| quota_string_at_any_from_map(map, keys))
}

pub(crate) fn quota_string_at_any_from_map(
    map: &serde_json::Map<String, Value>,
    keys: &[&str],
) -> Option<String> {
    keys.iter()
        .filter_map(|key| map.get(*key))
        .find_map(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(ToOwned::to_owned)
}

pub(crate) fn quota_u64_at_any(map: &serde_json::Map<String, Value>, keys: &[&str]) -> Option<u64> {
    keys.iter()
        .filter_map(|key| map.get(*key))
        .find_map(Value::as_u64)
}

pub(crate) fn quota_i64_at_any(map: &serde_json::Map<String, Value>, keys: &[&str]) -> Option<i64> {
    keys.iter()
        .filter_map(|key| map.get(*key))
        .find_map(Value::as_i64)
}

pub(crate) fn quota_f64_at_any(map: &serde_json::Map<String, Value>, keys: &[&str]) -> Option<f64> {
    keys.iter()
        .filter_map(|key| map.get(*key))
        .find_map(Value::as_f64)
        .filter(|value| value.is_finite())
}

pub(crate) fn normalize_quota_decimal(value: &Value) -> Option<String> {
    let text: Cow<'_, str> = match value {
        Value::String(value) => Cow::Borrowed(value.trim()),
        Value::Number(value) => Cow::Owned(value.to_string()),
        Value::Null => return None,
        _ => return None,
    };
    if text.is_empty() || text.len() > 4_096 {
        return None;
    }
    let (negative, unsigned) = text
        .strip_prefix('-')
        .map_or((false, text.as_ref()), |value| (true, value));
    let (mantissa, exponent) =
        unsigned
            .split_once(['e', 'E'])
            .map_or((unsigned, 0i32), |(mantissa, exponent)| {
                exponent
                    .parse::<i32>()
                    .ok()
                    .filter(|value| value.unsigned_abs() <= 4_096)
                    .map(|exponent| (mantissa, exponent))
                    .unwrap_or(("", 0))
            });
    if mantissa.is_empty() {
        return None;
    }
    let (whole, fraction) = mantissa.split_once('.').unwrap_or((mantissa, ""));
    if whole.is_empty() && fraction.is_empty()
        || !whole.bytes().all(|byte| byte.is_ascii_digit())
        || !fraction.bytes().all(|byte| byte.is_ascii_digit())
    {
        return None;
    }
    let digits = format!("{whole}{fraction}");
    let decimal_position = i64::try_from(whole.len()).ok()? + i64::from(exponent);
    let expanded = if decimal_position <= 0 {
        let zeroes = usize::try_from(-decimal_position).ok()?;
        format!("0.{}{}", "0".repeat(zeroes), digits)
    } else if decimal_position >= i64::try_from(digits.len()).ok()? {
        let zeroes = usize::try_from(decimal_position)
            .ok()?
            .saturating_sub(digits.len());
        format!("{digits}{}", "0".repeat(zeroes))
    } else {
        let position = usize::try_from(decimal_position).ok()?;
        format!("{}.{}", &digits[..position], &digits[position..])
    };
    let (whole, fraction) = expanded
        .split_once('.')
        .map_or((expanded.as_str(), ""), |parts| parts);
    let whole = whole.trim_start_matches('0');
    let fraction = fraction.trim_end_matches('0');
    let whole = if whole.is_empty() { "0" } else { whole };
    let mut normalized = if fraction.is_empty() {
        whole.to_string()
    } else {
        format!("{whole}.{fraction}")
    };
    if negative && normalized != "0" {
        normalized.insert(0, '-');
    }
    Some(normalized)
}
