use super::*;

#[derive(Debug, Clone, Default)]
pub(crate) struct BucketLabelStats {
    pub(crate) document_count: usize,
    pub(crate) title_document_frequency: HashMap<String, usize>,
    pub(crate) token_document_frequency: HashMap<String, usize>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub(crate) enum TitleCandidateSource {
    SpanTitle,
    SummaryPreview,
    TodoExcerpt,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SpanTitleOrigin {
    UserPrompt,
    ThreadName,
    SessionTitle,
    SessionTitleWeak,
    SummaryDerived,
    TodoDerived,
    Default,
    Other,
}

#[derive(Debug, Clone)]
pub(crate) struct TitleCandidate {
    pub(crate) title: String,
    pub(crate) normalized: String,
    pub(crate) signal_score: i32,
    pub(crate) source: TitleCandidateSource,
    pub(crate) span_title_origin: Option<SpanTitleOrigin>,
    pub(crate) span_index: usize,
    pub(crate) topic_tokens: Vec<String>,
}

#[cfg(test)]
pub(crate) fn choose_work_item_title(spans: &[SpanContext]) -> String {
    choose_work_item_title_with_stats(spans, &BucketLabelStats::default())
}

pub(crate) fn choose_work_item_title_with_stats(
    spans: &[SpanContext],
    bucket_label_stats: &BucketLabelStats,
) -> String {
    let primary_candidates = collect_primary_title_candidates(spans);
    if primary_title_candidates_are_sufficient(&primary_candidates) {
        if let Some(title) = best_title_from_candidates(&primary_candidates, bucket_label_stats) {
            return title;
        }
    }
    let ordered_candidates = collect_title_candidates(spans);
    if let Some(title) = best_title_from_candidates(&ordered_candidates, bucket_label_stats) {
        return title;
    }
    for context in spans {
        let Some(branch_family) = context.span.branch_family.as_deref() else {
            continue;
        };
        let Some(title) = humanize_branch_family(branch_family) else {
            continue;
        };
        if !task_title_is_generic(Some(title.as_str()))
            && !task_title_is_weak_signal(Some(title.as_str()))
        {
            return title;
        }
    }
    if let Some(title) = choose_relaxed_work_item_title_with_stats(spans, bucket_label_stats) {
        return title;
    }
    "Unresolved work item".to_string()
}

pub(crate) fn primary_title_candidates_are_sufficient(candidates: &[TitleCandidate]) -> bool {
    candidates.iter().any(|candidate| {
        matches!(
            candidate.span_title_origin,
            Some(
                SpanTitleOrigin::UserPrompt
                    | SpanTitleOrigin::SummaryDerived
                    | SpanTitleOrigin::TodoDerived
            )
        ) && candidate.signal_score > 0
    })
}

pub(crate) fn best_title_from_candidates(
    ordered_candidates: &[TitleCandidate],
    bucket_label_stats: &BucketLabelStats,
) -> Option<String> {
    let mut best_title = None::<String>;
    let mut best_score = i32::MIN;
    let mut frequencies = HashMap::<String, usize>::new();
    let mut topic_frequencies = HashMap::<String, usize>::new();
    let mut source_support = HashMap::<String, BTreeSet<TitleCandidateSource>>::new();

    for candidate in ordered_candidates {
        *frequencies.entry(candidate.normalized.clone()).or_default() += 1;
        for token in &candidate.topic_tokens {
            *topic_frequencies.entry(token.clone()).or_default() += 1;
        }
        source_support
            .entry(candidate.normalized.clone())
            .or_default()
            .insert(candidate.source);
    }

    for candidate in ordered_candidates {
        let frequency = frequencies.get(&candidate.normalized).copied().unwrap_or(1);
        let topic_overlap = candidate
            .topic_tokens
            .iter()
            .map(|token| {
                topic_frequencies
                    .get(token)
                    .copied()
                    .unwrap_or_default()
                    .saturating_sub(1)
            })
            .sum::<usize>();
        let score = title_candidate_score(
            candidate,
            frequency,
            topic_overlap,
            source_support
                .get(&candidate.normalized)
                .map_or(1, BTreeSet::len),
            ordered_candidates,
            &frequencies,
            bucket_label_stats,
        );
        if score > best_score {
            best_score = score;
            best_title = Some(candidate.title.clone());
        }
    }

    if let Some(title) = best_title {
        return Some(title);
    }
    None
}

pub(crate) fn title_candidate_score(
    candidate: &TitleCandidate,
    frequency: usize,
    topic_overlap: usize,
    source_support_count: usize,
    ordered_candidates: &[TitleCandidate],
    frequencies: &HashMap<String, usize>,
    bucket_label_stats: &BucketLabelStats,
) -> i32 {
    let title = candidate.title.as_str();
    let token_count = candidate.normalized.split_whitespace().count();
    let length = title.chars().count();
    let lowercase = title.to_ascii_lowercase();
    let digit_count = title
        .chars()
        .filter(|character| character.is_ascii_digit())
        .count();
    let alpha_count = title
        .chars()
        .filter(|character| character.is_ascii_alphabetic())
        .count();
    let opaque_token_count = title
        .split_whitespace()
        .filter(|token| looks_like_opaque_candidate_token(token))
        .count();

    let mut score = 0;
    let mut code_penalties = 0;
    score += match token_count {
        3..=9 => 10,
        2..=12 => 7,
        13..=18 => 2,
        0..=1 => -8,
        _ => -4,
    };
    score += match length {
        18..=72 => 6,
        10..=96 => 2,
        97..=140 => -2,
        _ => -6,
    };
    if title
        .chars()
        .next()
        .is_some_and(|character| matches!(character, '-' | '=' | '`' | '"' | '['))
    {
        score -= 4;
    }
    if title
        .chars()
        .next()
        .is_some_and(|character| character.is_ascii_digit())
    {
        score -= 3;
    }
    if digit_count > alpha_count && digit_count >= 4 {
        score -= 4;
    }
    if lowercase.ends_with('?') {
        score -= 2;
    }
    score -= (opaque_token_count.min(3) as i32) * 4;

    for fragment in [
        "%%bash",
        "pip install",
        "mkdir -p",
        "export ",
        "/users/",
        "/kaggle/",
        ".jsonl",
        ".csv",
        ".ipynb",
        "http://",
        "https://",
        " | ",
        " = ",
        "==",
        "```",
        "<turn|>",
        "automation:",
        "tool web_search",
        "tool apply_patch",
        "token=eyj",
        "jupyter-proxy.kaggle.net",
    ] {
        if lowercase.contains(fragment) {
            code_penalties += 1;
        }
    }

    score -= code_penalties * 5;
    score += candidate.signal_score * 2;
    score += task_title_corpus_specificity_score(title, bucket_label_stats) * 2;
    score += task_title_corpus_phraseness_score(candidate, bucket_label_stats);
    score -= title_candidate_completeness_penalty(candidate, ordered_candidates, frequencies);
    score += title_candidate_source_bonus(candidate, source_support_count);
    score += title_candidate_context_score(candidate, topic_overlap);
    score += title_candidate_position_bonus(candidate);
    if code_penalties == 0 && (source_support_count > 1 || topic_overlap > 0) {
        score += (frequency.saturating_sub(1).min(2) as i32) * 2;
    }
    score += (topic_overlap.min(12) as i32) * 2;
    if frequency == 1 && topic_overlap == 0 {
        score -= 4;
    }
    if matches!(candidate.source, TitleCandidateSource::SpanTitle)
        && source_support_count == 1
        && topic_overlap == 0
    {
        score -= 8;
    }
    if matches!(candidate.source, TitleCandidateSource::SpanTitle)
        && source_support_count == 1
        && task_title_corpus_specificity_score(title, bucket_label_stats) <= 0
    {
        score -= 6;
    }
    if matches!(
        candidate.span_title_origin,
        Some(SpanTitleOrigin::ThreadName | SpanTitleOrigin::SessionTitleWeak)
    ) && source_support_count == 1
        && topic_overlap == 0
    {
        score -= 10;
    }
    if matches!(candidate.span_title_origin, Some(SpanTitleOrigin::Default)) {
        score -= 10;
    }

    score
}

pub(crate) fn smoothed_inverse_document_frequency(
    document_count: usize,
    document_frequency: usize,
) -> f64 {
    if document_count == 0 {
        return 0.0;
    }
    ((document_count as f64 + 1.0) / (document_frequency as f64 + 1.0)).ln()
}

pub(crate) fn task_title_corpus_specificity_score(
    title: &str,
    bucket_label_stats: &BucketLabelStats,
) -> i32 {
    if bucket_label_stats.document_count <= 1 {
        return 0;
    }
    let normalized = normalize_task_title(title);
    if normalized.is_empty() {
        return -6;
    }
    let title_document_frequency = bucket_label_stats
        .title_document_frequency
        .get(&normalized)
        .copied()
        .unwrap_or(1);
    let title_ratio = title_document_frequency as f64 / bucket_label_stats.document_count as f64;
    let title_idf = smoothed_inverse_document_frequency(
        bucket_label_stats.document_count,
        title_document_frequency,
    );
    let topic_tokens = title_topic_tokens(title);
    let average_token_idf = if topic_tokens.is_empty() {
        0.0
    } else {
        topic_tokens
            .iter()
            .map(|token| {
                let token_document_frequency = bucket_label_stats
                    .token_document_frequency
                    .get(token)
                    .copied()
                    .unwrap_or(1);
                smoothed_inverse_document_frequency(
                    bucket_label_stats.document_count,
                    token_document_frequency,
                )
            })
            .sum::<f64>()
            / topic_tokens.len() as f64
    };
    let content_bonus = (topic_tokens.len().min(6) as f64) * 0.4;

    ((average_token_idf * 4.0) + (title_idf * 2.5) + content_bonus - (title_ratio * 14.0)).round()
        as i32
}

pub(crate) fn task_title_corpus_phraseness_score(
    candidate: &TitleCandidate,
    bucket_label_stats: &BucketLabelStats,
) -> i32 {
    if bucket_label_stats.document_count <= 1 || candidate.topic_tokens.len() < 2 {
        return 0;
    }
    let title_document_frequency = bucket_label_stats
        .title_document_frequency
        .get(&candidate.normalized)
        .copied()
        .unwrap_or(1);
    let phrase_probability =
        title_document_frequency as f64 / bucket_label_stats.document_count as f64;
    let independent_probability = candidate
        .topic_tokens
        .iter()
        .map(|token| {
            bucket_label_stats
                .token_document_frequency
                .get(token)
                .copied()
                .unwrap_or(1) as f64
                / bucket_label_stats.document_count as f64
        })
        .product::<f64>();
    if phrase_probability <= 0.0 || independent_probability <= 0.0 {
        return 0;
    }
    ((phrase_probability / independent_probability).ln() * 2.0)
        .clamp(-6.0, 8.0)
        .round() as i32
}

pub(crate) fn title_candidate_source_bonus(
    candidate: &TitleCandidate,
    source_support_count: usize,
) -> i32 {
    let source_bonus = match candidate.source {
        TitleCandidateSource::SpanTitle => match candidate.span_title_origin {
            Some(SpanTitleOrigin::UserPrompt) => 6,
            Some(SpanTitleOrigin::SummaryDerived) => 5,
            Some(SpanTitleOrigin::TodoDerived) => 6,
            Some(SpanTitleOrigin::ThreadName) => -4,
            Some(SpanTitleOrigin::SessionTitle) => -2,
            Some(SpanTitleOrigin::SessionTitleWeak) => -5,
            Some(SpanTitleOrigin::Default) => -8,
            Some(SpanTitleOrigin::Other) | None => 0,
        },
        TitleCandidateSource::SummaryPreview => 5,
        TitleCandidateSource::TodoExcerpt => 6,
    };
    source_bonus + ((source_support_count.saturating_sub(1).min(2) as i32) * 2)
}

pub(crate) fn title_candidate_position_bonus(candidate: &TitleCandidate) -> i32 {
    match candidate.span_index {
        0 => 2,
        1 => 1,
        _ => 0,
    }
}

pub(crate) fn title_candidate_context_score(
    candidate: &TitleCandidate,
    topic_overlap: usize,
) -> i32 {
    let topic_token_count = candidate.topic_tokens.len();
    if topic_token_count == 0 {
        return -6;
    }
    let average_overlap = topic_overlap as f64 / topic_token_count as f64;
    match average_overlap {
        overlap if overlap >= 3.0 => 6,
        overlap if overlap >= 1.5 => 3,
        0.0 => -2,
        _ => 0,
    }
}

pub(crate) fn title_candidate_completeness_penalty(
    candidate: &TitleCandidate,
    ordered_candidates: &[TitleCandidate],
    frequencies: &HashMap<String, usize>,
) -> i32 {
    let candidate_frequency = frequencies.get(&candidate.normalized).copied().unwrap_or(1);
    if candidate_frequency == 0 {
        return 0;
    }
    let candidate_token_set = candidate
        .topic_tokens
        .iter()
        .map(String::as_str)
        .collect::<HashSet<_>>();
    let mut max_conditional_support = 0.0f64;
    for other in ordered_candidates {
        if other.normalized == candidate.normalized || other.title.len() <= candidate.title.len() {
            continue;
        }
        let subsumes = other.normalized.contains(&candidate.normalized)
            || (!candidate_token_set.is_empty()
                && candidate_token_set.len() >= 2
                && candidate_token_set.iter().all(|token| {
                    other
                        .topic_tokens
                        .iter()
                        .any(|other_token| other_token.as_str() == *token)
                }));
        if !subsumes {
            continue;
        }
        let other_frequency = frequencies.get(&other.normalized).copied().unwrap_or(1);
        let conditional_support = other_frequency as f64 / candidate_frequency as f64;
        if conditional_support > max_conditional_support {
            max_conditional_support = conditional_support;
        }
    }
    if max_conditional_support >= 0.75 {
        8
    } else if max_conditional_support >= 0.5 {
        4
    } else {
        0
    }
}

pub(crate) fn collect_title_candidates(spans: &[SpanContext]) -> Vec<TitleCandidate> {
    let mut candidates = Vec::<TitleCandidate>::new();
    for (span_index, context) in spans.iter().enumerate() {
        push_title_candidate(
            &mut candidates,
            Some(context.span.title.as_str()),
            TitleCandidateSource::SpanTitle,
            context.span.title_source.as_deref(),
            span_index,
        );
        if !span_title_needs_fallback_candidates(context) {
            continue;
        }
        push_title_candidate(
            &mut candidates,
            context.span.summary_preview.as_deref(),
            TitleCandidateSource::SummaryPreview,
            None,
            span_index,
        );
        push_title_candidate(
            &mut candidates,
            context.span.todo_excerpt.as_deref(),
            TitleCandidateSource::TodoExcerpt,
            None,
            span_index,
        );
    }
    candidates
}

pub(crate) fn collect_primary_title_candidates(spans: &[SpanContext]) -> Vec<TitleCandidate> {
    let mut candidates = Vec::<TitleCandidate>::new();
    for (span_index, context) in spans.iter().enumerate() {
        push_title_candidate(
            &mut candidates,
            Some(context.span.title.as_str()),
            TitleCandidateSource::SpanTitle,
            context.span.title_source.as_deref(),
            span_index,
        );
    }
    candidates
}

pub(crate) fn push_title_candidate(
    candidates: &mut Vec<TitleCandidate>,
    raw: Option<&str>,
    source: TitleCandidateSource,
    title_source: Option<&str>,
    span_index: usize,
) {
    let Some(title) = materialize_title_candidate(raw, source) else {
        return;
    };
    if task_title_is_generic(Some(title.as_str()))
        || task_title_is_weak_signal(Some(title.as_str()))
    {
        return;
    }
    let normalized = normalize_task_title(&title);
    if normalized.is_empty() {
        return;
    }
    let signal_score = task_title_signal_score(Some(title.as_str()));
    let topic_tokens = title_topic_tokens(&title).into_iter().collect::<Vec<_>>();
    candidates.push(TitleCandidate {
        title,
        normalized,
        signal_score,
        source,
        span_title_origin: span_title_origin(source, title_source),
        span_index,
        topic_tokens,
    });
}

pub(crate) fn choose_relaxed_work_item_title_with_stats(
    spans: &[SpanContext],
    bucket_label_stats: &BucketLabelStats,
) -> Option<String> {
    let ordered_candidates = collect_relaxed_title_candidates(spans);
    let best_title = best_title_from_candidates(&ordered_candidates, bucket_label_stats)?;
    (task_title_signal_score(Some(best_title.as_str())) > -6).then_some(best_title)
}

pub(crate) fn collect_relaxed_title_candidates(spans: &[SpanContext]) -> Vec<TitleCandidate> {
    let mut candidates = Vec::<TitleCandidate>::new();
    for (span_index, context) in spans.iter().enumerate() {
        push_relaxed_title_candidate(
            &mut candidates,
            Some(context.span.title.as_str()),
            TitleCandidateSource::SpanTitle,
            context.span.title_source.as_deref(),
            span_index,
        );
        if !span_title_needs_fallback_candidates(context) {
            continue;
        }
        push_relaxed_title_candidate(
            &mut candidates,
            context.span.summary_preview.as_deref(),
            TitleCandidateSource::SummaryPreview,
            None,
            span_index,
        );
        push_relaxed_title_candidate(
            &mut candidates,
            context.span.todo_excerpt.as_deref(),
            TitleCandidateSource::TodoExcerpt,
            None,
            span_index,
        );
    }
    candidates
}

pub(crate) fn push_relaxed_title_candidate(
    candidates: &mut Vec<TitleCandidate>,
    raw: Option<&str>,
    source: TitleCandidateSource,
    title_source: Option<&str>,
    span_index: usize,
) {
    if materialize_title_candidate(raw, source).is_some() {
        return;
    }
    let Some(title) = raw.and_then(|value| summarize_task_text(Some(value), 90)) else {
        return;
    };
    if task_title_is_session_meta(Some(title.as_str())) {
        return;
    }
    if !relaxed_candidate_looks_contentful(title.as_str()) {
        return;
    }
    let normalized = normalize_task_title(&title);
    if normalized.is_empty() {
        return;
    }
    let topic_tokens = title_topic_tokens(&title).into_iter().collect::<Vec<_>>();
    if topic_tokens.is_empty() && task_title_signal_score(Some(title.as_str())) < 0 {
        return;
    }
    let signal_score = task_title_signal_score(Some(title.as_str()));
    candidates.push(TitleCandidate {
        title,
        normalized,
        signal_score,
        source,
        span_title_origin: span_title_origin(source, title_source),
        span_index,
        topic_tokens,
    });
}

pub(crate) fn relaxed_candidate_looks_contentful(title: &str) -> bool {
    if task_title_is_generic(Some(title)) || task_title_is_weak_signal(Some(title)) {
        return false;
    }
    let signal_score = task_title_signal_score(Some(title));
    if signal_score <= 0 {
        return false;
    }
    let alpha_count = title
        .chars()
        .filter(|character| character.is_ascii_alphabetic())
        .count();
    let digit_count = title
        .chars()
        .filter(|character| character.is_ascii_digit())
        .count();
    let topic_token_count = title_topic_tokens(title).len();
    alpha_count > digit_count && (2..=8).contains(&topic_token_count)
}

pub(crate) fn span_title_needs_fallback_candidates(context: &SpanContext) -> bool {
    if context.title_is_generic()
        || context.title_is_weak_signal()
        || context.title_signal_score() < 8
    {
        return true;
    }
    !matches!(
        span_title_origin(
            TitleCandidateSource::SpanTitle,
            context.span.title_source.as_deref()
        ),
        Some(
            SpanTitleOrigin::UserPrompt
                | SpanTitleOrigin::SummaryDerived
                | SpanTitleOrigin::TodoDerived
        )
    )
}

pub(crate) fn materialize_title_candidate(
    raw: Option<&str>,
    source: TitleCandidateSource,
) -> Option<String> {
    match source {
        TitleCandidateSource::SpanTitle => raw
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(ToOwned::to_owned),
        TitleCandidateSource::SummaryPreview | TitleCandidateSource::TodoExcerpt => {
            task_title_from_prompt(raw)
        }
    }
}

pub(crate) fn span_title_origin(
    source: TitleCandidateSource,
    title_source: Option<&str>,
) -> Option<SpanTitleOrigin> {
    if !matches!(source, TitleCandidateSource::SpanTitle) {
        return None;
    }
    Some(match title_source.unwrap_or_default() {
        "user_prompt" => SpanTitleOrigin::UserPrompt,
        "thread_name" => SpanTitleOrigin::ThreadName,
        "session_title" => SpanTitleOrigin::SessionTitle,
        "session_title_weak" => SpanTitleOrigin::SessionTitleWeak,
        "summary" | "summary_diffs" | "generated_title" | "session_summary" => {
            SpanTitleOrigin::SummaryDerived
        }
        "todo_excerpt" => SpanTitleOrigin::TodoDerived,
        "default" => SpanTitleOrigin::Default,
        _ => SpanTitleOrigin::Other,
    })
}

pub(crate) fn looks_like_opaque_candidate_token(token: &str) -> bool {
    let trimmed = token.trim_matches(|character: char| {
        matches!(
            character,
            ',' | ';' | ':' | '"' | '\'' | '(' | ')' | '[' | ']'
        )
    });
    if trimmed.len() < 8 {
        return false;
    }
    let has_upper = trimmed
        .chars()
        .any(|character| character.is_ascii_uppercase());
    let has_lower = trimmed
        .chars()
        .any(|character| character.is_ascii_lowercase());
    let has_digit = trimmed.chars().any(|character| character.is_ascii_digit());
    let safe_chars = trimmed
        .chars()
        .all(|character| character.is_ascii_alphanumeric() || matches!(character, '-' | '_'));
    safe_chars && has_digit && (has_upper || has_lower)
}

pub(crate) fn humanize_branch_family(value: &str) -> Option<String> {
    let normalized = normalize_task_title(value);
    if normalized.is_empty() || looks_like_issue_key_family(value) {
        return None;
    }
    let mut characters = normalized.chars();
    let first = characters.next()?.to_ascii_uppercase();
    Some(format!("{first}{}", characters.collect::<String>()))
}

pub(crate) fn looks_like_issue_key_family(value: &str) -> bool {
    let Some((left, right)) = value.trim().split_once('-') else {
        return false;
    };
    !left.is_empty()
        && !right.is_empty()
        && left
            .chars()
            .all(|character| character.is_ascii_lowercase() || character.is_ascii_digit())
        && right.chars().all(|character| character.is_ascii_digit())
}
