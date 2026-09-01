use anyhow::{Context, Result};
use chrono::{DateTime, Local, NaiveDate, Utc};
use serde::Serialize;
use serde_json::{json, Value};
use statsai_core::{home_dir, Subscription, SubscriptionStatus};

pub(crate) fn format_cursor(date: Option<String>, id: Option<&str>) -> String {
    match (date, id) {
        (Some(date), Some(id)) => format!("{date}/{id}"),
        _ => "none".to_string(),
    }
}

pub(crate) fn format_local_timestamp(timestamp: DateTime<Utc>) -> String {
    timestamp
        .with_timezone(&Local)
        .format("%Y-%m-%d %H:%M:%S %Z")
        .to_string()
}

pub(crate) fn print_json_lines<T: Serialize>(values: &[T]) -> Result<()> {
    for value in values {
        println!("{}", serde_json::to_string(value)?);
    }
    Ok(())
}

pub(crate) fn parse_date(value: &str) -> Result<DateTime<Utc>> {
    if let Ok(date) = DateTime::parse_from_rfc3339(value) {
        return Ok(date.with_timezone(&Utc));
    }
    let date = NaiveDate::parse_from_str(value, "%Y-%m-%d")?;
    let datetime = date
        .and_hms_opt(0, 0, 0)
        .context("failed to build midnight timestamp")?;
    Ok(datetime.and_utc())
}

pub(crate) fn abbreviate_home(path: &str) -> String {
    let Some(home) = home_dir() else {
        return path.to_string();
    };
    let home = home.to_string_lossy();
    path.strip_prefix(home.as_ref())
        .map(|rest| format!("~{rest}"))
        .unwrap_or_else(|| path.to_string())
}

pub(crate) fn format_u64(value: u64) -> String {
    let text = value.to_string();
    let mut out = String::with_capacity(text.len() + text.len() / 3);
    for (index, ch) in text.chars().rev().enumerate() {
        if index > 0 && index % 3 == 0 {
            out.push(',');
        }
        out.push(ch);
    }
    out.chars().rev().collect()
}

pub(crate) fn major_unit_amount(cents: i64) -> f64 {
    cents as f64 / 100.0
}

pub(crate) fn usd_amount_json(cost: Option<i64>) -> Value {
    cost.map_or(Value::Null, |cents| json!(major_unit_amount(cents)))
}

pub(crate) fn format_cost(cost: Option<i64>) -> String {
    cost.map(|cents| {
        let dollars = major_unit_amount(cents);
        format!("${dollars:.2}")
    })
    .unwrap_or_else(|| "unknown".to_string())
}

pub(crate) fn format_subscription_price(price_cents: i64, currency: &str) -> String {
    let price = major_unit_amount(price_cents);
    if currency.eq_ignore_ascii_case("USD") {
        format!("${price:.2}")
    } else {
        format!("{price:.2} {currency}")
    }
}

pub(crate) fn subscription_json_value(subscription: &Subscription) -> Value {
    let mut value = serde_json::to_value(subscription).expect("serialize subscription");
    value["price_cents"] = json!(subscription.price);
    value["price"] = json!(major_unit_amount(subscription.price));
    value
}

pub(crate) fn print_subscription_json(subscription: &Subscription) -> Result<()> {
    println!(
        "{}",
        serde_json::to_string_pretty(&subscription_json_value(subscription))?
    );
    Ok(())
}

pub(crate) fn format_ratio(value: Option<f64>) -> String {
    value
        .map(|value| format!("{value:.2}x"))
        .unwrap_or_else(|| "unknown".to_string())
}

pub(crate) fn truncate_label(value: &str, width: usize) -> String {
    if value.chars().count() <= width {
        return value.to_string();
    }
    value
        .chars()
        .take(width.saturating_sub(1))
        .collect::<String>()
        + "…"
}

pub(crate) fn subscription_status_label(status: &SubscriptionStatus) -> &'static str {
    match status {
        SubscriptionStatus::Active => "active",
        SubscriptionStatus::Paused => "paused",
        SubscriptionStatus::Cancelled => "cancelled",
    }
}
