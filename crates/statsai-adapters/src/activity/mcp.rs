//! MCP name splitters. Unknown or unmatched names stay unclassified.

/// Split `mcp__<server>__<tool>`. Server names may contain single underscores.
#[must_use]
pub fn split_mcp_double_underscore(name: &str) -> Option<(&str, &str)> {
    let rest = name.strip_prefix("mcp__")?;
    let (server, tool) = rest.split_once("__")?;
    if server.is_empty() || tool.is_empty() {
        None
    } else {
        Some((server, tool))
    }
}

#[must_use]
pub fn sanitize_opencode_mcp_token(value: &str) -> String {
    value
        .chars()
        .map(|ch| {
            if ch.is_ascii_alphanumeric() || ch == '_' || ch == '-' {
                ch
            } else {
                '_'
            }
        })
        .collect()
}

/// OpenCode registers MCP tools as `{sanitized_server}_{sanitized_tool}`.
///
/// Confirmed against `packages/opencode/src/mcp/index.ts`: `sanitize(clientName) + "_" + sanitize(mcpTool.name)`.
/// Matching uses the longest configured server key whose sanitized form is a prefix.
#[must_use]
pub fn split_opencode_mcp_name<'a>(
    tool_name: &'a str,
    configured_servers: &[String],
) -> Option<(String, &'a str)> {
    let mut best: Option<(String, &'a str, usize)> = None;
    for server in configured_servers {
        let sanitized = sanitize_opencode_mcp_token(server);
        if sanitized.is_empty() {
            continue;
        }
        let prefix = format!("{sanitized}_");
        if let Some(tool) = tool_name.strip_prefix(&prefix) {
            if tool.is_empty() {
                continue;
            }
            if best
                .as_ref()
                .is_none_or(|(_, _, len)| sanitized.len() > *len)
            {
                best = Some((server.clone(), tool, sanitized.len()));
            }
        }
    }
    best.map(|(server, tool, _)| (server, tool))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn splits_mcp_double_underscore_names() {
        assert_eq!(
            split_mcp_double_underscore("mcp__example_server__example_tool"),
            Some(("example_server", "example_tool"))
        );
        assert_eq!(
            split_mcp_double_underscore("mcp__Server_Name__tool"),
            Some(("Server_Name", "tool"))
        );
        assert_eq!(split_mcp_double_underscore("Bash"), None);
        assert_eq!(split_mcp_double_underscore("mcp__onlyserver"), None);
        assert_eq!(split_mcp_double_underscore("mcp____tool"), None);
    }

    #[test]
    fn opencode_mcp_prefers_longest_configured_prefix() {
        let servers = vec!["jira".to_string(), "jira_cloud".to_string()];
        assert_eq!(
            split_opencode_mcp_name("jira_cloud_search_issues", &servers),
            Some(("jira_cloud".to_string(), "search_issues"))
        );
        assert_eq!(
            split_opencode_mcp_name("jira_search_issues", &servers),
            Some(("jira".to_string(), "search_issues"))
        );
        assert_eq!(split_opencode_mcp_name("read", &servers), None);
    }
}
