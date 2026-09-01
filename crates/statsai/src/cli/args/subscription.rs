use super::*;

#[derive(Debug, Args)]
pub(crate) struct SubscriptionCommand {
    #[command(subcommand)]
    pub(crate) command: SubscriptionSubcommand,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct SubscriptionPrice(i64);

impl SubscriptionPrice {
    pub(crate) const fn cents(self) -> i64 {
        self.0
    }
}

impl FromStr for SubscriptionPrice {
    type Err = String;

    fn from_str(value: &str) -> std::result::Result<Self, Self::Err> {
        let value = value.trim();
        let (whole, fractional) = match value.split_once('.') {
            Some((whole, fractional)) => (whole, Some(fractional)),
            None => (value, None),
        };
        if whole.is_empty() || !whole.bytes().all(|byte| byte.is_ascii_digit()) {
            return Err("price must be a non-negative decimal amount".to_string());
        }
        if fractional.is_some_and(|fractional| {
            fractional.is_empty()
                || fractional.len() > 2
                || !fractional.bytes().all(|byte| byte.is_ascii_digit())
        }) {
            return Err("price must use at most two decimal places".to_string());
        }

        let whole = whole
            .parse::<u64>()
            .map_err(|_| "price is too large".to_string())?;
        let fractional_cents = match fractional {
            None => 0,
            Some(fractional) if fractional.len() == 1 => {
                fractional
                    .parse::<u64>()
                    .map_err(|_| "price is invalid".to_string())?
                    * 10
            }
            Some(fractional) => fractional
                .parse::<u64>()
                .map_err(|_| "price is invalid".to_string())?,
        };
        let cents = whole
            .checked_mul(100)
            .and_then(|cents| cents.checked_add(fractional_cents))
            .ok_or_else(|| "price is too large".to_string())?;
        if cents > MAX_SUBSCRIPTION_PRICE_CENTS as u64 {
            return Err("price must not exceed 1000000.00".to_string());
        }
        Ok(Self(cents as i64))
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct CurrencyCode(String);

impl CurrencyCode {
    pub(crate) fn into_string(self) -> String {
        self.0
    }
}

impl FromStr for CurrencyCode {
    type Err = String;

    fn from_str(value: &str) -> std::result::Result<Self, Self::Err> {
        let value = value.trim();
        if value.len() != 3 || !value.bytes().all(|byte| byte.is_ascii_alphabetic()) {
            return Err("currency must be a three-letter code such as USD".to_string());
        }
        Ok(Self(value.to_ascii_uppercase()))
    }
}

#[derive(Debug, Subcommand)]
pub(crate) enum SubscriptionSubcommand {
    #[command(about = "Register a subscription period")]
    Add {
        #[arg(long, help = "Provider name (claude_code, codex)")]
        provider: String,
        #[arg(long, help = "Existing provider account identifier")]
        provider_account_id: Option<String>,
        #[arg(long, help = "Canonical provider user/account identifier")]
        provider_user_id: Option<String>,
        #[arg(long, help = "Provider email for this account")]
        email: Option<String>,
        #[arg(long, help = "Display label for this account")]
        label: Option<String>,
        #[arg(long, help = "Plan name (e.g. Pro, Max, Team)")]
        plan: String,
        #[arg(
            long,
            help = "Non-negative decimal subscription price (maximum 1000000.00)"
        )]
        price: SubscriptionPrice,
        #[arg(long, default_value = "USD", help = "Three-letter currency code")]
        currency: CurrencyCode,
        #[arg(long, help = "Date the subscription was paid (YYYY-MM-DD or RFC 3339)")]
        paid_at: Option<String>,
        #[arg(long, help = "Subscription period start (YYYY-MM-DD or RFC 3339)")]
        started_at: String,
        #[arg(long, help = "Subscription period end (exclusive)")]
        ended_at: Option<String>,
    },
    #[command(about = "Change to a new subscription period and close the current one")]
    Change {
        #[arg(long, help = "Provider name (claude_code, codex)")]
        provider: String,
        #[arg(long, help = "Existing provider account identifier")]
        provider_account_id: Option<String>,
        #[arg(long, help = "Canonical provider user/account identifier")]
        provider_user_id: Option<String>,
        #[arg(long, help = "Provider email for this account")]
        email: Option<String>,
        #[arg(long, help = "Display label for this account")]
        label: Option<String>,
        #[arg(long, help = "Plan name (e.g. Pro, Max, Team)")]
        plan: String,
        #[arg(
            long,
            help = "Non-negative decimal subscription price (maximum 1000000.00)"
        )]
        price: SubscriptionPrice,
        #[arg(long, default_value = "USD", help = "Three-letter currency code")]
        currency: CurrencyCode,
        #[arg(long, help = "Date the subscription was paid (YYYY-MM-DD or RFC 3339)")]
        paid_at: Option<String>,
        #[arg(long, help = "New subscription period start (YYYY-MM-DD or RFC 3339)")]
        started_at: String,
    },
    #[command(about = "End the active subscription period")]
    End {
        #[arg(long, help = "Provider name (claude_code, codex)")]
        provider: String,
        #[arg(long, help = "Existing provider account identifier")]
        provider_account_id: Option<String>,
        #[arg(long, help = "Canonical provider user/account identifier")]
        provider_user_id: Option<String>,
        #[arg(long, help = "Provider email for this account")]
        email: Option<String>,
        #[arg(long, help = "Subscription period end (exclusive, defaults to now)")]
        ended_at: Option<String>,
    },
    #[command(about = "Remove a subscription period")]
    Remove {
        #[arg(long, help = "Provider name (claude_code, codex)")]
        provider: String,
        #[arg(long, help = "Existing provider account identifier")]
        provider_account_id: Option<String>,
        #[arg(long, help = "Canonical provider user/account identifier")]
        provider_user_id: Option<String>,
        #[arg(long, help = "Provider email for this account")]
        email: Option<String>,
        #[arg(long, help = "Plan name (e.g. Pro, Max, Team)")]
        plan: Option<String>,
        #[arg(long, help = "Subscription period start (YYYY-MM-DD or RFC 3339)")]
        started_at: Option<String>,
        #[arg(long, help = "Remove the active subscription period")]
        current: bool,
    },
    #[command(about = "List all registered subscriptions")]
    List,
}
