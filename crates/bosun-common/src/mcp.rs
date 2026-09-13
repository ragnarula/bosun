//! MCP server registry types: the stored row, the API listing that omits
//! secrets, and the discovered tool shapes the loop advertises and calls.
//!
//! The store row is the source of truth for one configured MCP server. List
//! and read endpoints never return the secret columns; the load-one-with-
//! secrets shape stays inside the store and the connection manager.

use serde::Deserialize;
use serde::Serialize;

use crate::tool::tool_name_is_valid;

/// A configured MCP server's authentication kind.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum McpAuth {
    /// No authentication.
    None,
    /// A static bearer token (or a custom header value) sent on every
    /// request.
    Bearer,
    /// The MCP authorization spec's auth-code flow with PKCE.
    #[serde(rename = "oauth")]
    OAuth,
}

impl McpAuth {
    pub fn as_str(self) -> &'static str {
        match self {
            McpAuth::None => "none",
            McpAuth::Bearer => "bearer",
            McpAuth::OAuth => "oauth",
        }
    }
}

/// A configured MCP server as listed and returned by the registry routes.
/// Secret values are deliberately absent; the store's load-with-secrets
/// shape is separate so a list can never leak them.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct McpServer {
    pub name: String,
    pub url: String,
    pub enabled: bool,
    pub auth: McpAuth,
    /// True when an OAuth access token is stored; the token itself is never
    /// returned.
    pub oauth_authorized: bool,
    /// The stored access token's expiry, seconds since the unix epoch.
    pub oauth_expires_at_secs: Option<i64>,
    /// The URL the operator must visit to re-authorize the server after an
    /// `insufficient_scope` challenge. The connection manager holds it for as
    /// long as its flow is pending; the store never holds it.
    #[serde(default)]
    pub reauthorize_url: Option<String>,
    pub added_at_secs: i64,
    pub updated_at_secs: Option<i64>,
    pub last_error: Option<String>,
}

/// The full server row including secrets. Only the connection manager and
/// the OAuth flow load this shape; it is never serialized to a client.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct McpServerSecret {
    pub name: String,
    pub url: String,
    pub enabled: bool,
    pub auth: McpAuth,
    pub bearer_token: Option<String>,
    pub oauth_client_id: Option<String>,
    pub oauth_client_secret: Option<String>,
    pub oauth_access_token: Option<String>,
    pub oauth_refresh_token: Option<String>,
    pub oauth_expires_at_secs: Option<i64>,
    /// The space-separated scopes the stored token was granted, so a step-up
    /// re-authorization can ask for the union with a challenge's scope.
    pub oauth_scope: Option<String>,
    pub added_at_secs: i64,
    pub updated_at_secs: Option<i64>,
    pub last_error: Option<String>,
}

/// One tool discovered from a connected server. `schema` is the tool's
/// `inputSchema`, kept as a `serde_json::Value` so the loop hands it to the
/// provider adapters exactly like a canonical tool.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct McpTool {
    pub name: String,
    pub description: String,
    pub schema: serde_json::Value,
}

/// One `tools/call` result, mapped to the text a session shows. The manager's
/// client produces it and the loop records it, so both sides share the shape.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct McpCallOutcome {
    /// The content blocks as text, with `structuredContent` as compact JSON
    /// appended when the server sent it.
    pub text: String,
    /// The server's `isError`, or true for a JSON-RPC error answer.
    pub is_error: bool,
}

/// The text result of one MCP tool call, mapped from the spec's content
/// blocks. `structured_content` is serialized as compact JSON text; blocks
/// that are not text are described briefly.
pub fn mcp_tool_result_text(content: &[serde_json::Value]) -> String {
    let mut parts = Vec::new();
    for block in content {
        match block.get("type").and_then(|value| value.as_str()) {
            Some("text") => parts.push(
                block
                    .get("text")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string(),
            ),
            Some("image") => parts.push("[image]".to_string()),
            Some("audio") => parts.push("[audio]".to_string()),
            Some("resource") => parts.push("[resource]".to_string()),
            Some("resource_link") => parts.push("[resource link]".to_string()),
            Some(other) => parts.push(format!("[{other} content]")),
            None => {}
        }
    }
    parts.join("\n")
}

/// Serializes a structured content block as compact JSON text.
pub fn mcp_structured_content_text(value: &serde_json::Value) -> String {
    serde_json::to_string(value).unwrap_or_default()
}

/// One tool a server offers, as the model sees it. The exposed name and the
/// description the model reads are on `tool`; `server_name` is what a
/// `tools/call` must send, because the server never hears the exposed name.
#[derive(Debug, Clone, PartialEq)]
pub struct McpAdvertisedTool {
    pub server: String,
    /// The tool's own name on that server.
    pub server_name: String,
    pub tool: McpTool,
}

/// The result of exposing a set of servers' tools to the model: the tools the
/// loop advertises.
#[derive(Debug, Clone, PartialEq)]
pub struct McpExposure {
    pub advertised: Vec<McpAdvertisedTool>,
}

/// Maps a server's discovered tools onto the model-facing names the loop
/// advertises, per the naming rules:
///
/// - A tool whose name is valid and used by only one server is exposed bare,
///   with its description prefixed `[server]` so it stays attributable.
/// - Otherwise it is exposed as `<server>-<tool>`, both parts sanitised to
///   `[a-zA-Z0-9_-]`, with the tool part truncated so the whole name fits 64
///   characters.
/// - A tool whose exposed name is already taken is dropped deterministically.
///
/// Servers are processed in name order, and tools in their listed order, so
/// the same input always drops the same tools. `reserved` carries the names
/// already taken by the canonical surface.
pub fn expose_mcp_tools(
    mut servers: Vec<(String, Vec<McpTool>)>,
    reserved: &[String],
) -> McpExposure {
    servers.sort_by(|a, b| a.0.cmp(&b.0));
    // A tool name that appears on more than one server is ambiguous and must
    // be namespaced everywhere, even when one server's name would sort first.
    let mut tool_owner: std::collections::HashMap<String, u32> = std::collections::HashMap::new();
    for (_, tools) in &servers {
        for tool in tools {
            *tool_owner.entry(tool.name.clone()).or_default() += 1;
        }
    }
    let mut used: std::collections::HashSet<String> = reserved.iter().cloned().collect();
    let mut advertised = Vec::new();
    for (server, tools) in servers {
        for mut tool in tools {
            let server_name = tool.name.clone();
            let ambiguous = tool_owner.get(tool.name.as_str()).copied().unwrap_or(0) > 1;
            let bare = tool_name_is_valid(&tool.name) && !ambiguous && !used.contains(&tool.name);
            let exposed = if bare {
                tool.description = format!("[{}] {}", server, tool.description);
                tool.name.clone()
            } else {
                let name = namespaced_name(&server, &tool.name);
                if used.contains(&name) {
                    tracing::warn!(
                        server = %server,
                        tool = %tool.name,
                        "MCP tool dropped: exposed name collides"
                    );
                    continue;
                }
                name
            };
            used.insert(exposed.clone());
            tool.name = exposed;
            advertised.push(McpAdvertisedTool {
                server: server.clone(),
                server_name,
                tool,
            });
        }
    }
    McpExposure { advertised }
}

/// Parses a session's stored `mcp_servers` value: the comma-separated list of
/// server names that `resolve_mcp_servers` writes. Empty entries are dropped
/// and duplicates keep their first occurrence, so a hand-edited row cannot
/// make a session ask for one server twice.
pub fn parse_mcp_servers(value: &str) -> Vec<String> {
    let mut names = Vec::new();
    for raw in value.split(',') {
        let name = raw.trim();
        if name.is_empty() || names.iter().any(|known| known == name) {
            continue;
        }
        names.push(name.to_string());
    }
    names
}

/// The exposed `<server>-<tool>` name: both parts sanitised, the tool part
/// truncated so the whole name fits the provider pattern's 64-character cap.
///
/// A server name longer than 63 characters cannot keep its whole prefix
/// inside that cap, so the prefix is truncated too. Two such names that share
/// their first 63 characters therefore produce the same exposed name, and the
/// later tool is dropped as a collision.
fn namespaced_name(server: &str, tool: &str) -> String {
    let mut server_part = sanitise_name_part(server);
    let mut tool_part = sanitise_name_part(tool);
    if server_part.len() > 63 {
        server_part.truncate(63);
    }
    let max_tool = 64usize.saturating_sub(server_part.len() + 1);
    tool_part.truncate(max_tool);
    format!("{server_part}-{tool_part}")
}

/// Maps a name part onto `[a-zA-Z0-9_-]`, replacing every other character
/// with an underscore. A multi-byte character becomes one underscore, so the
/// result can be shorter in bytes than the input.
fn sanitise_name_part(part: &str) -> String {
    let mut out: String = part
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '_' || c == '-' {
                c
            } else {
                '_'
            }
        })
        .collect();
    if out.is_empty() {
        out.push('_');
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn auth_kinds_serialize_snake_case() {
        assert_eq!(serde_json::to_string(&McpAuth::None).unwrap(), "\"none\"");
        assert_eq!(
            serde_json::to_string(&McpAuth::Bearer).unwrap(),
            "\"bearer\""
        );
        assert_eq!(serde_json::to_string(&McpAuth::OAuth).unwrap(), "\"oauth\"");
    }

    #[test]
    fn tool_result_text_concatenates_text_and_describes_other_blocks() {
        let content = serde_json::json!([
            { "type": "text", "text": "first" },
            { "type": "image" },
            { "type": "text", "text": "second" },
            { "type": "resource" },
        ]);
        assert_eq!(
            mcp_tool_result_text(content.as_array().unwrap()),
            "first\n[image]\nsecond\n[resource]"
        );
    }

    #[test]
    fn structured_content_is_compact_json() {
        assert_eq!(
            mcp_structured_content_text(&serde_json::json!({ "a": [1, 2] })),
            "{\"a\":[1,2]}"
        );
    }

    fn tool(name: &str) -> McpTool {
        McpTool {
            name: name.into(),
            description: "does things".into(),
            schema: serde_json::json!({ "type": "object" }),
        }
    }

    fn names(advertised: &[McpAdvertisedTool]) -> Vec<String> {
        advertised
            .iter()
            .map(|advertised| advertised.tool.name.clone())
            .collect()
    }

    #[test]
    fn an_unambiguous_valid_name_is_exposed_bare_with_a_server_prefix() {
        let exposure = expose_mcp_tools(
            vec![("srv".into(), vec![tool("lookup")])],
            &["shell".into()],
        );
        assert_eq!(names(&exposure.advertised), ["lookup"]);
        assert_eq!(exposure.advertised[0].tool.description, "[srv] does things");
        assert_eq!(exposure.advertised[0].server, "srv");
        assert_eq!(exposure.advertised[0].server_name, "lookup");
    }

    #[test]
    fn an_invalid_or_colliding_name_is_namespaced_without_a_prefix() {
        let exposure = expose_mcp_tools(
            vec![
                ("srv".into(), vec![tool("look.up")]),
                ("other".into(), vec![tool("shell")]),
            ],
            &["shell".into()],
        );
        let by_name: std::collections::HashMap<_, _> = exposure
            .advertised
            .iter()
            .map(|advertised| {
                (
                    advertised.tool.name.clone(),
                    (
                        advertised.tool.description.clone(),
                        advertised.server_name.clone(),
                    ),
                )
            })
            .collect();
        assert_eq!(by_name.get("srv-look_up").unwrap().0, "does things");
        assert_eq!(by_name.get("other-shell").unwrap().0, "does things");
        // The exposed name a namespaced tool carries is never what the
        // server hears: the call sends the name the server itself reports.
        assert_eq!(by_name.get("srv-look_up").unwrap().1, "look.up");
        assert_eq!(by_name.get("other-shell").unwrap().1, "shell");
    }

    #[test]
    fn a_bare_name_already_taken_by_an_earlier_server_is_namespaced() {
        let exposure = expose_mcp_tools(
            vec![
                ("zeta".into(), vec![tool("shared")]),
                ("alpha".into(), vec![tool("shared")]),
            ],
            &[],
        );
        assert_eq!(names(&exposure.advertised), ["alpha-shared", "zeta-shared"]);
    }

    #[test]
    fn a_namespaced_name_that_collides_is_dropped_deterministically() {
        let exposure = expose_mcp_tools(
            vec![
                ("srv".into(), vec![tool("run")]),
                ("srv".into(), vec![tool("run")]),
            ],
            &[],
        );
        // The later server's tool is dropped, so only the first is advertised.
        assert_eq!(names(&exposure.advertised), ["srv-run"]);
    }

    #[test]
    fn server_names_that_share_their_first_63_characters_collide() {
        // Both prefixes are cut to 63 characters, so each server exposes the
        // same name for its tool and the later server's tool is dropped.
        let shared = "s".repeat(63);
        let exposure = expose_mcp_tools(
            vec![
                (format!("{shared}x"), vec![tool("run")]),
                (format!("{shared}y"), vec![tool("run")]),
            ],
            &[],
        );
        assert_eq!(names(&exposure.advertised), [format!("{shared}-")]);
    }

    #[test]
    fn overflow_truncates_the_tool_part_and_keeps_the_server_prefix() {
        let exposure = expose_mcp_tools(vec![("srv".into(), vec![tool(&"t".repeat(80))])], &[]);
        assert_eq!(
            names(&exposure.advertised),
            [format!("srv-{}", "t".repeat(60))]
        );
    }

    #[test]
    fn names_that_would_overflow_the_server_part_are_still_valid() {
        let server = "s".repeat(70);
        let exposure = expose_mcp_tools(vec![(server.clone(), vec![tool("tool!")])], &[]);
        let exposed = &exposure.advertised[0].tool.name;
        assert!(tool_name_is_valid(exposed), "{exposed}");
        assert!(exposed.starts_with(&"s".repeat(63)));
    }

    #[test]
    fn sanitisation_keeps_valid_characters_and_replaces_the_rest() {
        let exposure = expose_mcp_tools(vec![("a/b c".into(), vec![tool("do it!")])], &[]);
        assert_eq!(names(&exposure.advertised), ["a_b_c-do_it_"]);
    }

    #[test]
    fn a_session_server_list_parses_trimmed_and_deduplicated() {
        assert_eq!(parse_mcp_servers(""), Vec::<String>::new());
        assert_eq!(
            parse_mcp_servers("srv-a, srv-b ,srv-a"),
            ["srv-a".to_string(), "srv-b".to_string()]
        );
        assert_eq!(
            parse_mcp_servers(" , srv-a, "),
            ["srv-a".to_string()],
            "empty entries are dropped"
        );
    }
}
