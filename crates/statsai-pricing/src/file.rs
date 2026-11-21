//! Parsing and validation of the pricing data file (`pricing.json`).
//!
//! The same checks apply to the embedded file, a fetched file, and a copy read
//! back from a store, so a table in memory is always one that passed them.

use std::collections::{HashMap, HashSet};
use std::fmt;
use std::marker::PhantomData;

use chrono::NaiveDate;
use serde::de::{self, Deserializer, MapAccess, Visitor};
use serde::Deserialize;

use crate::catalog::ModelPricing;
use crate::table::{
    FastPricing, FastRates, LongContext, ModelEntry, NamePattern, Period, Periods, PricingTable,
    TokenThreshold,
};

/// The newest pricing file format this binary reads. Formats
/// `1..=PRICING_FORMAT` are known.
pub const PRICING_FORMAT: u32 = 1;

const MAX_FILE_BYTES: usize = 1024 * 1024;
const MAX_NAME_BYTES: usize = 128;
const MAX_CATALOG_CHARS: usize = 100;
const MAX_RATE: f64 = 1000.0;
const MAX_MULTIPLIER: f64 = 20.0;
const MAX_TOKEN_THRESHOLD: u64 = 100_000_000;
/// The highest `version` a file may have. Versions go up by one per change,
/// so this is never reached in practice; it keeps a typo such as
/// `18446744073709551615` from being adopted, after which no real file could
/// ever be newer.
const MAX_VERSION: u64 = 1_000_000;

/// Why a pricing file cannot be used.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PricingFileError {
    /// The file declares a structure version this binary cannot read.
    UnknownFormat { format: u64, supported: u32 },
    /// The file is not a valid pricing file of a known format.
    Invalid(String),
}

impl fmt::Display for PricingFileError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::UnknownFormat { format, supported } => write!(
                formatter,
                "pricing file format {format} is unknown; this StatsAI binary reads formats up to {supported}"
            ),
            Self::Invalid(message) => write!(formatter, "invalid pricing file: {message}"),
        }
    }
}

impl std::error::Error for PricingFileError {}

/// The two fields every pricing file format carries.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PricingFileHeader {
    pub format: u64,
    pub version: u64,
}

#[derive(Deserialize)]
struct RawHeader {
    format: u64,
    version: u64,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawFile {
    #[serde(rename = "format")]
    _format: u64,
    version: u64,
    catalog: String,
    #[serde(deserialize_with = "unique_keys")]
    models: Vec<(String, RawModel)>,
    #[serde(deserialize_with = "unique_keys")]
    equivalents: Vec<(String, Vec<RawEquivalentPeriod>)>,
    #[serde(deserialize_with = "unique_keys")]
    names: Vec<(String, Vec<String>)>,
    patterns: Vec<RawPattern>,
    #[serde(deserialize_with = "unique_keys")]
    reversed: Vec<(String, String)>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawModel {
    #[serde(default, rename = "note")]
    _note: Option<String>,
    #[serde(default)]
    source: Option<String>,
    prices: Vec<RawPricePeriod>,
    #[serde(default)]
    long_context: Option<RawLongContext>,
    #[serde(default)]
    fast: Option<RawFast>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawPricePeriod {
    #[serde(default)]
    from: Option<String>,
    input: f64,
    cache_write: f64,
    cache_read: f64,
    output: f64,
    #[serde(default, rename = "note")]
    _note: Option<String>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawLongContext {
    #[serde(default)]
    above: Option<u64>,
    #[serde(default)]
    at_least: Option<u64>,
    input: f64,
    output: f64,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawFast {
    #[serde(default)]
    multiplier: Option<f64>,
    #[serde(default)]
    prices: Option<Vec<RawPricePeriod>>,
    #[serde(default)]
    max_prompt: Option<u64>,
    #[serde(default)]
    long_context: Option<RawLongContext>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawEquivalentPeriod {
    #[serde(default)]
    from: Option<String>,
    model: String,
    #[serde(default, rename = "note")]
    _note: Option<String>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawPattern {
    contains: Vec<String>,
    model: String,
    #[serde(default)]
    whole_version: bool,
    #[serde(default, rename = "note")]
    _note: Option<String>,
}

/// Deserializes a JSON object into its entries, rejecting a repeated key
/// rather than letting the later value silently win.
fn unique_keys<'de, D, V>(deserializer: D) -> Result<Vec<(String, V)>, D::Error>
where
    D: Deserializer<'de>,
    V: Deserialize<'de>,
{
    struct UniqueKeys<V>(PhantomData<V>);

    impl<'de, V: Deserialize<'de>> Visitor<'de> for UniqueKeys<V> {
        type Value = Vec<(String, V)>;

        fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
            formatter.write_str("an object with unique keys")
        }

        fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<Self::Value, A::Error> {
            let mut seen = HashSet::new();
            let mut entries = Vec::new();
            while let Some(key) = map.next_key::<String>()? {
                if !seen.insert(key.clone()) {
                    return Err(de::Error::custom(format_args!("duplicate key `{key}`")));
                }
                entries.push((key, map.next_value()?));
            }
            Ok(entries)
        }
    }

    deserializer.deserialize_map(UniqueKeys(PhantomData))
}

impl PricingTable {
    /// Parses and validates a pricing file.
    ///
    /// # Errors
    ///
    /// [`PricingFileError::UnknownFormat`] when the file's `format` is not one
    /// this binary reads; [`PricingFileError::Invalid`] for anything else
    /// wrong with it.
    pub fn parse(text: &str) -> Result<PricingTable, PricingFileError> {
        let header = Self::header(text)?;
        let format = u32::try_from(header.format)
            .ok()
            .filter(|format| (1..=PRICING_FORMAT).contains(format))
            .ok_or(PricingFileError::UnknownFormat {
                format: header.format,
                supported: PRICING_FORMAT,
            })?;
        let raw: RawFile = serde_json::from_str(text)
            .map_err(|error| PricingFileError::Invalid(error.to_string()))?;
        build_table(text, format, raw).map_err(PricingFileError::Invalid)
    }

    /// Reads only `format` and `version` (both required integers), ignoring
    /// the rest; for files whose format may be unknown.
    ///
    /// # Errors
    ///
    /// [`PricingFileError::Invalid`] when the text is too large, is not a JSON
    /// object, or lacks either integer.
    pub fn header(text: &str) -> Result<PricingFileHeader, PricingFileError> {
        if text.len() > MAX_FILE_BYTES {
            return Err(PricingFileError::Invalid(format!(
                "the file is {} bytes; the limit is {MAX_FILE_BYTES}",
                text.len()
            )));
        }
        let header: RawHeader = serde_json::from_str(text).map_err(|error| {
            PricingFileError::Invalid(format!("cannot read `format` and `version`: {error}"))
        })?;
        Ok(PricingFileHeader {
            format: header.format,
            version: header.version,
        })
    }
}

fn build_table(text: &str, format: u32, raw: RawFile) -> Result<PricingTable, String> {
    if !(1..=MAX_VERSION).contains(&raw.version) {
        return Err(format!(
            "`version` {} must be within 1 to {MAX_VERSION}",
            raw.version
        ));
    }
    check_catalog(&raw.catalog)?;

    let mut models = HashMap::with_capacity(raw.models.len());
    for (name, model) in raw.models {
        check_name("model name", &name)?;
        let entry = build_model(model).map_err(|error| format!("model `{name}`: {error}"))?;
        models.insert(name, entry);
    }

    let mut equivalents = HashMap::with_capacity(raw.equivalents.len());
    for (name, periods) in raw.equivalents {
        check_name("equivalent name", &name)?;
        if models.contains_key(&name) {
            return Err(format!("`{name}` is in both `models` and `equivalents`"));
        }
        let periods = periods
            .into_iter()
            .map(|period| {
                check_name("equivalent target", &period.model)?;
                if !models.contains_key(&period.model) {
                    return Err(format!("target `{}` is not in `models`", period.model));
                }
                Ok((period.from, period.model))
            })
            .collect::<Result<Vec<_>, String>>()
            .and_then(build_periods)
            .map_err(|error| format!("equivalent `{name}`: {error}"))?;
        equivalents.insert(name, periods);
    }

    let names = build_names(raw.names)?;

    let mut patterns = Vec::with_capacity(raw.patterns.len());
    for (index, pattern) in raw.patterns.into_iter().enumerate() {
        let position = index + 1;
        if pattern.contains.is_empty() {
            return Err(format!("pattern {position}: `contains` is empty"));
        }
        for needle in &pattern.contains {
            check_name("pattern string", needle)
                .map_err(|error| format!("pattern {position}: {error}"))?;
        }
        check_name("pattern target", &pattern.model)
            .map_err(|error| format!("pattern {position}: {error}"))?;
        patterns.push(NamePattern {
            contains: pattern.contains,
            model: pattern.model,
            whole_version: pattern.whole_version,
        });
    }

    let mut reversed = HashMap::with_capacity(raw.reversed.len());
    for (key, target) in raw.reversed {
        check_reversed_key(&key)?;
        check_name("reversed target", &target)?;
        reversed.insert(key, target);
    }

    Ok(PricingTable {
        text: text.to_string(),
        format,
        version: raw.version,
        catalog: raw.catalog,
        models,
        equivalents,
        names,
        patterns,
        reversed,
    })
}

fn check_catalog(catalog: &str) -> Result<(), String> {
    if catalog.is_empty()
        || catalog.chars().count() > MAX_CATALOG_CHARS
        || !catalog.contains(':')
        || catalog.chars().any(char::is_control)
    {
        return Err(format!(
            "`catalog` must be 1 to {MAX_CATALOG_CHARS} characters containing ':', got {catalog:?}"
        ));
    }
    Ok(())
}

fn check_name(kind: &str, name: &str) -> Result<(), String> {
    let valid = !name.is_empty()
        && name.len() <= MAX_NAME_BYTES
        && name
            .bytes()
            .all(|byte| byte.is_ascii_graphic() && !byte.is_ascii_uppercase());
    if valid {
        Ok(())
    } else {
        Err(format!(
            "{kind} {name:?} must be 1 to {MAX_NAME_BYTES} bytes of lowercase ASCII with no whitespace"
        ))
    }
}

/// `reversed` keys are `claude-{version}-{family}`, the key the reversed-name
/// parser builds; any other shape could never be looked up.
fn check_reversed_key(key: &str) -> Result<(), String> {
    check_name("reversed key", key)?;
    let shaped = key
        .strip_prefix("claude-")
        .and_then(|rest| rest.split_once('-'))
        .is_some_and(|(version, family)| {
            !version.is_empty() && !family.is_empty() && !family.contains('-')
        });
    if shaped {
        Ok(())
    } else {
        Err(format!(
            "reversed key {key:?} must have the form claude-<version>-<family>"
        ))
    }
}

fn build_names(raw: Vec<(String, Vec<String>)>) -> Result<HashMap<String, String>, String> {
    let mut names = HashMap::new();
    for (canonical, _) in &raw {
        check_name("names key", canonical)?;
        names.insert(canonical.clone(), canonical.clone());
    }
    let keys: HashSet<String> = names.keys().cloned().collect();
    for (canonical, spellings) in raw {
        for spelling in spellings {
            check_name("spelling", &spelling)?;
            if keys.contains(&spelling) {
                return Err(format!(
                    "spelling `{spelling}` of `{canonical}` is itself a `names` key"
                ));
            }
            if names.insert(spelling.clone(), canonical.clone()).is_some() {
                return Err(format!(
                    "spelling `{spelling}` appears more than once in `names`"
                ));
            }
        }
    }
    Ok(names)
}

fn build_model(raw: RawModel) -> Result<ModelEntry, String> {
    if let Some(source) = &raw.source {
        let valid = !source.is_empty()
            && source.len() <= MAX_NAME_BYTES
            && source
                .bytes()
                .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'_');
        if !valid {
            return Err(format!(
                "`source` {source:?} must be 1 to {MAX_NAME_BYTES} characters of [a-z0-9_]"
            ));
        }
    }
    let prices = build_price_periods(raw.prices).map_err(|error| format!("`prices`: {error}"))?;
    let long_context = raw
        .long_context
        .map(build_long_context)
        .transpose()
        .map_err(|error| format!("`long_context`: {error}"))?;
    let fast = raw
        .fast
        .map(build_fast)
        .transpose()
        .map_err(|error| format!("`fast`: {error}"))?;
    Ok(ModelEntry {
        source: raw.source,
        prices,
        long_context,
        fast,
    })
}

fn build_fast(raw: RawFast) -> Result<FastPricing, String> {
    let rates = match (raw.multiplier, raw.prices) {
        (Some(multiplier), None) => {
            check_multiplier("multiplier", multiplier)?;
            FastRates::Multiplier(multiplier)
        }
        (None, Some(prices)) => FastRates::Prices(
            build_price_periods(prices).map_err(|error| format!("`prices`: {error}"))?,
        ),
        _ => return Err("needs exactly one of `multiplier` or `prices`".to_string()),
    };
    if let Some(max_prompt) = raw.max_prompt {
        check_threshold("max_prompt", max_prompt)?;
    }
    let long_context = raw
        .long_context
        .map(build_long_context)
        .transpose()
        .map_err(|error| format!("`long_context`: {error}"))?;
    Ok(FastPricing {
        rates,
        max_prompt: raw.max_prompt,
        long_context,
    })
}

fn build_long_context(raw: RawLongContext) -> Result<LongContext, String> {
    let threshold = match (raw.above, raw.at_least) {
        (Some(above), None) => {
            check_threshold("above", above)?;
            TokenThreshold::Above(above)
        }
        (None, Some(at_least)) => {
            check_threshold("at_least", at_least)?;
            TokenThreshold::AtLeast(at_least)
        }
        _ => return Err("needs exactly one of `above` or `at_least`".to_string()),
    };
    check_multiplier("input", raw.input)?;
    check_multiplier("output", raw.output)?;
    Ok(LongContext {
        threshold,
        input: raw.input,
        output: raw.output,
    })
}

fn build_price_periods(raw: Vec<RawPricePeriod>) -> Result<Periods<ModelPricing>, String> {
    raw.into_iter()
        .map(|period| {
            for (field, rate) in [
                ("input", period.input),
                ("cache_write", period.cache_write),
                ("cache_read", period.cache_read),
                ("output", period.output),
            ] {
                if !rate.is_finite() || !(0.0..=MAX_RATE).contains(&rate) {
                    return Err(format!(
                        "rate `{field}` {rate} must be within 0 to {MAX_RATE} USD per million tokens"
                    ));
                }
            }
            Ok((
                period.from,
                ModelPricing {
                    input_per_million: period.input,
                    cache_creation_per_million: period.cache_write,
                    cached_input_per_million: period.cache_read,
                    output_per_million: period.output,
                },
            ))
        })
        .collect::<Result<Vec<_>, String>>()
        .and_then(build_periods)
}

/// Checks a period list: non-empty, the first `from` null, later ones valid
/// UTC dates in strictly increasing order.
fn build_periods<T>(raw: Vec<(Option<String>, T)>) -> Result<Periods<T>, String> {
    if raw.is_empty() {
        return Err("needs at least one period".to_string());
    }
    let mut periods = Vec::with_capacity(raw.len());
    let mut previous: Option<NaiveDate> = None;
    for (index, (from, value)) in raw.into_iter().enumerate() {
        let from = match (index, from) {
            (0, None) => None,
            (0, Some(from)) => {
                return Err(format!(
                    "the first period's `from` must be null, not {from:?}"
                ));
            }
            (_, None) => {
                return Err(format!("period {} needs a `from` date", index + 1));
            }
            (_, Some(from)) => {
                let date = parse_date(&from)?;
                if let Some(previous) = previous.filter(|previous| date <= *previous) {
                    return Err(format!(
                        "period dates must strictly increase: {date} follows {previous}"
                    ));
                }
                previous = Some(date);
                Some(date)
            }
        };
        periods.push(Period { from, value });
    }
    Ok(Periods(periods))
}

fn parse_date(text: &str) -> Result<NaiveDate, String> {
    let bytes = text.as_bytes();
    let shaped = bytes.len() == 10
        && bytes.iter().enumerate().all(|(index, byte)| match index {
            4 | 7 => *byte == b'-',
            _ => byte.is_ascii_digit(),
        });
    shaped
        .then(|| NaiveDate::parse_from_str(text, "%Y-%m-%d").ok())
        .flatten()
        .ok_or_else(|| format!("`from` {text:?} is not a YYYY-MM-DD date"))
}

fn check_multiplier(field: &str, value: f64) -> Result<(), String> {
    if value.is_finite() && value > 0.0 && value <= MAX_MULTIPLIER {
        Ok(())
    } else {
        Err(format!(
            "multiplier `{field}` {value} must be above 0 and at most {MAX_MULTIPLIER}"
        ))
    }
}

fn check_threshold(field: &str, value: u64) -> Result<(), String> {
    if (1..=MAX_TOKEN_THRESHOLD).contains(&value) {
        Ok(())
    } else {
        Err(format!(
            "token threshold `{field}` {value} must be within 1 to {MAX_TOKEN_THRESHOLD}"
        ))
    }
}
