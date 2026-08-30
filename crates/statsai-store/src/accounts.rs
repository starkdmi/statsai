use super::*;

#[derive(Debug, Deserialize)]
struct SubscriptionCompat {
    #[serde(default = "default_subscription_schema_version")]
    schema_version: String,
    subscription_id: SubscriptionId,
    provider: String,
    provider_account_id: Option<ProviderAccountId>,
    plan_name: String,
    price: f64,
    currency: String,
    billing_period: BillingPeriod,
    paid_at: Option<DateTime<Utc>>,
    renewal_day: Option<u8>,
    started_at: Option<DateTime<Utc>>,
    ended_at: Option<DateTime<Utc>>,
    current_period_ends_at: Option<DateTime<Utc>>,
    #[serde(default = "default_subscription_status_active")]
    status: SubscriptionStatus,
    #[serde(default = "default_identity_source_unknown")]
    record_source: IdentitySource,
    verified_at: Option<DateTime<Utc>>,
    notes: Option<String>,
}

fn default_subscription_schema_version() -> String {
    SUBSCRIPTION_SCHEMA_VERSION.to_string()
}

fn default_subscription_status_active() -> SubscriptionStatus {
    SubscriptionStatus::Active
}

fn default_identity_source_unknown() -> IdentitySource {
    IdentitySource::Unknown
}

pub(crate) fn deserialize_subscription_payload(
    payload: &str,
    provider_account_id_column: Option<&str>,
) -> Result<Subscription> {
    if let Ok(subscription) = serde_json::from_str(payload) {
        return Ok(subscription);
    }

    let compat: SubscriptionCompat =
        serde_json::from_str(payload).context("deserialize legacy subscription payload")?;
    let provider_account_id = compat
        .provider_account_id
        .or_else(|| {
            provider_account_id_column
                .map(str::trim)
                .filter(|value| !value.is_empty())
                .map(|value| ProviderAccountId(value.to_string()))
        })
        .unwrap_or_else(|| {
            provider_account_id(
                &compat.provider,
                &format!("legacy_subscription:{}", compat.subscription_id.0),
            )
        });
    let started_at = compat
        .started_at
        .or(compat.paid_at)
        .or(compat.current_period_ends_at)
        .or(compat.ended_at)
        .unwrap_or(DateTime::<Utc>::UNIX_EPOCH);

    Ok(Subscription {
        schema_version: compat.schema_version,
        subscription_id: compat.subscription_id,
        provider: compat.provider,
        provider_account_id,
        plan_name: compat.plan_name,
        price: (compat.price * 100.0).round() as i64,
        currency: compat.currency,
        billing_period: compat.billing_period,
        paid_at: compat.paid_at,
        renewal_day: compat.renewal_day,
        started_at,
        ended_at: compat.ended_at,
        current_period_ends_at: compat.current_period_ends_at,
        status: compat.status,
        record_source: compat.record_source,
        verified_at: compat.verified_at,
        notes: compat.notes,
    })
}

fn parse_bool_metadata_value(key: &str, value: &str) -> Result<bool> {
    match value.trim() {
        "1" | "true" => Ok(true),
        "0" | "false" => Ok(false),
        other => bail!("invalid boolean metadata value for {key}: {other}"),
    }
}

impl Store {
    pub fn upsert_account(&self, account: &ProviderAccount) -> Result<()> {
        let payload = serde_json::to_string(account)?;
        self.conn.execute(
            r#"
            INSERT INTO provider_accounts (provider_account_id, provider, payload, updated_at)
            VALUES (?1, ?2, ?3, ?4)
            ON CONFLICT(provider_account_id) DO UPDATE SET
              provider = excluded.provider,
              payload = excluded.payload,
              updated_at = excluded.updated_at
            "#,
            params![
                &account.provider_account_id.0,
                &account.provider,
                &payload,
                account.updated_at.to_rfc3339()
            ],
        )?;
        Ok(())
    }

    pub fn account(
        &self,
        provider_account_id: &ProviderAccountId,
    ) -> Result<Option<ProviderAccount>> {
        Ok(self
            .conn
            .query_row(
                "SELECT payload FROM provider_accounts WHERE provider_account_id = ?1",
                params![&provider_account_id.0],
                |row| row.get::<_, String>(0),
            )
            .optional()?
            .map(|payload| serde_json::from_str(&payload))
            .transpose()?)
    }

    pub fn list_accounts(&self) -> Result<Vec<ProviderAccount>> {
        let mut stmt = self.conn.prepare(
            "SELECT payload FROM provider_accounts ORDER BY provider, provider_account_id",
        )?;
        let rows = stmt.query_map([], |row| row.get::<_, String>(0))?;
        let mut accounts = Vec::new();
        for row in rows {
            accounts.push(serde_json::from_str(&row?)?);
        }
        Ok(accounts)
    }

    pub fn delete_account(&self, provider_account_id: &ProviderAccountId) -> Result<bool> {
        Ok(self.conn.execute(
            "DELETE FROM provider_accounts WHERE provider_account_id = ?1",
            params![&provider_account_id.0],
        )? > 0)
    }

    pub fn upsert_source_account_assignment(
        &self,
        assignment: &SourceAccountAssignment,
    ) -> Result<()> {
        let payload = serde_json::to_string(assignment)?;
        self.conn.execute(
            r#"
            INSERT INTO source_account_assignments (
              assignment_id, source_id, provider, provider_account_id,
              started_at, ended_at, payload, updated_at
            )
            VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)
            ON CONFLICT(assignment_id) DO UPDATE SET
              source_id = excluded.source_id,
              provider = excluded.provider,
              provider_account_id = excluded.provider_account_id,
              started_at = excluded.started_at,
              ended_at = excluded.ended_at,
              payload = excluded.payload,
              updated_at = excluded.updated_at
            "#,
            params![
                &assignment.assignment_id.0,
                &assignment.source_id.0,
                &assignment.provider,
                &assignment.provider_account_id.0,
                assignment.started_at.to_rfc3339(),
                assignment.ended_at.map(|date| date.to_rfc3339()),
                &payload,
                assignment.updated_at.to_rfc3339(),
            ],
        )?;
        Ok(())
    }

    pub fn source_account_assignment(
        &self,
        assignment_id: &SourceAccountAssignmentId,
    ) -> Result<Option<SourceAccountAssignment>> {
        Ok(self
            .conn
            .query_row(
                "SELECT payload FROM source_account_assignments WHERE assignment_id = ?1",
                params![&assignment_id.0],
                |row| row.get::<_, String>(0),
            )
            .optional()?
            .map(|payload| serde_json::from_str(&payload))
            .transpose()?)
    }

    pub fn list_source_account_assignments(&self) -> Result<Vec<SourceAccountAssignment>> {
        let mut stmt = self.conn.prepare(
            r#"
            SELECT payload
            FROM source_account_assignments
            ORDER BY provider, source_id, started_at, assignment_id
            "#,
        )?;
        let rows = stmt.query_map([], |row| row.get::<_, String>(0))?;
        let mut assignments = Vec::new();
        for row in rows {
            assignments.push(serde_json::from_str(&row?)?);
        }
        Ok(assignments)
    }

    pub fn list_source_account_assignments_for_source(
        &self,
        source_id: &SourceId,
    ) -> Result<Vec<SourceAccountAssignment>> {
        let mut stmt = self.conn.prepare(
            r#"
            SELECT payload
            FROM source_account_assignments
            WHERE source_id = ?1
            ORDER BY started_at, assignment_id
            "#,
        )?;
        let rows = stmt.query_map(params![&source_id.0], |row| row.get::<_, String>(0))?;
        let mut assignments = Vec::new();
        for row in rows {
            assignments.push(serde_json::from_str(&row?)?);
        }
        Ok(assignments)
    }

    pub fn delete_source_account_assignment(
        &self,
        assignment_id: &SourceAccountAssignmentId,
    ) -> Result<bool> {
        Ok(self.conn.execute(
            "DELETE FROM source_account_assignments WHERE assignment_id = ?1",
            params![&assignment_id.0],
        )? > 0)
    }

    pub fn upsert_subscription(&self, subscription: &Subscription) -> Result<()> {
        let payload = serde_json::to_string(subscription)?;
        self.conn.execute(
            r#"
            INSERT INTO subscriptions (subscription_id, provider, provider_account_id, payload)
            VALUES (?1, ?2, ?3, ?4)
            ON CONFLICT(subscription_id) DO UPDATE SET
              provider = excluded.provider,
              provider_account_id = excluded.provider_account_id,
              payload = excluded.payload
            "#,
            params![
                &subscription.subscription_id.0,
                &subscription.provider,
                subscription.provider_account_id.0.as_str(),
                &payload
            ],
        )?;
        Ok(())
    }

    pub fn subscription(&self, subscription_id: &SubscriptionId) -> Result<Option<Subscription>> {
        let row = self
            .conn
            .query_row(
                "SELECT payload, provider_account_id FROM subscriptions WHERE subscription_id = ?1",
                params![&subscription_id.0],
                |row| Ok((row.get::<_, String>(0)?, row.get::<_, Option<String>>(1)?)),
            )
            .optional()?;
        match row {
            Some((payload, provider_account_id)) => Ok(Some(deserialize_subscription_payload(
                &payload,
                provider_account_id.as_deref(),
            )?)),
            None => Ok(None),
        }
    }

    pub fn list_subscriptions(&self) -> Result<Vec<Subscription>> {
        let mut stmt = self
            .conn
            .prepare(
                "SELECT payload, provider_account_id FROM subscriptions ORDER BY provider, subscription_id",
            )?;
        let rows = stmt.query_map([], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, Option<String>>(1)?))
        })?;
        let mut subscriptions = Vec::new();
        for row in rows {
            let (payload, provider_account_id) = row?;
            subscriptions.push(deserialize_subscription_payload(
                &payload,
                provider_account_id.as_deref(),
            )?);
        }
        Ok(subscriptions)
    }

    pub fn delete_subscription(&self, subscription_id: &SubscriptionId) -> Result<bool> {
        Ok(self.conn.execute(
            "DELETE FROM subscriptions WHERE subscription_id = ?1",
            params![&subscription_id.0],
        )? > 0)
    }

    pub fn metadata_value(&self, key: &str) -> Result<Option<String>> {
        self.conn
            .query_row(
                "SELECT value FROM local_metadata WHERE key = ?1",
                params![key],
                |row| row.get::<_, String>(0),
            )
            .optional()
            .map_err(Into::into)
    }

    pub fn set_metadata_value(&self, key: &str, value: &str) -> Result<()> {
        self.conn.execute(
            r#"
            INSERT INTO local_metadata (key, value, updated_at)
            VALUES (?1, ?2, ?3)
            ON CONFLICT(key) DO UPDATE SET
              value = excluded.value,
              updated_at = excluded.updated_at
            "#,
            params![key, value, Utc::now().to_rfc3339()],
        )?;
        Ok(())
    }

    pub fn sync_preferences(&self) -> Result<SyncPreferences> {
        let include_projects = self
            .metadata_value(SYNC_INCLUDE_PROJECTS_METADATA_KEY)?
            .as_deref()
            .map(|value| parse_bool_metadata_value(SYNC_INCLUDE_PROJECTS_METADATA_KEY, value))
            .transpose()?
            .unwrap_or(false);
        let include_tasks = self
            .metadata_value(SYNC_INCLUDE_TASKS_METADATA_KEY)?
            .as_deref()
            .map(|value| parse_bool_metadata_value(SYNC_INCLUDE_TASKS_METADATA_KEY, value))
            .transpose()?
            .unwrap_or(false);
        Ok(SyncPreferences {
            include_projects,
            include_tasks,
        }
        .normalized())
    }

    pub fn set_sync_preferences(&self, preferences: SyncPreferences) -> Result<()> {
        let preferences = preferences.normalized();
        self.set_metadata_value(
            SYNC_INCLUDE_PROJECTS_METADATA_KEY,
            if preferences.include_projects {
                "1"
            } else {
                "0"
            },
        )?;
        self.set_metadata_value(
            SYNC_INCLUDE_TASKS_METADATA_KEY,
            if preferences.include_tasks { "1" } else { "0" },
        )?;
        Ok(())
    }
}
