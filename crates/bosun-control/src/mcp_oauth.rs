//! OAuth authorization for MCP servers: the auth-code flow with PKCE, per
//! the MCP authorization spec's HTTP-server rules. The web pane starts the
//! flow through this module, the authorization server redirects back to the
//! control plane's callback, and the callback exchanges the code for tokens
//! stored on the server row.
//!
//! The connection manager calls the refresh helper when a call finds an
//! expired or rejected token, and the step-up helper when the server
//! challenges for more scopes than the token carries.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::SystemTime;

use bosun_agent::config::resolve_api_key;
use bosun_agent::provider::ProviderError;
use bosun_common::mcp::McpServerSecret;
use bosun_store::store::Store;
use bosun_store::store::StoreError;
use reqwest::header::AUTHORIZATION;
use serde::Deserialize;
use sha2::Digest;
use sha2::Sha256;
use thiserror::Error;
use tracing::info;
use tracing::instrument;

const BASE64URL_ALPHABET: &[u8; 64] =
    b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";

/// How long a pending authorization flow stays usable before it is dropped.
const PENDING_FLOW_TTL_SECS: i64 = 600;

/// Errors a caller can act on, plus a catch-all for internal failures. The
/// variants carry no tokens or other secrets.
#[derive(Debug, Error)]
pub enum McpOAuthError {
    #[error("oauth_redirect_uri is not configured or is not a valid URL")]
    RedirectUriNotConfigured,

    #[error("oauth_client_id is required to authorize the MCP server")]
    ClientIdRequired,

    #[error("environment variable {var} is not set")]
    MissingEnvVar { var: String },

    #[error("OAuth discovery failed: {detail}")]
    DiscoveryFailed { detail: String },

    #[error("the authorization server does not support PKCE S256")]
    PkceUnsupported,

    #[error("authorization response issuer does not match the expected issuer")]
    IssuerMismatch,

    #[error("authorization response was missing the iss parameter")]
    MissingIss,

    #[error("unknown or expired authorization state")]
    StateMismatch,

    #[error("no refresh token is stored; the server must be re-authorized")]
    NoRefreshToken,

    #[error("token exchange failed: {detail}")]
    TokenExchangeFailed { detail: String },

    #[error("internal error")]
    Internal(#[from] anyhow::Error),
}

fn store_error(error: StoreError) -> McpOAuthError {
    McpOAuthError::Internal(error.into())
}

/// The state recorded between authorize-start and the callback, keyed by the
/// `state` query parameter. It is removed on use, so a state value is
/// single-use.
#[derive(Debug, Clone)]
struct PendingFlow {
    server_name: String,
    code_verifier: String,
    expected_issuer: String,
    redirect_uri: String,
    // The scopes this flow asked for, which the row records when the token
    // response names no scope of its own.
    requested_scope: String,
    /// The URL the web pane navigates the operator to for this flow. The flow
    /// is what makes the URL usable, so the two are held together.
    authorize_url: String,
    iss_parameter_supported: bool,
    /// Unix seconds when this flow was recorded; older entries are dropped so
    /// the pending map cannot grow without bound.
    added_at: i64,
}

/// The flow pending for one server, as the connection manager reads it: the
/// URL the operator must visit and the scopes that flow asks for.
pub(crate) struct PendingAuthorize {
    pub(crate) url: String,
    pub(crate) scope: String,
}

/// One protected resource metadata document, per the MCP authorization spec.
#[derive(Debug, Deserialize)]
struct ProtectedResourceMetadata {
    authorization_servers: Vec<String>,
    #[serde(default)]
    scopes_supported: Vec<String>,
    /// The scope the server challenged in its 401 `WWW-Authenticate` header.
    /// When present it takes priority over `scopes_supported` for the initial
    /// authorization request.
    #[serde(default)]
    challenge_scope: Option<String>,
}

/// One authorization server metadata document, per RFC 8414 / OIDC Discovery.
#[derive(Debug, Deserialize)]
struct AuthorizationServerMetadata {
    issuer: String,
    authorization_endpoint: String,
    token_endpoint: String,
    #[serde(default)]
    code_challenge_methods_supported: Option<Vec<String>>,
    #[serde(default)]
    authorization_response_iss_parameter_supported: Option<bool>,
}

#[derive(Debug, Deserialize)]
struct TokenResponse {
    access_token: Option<String>,
    refresh_token: Option<String>,
    expires_in: Option<u64>,
    /// The scope the server granted. RFC 6749 section 5.1 lets it differ from
    /// the one requested, and says it equals the requested scope when the
    /// server omits it.
    scope: Option<String>,
}

/// The query parameters the authorization server sends back to the callback.
#[derive(Debug, Deserialize)]
pub struct OAuthCallbackQuery {
    pub code: Option<String>,
    pub state: Option<String>,
    pub error: Option<String>,
    pub iss: Option<String>,
}

/// The OAuth machinery the authorize and callback routes share: one reqwest
/// client, the store the tokens land in, the configured redirect URI, and
/// the in-memory map of pending flows. It is cheap to clone.
#[derive(Clone)]
pub struct McpOAuthContext {
    client: reqwest::Client,
    store: Store,
    redirect_uri: Option<String>,
    pending: Arc<tokio::sync::RwLock<HashMap<String, PendingFlow>>>,
}

impl McpOAuthContext {
    pub fn new(client: reqwest::Client, store: Store, redirect_uri: Option<String>) -> Self {
        Self {
            client,
            store,
            redirect_uri,
            pending: Arc::new(tokio::sync::RwLock::new(HashMap::new())),
        }
    }

    /// Starts the auth-code flow for an OAuth server and returns the
    /// authorization URL the web pane navigates to. The requested scope is the
    /// server's 401 `WWW-Authenticate` challenge scope when present, else the
    /// protected resource metadata's `scopes_supported`, joined with the
    /// scopes the row's token was already granted: asking for less than the
    /// operator granted would take access away.
    #[instrument(skip_all)]
    pub async fn start_authorization(
        &self,
        server: &McpServerSecret,
    ) -> Result<String, McpOAuthError> {
        let discovery = self.resolve_all(server).await?;
        let scope = discovery
            .resource
            .challenge_scope
            .clone()
            .unwrap_or_else(|| discovery.resource.scopes_supported.join(" "));
        let scope = union_scopes(server.oauth_scope.as_deref().unwrap_or(""), &scope);
        self.begin(server, &scope, &discovery).await
    }

    /// Re-runs authorize-start for a step-up after the server challenged for
    /// scopes the stored token does not carry. `challenge_scope` is the scope
    /// the challenge named, when it named one. The request asks for the union
    /// of the scopes the row's token already carries and the challenge's, plus
    /// the resource's advertised scopes when the challenge named none: a
    /// request for the refused token's own scopes would meet the same
    /// challenge once the operator completes the flow.
    #[instrument(skip_all)]
    pub async fn reauthorize_with_scopes(
        &self,
        server: &McpServerSecret,
        challenge_scope: Option<&str>,
    ) -> Result<String, McpOAuthError> {
        let discovery = self.resolve_all(server).await?;
        let scope = union_scopes(
            server.oauth_scope.as_deref().unwrap_or(""),
            challenge_scope.unwrap_or(""),
        );
        let scope = match challenge_scope {
            Some(_) => scope,
            None => union_scopes(&scope, &discovery.resource.scopes_supported.join(" ")),
        };
        self.begin(server, &scope, &discovery).await
    }

    /// Completes the callback: validates `state` and `iss`, exchanges the
    /// code, and stores the access and refresh tokens with the expiry.
    /// Returns the name of the server that was authorized, so the caller can
    /// connect it with the tokens it just stored.
    #[instrument(skip_all)]
    pub async fn complete_authorization(
        &self,
        query: &OAuthCallbackQuery,
    ) -> Result<String, McpOAuthError> {
        let state = query.state.as_deref().ok_or(McpOAuthError::StateMismatch)?;
        let pending = self.take_pending(state).await?;

        // RFC 9207: validate iss before acting on the redirect, so an
        // attacker-supplied error cannot be honoured on a mismatch.
        if let Some(iss) = query.iss.as_deref() {
            if iss != pending.expected_issuer {
                return Err(McpOAuthError::IssuerMismatch);
            }
        } else if pending.iss_parameter_supported {
            return Err(McpOAuthError::MissingIss);
        }

        if let Some(error) = query.error.as_deref() {
            return Err(McpOAuthError::TokenExchangeFailed {
                detail: format!("authorization server rejected the request: {error}"),
            });
        }

        let code = query
            .code
            .as_deref()
            .ok_or_else(|| McpOAuthError::TokenExchangeFailed {
                detail: "authorization response had no code".to_string(),
            })?;
        let server = self
            .store
            .load_mcp_server(&pending.server_name)
            .await
            .map_err(store_error)?
            .ok_or_else(|| {
                McpOAuthError::Internal(anyhow::anyhow!(
                    "MCP server {} was not found",
                    pending.server_name
                ))
            })?;

        let discovery = self.resolve_all(&server).await?;
        let resource = canonical_resource(&server.url)?;
        let token = self
            .token_request(
                &discovery.authorization_server.token_endpoint,
                &server,
                vec![
                    ("grant_type".to_string(), "authorization_code".to_string()),
                    ("code".to_string(), code.to_string()),
                    ("redirect_uri".to_string(), pending.redirect_uri.clone()),
                    ("code_verifier".to_string(), pending.code_verifier.clone()),
                    ("resource".to_string(), resource),
                ],
            )
            .await?;

        let access_token = required_access_token(&token)?;
        // The refresh token may be absent on a re-authorization; keep the
        // previously stored one instead of clearing it.
        let refresh_token = token
            .refresh_token
            .clone()
            .or_else(|| server.oauth_refresh_token.clone());
        // RFC 6749 section 5.1: a response that names a scope granted that
        // one, which may differ from the scope the flow asked for; a response
        // that names none granted the requested scope.
        let requested_scope = pending.requested_scope.trim();
        let scope =
            named_scope(&token).or((!requested_scope.is_empty()).then_some(requested_scope));
        let expires_at = expires_at(token.expires_in);
        self.store
            .set_mcp_oauth_tokens(
                &server.name,
                &access_token,
                refresh_token.as_deref(),
                expires_at,
                scope,
            )
            .await
            .map_err(store_error)?;
        self.store
            .clear_mcp_server_error(&server.name)
            .await
            .map_err(store_error)?;
        info!(server = %server.name, "MCP server authorized");
        Ok(server.name)
    }

    /// Refreshes a stored access token using the stored refresh token, and
    /// stores the replacement pair. When the response rotates the refresh
    /// token, the stored one is replaced.
    #[instrument(skip_all)]
    pub async fn refresh_mcp_token(&self, name: &str) -> Result<(), McpOAuthError> {
        let server = self
            .store
            .load_mcp_server(name)
            .await
            .map_err(store_error)?
            .ok_or_else(|| {
                McpOAuthError::Internal(anyhow::anyhow!("MCP server {name} was not found"))
            })?;
        let Some(refresh_token) = server.oauth_refresh_token.clone() else {
            return Err(McpOAuthError::NoRefreshToken);
        };

        let discovery = self.resolve_all(&server).await?;
        let resource = canonical_resource(&server.url)?;
        let token = self
            .token_request(
                &discovery.authorization_server.token_endpoint,
                &server,
                vec![
                    ("grant_type".to_string(), "refresh_token".to_string()),
                    ("refresh_token".to_string(), refresh_token),
                    ("resource".to_string(), resource),
                ],
            )
            .await?;

        let access_token = required_access_token(&token)?;
        let refresh_token = token
            .refresh_token
            .clone()
            .or_else(|| server.oauth_refresh_token.clone());
        // A refresh response that omits `expires_in` leaves the stored expiry
        // in place rather than clearing it, and one that names no scope keeps
        // the stored scopes.
        let expires_at = expires_at(token.expires_in).or(server.oauth_expires_at_secs);
        let scope = named_scope(&token)
            .map(str::to_string)
            .or_else(|| server.oauth_scope.clone());
        self.store
            .set_mcp_oauth_tokens(
                &server.name,
                &access_token,
                refresh_token.as_deref(),
                expires_at,
                scope.as_deref(),
            )
            .await
            .map_err(store_error)?;
        Ok(())
    }

    /// The shared authorize-start body: validate the client and PKCE support,
    /// generate the verifier/challenge and state, build the URL, and record
    /// the flow.
    async fn begin(
        &self,
        server: &McpServerSecret,
        requested_scope: &str,
        discovery: &Discovery,
    ) -> Result<String, McpOAuthError> {
        let redirect_uri = self.redirect_uri()?;

        let client_id = server
            .oauth_client_id
            .as_deref()
            .map(str::trim)
            .filter(|client_id| !client_id.is_empty())
            .ok_or(McpOAuthError::ClientIdRequired)?;
        let client_id = resolve_credential(client_id)?;

        match &discovery
            .authorization_server
            .code_challenge_methods_supported
        {
            Some(methods) if methods.iter().any(|method| method == "S256") => {}
            _ => return Err(McpOAuthError::PkceUnsupported),
        }

        let state = uuid::Uuid::new_v4().to_string();
        let verifier = code_verifier();
        let challenge = code_challenge(&verifier);
        let resource = canonical_resource(&server.url)?;
        let mut url = reqwest::Url::parse(&discovery.authorization_server.authorization_endpoint)
            .map_err(|error| McpOAuthError::DiscoveryFailed {
            detail: format!("authorization endpoint is not a valid URL: {error}"),
        })?;
        {
            let mut query = url.query_pairs_mut();
            query.append_pair("response_type", "code");
            query.append_pair("client_id", &client_id);
            query.append_pair("redirect_uri", &redirect_uri);
            if !requested_scope.is_empty() {
                query.append_pair("scope", requested_scope);
            }
            query.append_pair("code_challenge", &challenge);
            query.append_pair("code_challenge_method", "S256");
            query.append_pair("state", &state);
            query.append_pair("resource", &resource);
        }
        let authorization_url = url.to_string();

        // Recorded only once the URL is built, so a failure above leaves no
        // pending flow behind.
        self.insert_pending(
            state,
            PendingFlow {
                server_name: server.name.clone(),
                code_verifier: verifier,
                expected_issuer: discovery.issuer.clone(),
                redirect_uri,
                requested_scope: requested_scope.to_string(),
                iss_parameter_supported: discovery
                    .authorization_server
                    .authorization_response_iss_parameter_supported
                    .unwrap_or(false),
                authorize_url: authorization_url.clone(),
                added_at: bosun_common::time::unix_secs(SystemTime::now()),
            },
        )
        .await;

        Ok(authorization_url)
    }

    fn redirect_uri(&self) -> Result<String, McpOAuthError> {
        let uri = self
            .redirect_uri
            .as_deref()
            .ok_or(McpOAuthError::RedirectUriNotConfigured)?;
        reqwest::Url::parse(uri)
            .map(|_| uri.to_string())
            .map_err(|_| McpOAuthError::RedirectUriNotConfigured)
    }

    /// Records a pending flow, dropping any flow older than ten minutes and
    /// any earlier flow for the same server. Both bounds keep the map small:
    /// one entry per server, none older than the TTL, so repeated authorize
    /// requests cannot grow it without limit.
    async fn insert_pending(&self, state: String, flow: PendingFlow) {
        let cutoff = bosun_common::time::unix_secs(SystemTime::now()) - PENDING_FLOW_TTL_SECS;
        let server_name = flow.server_name.clone();
        let mut pending = self.pending.write().await;
        pending.retain(|_, existing| {
            existing.added_at >= cutoff && existing.server_name != server_name
        });
        pending.insert(state, flow);
    }

    /// Removes and returns the pending flow for `state`, dropping stale
    /// entries first. An unknown or expired state is a state mismatch.
    async fn take_pending(&self, state: &str) -> Result<PendingFlow, McpOAuthError> {
        let cutoff = bosun_common::time::unix_secs(SystemTime::now()) - PENDING_FLOW_TTL_SECS;
        let mut pending = self.pending.write().await;
        pending.retain(|_, existing| existing.added_at >= cutoff);
        pending.remove(state).ok_or(McpOAuthError::StateMismatch)
    }

    /// The flow pending for `server_name`: the URL the operator must visit and
    /// the scopes that flow asks for, so a caller can name the scopes the
    /// operator will actually grant. A flow is single-use and expires, so a
    /// flow a callback consumed or the TTL dropped is not returned.
    pub(crate) async fn pending_authorize(&self, server_name: &str) -> Option<PendingAuthorize> {
        let cutoff = bosun_common::time::unix_secs(SystemTime::now()) - PENDING_FLOW_TTL_SECS;
        let pending = self.pending.read().await;
        pending
            .values()
            .find(|flow| flow.server_name == server_name && flow.added_at >= cutoff)
            .map(|flow| PendingAuthorize {
                url: flow.authorize_url.clone(),
                scope: flow.requested_scope.clone(),
            })
    }

    /// Discovers the protected resource metadata and the authorization server
    /// metadata it names, in one pass.
    async fn resolve_all(&self, server: &McpServerSecret) -> Result<Discovery, McpOAuthError> {
        let resource = self.discover_protected_resource(server).await?;
        let issuer = resource
            .authorization_servers
            .first()
            .cloned()
            .ok_or_else(|| McpOAuthError::DiscoveryFailed {
                detail: "protected resource metadata has no authorization_servers".to_string(),
            })?;
        let authorization_server = self.discover_authorization_server(&issuer).await?;
        Ok(Discovery {
            issuer,
            resource,
            authorization_server,
        })
    }

    /// Discovers the protected resource's metadata document: first from a 401
    /// `WWW-Authenticate` challenge, then from the well-known URIs.
    async fn discover_protected_resource(
        &self,
        server: &McpServerSecret,
    ) -> Result<ProtectedResourceMetadata, McpOAuthError> {
        let server_url =
            reqwest::Url::parse(&server.url).map_err(|error| McpOAuthError::DiscoveryFailed {
                detail: format!("server URL is not a valid URL: {error}"),
            })?;
        let probe = self
            .client
            .get(server_url.clone())
            .send()
            .await
            .map_err(|error| McpOAuthError::DiscoveryFailed {
                detail: format!("request to {server_url} failed: {error}"),
            })?;
        if probe.status() == reqwest::StatusCode::UNAUTHORIZED {
            let challenge = probe
                .headers()
                .get(reqwest::header::WWW_AUTHENTICATE)
                .and_then(|value| value.to_str().ok())
                .unwrap_or("");
            if let Some(metadata_url) = resource_metadata_url(challenge) {
                let mut metadata = self
                    .fetch_metadata::<ProtectedResourceMetadata>(&metadata_url)
                    .await?
                    .ok_or_else(|| McpOAuthError::DiscoveryFailed {
                        detail: format!("resource metadata {metadata_url} was not served"),
                    })?;
                metadata.challenge_scope = auth_param(challenge, "scope");
                return Ok(metadata);
            }
        }

        for candidate in protected_resource_well_knowns(&server_url) {
            if let Some(metadata) = self.fetch_metadata(&candidate).await? {
                return Ok(metadata);
            }
        }
        Err(McpOAuthError::DiscoveryFailed {
            detail: "no protected resource metadata found".to_string(),
        })
    }

    /// Discovers the authorization server metadata document from the issuer,
    /// using the spec's path-insertion well-known order, and checks the
    /// returned issuer is exactly the one asked for.
    async fn discover_authorization_server(
        &self,
        issuer: &str,
    ) -> Result<AuthorizationServerMetadata, McpOAuthError> {
        let candidates = authorization_server_well_knowns(issuer)?;
        for candidate in &candidates {
            let Some(metadata) = self
                .fetch_metadata::<AuthorizationServerMetadata>(candidate)
                .await?
            else {
                continue;
            };
            if metadata.issuer != issuer {
                return Err(McpOAuthError::DiscoveryFailed {
                    detail: format!(
                        "authorization server issuer {} does not match {issuer}",
                        metadata.issuer
                    ),
                });
            }
            return Ok(metadata);
        }
        Err(McpOAuthError::DiscoveryFailed {
            detail: format!("no authorization server metadata found for {issuer}"),
        })
    }

    /// Fetches and parses a metadata document; a non-success answer is `None`
    /// so a caller can move on to the next candidate.
    async fn fetch_metadata<T>(&self, url: &str) -> Result<Option<T>, McpOAuthError>
    where
        T: for<'de> Deserialize<'de>,
    {
        let response =
            self.client
                .get(url)
                .send()
                .await
                .map_err(|error| McpOAuthError::DiscoveryFailed {
                    detail: format!("request to {url} failed: {error}"),
                })?;
        if !response.status().is_success() {
            return Ok(None);
        }
        let metadata =
            response
                .json::<T>()
                .await
                .map_err(|error| McpOAuthError::DiscoveryFailed {
                    detail: format!("failed to parse metadata from {url}: {error}"),
                })?;
        Ok(Some(metadata))
    }

    /// POSTs a token request. A confidential client (a stored secret) sends
    /// HTTP Basic auth; a public client sends its `client_id` in the body.
    async fn token_request(
        &self,
        endpoint: &str,
        server: &McpServerSecret,
        mut params: Vec<(String, String)>,
    ) -> Result<TokenResponse, McpOAuthError> {
        let request = self.client.post(endpoint);
        let request = match &server.oauth_client_secret {
            Some(secret) => {
                let client_id = match &server.oauth_client_id {
                    Some(client_id) => resolve_credential(client_id)?,
                    None => String::new(),
                };
                let secret = resolve_credential(secret)?;
                request.header(AUTHORIZATION, basic_auth(&client_id, &secret))
            }
            None => {
                if let Some(client_id) = &server.oauth_client_id {
                    params.push(("client_id".to_string(), resolve_credential(client_id)?));
                }
                request
            }
        };
        let response = request.form(&params).send().await.map_err(|error| {
            McpOAuthError::TokenExchangeFailed {
                detail: format!("request to {endpoint} failed: {error}"),
            }
        })?;
        let status = response.status();
        if !status.is_success() {
            let body = response.text().await.unwrap_or_default();
            let detail = if body.is_empty() {
                status.to_string()
            } else {
                body.chars().take(200).collect::<String>()
            };
            return Err(McpOAuthError::TokenExchangeFailed {
                detail: format!("token endpoint returned {status}: {detail}"),
            });
        }
        response
            .json::<TokenResponse>()
            .await
            .map_err(|error| McpOAuthError::TokenExchangeFailed {
                detail: format!("failed to parse the token response: {error}"),
            })
    }
}

struct Discovery {
    issuer: String,
    resource: ProtectedResourceMetadata,
    authorization_server: AuthorizationServerMetadata,
}

fn required_access_token(token: &TokenResponse) -> Result<String, McpOAuthError> {
    token
        .access_token
        .clone()
        .filter(|token| !token.is_empty())
        .ok_or_else(|| McpOAuthError::TokenExchangeFailed {
            detail: "token response had no access_token".to_string(),
        })
}

/// The scope a token response names, trimmed. A response that names no scope
/// of its own grants the scope the request carried, so a blank one counts as
/// naming none.
fn named_scope(token: &TokenResponse) -> Option<&str> {
    token
        .scope
        .as_deref()
        .map(str::trim)
        .filter(|scope| !scope.is_empty())
}

fn expires_at(expires_in: Option<u64>) -> Option<i64> {
    let now = bosun_common::time::unix_secs(SystemTime::now());
    expires_in.map(|seconds| now.saturating_add(i64::try_from(seconds).unwrap_or(i64::MAX)))
}

/// The resource parameter: the server's canonical URL with a trailing slash
/// trimmed.
fn canonical_resource(server_url: &str) -> Result<String, McpOAuthError> {
    let url = reqwest::Url::parse(server_url).map_err(|error| McpOAuthError::DiscoveryFailed {
        detail: format!("server URL is not a valid URL: {error}"),
    })?;
    Ok(url.as_str().trim_end_matches('/').to_string())
}

/// A PKCE S256 code verifier: 64 unreserved characters made by joining two
/// UUIDs in their simple (hyphen-free) form.
pub fn code_verifier() -> String {
    format!(
        "{}{}",
        uuid::Uuid::new_v4().simple(),
        uuid::Uuid::new_v4().simple()
    )
}

/// The S256 code challenge: base64url-encoded SHA-256 of the verifier, with
/// no padding.
pub fn code_challenge(verifier: &str) -> String {
    base64url_no_pad(&Sha256::digest(verifier.as_bytes()))
}

/// Base64url (alphabet `A-Za-z0-9-_`) without `=` padding.
fn base64url_no_pad(bytes: &[u8]) -> String {
    encode_base64(bytes, BASE64URL_ALPHABET, false)
}

/// Standard base64 (alphabet `A-Za-z0-9+/`) with `=` padding. Shared with
/// the MCP client, whose mirrored header values use the same encoding.
pub(crate) fn standard_base64(bytes: &[u8]) -> String {
    encode_base64(
        bytes,
        b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/",
        true,
    )
}

fn encode_base64(bytes: &[u8], alphabet: &[u8; 64], pad: bool) -> String {
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let b0 = chunk[0] as u32;
        let b1 = *chunk.get(1).unwrap_or(&0) as u32;
        let b2 = *chunk.get(2).unwrap_or(&0) as u32;
        let n = (b0 << 16) | (b1 << 8) | b2;
        out.push(alphabet[((n >> 18) & 63) as usize] as char);
        out.push(alphabet[((n >> 12) & 63) as usize] as char);
        if chunk.len() > 1 {
            out.push(alphabet[((n >> 6) & 63) as usize] as char);
        }
        if chunk.len() > 2 {
            out.push(alphabet[(n & 63) as usize] as char);
        }
    }
    if pad {
        while !out.len().is_multiple_of(4) {
            out.push('=');
        }
    }
    out
}

/// The union of two space-separated scope lists, deduplicated and order-
/// stable: `a`'s scopes first, then `b`'s scopes that are not already there.
pub(crate) fn union_scopes(a: &str, b: &str) -> String {
    let mut seen = std::collections::HashSet::new();
    let mut parts = Vec::new();
    for scope in a.split_whitespace().chain(b.split_whitespace()) {
        if seen.insert(scope.to_string()) {
            parts.push(scope);
        }
    }
    parts.join(" ")
}

/// Parses the `resource_metadata` parameter out of a
/// `WWW-Authenticate: Bearer ...` challenge, per RFC 6750's comma-separated
/// `key=value` / `key="quoted"` grammar.
fn resource_metadata_url(header: &str) -> Option<String> {
    let (scheme, rest) = header.split_once(' ')?;
    if !scheme.eq_ignore_ascii_case("bearer") {
        return None;
    }
    // `rest` is already the parameter list, so parse it directly; passing it
    // back through `auth_param` would strip a second "scheme" and drop the
    // first parameter.
    parse_auth_params(rest)
        .into_iter()
        .find_map(|(key, value)| {
            key.eq_ignore_ascii_case("resource_metadata")
                .then_some(value)
        })
}

/// Reads one auth-param from a `Bearer` challenge's parameter list. The scheme
/// must be `Bearer` (whatever its case): a challenge in another scheme names a
/// credential the bearer token cannot answer. The name match is
/// case-insensitive per RFC 7235; the value keeps its case.
pub(crate) fn auth_param(header: &str, name: &str) -> Option<String> {
    let (scheme, rest) = header.split_once(' ')?;
    if !scheme.eq_ignore_ascii_case("bearer") {
        return None;
    }
    parse_auth_params(rest)
        .into_iter()
        .find_map(|(key, value)| key.eq_ignore_ascii_case(name).then_some(value))
}

fn parse_auth_params(input: &str) -> Vec<(String, String)> {
    let mut out = Vec::new();
    let bytes = input.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        while i < bytes.len() && (bytes[i] == b' ' || bytes[i] == b',') {
            i += 1;
        }
        let key_start = i;
        while i < bytes.len() && bytes[i] != b'=' {
            i += 1;
        }
        let key = input[key_start..i].trim();
        if i >= bytes.len() {
            break;
        }
        i += 1; // skip '='
        while i < bytes.len() && bytes[i] == b' ' {
            i += 1;
        }
        let value = if i < bytes.len() && bytes[i] == b'"' {
            i += 1;
            let value_start = i;
            while i < bytes.len() && bytes[i] != b'"' {
                i += 1;
            }
            let value = &input[value_start..i];
            if i < bytes.len() {
                i += 1; // closing quote
            }
            value
        } else {
            let value_start = i;
            while i < bytes.len() && bytes[i] != b',' {
                i += 1;
            }
            input[value_start..i].trim()
        };
        out.push((key.to_string(), value.to_string()));
    }
    out
}

/// The protected resource metadata well-known URIs, in probe order.
fn protected_resource_well_knowns(server_url: &reqwest::Url) -> Vec<String> {
    let origin = server_url.origin().ascii_serialization();
    let path = server_url.path().trim_matches('/');
    if path.is_empty() {
        vec![format!("{origin}/.well-known/oauth-protected-resource")]
    } else {
        vec![
            format!("{origin}/.well-known/oauth-protected-resource/{path}"),
            format!("{origin}/.well-known/oauth-protected-resource"),
        ]
    }
}

/// The authorization server metadata well-known URIs, in the spec's
/// path-insertion priority order.
fn authorization_server_well_knowns(issuer: &str) -> Result<Vec<String>, McpOAuthError> {
    let url = reqwest::Url::parse(issuer).map_err(|error| McpOAuthError::DiscoveryFailed {
        detail: format!("authorization server issuer is not a valid URL: {error}"),
    })?;
    let origin = url.origin().ascii_serialization();
    let path = url.path().trim_matches('/');
    if path.is_empty() {
        Ok(vec![
            format!("{origin}/.well-known/oauth-authorization-server"),
            format!("{origin}/.well-known/openid-configuration"),
        ])
    } else {
        Ok(vec![
            format!("{origin}/.well-known/oauth-authorization-server/{path}"),
            format!("{origin}/.well-known/openid-configuration/{path}"),
            format!("{origin}/{path}/.well-known/openid-configuration"),
        ])
    }
}

/// Resolves one stored credential field the way a model `api_key` resolves:
/// `env:VAR` reads `VAR` from the environment, anything else is used
/// literally.
fn resolve_credential(value: &str) -> Result<String, McpOAuthError> {
    match resolve_api_key(value) {
        Ok(resolved) => Ok(resolved),
        Err(ProviderError::MissingEnvVar { var }) => Err(McpOAuthError::MissingEnvVar { var }),
        Err(error) => Err(McpOAuthError::Internal(error.into())),
    }
}

/// The HTTP Basic credential for a confidential client: standard base64 of
/// `client_id:client_secret`, per RFC 7617.
fn basic_auth(client_id: &str, client_secret: &str) -> String {
    format!(
        "Basic {}",
        standard_base64(
            format!("{}:{}", form_encode(client_id), form_encode(client_secret)).as_bytes()
        )
    )
}

/// Percent-encodes one half of the Basic credentials as
/// `application/x-www-form-urlencoded`, per RFC 6749 section 2.3.1. A `+` or
/// `/` in a base64 secret is not form-safe, so it must be encoded before the
/// halves are joined.
fn form_encode(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for byte in value.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' => {
                out.push(byte as char);
            }
            b' ' => out.push('+'),
            other => out.push_str(&format!("%{other:02X}")),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use bosun_common::mcp::McpAuth;
    use serde_json::Value;
    use serde_json::json;

    use super::*;

    fn test_server(url: &str) -> McpServerSecret {
        McpServerSecret {
            name: "srv".to_string(),
            url: url.to_string(),
            enabled: true,
            auth: McpAuth::OAuth,
            bearer_token: None,
            oauth_client_id: Some("client".to_string()),
            oauth_client_secret: None,
            oauth_access_token: None,
            oauth_refresh_token: None,
            oauth_expires_at_secs: None,
            oauth_scope: None,
            added_at_secs: 1,
            updated_at_secs: None,
            last_error: None,
        }
    }

    fn redirect_uri() -> String {
        "http://127.0.0.1:8090/mcp/oauth/callback".to_string()
    }

    /// A pending flow recorded at `added_at`, with the fields a test does not
    /// vary left at fixed values.
    fn pending_flow(added_at: i64) -> PendingFlow {
        PendingFlow {
            server_name: "srv".to_string(),
            code_verifier: "verifier".to_string(),
            expected_issuer: "issuer".to_string(),
            redirect_uri: redirect_uri(),
            requested_scope: "files:read".to_string(),
            authorize_url: "https://issuer.example/authorize".to_string(),
            iss_parameter_supported: false,
            added_at,
        }
    }

    #[test]
    fn expires_at_saturates_instead_of_wrapping() {
        assert_eq!(expires_at(None), None);
        assert_eq!(expires_at(Some(u64::MAX)), Some(i64::MAX));
    }

    #[test]
    fn base64url_vectors_match() {
        assert_eq!(base64url_no_pad(b""), "");
        assert_eq!(base64url_no_pad(b"f"), "Zg");
        assert_eq!(base64url_no_pad(b"fo"), "Zm8");
        assert_eq!(base64url_no_pad(b"foo"), "Zm9v");
        assert_eq!(base64url_no_pad(b"foob"), "Zm9vYg");
        assert_eq!(base64url_no_pad(b"fooba"), "Zm9vYmE");
        assert_eq!(base64url_no_pad(b"foobar"), "Zm9vYmFy");
    }

    #[test]
    fn standard_base64_pads_and_uses_the_standard_alphabet() {
        assert_eq!(standard_base64(b"user:pass"), "dXNlcjpwYXNz");
        assert_eq!(standard_base64(b"f"), "Zg==");
    }

    #[test]
    fn form_encode_escapes_what_the_form_body_would_misread() {
        assert_eq!(form_encode("user"), "user");
        assert_eq!(form_encode("a b"), "a+b");
        assert_eq!(form_encode("c:d"), "c%3Ad");
        assert_eq!(form_encode("a+b/c="), "a%2Bb%2Fc%3D");
    }

    #[test]
    fn auth_params_match_names_case_insensitively() {
        let header = "Bearer Resource_Metadata=\"http://example\" Scope=\"Files:Read\"";
        assert_eq!(
            auth_param(header, "resource_metadata").as_deref(),
            Some("http://example")
        );
        assert_eq!(auth_param(header, "scope").as_deref(), Some("Files:Read"));
        assert_eq!(auth_param(header, "SCOPE").as_deref(), Some("Files:Read"));
    }

    #[test]
    fn resource_metadata_url_reads_the_first_challenge_parameter() {
        assert_eq!(
            resource_metadata_url(
                r#"Bearer resource_metadata="http://x/.well-known/oauth-protected-resource", scope="files:read""#
            )
            .as_deref(),
            Some("http://x/.well-known/oauth-protected-resource")
        );
    }

    #[test]
    fn code_verifier_is_64_unreserved_characters() {
        let verifier = code_verifier();
        assert_eq!(verifier.len(), 64);
        assert!(verifier.bytes().all(|byte| byte.is_ascii_alphanumeric()));
    }

    #[test]
    fn code_challenge_matches_the_s256_digest() {
        let verifier = "dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXk";
        assert_eq!(
            code_challenge(verifier),
            "E9Melhoa2OwvFrEMTJguCHaoeK1t8URWbuGJSstw-cM"
        );
    }

    #[test]
    fn union_scopes_deduplicates_and_preserves_order() {
        assert_eq!(union_scopes("", ""), "");
        assert_eq!(union_scopes("a b", "b c"), "a b c");
        assert_eq!(
            union_scopes("read write", "write execute"),
            "read write execute"
        );
        assert_eq!(union_scopes("a a b", "b c"), "a b c");
        assert_eq!(union_scopes("", "a b"), "a b");
    }

    /// One local axum stub serving the protected resource, its metadata, the
    /// authorization server metadata, and the token endpoint. It returns the
    /// address, the issuer, the canned token response, every token request's
    /// form fields, and every token request's `Authorization` header (`None`
    /// when the client sent none).
    async fn oauth_stub() -> (
        std::net::SocketAddr,
        String,
        Arc<std::sync::Mutex<Value>>,
        Arc<std::sync::Mutex<Vec<HashMap<String, String>>>>,
        Arc<std::sync::Mutex<Vec<Option<String>>>>,
    ) {
        oauth_stub_with_scope(None).await
    }

    /// The same stub as [`oauth_stub`], but the 401 challenge carries the
    /// given `scope` auth-param so a test can assert the initial request
    /// prefers the challenge scope over `scopes_supported`.
    async fn oauth_stub_with_scope(
        challenge_scope: Option<&str>,
    ) -> (
        std::net::SocketAddr,
        String,
        Arc<std::sync::Mutex<Value>>,
        Arc<std::sync::Mutex<Vec<HashMap<String, String>>>>,
        Arc<std::sync::Mutex<Vec<Option<String>>>>,
    ) {
        use axum::Router;
        use axum::extract::State;
        use axum::http::StatusCode;
        use axum::http::header;
        use axum::routing::get;
        use axum::routing::post;

        #[derive(Clone)]
        struct Stub {
            issuer: String,
            token_response: Arc<std::sync::Mutex<Value>>,
            token_requests: Arc<std::sync::Mutex<Vec<HashMap<String, String>>>>,
            token_authorizations: Arc<std::sync::Mutex<Vec<Option<String>>>>,
            challenge_scope: Option<String>,
        }

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let issuer = format!("http://{addr}");
        let token_response: Arc<std::sync::Mutex<Value>> = Arc::new(std::sync::Mutex::new(json!({
            "access_token": "access",
            "refresh_token": "refresh",
            "expires_in": 3600,
        })));
        let token_requests: Arc<std::sync::Mutex<Vec<HashMap<String, String>>>> =
            Arc::new(std::sync::Mutex::new(Vec::new()));
        let token_authorizations: Arc<std::sync::Mutex<Vec<Option<String>>>> =
            Arc::new(std::sync::Mutex::new(Vec::new()));
        let state = Stub {
            issuer: issuer.clone(),
            token_response: token_response.clone(),
            token_requests: token_requests.clone(),
            token_authorizations: token_authorizations.clone(),
            challenge_scope: challenge_scope.map(str::to_string),
        };

        async fn mcp(State(stub): State<Stub>) -> impl axum::response::IntoResponse {
            // The metadata lives only at this URL, so a test that gets through
            // discovery did it by parsing the 401 challenge header.
            let metadata = format!("{}/oauth-protected-resource", stub.issuer);
            let scope = stub
                .challenge_scope
                .map(|scope| format!(", scope=\"{scope}\""))
                .unwrap_or_default();
            (
                StatusCode::UNAUTHORIZED,
                [(
                    header::WWW_AUTHENTICATE,
                    format!("Bearer resource_metadata=\"{metadata}\"{scope}"),
                )],
            )
        }

        async fn protected_metadata(State(stub): State<Stub>) -> impl axum::response::IntoResponse {
            axum::Json(json!({
                "authorization_servers": [stub.issuer],
                "scopes_supported": ["read", "write"],
            }))
        }

        async fn as_metadata(State(stub): State<Stub>) -> impl axum::response::IntoResponse {
            axum::Json(json!({
                "issuer": stub.issuer,
                "authorization_endpoint": format!("{}/authorize", stub.issuer),
                "token_endpoint": format!("{}/token", stub.issuer),
                "code_challenge_methods_supported": ["S256"],
                "authorization_response_iss_parameter_supported": true,
                "scopes_supported": ["read", "write"],
            }))
        }

        async fn token(
            State(stub): State<Stub>,
            headers: axum::http::HeaderMap,
            form: axum::extract::Form<HashMap<String, String>>,
        ) -> impl axum::response::IntoResponse {
            stub.token_requests.lock().unwrap().push(form.0);
            stub.token_authorizations.lock().unwrap().push(
                headers
                    .get(header::AUTHORIZATION)
                    .and_then(|value| value.to_str().ok())
                    .map(str::to_string),
            );
            axum::Json(stub.token_response.lock().unwrap().clone())
        }

        let app = Router::new()
            .route("/mcp", get(mcp))
            .route("/oauth-protected-resource", get(protected_metadata))
            .route("/.well-known/oauth-authorization-server", get(as_metadata))
            .route("/token", post(token))
            .with_state(state);

        tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        (
            addr,
            issuer,
            token_response,
            token_requests,
            token_authorizations,
        )
    }

    async fn context(dir: &tempfile::TempDir) -> (McpOAuthContext, Store) {
        let store = Store::open(&dir.path().join("store.db")).unwrap();
        let context =
            McpOAuthContext::new(reqwest::Client::new(), store.clone(), Some(redirect_uri()));
        (context, store)
    }

    fn state_from_url(authorize_url: &str) -> String {
        let url = reqwest::Url::parse(authorize_url).unwrap();
        url.query_pairs()
            .find(|(key, _)| key == "state")
            .map(|(_, value)| value.to_string())
            .unwrap()
    }

    fn query_value(authorize_url: &str, key: &str) -> String {
        let url = reqwest::Url::parse(authorize_url).unwrap();
        url.query_pairs()
            .find(|(candidate, _)| candidate == key)
            .map(|(_, value)| value.to_string())
            .unwrap_or_default()
    }

    #[tokio::test]
    async fn full_flow_exchanges_the_code_and_stores_tokens() {
        let (addr, issuer, _, token_requests, token_authorizations) = oauth_stub().await;
        let dir = tempfile::tempdir().unwrap();
        let (context, store) = context(&dir).await;
        let server = test_server(&format!("http://{addr}/mcp"));
        store.insert_mcp_server(&server).await.unwrap();

        let authorize_url = context
            .start_authorization(&store.load_mcp_server("srv").await.unwrap().unwrap())
            .await
            .unwrap();
        let state = state_from_url(&authorize_url);

        assert_eq!(query_value(&authorize_url, "response_type"), "code");
        assert_eq!(query_value(&authorize_url, "client_id"), "client");
        assert_eq!(query_value(&authorize_url, "redirect_uri"), redirect_uri());
        assert_eq!(query_value(&authorize_url, "scope"), "read write");
        assert_eq!(query_value(&authorize_url, "code_challenge_method"), "S256");
        assert_eq!(
            query_value(&authorize_url, "resource"),
            format!("http://{addr}/mcp")
        );
        assert!(!query_value(&authorize_url, "code_challenge").is_empty());

        let query = OAuthCallbackQuery {
            code: Some("stub-code".to_string()),
            state: Some(state),
            error: None,
            iss: Some(issuer),
        };
        let authorized = context.complete_authorization(&query).await.unwrap();
        assert_eq!(authorized, "srv");

        // The guard is scoped so it is not held across the store call below.
        {
            let requests = token_requests.lock().unwrap();
            assert_eq!(requests.len(), 1);
            let exchange = &requests[0];
            assert_eq!(
                exchange.get("grant_type").map(String::as_str),
                Some("authorization_code")
            );
            assert_eq!(exchange.get("code").map(String::as_str), Some("stub-code"));
            assert_eq!(
                exchange.get("client_id").map(String::as_str),
                Some("client")
            );
            assert!(exchange.get("code_verifier").is_some_and(|v| v.len() == 64));
            let expected_resource = format!("http://{addr}/mcp");
            assert_eq!(
                exchange.get("resource").map(String::as_str),
                Some(expected_resource.as_str())
            );
            // A public client sends its client_id in the body and no Basic auth.
            assert!(token_authorizations.lock().unwrap()[0].is_none());
        }

        let stored = store.load_mcp_server("srv").await.unwrap().unwrap();
        assert_eq!(stored.oauth_access_token.as_deref(), Some("access"));
        assert_eq!(stored.oauth_refresh_token.as_deref(), Some("refresh"));
        assert!(stored.oauth_expires_at_secs.is_some());
        assert_eq!(
            stored.oauth_scope.as_deref(),
            Some("read write"),
            "the granted scopes are what the flow asked for"
        );
        assert_eq!(stored.last_error, None);
    }

    #[tokio::test]
    async fn a_challenge_scope_wins_over_scopes_supported() {
        let (addr, _issuer, _, _, _) = oauth_stub_with_scope(Some("files:read")).await;
        let dir = tempfile::tempdir().unwrap();
        let (context, store) = context(&dir).await;
        store
            .insert_mcp_server(&test_server(&format!("http://{addr}/mcp")))
            .await
            .unwrap();

        let authorize_url = context
            .start_authorization(&store.load_mcp_server("srv").await.unwrap().unwrap())
            .await
            .unwrap();
        assert_eq!(query_value(&authorize_url, "scope"), "files:read");
    }

    #[tokio::test]
    async fn the_granted_scope_the_token_response_names_is_what_the_row_records() {
        let (addr, issuer, token_response, _, _) = oauth_stub().await;
        let dir = tempfile::tempdir().unwrap();
        let (context, store) = context(&dir).await;
        store
            .insert_mcp_server(&test_server(&format!("http://{addr}/mcp")))
            .await
            .unwrap();

        // RFC 6749 section 5.1: the server names the scope it granted, which
        // may be narrower than the one the flow asked for.
        *token_response.lock().unwrap() = json!({
            "access_token": "access",
            "refresh_token": "refresh",
            "expires_in": 3600,
            "scope": "read",
        });
        let authorize_url = context
            .start_authorization(&store.load_mcp_server("srv").await.unwrap().unwrap())
            .await
            .unwrap();
        assert_eq!(query_value(&authorize_url, "scope"), "read write");
        let query = OAuthCallbackQuery {
            code: Some("stub-code".to_string()),
            state: Some(state_from_url(&authorize_url)),
            error: None,
            iss: Some(issuer.clone()),
        };
        context.complete_authorization(&query).await.unwrap();

        let stored = store.load_mcp_server("srv").await.unwrap().unwrap();
        assert_eq!(stored.oauth_scope.as_deref(), Some("read"));

        // A response that names no scope granted the requested one.
        *token_response.lock().unwrap() = json!({ "access_token": "access-2" });
        let authorize_url = context
            .start_authorization(&store.load_mcp_server("srv").await.unwrap().unwrap())
            .await
            .unwrap();
        let query = OAuthCallbackQuery {
            code: Some("stub-code".to_string()),
            state: Some(state_from_url(&authorize_url)),
            error: None,
            iss: Some(issuer.clone()),
        };
        context.complete_authorization(&query).await.unwrap();

        let stored = store.load_mcp_server("srv").await.unwrap().unwrap();
        assert_eq!(stored.oauth_scope.as_deref(), Some("read write"));

        // A scope of blanks names none either.
        *token_response.lock().unwrap() = json!({
            "access_token": "access-3",
            "scope": "   ",
        });
        let authorize_url = context
            .start_authorization(&store.load_mcp_server("srv").await.unwrap().unwrap())
            .await
            .unwrap();
        let query = OAuthCallbackQuery {
            code: Some("stub-code".to_string()),
            state: Some(state_from_url(&authorize_url)),
            error: None,
            iss: Some(issuer),
        };
        context.complete_authorization(&query).await.unwrap();

        let stored = store.load_mcp_server("srv").await.unwrap().unwrap();
        assert_eq!(stored.oauth_scope.as_deref(), Some("read write"));
    }

    #[tokio::test]
    async fn a_re_authorization_keeps_the_scopes_the_operator_already_granted() {
        // The challenge names only `files:write`, and the row's token already
        // carries `files:read`. Neither is a scope the resource advertises, so
        // the request names both only if the stored scopes are joined in.
        let (addr, _issuer, _, _, _) = oauth_stub_with_scope(Some("files:write")).await;
        let dir = tempfile::tempdir().unwrap();
        let (context, store) = context(&dir).await;
        let mut server = test_server(&format!("http://{addr}/mcp"));
        server.oauth_scope = Some("files:read".to_string());
        store.insert_mcp_server(&server).await.unwrap();

        let authorize_url = context
            .start_authorization(&store.load_mcp_server("srv").await.unwrap().unwrap())
            .await
            .unwrap();
        assert_eq!(
            query_value(&authorize_url, "scope"),
            "files:read files:write",
            "the scopes the token already carries are not dropped"
        );
    }

    #[tokio::test]
    async fn an_iss_mismatch_is_rejected_and_stores_no_tokens() {
        let (addr, _issuer, _, _, _) = oauth_stub().await;
        let dir = tempfile::tempdir().unwrap();
        let (context, store) = context(&dir).await;
        store
            .insert_mcp_server(&test_server(&format!("http://{addr}/mcp")))
            .await
            .unwrap();

        let authorize_url = context
            .start_authorization(&store.load_mcp_server("srv").await.unwrap().unwrap())
            .await
            .unwrap();
        let state = state_from_url(&authorize_url);

        let query = OAuthCallbackQuery {
            code: Some("stub-code".to_string()),
            state: Some(state),
            error: None,
            iss: Some("http://evil.example".to_string()),
        };
        assert!(matches!(
            context.complete_authorization(&query).await,
            Err(McpOAuthError::IssuerMismatch)
        ));
        let stored = store.load_mcp_server("srv").await.unwrap().unwrap();
        assert!(stored.oauth_access_token.is_none());
        assert!(stored.oauth_refresh_token.is_none());
    }

    #[tokio::test]
    async fn a_missing_iss_is_rejected_when_the_server_requires_it() {
        let (addr, _issuer, _, _, _) = oauth_stub().await;
        let dir = tempfile::tempdir().unwrap();
        let (context, store) = context(&dir).await;
        store
            .insert_mcp_server(&test_server(&format!("http://{addr}/mcp")))
            .await
            .unwrap();

        let authorize_url = context
            .start_authorization(&store.load_mcp_server("srv").await.unwrap().unwrap())
            .await
            .unwrap();
        let state = state_from_url(&authorize_url);

        let query = OAuthCallbackQuery {
            code: Some("stub-code".to_string()),
            state: Some(state),
            error: None,
            iss: None,
        };
        assert!(matches!(
            context.complete_authorization(&query).await,
            Err(McpOAuthError::MissingIss)
        ));
    }

    #[tokio::test]
    async fn refresh_uses_and_rotates_the_stored_refresh_token() {
        let (addr, _issuer, token_response, token_requests, _) = oauth_stub().await;
        let dir = tempfile::tempdir().unwrap();
        let (context, store) = context(&dir).await;
        let mut server = test_server(&format!("http://{addr}/mcp"));
        server.oauth_refresh_token = Some("old-refresh".to_string());
        store.insert_mcp_server(&server).await.unwrap();

        *token_response.lock().unwrap() = json!({
            "access_token": "new-access",
            "refresh_token": "new-refresh",
            "expires_in": 120,
        });
        context.refresh_mcp_token("srv").await.unwrap();

        let stored = store.load_mcp_server("srv").await.unwrap().unwrap();
        assert_eq!(stored.oauth_access_token.as_deref(), Some("new-access"));
        assert_eq!(stored.oauth_refresh_token.as_deref(), Some("new-refresh"));
        assert!(stored.oauth_expires_at_secs.is_some());

        let requests = token_requests.lock().unwrap();
        assert_eq!(requests.len(), 1);
        let refresh = &requests[0];
        assert_eq!(
            refresh.get("grant_type").map(String::as_str),
            Some("refresh_token")
        );
        assert_eq!(
            refresh.get("refresh_token").map(String::as_str),
            Some("old-refresh")
        );
        let expected_resource = format!("http://{addr}/mcp");
        assert_eq!(
            refresh.get("resource").map(String::as_str),
            Some(expected_resource.as_str())
        );
    }

    #[tokio::test]
    async fn refresh_without_a_new_expiry_keeps_the_stored_one() {
        let (addr, _issuer, token_response, _, _) = oauth_stub().await;
        let dir = tempfile::tempdir().unwrap();
        let (context, store) = context(&dir).await;
        let mut server = test_server(&format!("http://{addr}/mcp"));
        server.oauth_refresh_token = Some("old-refresh".to_string());
        server.oauth_expires_at_secs = Some(1_800_000_100);
        store.insert_mcp_server(&server).await.unwrap();

        *token_response.lock().unwrap() = json!({ "access_token": "new-access" });
        context.refresh_mcp_token("srv").await.unwrap();

        let stored = store.load_mcp_server("srv").await.unwrap().unwrap();
        assert_eq!(stored.oauth_access_token.as_deref(), Some("new-access"));
        assert_eq!(stored.oauth_expires_at_secs, Some(1_800_000_100));
    }

    #[tokio::test]
    async fn a_refresh_keeps_the_stored_scope_unless_the_response_names_one() {
        let (addr, _issuer, token_response, _, _) = oauth_stub().await;
        let dir = tempfile::tempdir().unwrap();
        let (context, store) = context(&dir).await;
        let mut server = test_server(&format!("http://{addr}/mcp"));
        server.oauth_refresh_token = Some("old-refresh".to_string());
        server.oauth_scope = Some("files:read".to_string());
        store.insert_mcp_server(&server).await.unwrap();

        // RFC 6749 section 5.1: a response with no `scope` grants the same
        // scope as the request, which a refresh carries none of.
        *token_response.lock().unwrap() = json!({ "access_token": "access-1" });
        context.refresh_mcp_token("srv").await.unwrap();
        let stored = store.load_mcp_server("srv").await.unwrap().unwrap();
        assert_eq!(stored.oauth_scope.as_deref(), Some("files:read"));

        *token_response.lock().unwrap() = json!({
            "access_token": "access-2",
            "scope": "files:read files:write",
        });
        context.refresh_mcp_token("srv").await.unwrap();
        let stored = store.load_mcp_server("srv").await.unwrap().unwrap();
        assert_eq!(
            stored.oauth_scope.as_deref(),
            Some("files:read files:write")
        );

        // A scope of blanks names none either, so the stored one stays.
        *token_response.lock().unwrap() = json!({
            "access_token": "access-3",
            "scope": "  ",
        });
        context.refresh_mcp_token("srv").await.unwrap();
        let stored = store.load_mcp_server("srv").await.unwrap().unwrap();
        assert_eq!(
            stored.oauth_scope.as_deref(),
            Some("files:read files:write")
        );
    }

    #[tokio::test]
    async fn refresh_without_a_stored_refresh_token_asks_to_reauthorize() {
        let (addr, _issuer, _, _, _) = oauth_stub().await;
        let dir = tempfile::tempdir().unwrap();
        let (context, store) = context(&dir).await;
        store
            .insert_mcp_server(&test_server(&format!("http://{addr}/mcp")))
            .await
            .unwrap();

        assert!(matches!(
            context.refresh_mcp_token("srv").await,
            Err(McpOAuthError::NoRefreshToken)
        ));
    }

    #[tokio::test]
    async fn a_blank_client_id_is_refused() {
        let (addr, _issuer, _, _, _) = oauth_stub().await;
        let dir = tempfile::tempdir().unwrap();
        let (context, store) = context(&dir).await;
        let mut server = test_server(&format!("http://{addr}/mcp"));
        server.oauth_client_id = Some("   ".to_string());
        store.insert_mcp_server(&server).await.unwrap();

        assert!(matches!(
            context
                .start_authorization(&store.load_mcp_server("srv").await.unwrap().unwrap())
                .await,
            Err(McpOAuthError::ClientIdRequired)
        ));
    }

    #[tokio::test]
    async fn a_confidential_client_sends_basic_auth_on_the_token_exchange() {
        let (addr, issuer, _, _, token_authorizations) = oauth_stub().await;
        let dir = tempfile::tempdir().unwrap();
        let (context, store) = context(&dir).await;
        let mut server = test_server(&format!("http://{addr}/mcp"));
        server.oauth_client_secret = Some("secret".to_string());
        store.insert_mcp_server(&server).await.unwrap();

        let authorize_url = context
            .start_authorization(&store.load_mcp_server("srv").await.unwrap().unwrap())
            .await
            .unwrap();
        let query = OAuthCallbackQuery {
            code: Some("stub-code".to_string()),
            state: Some(state_from_url(&authorize_url)),
            error: None,
            iss: Some(issuer),
        };
        context.complete_authorization(&query).await.unwrap();

        let authorizations = token_authorizations.lock().unwrap();
        assert_eq!(authorizations.len(), 1);
        assert_eq!(
            authorizations[0].as_deref(),
            Some("Basic Y2xpZW50OnNlY3JldA==")
        );
    }

    #[tokio::test]
    async fn env_var_client_credentials_resolve_before_they_are_sent() {
        let id_var = "BOSUN_TEST_MCP_CLIENT_ID";
        let secret_var = "BOSUN_TEST_MCP_CLIENT_SECRET";
        unsafe {
            std::env::set_var(id_var, "resolved-id");
            std::env::set_var(secret_var, "resolved-secret");
        }
        let (addr, issuer, _, _, token_authorizations) = oauth_stub().await;
        let dir = tempfile::tempdir().unwrap();
        let (context, store) = context(&dir).await;
        let mut server = test_server(&format!("http://{addr}/mcp"));
        server.oauth_client_id = Some(format!("env:{id_var}"));
        server.oauth_client_secret = Some(format!("env:{secret_var}"));
        store.insert_mcp_server(&server).await.unwrap();

        let authorize_url = context
            .start_authorization(&store.load_mcp_server("srv").await.unwrap().unwrap())
            .await
            .unwrap();
        assert_eq!(query_value(&authorize_url, "client_id"), "resolved-id");
        let query = OAuthCallbackQuery {
            code: Some("stub-code".to_string()),
            state: Some(state_from_url(&authorize_url)),
            error: None,
            iss: Some(issuer),
        };
        context.complete_authorization(&query).await.unwrap();

        let authorizations = token_authorizations.lock().unwrap();
        let expected = format!("Basic {}", standard_base64(b"resolved-id:resolved-secret"));
        assert_eq!(authorizations[0].as_deref(), Some(expected.as_str()));
    }

    #[tokio::test]
    async fn a_missing_env_var_client_id_is_named_in_the_error() {
        let var = "BOSUN_TEST_MCP_MISSING_CLIENT_ID";
        let (addr, _issuer, _, _, _) = oauth_stub().await;
        let dir = tempfile::tempdir().unwrap();
        let (context, store) = context(&dir).await;
        let mut server = test_server(&format!("http://{addr}/mcp"));
        server.oauth_client_id = Some(format!("env:{var}"));
        store.insert_mcp_server(&server).await.unwrap();

        // The variable must be absent for the error to be the one asserted.
        unsafe {
            std::env::remove_var(var);
        }
        assert!(matches!(
            context
                .start_authorization(&store.load_mcp_server("srv").await.unwrap().unwrap())
                .await,
            Err(McpOAuthError::MissingEnvVar { var }) if var == "BOSUN_TEST_MCP_MISSING_CLIENT_ID"
        ));
    }

    #[tokio::test]
    async fn a_missing_env_var_client_secret_is_named_in_the_error() {
        let var = "BOSUN_TEST_MCP_MISSING_CLIENT_SECRET";
        let (addr, issuer, _, _, _) = oauth_stub().await;
        let dir = tempfile::tempdir().unwrap();
        let (context, store) = context(&dir).await;
        let mut server = test_server(&format!("http://{addr}/mcp"));
        server.oauth_client_secret = Some(format!("env:{var}"));
        store.insert_mcp_server(&server).await.unwrap();
        unsafe {
            std::env::remove_var(var);
        }

        // The client id resolves, because the flow starts with it; the secret
        // only reaches the token request, which fails on the missing variable.
        let authorize_url = context
            .start_authorization(&store.load_mcp_server("srv").await.unwrap().unwrap())
            .await
            .unwrap();
        let query = OAuthCallbackQuery {
            code: Some("stub-code".to_string()),
            state: Some(state_from_url(&authorize_url)),
            error: None,
            iss: Some(issuer),
        };
        assert!(matches!(
            context.complete_authorization(&query).await,
            Err(McpOAuthError::MissingEnvVar { var })
                if var == "BOSUN_TEST_MCP_MISSING_CLIENT_SECRET"
        ));
    }

    #[tokio::test]
    async fn pending_authorize_returns_a_live_flow_and_ignores_an_expired_one() {
        let dir = tempfile::tempdir().unwrap();
        let (context, _) = context(&dir).await;
        let now = bosun_common::time::unix_secs(SystemTime::now());
        let stale = now - PENDING_FLOW_TTL_SECS - 1;
        context
            .insert_pending("stale".to_string(), pending_flow(stale))
            .await;

        // A flow that has expired cannot complete, so it is not returned.
        assert!(context.pending_authorize("srv").await.is_none());

        context
            .insert_pending("fresh".to_string(), pending_flow(now))
            .await;
        let pending = context.pending_authorize("srv").await.unwrap();
        assert_eq!(pending.url, "https://issuer.example/authorize");
        assert_eq!(pending.scope, "files:read");
        assert!(context.pending_authorize("other").await.is_none());
    }

    #[tokio::test]
    async fn a_pending_flow_older_than_the_ttl_is_not_returned() {
        let dir = tempfile::tempdir().unwrap();
        let (context, _) = context(&dir).await;
        let stale = bosun_common::time::unix_secs(SystemTime::now()) - PENDING_FLOW_TTL_SECS - 1;
        let now = bosun_common::time::unix_secs(SystemTime::now());
        context
            .insert_pending("stale".to_string(), pending_flow(stale))
            .await;

        assert!(matches!(
            context.take_pending("stale").await,
            Err(McpOAuthError::StateMismatch)
        ));

        // Recording a flow also prunes the expired ones, so a run of started
        // and abandoned authorizations cannot grow the map without bound.
        context
            .insert_pending("stale".to_string(), pending_flow(stale))
            .await;
        context
            .insert_pending("fresh".to_string(), pending_flow(now))
            .await;
        let pending = context.pending.read().await;
        assert!(!pending.contains_key("stale"));
        assert!(pending.contains_key("fresh"));
    }

    #[tokio::test]
    async fn a_second_flow_for_one_server_replaces_the_first() {
        let dir = tempfile::tempdir().unwrap();
        let (context, _) = context(&dir).await;
        let now = bosun_common::time::unix_secs(SystemTime::now());
        context
            .insert_pending("first".to_string(), pending_flow(now))
            .await;
        context
            .insert_pending("second".to_string(), pending_flow(now))
            .await;

        // One pending flow per server, so repeated authorize requests cannot
        // grow the map: the newer flow replaces the older one.
        let pending = context.pending.read().await;
        assert_eq!(pending.len(), 1);
        assert!(pending.contains_key("second"));
    }

    #[tokio::test]
    async fn a_second_flow_for_a_different_server_is_kept() {
        let dir = tempfile::tempdir().unwrap();
        let (context, _) = context(&dir).await;
        let now = bosun_common::time::unix_secs(SystemTime::now());
        context
            .insert_pending("first".to_string(), pending_flow(now))
            .await;
        let mut other = pending_flow(now);
        other.server_name = "other".to_string();
        context.insert_pending("second".to_string(), other).await;

        let pending = context.pending.read().await;
        assert_eq!(pending.len(), 2);
    }

    #[tokio::test]
    async fn metadata_without_s256_fails_pkce_unsupported() {
        assert_pkce_unsupported(Some(vec!["plain"])).await;
    }

    #[tokio::test]
    async fn metadata_without_code_challenge_methods_fails_pkce_unsupported() {
        assert_pkce_unsupported(None).await;
    }

    async fn assert_pkce_unsupported(methods: Option<Vec<&str>>) {
        use axum::Router;
        use axum::extract::State;
        use axum::http::StatusCode;
        use axum::http::header;
        use axum::routing::get;

        #[derive(Clone)]
        struct Stub {
            issuer: String,
            methods: Option<Vec<String>>,
        }

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let issuer = format!("http://{addr}");
        let state = Stub {
            issuer: issuer.clone(),
            methods: methods.map(|methods| methods.into_iter().map(str::to_string).collect()),
        };

        async fn mcp(State(stub): State<Stub>) -> impl axum::response::IntoResponse {
            let metadata = format!(
                "http://{}/.well-known/oauth-protected-resource",
                stub.issuer.trim_start_matches("http://")
            );
            (
                StatusCode::UNAUTHORIZED,
                [(
                    header::WWW_AUTHENTICATE,
                    format!("Bearer resource_metadata=\"{metadata}\""),
                )],
            )
        }

        async fn protected_metadata(State(stub): State<Stub>) -> impl axum::response::IntoResponse {
            axum::Json(json!({
                "authorization_servers": [stub.issuer],
            }))
        }

        async fn as_metadata(State(stub): State<Stub>) -> impl axum::response::IntoResponse {
            let mut metadata = json!({
                "issuer": stub.issuer,
                "authorization_endpoint": format!("{}/authorize", stub.issuer),
                "token_endpoint": format!("{}/token", stub.issuer),
                "authorization_response_iss_parameter_supported": true,
            });
            if let Some(methods) = stub.methods {
                metadata["code_challenge_methods_supported"] = json!(methods);
            }
            axum::Json(metadata)
        }

        let app = Router::new()
            .route("/mcp", get(mcp))
            .route(
                "/.well-known/oauth-protected-resource",
                get(protected_metadata),
            )
            .route("/.well-known/oauth-authorization-server", get(as_metadata))
            .with_state(state);

        tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });

        let dir = tempfile::tempdir().unwrap();
        let (context, store) = context(&dir).await;
        store
            .insert_mcp_server(&test_server(&format!("http://{addr}/mcp")))
            .await
            .unwrap();

        assert!(matches!(
            context
                .start_authorization(&store.load_mcp_server("srv").await.unwrap().unwrap())
                .await,
            Err(McpOAuthError::PkceUnsupported)
        ));
    }
}
