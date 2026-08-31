use super::*;

impl Store {
    pub fn upsert_task_verification(
        &self,
        action: TaskVerificationAction,
    ) -> Result<TaskVerification> {
        let now = Utc::now();
        let action_kind = action.action_kind().to_string();
        let action_key = action.action_key();
        let conflicting = self.conflicting_task_verifications(&action)?;
        let existing = latest_task_verification(conflicting.iter());
        let verification = TaskVerification {
            schema_version: TASK_VERIFICATION_SCHEMA_VERSION.to_string(),
            verification_id: existing
                .as_ref()
                .map(|verification| verification.verification_id.clone())
                .unwrap_or_else(|| task_verification_id(&action_kind, &action_key)),
            action_key: action_key.clone(),
            action,
            created_at: existing
                .as_ref()
                .map(|verification| verification.created_at)
                .unwrap_or(now),
            updated_at: now,
        };
        let payload = serde_json::to_string(&verification)?;
        super::begin_immediate_transaction_with_retry(&self.conn)?;
        let result = (|| {
            self.delete_task_verifications_by_ids(
                &conflicting
                    .iter()
                    .map(|verification| verification.verification_id.0.as_str())
                    .collect::<Vec<_>>(),
            )?;
            self.conn.execute(
                r#"
                INSERT INTO task_verifications (
                  verification_id, action_kind, action_key, updated_at, payload
                )
                VALUES (?1, ?2, ?3, ?4, ?5)
                "#,
                params![
                    &verification.verification_id.0,
                    &action_kind,
                    &verification.action_key,
                    verification.updated_at.to_rfc3339(),
                    &payload,
                ],
            )?;
            Ok(())
        })();
        match result {
            Ok(()) => {
                commit_transaction(&self.conn)?;
            }
            Err(error) => {
                rollback(&self.conn);
                return Err(error);
            }
        }
        Ok(verification)
    }

    pub fn task_verifications(&self) -> Result<Vec<TaskVerification>> {
        let mut statement = self.conn.prepare(
            "SELECT payload FROM task_verifications ORDER BY updated_at, verification_id",
        )?;
        let rows = statement.query_map([], |row| row.get::<_, String>(0))?;
        let mut verifications = Vec::new();
        for row in rows {
            verifications.push(serde_json::from_str(&row?)?);
        }
        Ok(resolve_task_verifications(verifications))
    }

    pub fn record_task_verifications_synced(
        &self,
        sink: &str,
        target: &str,
        verifications: &[TaskVerification],
    ) -> Result<()> {
        if verifications.is_empty() {
            return Ok(());
        }
        self.with_immediate_transaction(|| {
            self.record_task_verifications_synced_in_transaction(sink, target, verifications)
        })
    }

    pub(crate) fn record_task_verifications_synced_in_transaction(
        &self,
        sink: &str,
        target: &str,
        verifications: &[TaskVerification],
    ) -> Result<()> {
        for verification in verifications {
            self.record_entity_synced(
                sink,
                target,
                "task_verification",
                &verification.verification_id.0,
                &task_verification_payload_hash(verification)?,
            )?;
        }
        Ok(())
    }

    pub fn merge_task_verification(&self, verification: &TaskVerification) -> Result<bool> {
        let conflicting = self.conflicting_task_verifications(&verification.action)?;
        let existing = latest_task_verification(conflicting.iter());
        if existing
            .as_ref()
            .is_some_and(|current| !task_verification_is_newer(verification, current))
        {
            return Ok(false);
        }

        let mut verification = verification.clone();
        verification.action_key = verification.action.action_key();
        let payload = serde_json::to_string(&verification)?;
        self.with_immediate_transaction(|| {
            self.delete_task_verifications_by_ids(
                &conflicting
                    .iter()
                    .map(|current| current.verification_id.0.as_str())
                    .collect::<Vec<_>>(),
            )?;
            self.conn.execute(
                r#"
                INSERT INTO task_verifications (
                  verification_id, action_kind, action_key, updated_at, payload
                )
                VALUES (?1, ?2, ?3, ?4, ?5)
                "#,
                params![
                    &verification.verification_id.0,
                    verification.action.action_kind(),
                    &verification.action_key,
                    verification.updated_at.to_rfc3339(),
                    &payload,
                ],
            )?;
            Ok(())
        })?;
        Ok(true)
    }

    pub fn project_buckets_for_task_verification(
        &self,
        verification: &TaskVerification,
    ) -> Result<BTreeSet<String>> {
        let span_ids = verification
            .action
            .span_ids()
            .into_iter()
            .map(|span_id| span_id.0.clone())
            .collect::<Vec<_>>();
        if span_ids.is_empty() {
            return Ok(BTreeSet::new());
        }
        let mut buckets = BTreeSet::new();
        let mut statement = self
            .conn
            .prepare("SELECT project_bucket FROM task_spans WHERE span_id = ?1")?;
        for span_id in &span_ids {
            if let Some(project_bucket) = statement
                .query_row(params![span_id], |row| row.get::<_, String>(0))
                .optional()?
            {
                buckets.insert(project_bucket);
            }
        }
        Ok(buckets)
    }

    pub(crate) fn conflicting_task_verifications(
        &self,
        action: &TaskVerificationAction,
    ) -> Result<Vec<TaskVerification>> {
        Ok(self
            .task_verifications_by_action_keys(&task_verification_lookup_keys(action))?
            .into_iter()
            .filter(|current| {
                task_verification_resolution_key(&current.action)
                    == task_verification_resolution_key(action)
            })
            .collect())
    }

    pub(crate) fn task_verifications_by_action_keys(
        &self,
        action_keys: &[String],
    ) -> Result<Vec<TaskVerification>> {
        let mut statement = self
            .conn
            .prepare("SELECT payload FROM task_verifications WHERE action_key = ?1")?;
        let mut verifications = Vec::new();
        for action_key in action_keys {
            let rows = statement.query_map(params![action_key], |row| row.get::<_, String>(0))?;
            for row in rows {
                verifications.push(serde_json::from_str(&row?)?);
            }
        }
        Ok(verifications)
    }

    pub(crate) fn delete_task_verifications_by_ids(&self, verification_ids: &[&str]) -> Result<()> {
        let mut statement = self
            .conn
            .prepare("DELETE FROM task_verifications WHERE verification_id = ?1")?;
        for verification_id in verification_ids {
            statement.execute(params![verification_id])?;
        }
        Ok(())
    }

    pub(crate) fn relevant_task_verifications(
        &self,
        project_buckets: &BTreeSet<String>,
    ) -> Result<Vec<TaskVerification>> {
        if project_buckets.is_empty() {
            return Ok(Vec::new());
        }
        let verifications = self.task_verifications()?;
        let relevant_span_ids = self.span_ids_for_project_buckets(project_buckets)?;
        Ok(verifications
            .into_iter()
            .filter(|verification| {
                verification
                    .action
                    .span_ids()
                    .into_iter()
                    .any(|span_id| relevant_span_ids.contains(span_id.0.as_str()))
            })
            .collect())
    }

    pub(crate) fn span_ids_for_project_buckets(
        &self,
        project_buckets: &BTreeSet<String>,
    ) -> Result<HashSet<String>> {
        let buckets = project_buckets.iter().cloned().collect::<Vec<_>>();
        let mut span_ids = HashSet::new();
        for chunk in buckets.chunks(SQLITE_BUCKET_CHUNK_SIZE) {
            let placeholders = sqlite_in_clause_placeholders(chunk.len());
            let sql =
                format!("SELECT span_id FROM task_spans WHERE project_bucket IN ({placeholders})");
            let params = sqlite_string_params(chunk);
            let mut statement = self.conn.prepare(&sql)?;
            let rows = statement.query_map(params.as_slice(), |row| row.get::<_, String>(0))?;
            for row in rows {
                span_ids.insert(row?);
            }
        }
        Ok(span_ids)
    }
}

pub(crate) fn task_verification_lookup_keys(action: &TaskVerificationAction) -> Vec<String> {
    match action.anchor_span_id() {
        Some(anchor_span_id) => vec![
            format!("status:{}", anchor_span_id.0),
            format!("rename:{}", anchor_span_id.0),
            format!("anchor:{}", anchor_span_id.0),
            format!("accept:{}", anchor_span_id.0),
            format!("reject:{}", anchor_span_id.0),
        ],
        None => vec![action.action_key()],
    }
}

pub(crate) fn task_verification_resolution_key(action: &TaskVerificationAction) -> String {
    match action {
        TaskVerificationAction::Accept { anchor_span_id, .. }
        | TaskVerificationAction::Reject { anchor_span_id, .. } => {
            format!("status:{}", anchor_span_id.0)
        }
        TaskVerificationAction::Rename { anchor_span_id, .. } => {
            format!("rename:{}", anchor_span_id.0)
        }
        TaskVerificationAction::Split { .. } | TaskVerificationAction::Merge { .. } => {
            action.action_key()
        }
    }
}

pub(crate) fn latest_task_verification<'a>(
    verifications: impl Iterator<Item = &'a TaskVerification>,
) -> Option<TaskVerification> {
    verifications
        .max_by(|left, right| {
            left.updated_at
                .cmp(&right.updated_at)
                .then_with(|| left.verification_id.0.cmp(&right.verification_id.0))
        })
        .cloned()
}

pub(crate) fn task_verification_payload_hash(verification: &TaskVerification) -> Result<String> {
    Ok(hash_text(&serde_json::to_string(verification)?))
}

pub(crate) fn task_verification_is_newer(
    left: &TaskVerification,
    right: &TaskVerification,
) -> bool {
    left.updated_at > right.updated_at
        || (left.updated_at == right.updated_at && left.verification_id.0 > right.verification_id.0)
}

pub(crate) fn task_verification_is_after_cursor(
    verification: &TaskVerification,
    cursor: Option<&TaskVerificationCursor>,
) -> bool {
    let Some(cursor) = cursor else {
        return true;
    };
    verification.updated_at > cursor.updated_at
        || (verification.updated_at == cursor.updated_at
            && verification.verification_id.0 > cursor.verification_id.0)
}

pub(crate) fn resolve_task_verifications(
    verifications: Vec<TaskVerification>,
) -> Vec<TaskVerification> {
    let mut resolved_by_key = HashMap::<String, TaskVerification>::new();
    for verification in verifications {
        let resolution_key = task_verification_resolution_key(&verification.action);
        let should_replace = resolved_by_key
            .get(&resolution_key)
            .is_none_or(|current| task_verification_is_newer(&verification, current));
        if should_replace {
            resolved_by_key.insert(resolution_key, verification);
        }
    }
    let mut resolved = resolved_by_key.into_values().collect::<Vec<_>>();
    resolved.sort_by(|left, right| {
        left.updated_at
            .cmp(&right.updated_at)
            .then_with(|| left.verification_id.0.cmp(&right.verification_id.0))
    });
    resolved
}
