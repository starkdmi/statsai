use super::*;

pub(crate) fn for_grok_jsonl_record(
    path: &Path,
    mut visit: impl FnMut(&str, &Value) -> Result<()>,
) -> Result<GrokJsonlParseStats> {
    if !path.is_file() {
        return Ok(GrokJsonlParseStats::default());
    }
    let file = File::open(path).with_context(|| format!("open {}", path.display()))?;
    let mut reader = BufReader::new(file);
    let mut parse_stats = GrokJsonlParseStats::default();
    let mut line_bytes = Vec::new();
    let mut index = 0usize;
    loop {
        let line_status =
            read_bounded_jsonl_line(&mut reader, &mut line_bytes, MAX_JSONL_RECORD_BYTES)
                .with_context(|| format!("read {} line {}", path.display(), index + 1))?;
        if line_status == BoundedLineRead::Eof {
            break;
        }
        index = index.saturating_add(1);
        if line_status == BoundedLineRead::Oversized {
            parse_stats.invalid_rows += 1;
            continue;
        }
        let Ok(line) = std::str::from_utf8(&line_bytes) else {
            parse_stats.invalid_rows += 1;
            continue;
        };
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        let value = match serde_json::from_str(trimmed) {
            Ok(value) => value,
            Err(_) => {
                parse_stats.invalid_rows += 1;
                continue;
            }
        };
        parse_stats.rows += 1;
        visit(trimmed, &value)?;
    }
    Ok(parse_stats)
}

pub(crate) fn for_grok_jsonl_value(
    path: &Path,
    mut visit: impl FnMut(&Value) -> Result<()>,
) -> Result<GrokJsonlParseStats> {
    for_grok_jsonl_record(path, |_line, value| visit(value))
}

pub(crate) fn grok_session_id_from_summary_path(summary_path: &Path) -> Option<String> {
    read_json_file(summary_path)
        .as_ref()
        .and_then(|value| grok_session_id_from_summary_value(value, summary_path))
        .or_else(|| {
            summary_path
                .parent()
                .and_then(|path| path.file_name())
                .and_then(|name| name.to_str())
                .map(ToOwned::to_owned)
        })
}

pub(crate) fn grok_session_id_from_summary_value(
    value: &Value,
    summary_path: &Path,
) -> Option<String> {
    value
        .pointer("/info/id")
        .and_then(Value::as_str)
        .map(ToOwned::to_owned)
        .or_else(|| {
            summary_path
                .parent()
                .and_then(|path| path.file_name())
                .and_then(|name| name.to_str())
                .map(ToOwned::to_owned)
        })
}

pub(crate) fn update_max(target: &mut Option<u64>, value: Option<&Value>) {
    if let Some(value) = value.and_then(value_as_u64) {
        *target = Some(target.unwrap_or(0).max(value));
    }
}
