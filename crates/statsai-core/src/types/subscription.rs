use super::account::{default_identity_source_unknown, IdentitySource};
use crate::ids::{ProviderAccountId, SubscriptionId};
use chrono::{DateTime, Utc};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum BillingPeriod {
    Monthly,
    Annual,
    Custom,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum SubscriptionStatus {
    Active,
    Paused,
    Cancelled,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct Subscription {
    pub schema_version: String,
    pub subscription_id: SubscriptionId,
    pub provider: String,
    pub provider_account_id: ProviderAccountId,
    pub plan_name: String,
    pub price: i64, // minor units (cents) of the currency
    pub currency: String,
    pub billing_period: BillingPeriod,
    pub paid_at: Option<DateTime<Utc>>,
    pub renewal_day: Option<u8>,
    pub started_at: DateTime<Utc>,
    pub ended_at: Option<DateTime<Utc>>,
    pub current_period_ends_at: Option<DateTime<Utc>>,
    pub status: SubscriptionStatus,
    #[serde(default = "default_identity_source_unknown")]
    pub record_source: IdentitySource,
    pub verified_at: Option<DateTime<Utc>>,
    pub notes: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct VerifiedSubscriptionState {
    pub plan_name: String,
    pub price: i64, // minor units (cents) of the currency
    pub currency: String,
    pub billing_period: BillingPeriod,
    pub paid_at: Option<DateTime<Utc>>,
    pub started_at: DateTime<Utc>,
    pub ended_at: Option<DateTime<Utc>>,
    pub current_period_ends_at: Option<DateTime<Utc>>,
    pub status: SubscriptionStatus,
    pub verified_at: Option<DateTime<Utc>>,
}
