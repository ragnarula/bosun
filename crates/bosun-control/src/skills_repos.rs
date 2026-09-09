//! Fetches a skill repository from GitHub and keeps the store's index for it
//! current: resolve the tracked ref to a commit SHA and, only when the ref
//! moved, list the recursive git tree, fetch the kept package files, and
//! ship the packages to the store in one transaction.

use std::collections::HashMap;

use axum::http::StatusCode;
use bosun_common::error::ErrorExt;
use bosun_common::skills::SkillPackage;
use bosun_common::skills::SkillReference;
use bosun_common::skills::parse_skill_metadata;
use bosun_store::store::Store;
use bytes::Bytes;
use serde::Deserialize;
use thiserror::Error;
use tracing::instrument;
use tracing::warn;

/// The largest SKILL.md or reference chunk the indexer keeps; larger chunks
/// are skipped without being fetched.
pub const MAX_SKILL_FILE_BYTES: usize = 65536;
/// How many leading bytes of a fetched chunk are scanned for a NUL byte
/// before the chunk is treated as binary and skipped.
const BINARY_SCAN_BYTES: usize = 8192;
/// The host in every package address; the store fixes `github.com` for now.
const PACKAGE_HOST: &str = "github.com";
/// Sent on every GitHub request; the API refuses requests without a
/// recognized user agent.
const GITHUB_USER_AGENT: &str = "bosun/0.9.2";

#[derive(Debug, Error)]
pub enum SkillsFetchError {
    #[error("repository {repo} was not found")]
    RepositoryNotFound { repo: String },

    #[error("repository {repo} is malformed; expected owner/name")]
    MalformedRepo { repo: String },

    #[error("ref {target_ref} of repository {repo} was not found")]
    RefNotFound { repo: String, target_ref: String },

    #[error("the git tree of {repo} at {sha} is truncated; refusing to index a partial tree")]
    TreeTruncated { repo: String, sha: String },

    #[error("failed to index {repo}: {detail}")]
    Index { repo: String, detail: String },

    #[error("github request {url} returned {status}")]
    HttpStatus { url: String, status: String },

    #[error("github request {url} failed")]
    Network { url: String },

    #[error("internal error")]
    Internal(#[from] anyhow::Error),
}

/// The GitHub REST and raw endpoints a repo fetch reads through. `api_base`
/// and `raw_base` are constructor parameters so tests can point them at a
/// local stub. The token, when set, is sent only as a bearer `Authorization`
/// header and is never logged or stored.
pub struct GitHubClient {
    api_base: String,
    raw_base: String,
    token: Option<String>,
    client: reqwest::Client,
}

impl GitHubClient {
    pub fn new(api_base: &str, raw_base: &str, token: Option<String>) -> Self {
        Self {
            api_base: api_base.trim_end_matches('/').to_string(),
            raw_base: raw_base.trim_end_matches('/').to_string(),
            token: token.map(|token| token.to_string()),
            client: reqwest::Client::new(),
        }
    }

    /// The repository's default branch, from `GET /repos/{owner}/{name}`.
    async fn default_branch(
        &self,
        repo: &str,
        owner: &str,
        name: &str,
    ) -> Result<String, SkillsFetchError> {
        let url = format!("{}/repos/{owner}/{name}", self.api_base);
        let response = self.send(&url).await?;
        if response.status() == StatusCode::NOT_FOUND {
            return Err(SkillsFetchError::RepositoryNotFound {
                repo: repo.to_string(),
            });
        }
        if !response.status().is_success() {
            return Err(SkillsFetchError::HttpStatus {
                url,
                status: response.status().to_string(),
            });
        }
        let json: RepoJson =
            response
                .json::<RepoJson>()
                .await
                .map_err(|error| SkillsFetchError::Index {
                    repo: repo.to_string(),
                    detail: format!("failed to parse the repository: {error}"),
                })?;
        Ok(json.default_branch)
    }

    /// The commit SHA that `r#ref` names, from
    /// `GET /repos/{owner}/{name}/commits/{r#ref}`.
    async fn commit_sha(
        &self,
        repo: &str,
        owner: &str,
        name: &str,
        r#ref: &str,
    ) -> Result<String, SkillsFetchError> {
        let target_ref = r#ref.to_string();
        let url = format!(
            "{}/repos/{owner}/{name}/commits/{target_ref}",
            self.api_base
        );
        let response = self.send(&url).await?;
        if response.status() == StatusCode::NOT_FOUND {
            return Err(SkillsFetchError::RefNotFound {
                repo: repo.to_string(),
                target_ref,
            });
        }
        if !response.status().is_success() {
            return Err(SkillsFetchError::HttpStatus {
                url,
                status: response.status().to_string(),
            });
        }
        let json: CommitJson =
            response
                .json::<CommitJson>()
                .await
                .map_err(|error| SkillsFetchError::Index {
                    repo: repo.to_string(),
                    detail: format!("failed to parse the commit: {error}"),
                })?;
        Ok(json.sha)
    }

    /// The repository's recursive git tree at `sha`, failing when GitHub
    /// truncated it. A truncated tree would silently drop packages, so it is
    /// an error rather than a partial index.
    async fn tree_entries(
        &self,
        repo: &str,
        owner: &str,
        name: &str,
        sha: &str,
    ) -> Result<Vec<TreeEntryJson>, SkillsFetchError> {
        let url = format!(
            "{}/repos/{owner}/{name}/git/trees/{sha}?recursive=1",
            self.api_base
        );
        let response = self.send(&url).await?;
        if !response.status().is_success() {
            return Err(SkillsFetchError::HttpStatus {
                url,
                status: response.status().to_string(),
            });
        }
        let json: TreeJson =
            response
                .json::<TreeJson>()
                .await
                .map_err(|error| SkillsFetchError::Index {
                    repo: repo.to_string(),
                    detail: format!("failed to parse the git tree: {error}"),
                })?;
        if json.truncated {
            return Err(SkillsFetchError::TreeTruncated {
                repo: repo.to_string(),
                sha: sha.to_string(),
            });
        }
        Ok(json.tree)
    }

    /// Sends a GET and returns the response, or a [`SkillsFetchError`] when
    /// the request itself failed to complete.
    async fn send(&self, url: &str) -> Result<reqwest::Response, SkillsFetchError> {
        let request = self.client.get(url).header("user-agent", GITHUB_USER_AGENT);
        let request = match &self.token {
            Some(token) => request.bearer_auth(token),
            None => request,
        };
        match request.send().await {
            Ok(response) => Ok(response),
            Err(_) => Err(SkillsFetchError::Network {
                url: url.to_string(),
            }),
        }
    }

    /// Fetches one kept file's bytes at the pinned `sha`. Public files come
    /// from the raw endpoint; with a token the file is fetched through the
    /// API contents endpoint, which answers raw bytes for the
    /// `application/vnd.github.raw` accept type.
    async fn fetch_file(
        &self,
        owner: &str,
        name: &str,
        sha: &str,
        path: &str,
    ) -> Result<Bytes, SkillsFetchError> {
        let url = match &self.token {
            Some(_) => format!(
                "{}/repos/{owner}/{name}/contents/{path}?ref={sha}",
                self.api_base
            ),
            None => format!("{}/{owner}/{name}/{sha}/{path}", self.raw_base),
        };
        let mut request = self
            .client
            .get(&url)
            .header("user-agent", GITHUB_USER_AGENT);
        if let Some(token) = &self.token {
            request = request
                .header("accept", "application/vnd.github.raw")
                .bearer_auth(token);
        }
        let response = match request.send().await {
            Ok(response) => response,
            Err(_) => return Err(SkillsFetchError::Network { url }),
        };
        if !response.status().is_success() {
            return Err(SkillsFetchError::HttpStatus {
                url,
                status: response.status().to_string(),
            });
        }
        match response.bytes().await {
            Ok(bytes) => Ok(bytes),
            Err(_) => Err(SkillsFetchError::Network { url }),
        }
    }
}

/// The repository's default branch JSON.
#[derive(Debug, Deserialize)]
struct RepoJson {
    default_branch: String,
}

/// The commit JSON a ref resolves to.
#[derive(Debug, Deserialize)]
struct CommitJson {
    sha: String,
}

/// The recursive git tree JSON.
#[derive(Debug, Deserialize)]
struct TreeJson {
    truncated: bool,
    tree: Vec<TreeEntryJson>,
}

/// One path entry in a git tree.
#[derive(Debug, Deserialize)]
struct TreeEntryJson {
    path: String,
    #[serde(rename = "type")]
    kind: String,
    size: Option<i64>,
}

/// Fetches the repo's packages and replaces the store's index for it in one
/// transaction, returning the resolved commit SHA on success. A resolve that
/// names the commit the store already indexed skips the replace: it is still
/// a fresh success, so any recorded error is cleared. Any failure — a missing
/// repo or ref, a truncated tree, a failed fetch — records the error on the
/// repo row through `set_skill_repo_error` and changes nothing else. A repo
/// with no package roots indexes zero packages without error.
#[instrument(skip_all)]
pub async fn fetch_and_index(
    client: &GitHubClient,
    store: &Store,
    repo: &str,
    r#ref: Option<&str>,
) -> Result<String, SkillsFetchError> {
    match index_repo(client, store, repo, r#ref).await {
        Ok(sha) => Ok(sha),
        Err(error) => {
            if let Err(record_error) = store.set_skill_repo_error(repo, &error.to_string()).await {
                warn!(
                    repo = %repo,
                    error = %record_error.display_chain(),
                    "failed to record the skill repo fetch error"
                );
            }
            Err(error)
        }
    }
}

/// Indexes the repo's packages and replaces the store's rows for it, but only
/// when the resolved SHA differs from the stored one — the ADR's forward-only
/// update. An unchanged SHA means the ref did not move: the resolve itself is
/// the new information, so any recorded error is cleared and the replace (and
/// the tree fetch that feeds it) is skipped.
async fn index_repo(
    client: &GitHubClient,
    store: &Store,
    repo: &str,
    r#ref: Option<&str>,
) -> Result<String, SkillsFetchError> {
    let (owner, name) = split_repo(repo)?;
    let target_ref = match r#ref {
        Some(r#ref) => r#ref.to_string(),
        None => client.default_branch(repo, &owner, &name).await?,
    };
    let sha = client.commit_sha(repo, &owner, &name, &target_ref).await?;
    // The ADR's update is forward-only: the tracked ref is re-resolved, and
    // only a moved ref pays for a re-index. A resolve that names the sha the
    // store already indexed is itself new information, so any recorded error
    // is cleared and the replace is skipped. A repo with no stored row or no
    // stored sha falls through to the replace, as before.
    let stored = store
        .get_skill_repo(repo)
        .await
        .map_err(|error| SkillsFetchError::Index {
            repo: repo.to_string(),
            detail: format!("failed to read the stored index: {}", error.display_chain()),
        })?;
    if stored.as_ref().and_then(|row| row.sha.as_deref()) == Some(sha.as_str()) {
        store
            .clear_skill_repo_error(repo)
            .await
            .map_err(|error| SkillsFetchError::Index {
                repo: repo.to_string(),
                detail: format!(
                    "failed to clear the stored error: {}",
                    error.display_chain()
                ),
            })?;
        return Ok(sha);
    }
    let entries = client.tree_entries(repo, &owner, &name, &sha).await?;
    let packages = build_packages(client, repo, &owner, &name, &sha, entries).await?;
    store
        .replace_skill_repo(repo, &sha, &packages)
        .await
        .map_err(|error| SkillsFetchError::Index {
            repo: repo.to_string(),
            detail: format!(
                "failed to replace the stored index: {}",
                error.display_chain()
            ),
        })?;
    Ok(sha)
}

/// Splits `owner/repo` into its two parts, rejecting anything that does not
/// split into exactly two non-empty parts.
fn split_repo(repo: &str) -> Result<(String, String), SkillsFetchError> {
    let parts: Vec<&str> = repo.split('/').collect();
    if parts.len() != 2 || parts[0].is_empty() || parts[1].is_empty() {
        return Err(SkillsFetchError::MalformedRepo {
            repo: repo.to_string(),
        });
    }
    Ok((parts[0].to_string(), parts[1].to_string()))
}

/// Builds the packages the tree's blob entries declare, fetching each kept
/// file at the pinned `sha`.
async fn build_packages(
    client: &GitHubClient,
    repo: &str,
    owner: &str,
    name: &str,
    sha: &str,
    entries: Vec<TreeEntryJson>,
) -> Result<Vec<SkillPackage>, SkillsFetchError> {
    let mut blobs = HashMap::<String, TreeEntryJson>::new();
    let mut roots = Vec::<String>::new();
    for entry in entries {
        if entry.kind != "blob" {
            continue;
        }
        let path = entry.path.clone();
        if let Some(root) = package_root_of(&path) {
            roots.push(root);
        }
        blobs.insert(path, entry);
    }
    roots.sort();
    let mut packages = Vec::new();
    for root in roots {
        if let Some(package) = build_package(client, repo, owner, name, sha, &blobs, &root).await? {
            packages.push(package);
        }
    }
    packages.sort_by(|a, b| a.address.cmp(&b.address));
    Ok(packages)
}

/// One package for a package root, or None when the root's SKILL.md is
/// missing or over-cap.
async fn build_package(
    client: &GitHubClient,
    repo: &str,
    owner: &str,
    name: &str,
    sha: &str,
    blobs: &HashMap<String, TreeEntryJson>,
    root: &str,
) -> Result<Option<SkillPackage>, SkillsFetchError> {
    let skill_path = format!("{root}/SKILL.md");
    let Some(skill_entry) = blobs.get(&skill_path) else {
        return Ok(None);
    };
    if over_cap(skill_entry) {
        return Ok(None);
    }
    let skill_bytes = client.fetch_file(owner, name, sha, &skill_path).await?;
    if skill_bytes.len() > MAX_SKILL_FILE_BYTES {
        return Ok(None);
    }
    let content = String::from_utf8_lossy(&skill_bytes).into_owned();
    let (metadata, instructions) = parse_skill_metadata(&content);
    let mut references = Vec::new();
    let root_prefix = format!("{root}/");
    for (blob_path, entry) in blobs.iter() {
        if blob_path == &skill_path || !blob_path.starts_with(root_prefix.as_str()) {
            continue;
        }
        if over_cap(entry) {
            continue;
        }
        let bytes = client.fetch_file(owner, name, sha, blob_path).await?;
        if bytes.len() > MAX_SKILL_FILE_BYTES || looks_binary(&bytes) {
            continue;
        }
        references.push(SkillReference {
            path: blob_path
                .strip_prefix(root_prefix.as_str())
                .expect("reference paths carry the package root prefix")
                .to_string(),
            content: String::from_utf8_lossy(&bytes).into_owned(),
        });
    }
    references.sort_by(|a, b| a.path.cmp(&b.path));
    Ok(Some(SkillPackage {
        address: format!("{PACKAGE_HOST}/{owner}/{name}/{root}"),
        repo: repo.to_string(),
        name: metadata.name.unwrap_or_else(|| package_name(root)),
        description: metadata.description,
        when_to_use: metadata.when_to_use,
        license: metadata.license,
        author: metadata.author,
        instructions,
        references,
        sha: sha.to_string(),
    }))
}

/// The package root directory a blob path belongs to, when the path is a
/// `**/skills/**/<dir>/SKILL.md` — a directory named `skills` anywhere in
/// the path, then the package directory, with anything in between — whose
/// package directory is not hidden. Hidden ancestors do not matter:
/// `.claude/skills/foo` and `skills/engineering/code-review` are package
/// roots.
fn package_root_of(path: &str) -> Option<String> {
    let package_path = path.strip_suffix("/SKILL.md")?;
    let segments: Vec<&str> = package_path.split('/').collect();
    if segments.len() < 2 {
        return None;
    }
    let package_dir = segments[segments.len() - 1];
    if package_dir.starts_with('.') {
        return None;
    }
    // A `skills` segment must sit somewhere above the package directory; the
    // segments between the two may be empty (`skills/<dir>`) or category
    // directories (`skills/<category>/<dir>`).
    segments[..segments.len() - 1]
        .iter()
        .any(|segment| *segment == "skills")
        .then(|| package_path.to_string())
}

/// The package's short name when its frontmatter carries no `name`: its
/// directory, the last path segment.
fn package_name(root: &str) -> String {
    let segments: Vec<&str> = root.split('/').collect();
    segments[segments.len() - 1].to_string()
}

/// Whether the tree-reported size already exceeds the chunk cap, so the file
/// can be skipped without fetching it.
fn over_cap(entry: &TreeEntryJson) -> bool {
    entry.size.unwrap_or(0) > MAX_SKILL_FILE_BYTES as i64
}

/// Whether the leading bytes of a fetched chunk contain a NUL byte, marking
/// the chunk as binary.
fn looks_binary(bytes: &Bytes) -> bool {
    let scan = bytes.len().min(BINARY_SCAN_BYTES);
    bytes.as_ref()[..scan]
        .windows(1)
        .any(|window| window[0] == 0)
}

#[cfg(test)]
mod tests {
    use std::net::SocketAddr;
    use std::sync::Arc;
    use std::sync::Mutex;

    use bosun_common::skills::SkillAd;
    use serde_json::Value;
    use serde_json::json;
    use tempfile::tempdir;
    use tokio::net::TcpListener;

    use super::*;

    /// One request the stub served: the path (query stripped), the raw
    /// request target with its query, plus the authorization, accept, and
    /// user-agent headers it carried, so a test can see which endpoint and
    /// ref was used.
    struct RequestLog {
        path: String,
        raw_target: String,
        bearer: bool,
        accept_raw: bool,
        user_agent: Option<String>,
    }

    /// One canned response the stub serves for a request path.
    #[derive(Clone)]
    struct StubResponse {
        status: u16,
        reason: &'static str,
        body: Bytes,
    }

    /// A local GitHub stub: each request path (query ignored) maps to a
    /// response, and every other path answers 404. The routes and the request
    /// log are shared across connections.
    async fn stub_server(
        routes: Arc<HashMap<String, StubResponse>>,
        log: Arc<Mutex<Vec<RequestLog>>>,
    ) -> SocketAddr {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            loop {
                let Ok((mut stream, _)) = listener.accept().await else {
                    break;
                };
                let (routes, log) = (routes.clone(), log.clone());
                tokio::spawn(async move {
                    use tokio::io::AsyncReadExt;
                    use tokio::io::AsyncWriteExt;
                    let mut buf = [0u8; 4096];
                    let mut read = 0;
                    while let Ok(n) = stream.read(&mut buf[read..]).await {
                        if n == 0 {
                            break;
                        }
                        read += n;
                        if buf[..read].windows(4).any(|w| w == b"\r\n\r\n") {
                            break;
                        }
                    }
                    let head = String::from_utf8_lossy(&buf[..read]).into_owned();
                    let Some(request_line) = head.split('\n').next() else {
                        return;
                    };
                    let parts: Vec<&str> = request_line.split(' ').collect();
                    let target = if parts.len() >= 2 { parts[1] } else { "" };
                    let path = target
                        .split_once('?')
                        .map(|(path, _)| path)
                        .unwrap_or(target);
                    log.lock().unwrap().push(RequestLog {
                        path: path.to_string(),
                        raw_target: target.to_string(),
                        bearer: header_value(&head, "authorization")
                            .unwrap_or_default()
                            .split_once(' ')
                            .map(|(scheme, _)| scheme)
                            .unwrap_or_default()
                            .eq_ignore_ascii_case("bearer"),
                        accept_raw: header_value(&head, "accept")
                            .unwrap_or_default()
                            .contains("application/vnd.github.raw"),
                        user_agent: header_value(&head, "user-agent"),
                    });
                    match routes.get(path) {
                        Some(response) => {
                            let header = format!(
                                "HTTP/1.1 {} {}\r\ncontent-length: {}\r\n\r\n",
                                response.status,
                                response.reason,
                                response.body.len()
                            );
                            let _ = stream.write_all(header.as_bytes()).await;
                            let _ = stream.write_all(response.body.as_ref()).await;
                        }
                        None => {
                            let _ = stream
                                .write_all(
                                    b"HTTP/1.1 404 Not Found\r\ncontent-length: 9\r\n\r\nnot found",
                                )
                                .await;
                        }
                    }
                });
            }
        });
        addr
    }

    fn header_value(head: &str, name: &str) -> Option<String> {
        head.split('\n').find_map(|line| {
            let (key, value) = line.split_once(':')?;
            if key.trim().eq_ignore_ascii_case(name) {
                Some(value.trim().to_string())
            } else {
                None
            }
        })
    }

    /// Builds the route table from path/response pairs.
    fn route_map(routes: &[(&str, StubResponse)]) -> HashMap<String, StubResponse> {
        let mut map = HashMap::<String, StubResponse>::new();
        for (path, response) in routes {
            map.insert(path.to_string(), response.clone());
        }
        map
    }

    fn text_response(status: u16, reason: &'static str, body: &str) -> StubResponse {
        StubResponse {
            status,
            reason,
            body: Bytes::from(body.to_string()),
        }
    }

    fn ok(body: &str) -> StubResponse {
        text_response(200, "OK", body)
    }

    fn ok_owned(body: String) -> StubResponse {
        text_response(200, "OK", &body)
    }

    fn internal_error(body: &str) -> StubResponse {
        text_response(500, "Internal Server Error", body)
    }

    fn binary_response(bytes: Vec<u8>) -> StubResponse {
        StubResponse {
            status: 200,
            reason: "OK",
            body: Bytes::from(bytes),
        }
    }

    /// One blob entry for a tree response.
    fn tree_blob(path: &str, size: i64) -> Value {
        json!({
            "path": path,
            "mode": "100644",
            "type": "blob",
            "size": size,
            "sha": "blob-sha",
        })
    }

    /// One directory entry for a tree response.
    fn tree_dir(path: &str) -> Value {
        json!({
            "path": path,
            "mode": "040000",
            "type": "tree",
            "sha": "tree-sha",
        })
    }

    fn commit_json(sha: &str) -> String {
        serde_json::to_string(&json!({
            "sha": sha,
            "commit": { "message": "the commit" },
        }))
        .unwrap()
    }

    fn tree_json(truncated: bool, tree: Vec<Value>) -> String {
        serde_json::to_string(&json!({
            "truncated": truncated,
            "tree": tree,
        }))
        .unwrap()
    }

    /// A fresh client pointed at the stub `addr`.
    fn client_at(addr: SocketAddr, token: Option<String>) -> GitHubClient {
        let base = format!("http://{addr}");
        GitHubClient::new(base.as_str(), base.as_str(), token)
    }

    #[tokio::test]
    async fn fetch_and_index_resolves_the_default_branch_and_writes_through() {
        let routes = Arc::new(route_map(&[
            (
                "/repos/owner/acme",
                ok_owned(
                    serde_json::to_string(&json!({
                        "default_branch": "main",
                        "full_name": "owner/acme",
                    }))
                    .unwrap(),
                ),
            ),
            (
                "/repos/owner/acme/commits/main",
                ok_owned(commit_json("sha1234")),
            ),
            (
                "/repos/owner/acme/git/trees/sha1234",
                ok_owned(tree_json(
                    false,
                    vec![
                        tree_dir("skills"),
                        tree_dir("skills/alpha"),
                        tree_blob("skills/alpha/SKILL.md", 40),
                    ],
                )),
            ),
            (
                "/owner/acme/sha1234/skills/alpha/SKILL.md",
                ok(
                    "---\ndescription: Does alpha\nwhen_to_use: When alpha is needed\n---\n\nDo alpha things.\n",
                ),
            ),
        ]));
        let log = Arc::new(Mutex::new(Vec::<RequestLog>::new()));
        let addr = stub_server(routes, log.clone()).await;

        let dir = tempdir().unwrap();
        let store = Store::open(&dir.path().join("sessions.db")).unwrap();
        store
            .insert_skill_repo("owner/acme", "github.com", None)
            .await
            .unwrap();
        let client = client_at(addr, None);

        let sha = fetch_and_index(&client, &store, "owner/acme", None)
            .await
            .unwrap();
        assert_eq!(sha, "sha1234");

        let ads = store.advertised_skill_packages().await.unwrap();
        assert_eq!(ads.len(), 1);
        assert_eq!(ads[0].address, "github.com/owner/acme/skills/alpha");
        assert_eq!(ads[0].name, "alpha");
        assert_eq!(ads[0].description, "Does alpha");

        let (instructions, paths) = store
            .load_skill_package("github.com/owner/acme/skills/alpha")
            .await
            .unwrap()
            .unwrap();
        assert_eq!(instructions, "\n\nDo alpha things.\n");
        assert!(paths.is_empty());

        let repos = store.list_skill_repos().await.unwrap();
        assert_eq!(repos[0].sha.as_deref(), Some("sha1234"));
        assert_eq!(repos[0].last_error, None);
        assert_eq!(repos[0].package_count, 1);

        // The default branch was resolved from the repo before the commit
        // call, and the file came from the raw endpoint at the pinned sha.
        let requested: Vec<String> = log.lock().unwrap().iter().map(|r| r.path.clone()).collect();
        assert!(
            requested
                .iter()
                .any(|path| path.as_str() == "/repos/owner/acme"),
            "expected a repo call: {requested:?}"
        );
        assert!(
            requested
                .iter()
                .any(|path| path.as_str() == "/repos/owner/acme/commits/main"),
            "expected a commit call for the resolved branch: {requested:?}"
        );
        assert!(
            requested
                .iter()
                .any(|path| path.as_str() == "/owner/acme/sha1234/skills/alpha/SKILL.md"),
            "expected a raw file fetch at the pinned sha: {requested:?}"
        );
    }

    #[tokio::test]
    async fn the_layout_matrix_indexes_every_package_root_shape() {
        let routes = Arc::new(route_map(&[
            (
                "/repos/owner/acme/commits/main",
                ok_owned(commit_json("sha1234")),
            ),
            (
                "/repos/owner/acme/git/trees/sha1234",
                ok_owned(tree_json(
                    false,
                    vec![
                        tree_dir("skills"),
                        tree_dir("skills/alpha"),
                        tree_dir("skills/alpha/sub"),
                        tree_dir("skills/categories"),
                        tree_dir("skills/categories/engineering"),
                        tree_dir("skills/categories/engineering/epsilon"),
                        tree_dir(".claude"),
                        tree_dir(".claude/skills"),
                        tree_dir(".opencode"),
                        tree_dir(".opencode/skills"),
                        tree_dir("plugins"),
                        tree_dir("plugins/x"),
                        tree_dir("plugins/x/skills"),
                        tree_dir("skills/lonely"),
                        tree_blob("README.md", 42),
                        tree_blob("skills/alpha/SKILL.md", 44),
                        tree_blob("skills/alpha/REFERENCE.md", 40),
                        tree_blob("skills/alpha/sub/SKILL.md", 19),
                        tree_blob("skills/alpha/sub/NOTES.md", 20),
                        tree_blob("skills/alpha/sub/README.md", 9),
                        tree_blob("skills/categories/engineering/epsilon/SKILL.md", 45),
                        tree_blob(".claude/skills/beta/SKILL.md", 40),
                        tree_blob(".opencode/skills/gamma/SKILL.md", 50),
                        tree_blob("plugins/x/skills/delta/SKILL.md", 44),
                        tree_blob("skills/.hidden/SKILL.md", 41),
                        tree_blob("skills/over/SKILL.md", 70_000),
                        tree_blob("skills/bin/SKILL.md", 40),
                        tree_blob("skills/bin/APP.bin", 512),
                        tree_blob("skills/bin/KEEP.md", 32),
                        tree_blob("skills/SKILL.md", 20),
                        tree_blob("fooskill/x/SKILL.md", 21),
                    ],
                )),
            ),
            (
                "/owner/acme/sha1234/skills/alpha/SKILL.md",
                ok(
                    "---\ndescription: Does alpha\nwhen_to_use: When alpha is needed\nlicense: MIT\nauthor: Ada\n---\n\nDo alpha things.\n",
                ),
            ),
            (
                "/owner/acme/sha1234/skills/alpha/REFERENCE.md",
                ok("alpha reference text"),
            ),
            (
                "/owner/acme/sha1234/skills/alpha/sub/SKILL.md",
                ok("nested skill body"),
            ),
            (
                "/owner/acme/sha1234/skills/alpha/sub/NOTES.md",
                ok("sub notes text"),
            ),
            (
                "/owner/acme/sha1234/skills/alpha/sub/README.md",
                ok("sub readme text"),
            ),
            (
                "/owner/acme/sha1234/skills/categories/engineering/epsilon/SKILL.md",
                ok("---\ndescription: Does epsilon\n---\n\nEpsilon instructions.\n"),
            ),
            (
                "/owner/acme/sha1234/.claude/skills/beta/SKILL.md",
                ok("---\ndescription: Does beta\n---\n\nBeta instructions.\n"),
            ),
            (
                "/owner/acme/sha1234/.opencode/skills/gamma/SKILL.md",
                ok("Gamma instructions.\nWith a second line.\n"),
            ),
            (
                "/owner/acme/sha1234/plugins/x/skills/delta/SKILL.md",
                ok("---\ndescription: Does delta\nlicense: MIT\n---\n\nDelta instructions.\n"),
            ),
            (
                "/owner/acme/sha1234/skills/bin/SKILL.md",
                ok("---\ndescription: Does bin\n---\n\nBin instructions.\n"),
            ),
            (
                "/owner/acme/sha1234/skills/bin/APP.bin",
                binary_response(vec![b'b', 0, b'a']),
            ),
            ("/owner/acme/sha1234/skills/bin/KEEP.md", ok("keep me")),
        ]));
        let log = Arc::new(Mutex::new(Vec::<RequestLog>::new()));
        let addr = stub_server(routes, log.clone()).await;

        let dir = tempdir().unwrap();
        let store = Store::open(&dir.path().join("sessions.db")).unwrap();
        store
            .insert_skill_repo("owner/acme", "github.com", Some("main"))
            .await
            .unwrap();
        let client = client_at(addr, None);

        let sha = fetch_and_index(&client, &store, "owner/acme", Some("main"))
            .await
            .unwrap();
        assert_eq!(sha, "sha1234");

        // All the layouts produce packages, sorted by address, with the
        // frontmatter description (or the first body line) advertised.
        assert_eq!(
            store.advertised_skill_packages().await.unwrap(),
            vec![
                SkillAd {
                    address: "github.com/owner/acme/.claude/skills/beta".into(),
                    name: "beta".into(),
                    description: "Does beta".into(),
                },
                SkillAd {
                    address: "github.com/owner/acme/.opencode/skills/gamma".into(),
                    name: "gamma".into(),
                    description: "Gamma instructions.".into(),
                },
                SkillAd {
                    address: "github.com/owner/acme/plugins/x/skills/delta".into(),
                    name: "delta".into(),
                    description: "Does delta".into(),
                },
                SkillAd {
                    address: "github.com/owner/acme/skills/alpha".into(),
                    name: "alpha".into(),
                    description: "Does alpha".into(),
                },
                SkillAd {
                    address: "github.com/owner/acme/skills/alpha/sub".into(),
                    name: "sub".into(),
                    description: "nested skill body".into(),
                },
                SkillAd {
                    address: "github.com/owner/acme/skills/bin".into(),
                    name: "bin".into(),
                    description: "Does bin".into(),
                },
                SkillAd {
                    address: "github.com/owner/acme/skills/categories/engineering/epsilon".into(),
                    name: "epsilon".into(),
                    description: "Does epsilon".into(),
                },
            ]
        );

        // The body is the instructions, and the package's other blobs —
        // including the nested package dirs — are references sorted by path.
        let (instructions, paths) = store
            .load_skill_package("github.com/owner/acme/skills/alpha")
            .await
            .unwrap()
            .unwrap();
        assert_eq!(instructions, "\n\nDo alpha things.\n");
        assert_eq!(
            paths,
            [
                "REFERENCE.md",
                "sub/NOTES.md",
                "sub/README.md",
                "sub/SKILL.md"
            ]
        );
        assert_eq!(
            store
                .read_skill_reference("github.com/owner/acme/skills/alpha", "REFERENCE.md")
                .await
                .unwrap()
                .as_deref(),
            Some("alpha reference text")
        );

        // The binary chunk is skipped, the text chunk beside it is kept.
        let (_, bin_paths) = store
            .load_skill_package("github.com/owner/acme/skills/bin")
            .await
            .unwrap()
            .unwrap();
        assert_eq!(bin_paths, ["KEEP.md"]);

        // A hidden package dir, an over-cap SKILL.md, and a dir without one
        // produce nothing.
        for address in [
            "github.com/owner/acme/skills/.hidden",
            "github.com/owner/acme/skills/over",
            "github.com/owner/acme/skills/lonely",
        ] {
            assert!(
                store.load_skill_package(address).await.unwrap().is_none(),
                "{address} should not exist"
            );
        }

        // The glob rule: a `SKILL.md` directly under `skills/`, or under a
        // non-`skills` segment, is not a package; a `skills/<category>/<dir>`
        // one is.
        let ads = store.advertised_skill_packages().await.unwrap();
        for address in [
            "github.com/owner/acme/skills",
            "github.com/owner/acme/fooskill/x",
        ] {
            assert!(
                !ads.iter().any(|ad| ad.address == address),
                "{address} should not be a package"
            );
        }

        let repos = store.list_skill_repos().await.unwrap();
        assert_eq!(repos[0].package_count, 7);

        // The over-cap and hidden SKILL.mds were never fetched.
        let requested: Vec<String> = log.lock().unwrap().iter().map(|r| r.path.clone()).collect();
        assert!(
            !requested
                .iter()
                .any(|path| path.as_str().contains("skills/over")),
            "the over-cap file must not be fetched: {requested:?}"
        );
        assert!(
            !requested
                .iter()
                .any(|path| path.as_str().contains("skills/.hidden")),
            "the hidden package's file must not be fetched: {requested:?}"
        );
    }

    #[tokio::test]
    async fn a_missing_repository_reports_repository_not_found() {
        let routes = Arc::new(HashMap::<String, StubResponse>::new());
        let log = Arc::new(Mutex::new(Vec::<RequestLog>::new()));
        let addr = stub_server(routes, log.clone()).await;

        let dir = tempdir().unwrap();
        let store = Store::open(&dir.path().join("sessions.db")).unwrap();
        store
            .insert_skill_repo("owner/ghost", "github.com", None)
            .await
            .unwrap();
        let client = client_at(addr, None);

        // No ref is tracked, so the default-branch resolution is the first
        // call, and its 404 is a missing repository.
        let error = fetch_and_index(&client, &store, "owner/ghost", None)
            .await
            .unwrap_err();
        assert!(matches!(
            &error,
            SkillsFetchError::RepositoryNotFound { repo } if repo == "owner/ghost"
        ));

        // The failure was recorded and nothing was stored.
        let repos = store.list_skill_repos().await.unwrap();
        assert_eq!(repos.len(), 1);
        assert_eq!(repos[0].sha, None);
        assert_eq!(repos[0].package_count, 0);
        assert!(repos[0].last_error.is_some());
        assert!(store.advertised_skill_packages().await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn a_missing_ref_reports_ref_not_found() {
        let routes = Arc::new(HashMap::<String, StubResponse>::new());
        let log = Arc::new(Mutex::new(Vec::<RequestLog>::new()));
        let addr = stub_server(routes, log.clone()).await;

        let dir = tempdir().unwrap();
        let store = Store::open(&dir.path().join("sessions.db")).unwrap();
        store
            .insert_skill_repo("owner/acme", "github.com", Some("main"))
            .await
            .unwrap();
        let client = client_at(addr, None);

        let error = fetch_and_index(&client, &store, "owner/acme", Some("main"))
            .await
            .unwrap_err();
        assert!(matches!(
            &error,
            SkillsFetchError::RefNotFound { repo, target_ref }
                if repo == "owner/acme" && target_ref == "main"
        ));

        let repos = store.list_skill_repos().await.unwrap();
        assert_eq!(repos[0].sha, None);
        assert!(repos[0].last_error.is_some());
    }

    #[tokio::test]
    async fn a_truncated_tree_fails_the_fetch_and_records_last_error() {
        let routes = Arc::new(route_map(&[
            (
                "/repos/owner/acme/commits/main",
                ok_owned(commit_json("sha1234")),
            ),
            (
                "/repos/owner/acme/git/trees/sha1234",
                ok_owned(tree_json(
                    true,
                    vec![tree_blob("skills/alpha/SKILL.md", 40)],
                )),
            ),
        ]));
        let log = Arc::new(Mutex::new(Vec::<RequestLog>::new()));
        let addr = stub_server(routes, log.clone()).await;

        let dir = tempdir().unwrap();
        let store = Store::open(&dir.path().join("sessions.db")).unwrap();
        store
            .insert_skill_repo("owner/acme", "github.com", Some("main"))
            .await
            .unwrap();
        let client = client_at(addr, None);

        let error = fetch_and_index(&client, &store, "owner/acme", Some("main"))
            .await
            .unwrap_err();
        assert!(matches!(
            &error,
            SkillsFetchError::TreeTruncated { repo, sha } if repo == "owner/acme" && sha == "sha1234"
        ));

        let repos = store.list_skill_repos().await.unwrap();
        assert_eq!(repos[0].sha, None);
        assert_eq!(repos[0].package_count, 0);
        assert!(repos[0].last_error.is_some());
        assert!(store.advertised_skill_packages().await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn a_failed_file_fetch_changes_nothing_and_records_last_error() {
        let routes = Arc::new(route_map(&[
            (
                "/repos/owner/acme/commits/main",
                ok_owned(commit_json("sha1234")),
            ),
            (
                "/repos/owner/acme/git/trees/sha1234",
                ok_owned(tree_json(
                    false,
                    vec![tree_blob("skills/alpha/SKILL.md", 40)],
                )),
            ),
            (
                "/owner/acme/sha1234/skills/alpha/SKILL.md",
                internal_error("boom"),
            ),
        ]));
        let log = Arc::new(Mutex::new(Vec::<RequestLog>::new()));
        let addr = stub_server(routes, log.clone()).await;

        let dir = tempdir().unwrap();
        let store = Store::open(&dir.path().join("sessions.db")).unwrap();
        store
            .insert_skill_repo("owner/acme", "github.com", Some("main"))
            .await
            .unwrap();
        let client = client_at(addr, None);

        let error = fetch_and_index(&client, &store, "owner/acme", Some("main"))
            .await
            .unwrap_err();
        assert!(matches!(
            &error,
            SkillsFetchError::HttpStatus { url, status }
                if url.ends_with("/owner/acme/sha1234/skills/alpha/SKILL.md")
                    && status == "500 Internal Server Error"
        ));

        let repos = store.list_skill_repos().await.unwrap();
        assert_eq!(repos[0].sha, None);
        assert_eq!(repos[0].package_count, 0);
        assert!(repos[0].last_error.is_some());
        assert!(store.advertised_skill_packages().await.unwrap().is_empty());
        assert!(
            store
                .load_skill_package("github.com/owner/acme/skills/alpha")
                .await
                .unwrap()
                .is_none()
        );
    }

    #[tokio::test]
    async fn a_repo_without_package_roots_indexes_zero_packages_without_error() {
        let routes = Arc::new(route_map(&[
            (
                "/repos/owner/acme/commits/main",
                ok_owned(commit_json("sha1234")),
            ),
            (
                "/repos/owner/acme/git/trees/sha1234",
                ok_owned(tree_json(
                    false,
                    vec![
                        tree_dir("docs"),
                        tree_dir("skills"),
                        tree_dir("skills/.hidden"),
                        tree_blob("README.md", 42),
                        tree_blob("docs/GUIDE.md", 64),
                        // No valid root: `skills/SKILL.md` has no package dir
                        // between `skills` and the file, and a `SKILL.md`
                        // with no `skills` ancestor anywhere is not a package.
                        tree_blob("skills/SKILL.md", 40),
                        tree_blob("docs/guides/alpha/SKILL.md", 40),
                    ],
                )),
            ),
        ]));
        let log = Arc::new(Mutex::new(Vec::<RequestLog>::new()));
        let addr = stub_server(routes, log.clone()).await;

        let dir = tempdir().unwrap();
        let store = Store::open(&dir.path().join("sessions.db")).unwrap();
        store
            .insert_skill_repo("owner/acme", "github.com", Some("main"))
            .await
            .unwrap();
        let client = client_at(addr, None);

        let sha = fetch_and_index(&client, &store, "owner/acme", Some("main"))
            .await
            .unwrap();
        assert_eq!(sha, "sha1234");
        assert!(store.advertised_skill_packages().await.unwrap().is_empty());
        let repos = store.list_skill_repos().await.unwrap();
        assert_eq!(repos[0].sha.as_deref(), Some("sha1234"));
        assert_eq!(repos[0].package_count, 0);
        assert_eq!(repos[0].last_error, None);
    }

    #[tokio::test]
    async fn an_unchanged_sha_skips_the_replace_and_clears_last_error() {
        let routes = Arc::new(route_map(&[
            (
                "/repos/owner/acme/commits/main",
                ok_owned(commit_json("sha1234")),
            ),
            (
                "/repos/owner/acme/git/trees/sha1234",
                ok_owned(tree_json(
                    false,
                    vec![
                        tree_dir("skills"),
                        tree_dir("skills/alpha"),
                        tree_blob("skills/alpha/SKILL.md", 40),
                    ],
                )),
            ),
            (
                "/owner/acme/sha1234/skills/alpha/SKILL.md",
                ok("---\ndescription: Does alpha\n---\n\nDo alpha things.\n"),
            ),
        ]));
        let log = Arc::new(Mutex::new(Vec::<RequestLog>::new()));
        let addr = stub_server(routes, log.clone()).await;

        let dir = tempdir().unwrap();
        let store = Store::open(&dir.path().join("sessions.db")).unwrap();
        store
            .insert_skill_repo("owner/acme", "github.com", Some("main"))
            .await
            .unwrap();
        let client = client_at(addr, None);

        let sha = fetch_and_index(&client, &store, "owner/acme", Some("main"))
            .await
            .unwrap();
        assert_eq!(sha, "sha1234");

        // A failed fetch leaves an error on the row; the next fresh resolve
        // that names the same sha clears it.
        store
            .set_skill_repo_error("owner/acme", "previous fetch failed")
            .await
            .unwrap();
        let first_fetch_requests = log.lock().unwrap().len();

        let sha = fetch_and_index(&client, &store, "owner/acme", Some("main"))
            .await
            .unwrap();
        assert_eq!(sha, "sha1234");

        // The second fetch re-resolved the ref and stopped there: only the
        // commit call is new, so no tree or file request, hence no replace.
        let requested: Vec<String> = log.lock().unwrap().iter().map(|r| r.path.clone()).collect();
        assert_eq!(requested.len(), first_fetch_requests + 1, "{requested:?}");
        assert_eq!(
            requested[first_fetch_requests], "/repos/owner/acme/commits/main",
            "{requested:?}"
        );

        // The stored index is untouched and the error is gone.
        assert_eq!(store.advertised_skill_packages().await.unwrap().len(), 1);
        let (instructions, _) = store
            .load_skill_package("github.com/owner/acme/skills/alpha")
            .await
            .unwrap()
            .unwrap();
        assert_eq!(instructions, "\n\nDo alpha things.\n");
        let repos = store.list_skill_repos().await.unwrap();
        assert_eq!(repos[0].sha.as_deref(), Some("sha1234"));
        assert_eq!(repos[0].package_count, 1);
        assert_eq!(repos[0].last_error, None);
    }

    #[tokio::test]
    async fn a_changed_sha_replaces_the_index() {
        // Two upstreams on one store: the first fetch indexes sha1, then the
        // second fetch points at a different stub whose ref resolves to sha2.
        let first = Arc::new(route_map(&[
            (
                "/repos/owner/acme/commits/main",
                ok_owned(commit_json("sha1")),
            ),
            (
                "/repos/owner/acme/git/trees/sha1",
                ok_owned(tree_json(
                    false,
                    vec![
                        tree_dir("skills"),
                        tree_dir("skills/alpha"),
                        tree_blob("skills/alpha/SKILL.md", 40),
                    ],
                )),
            ),
            (
                "/owner/acme/sha1/skills/alpha/SKILL.md",
                ok("---\ndescription: Does alpha\n---\n\nversion one instructions.\n"),
            ),
        ]));
        let first_log = Arc::new(Mutex::new(Vec::<RequestLog>::new()));
        let first_addr = stub_server(first, first_log.clone()).await;
        let second = Arc::new(route_map(&[
            (
                "/repos/owner/acme/commits/main",
                ok_owned(commit_json("sha2")),
            ),
            (
                "/repos/owner/acme/git/trees/sha2",
                ok_owned(tree_json(
                    false,
                    vec![
                        tree_dir("skills"),
                        tree_dir("skills/alpha"),
                        tree_blob("skills/alpha/SKILL.md", 40),
                    ],
                )),
            ),
            (
                "/owner/acme/sha2/skills/alpha/SKILL.md",
                ok("---\ndescription: Does alpha\n---\n\nversion two instructions.\n"),
            ),
        ]));
        let second_log = Arc::new(Mutex::new(Vec::<RequestLog>::new()));
        let second_addr = stub_server(second, second_log.clone()).await;

        let dir = tempdir().unwrap();
        let store = Store::open(&dir.path().join("sessions.db")).unwrap();
        store
            .insert_skill_repo("owner/acme", "github.com", Some("main"))
            .await
            .unwrap();

        let sha = fetch_and_index(
            &client_at(first_addr, None),
            &store,
            "owner/acme",
            Some("main"),
        )
        .await
        .unwrap();
        assert_eq!(sha, "sha1");

        let sha = fetch_and_index(
            &client_at(second_addr, None),
            &store,
            "owner/acme",
            Some("main"),
        )
        .await
        .unwrap();
        assert_eq!(sha, "sha2");

        // The moved ref replaced the index: the package holds the second
        // commit's content and the repo row names the second sha.
        let (instructions, _) = store
            .load_skill_package("github.com/owner/acme/skills/alpha")
            .await
            .unwrap()
            .unwrap();
        assert_eq!(instructions, "\n\nversion two instructions.\n");
        assert_eq!(
            second_log.lock().unwrap().len(),
            3,
            "the changed ref re-fetches the tree and the files"
        );
        let repos = store.list_skill_repos().await.unwrap();
        assert_eq!(repos[0].sha.as_deref(), Some("sha2"));
        assert_eq!(repos[0].package_count, 1);
        assert_eq!(repos[0].last_error, None);
    }

    #[tokio::test]
    async fn a_token_sends_file_fetches_through_the_api_contents_endpoint() {
        let routes = Arc::new(route_map(&[
            (
                "/repos/owner/acme/commits/main",
                ok_owned(commit_json("sha1234")),
            ),
            (
                "/repos/owner/acme/git/trees/sha1234",
                ok_owned(tree_json(
                    false,
                    vec![tree_blob("skills/alpha/SKILL.md", 34)],
                )),
            ),
            (
                "/repos/owner/acme/contents/skills/alpha/SKILL.md",
                ok("---\ndescription: Does alpha\n---\n\nDo things.\n"),
            ),
        ]));
        let log = Arc::new(Mutex::new(Vec::<RequestLog>::new()));
        let addr = stub_server(routes, log.clone()).await;

        let dir = tempdir().unwrap();
        let store = Store::open(&dir.path().join("sessions.db")).unwrap();
        store
            .insert_skill_repo("owner/acme", "github.com", Some("main"))
            .await
            .unwrap();
        let client = client_at(addr, Some("a-secret-token".to_string()));

        let sha = fetch_and_index(&client, &store, "owner/acme", Some("main"))
            .await
            .unwrap();
        assert_eq!(sha, "sha1234");

        // The file was fetched from the API contents endpoint, pinned to the
        // indexed sha, with the raw accept type and a bearer token; the raw
        // endpoint was never touched.
        {
            let requests = log.lock().unwrap();
            assert!(
                requests.iter().any(|request| {
                    request.path == "/repos/owner/acme/contents/skills/alpha/SKILL.md"
                        && request.raw_target.contains("?ref=sha1234")
                        && request.bearer
                        && request.accept_raw
                        && request.user_agent.as_deref() == Some(GITHUB_USER_AGENT)
                }),
                "expected a bearer-authenticated raw contents fetch pinned to the sha"
            );
            assert!(
                !requests
                    .iter()
                    .any(|request| request.path.starts_with("/owner/acme/")),
                "the public raw endpoint must not be used with a token"
            );
        }

        let ads = store.advertised_skill_packages().await.unwrap();
        assert_eq!(ads.len(), 1);
        assert_eq!(ads[0].address, "github.com/owner/acme/skills/alpha");
        assert_eq!(ads[0].description, "Does alpha");
    }

    #[test]
    fn split_repo_rejects_malformed_repos() {
        assert_eq!(
            split_repo("owner/acme").unwrap(),
            ("owner".to_string(), "acme".to_string())
        );
        for bad in ["a/b/c", "", "/acme", "owner/", "/"] {
            let error = split_repo(bad).unwrap_err();
            assert!(
                matches!(
                    &error,
                    SkillsFetchError::MalformedRepo { repo } if *repo == bad
                ),
                "expected {bad:?} to be rejected as malformed: {error:?}"
            );
        }
    }

    #[tokio::test]
    async fn a_frontmatter_name_sets_the_package_name_but_not_the_address() {
        let routes = Arc::new(route_map(&[
            (
                "/repos/owner/acme/commits/main",
                ok_owned(commit_json("sha1234")),
            ),
            (
                "/repos/owner/acme/git/trees/sha1234",
                ok_owned(tree_json(
                    false,
                    vec![
                        tree_dir("skills"),
                        tree_dir("skills/alpha"),
                        tree_blob("skills/alpha/SKILL.md", 40),
                    ],
                )),
            ),
            (
                "/owner/acme/sha1234/skills/alpha/SKILL.md",
                ok("---\nname: alpha-tool\ndescription: Does alpha\n---\n\nDo things.\n"),
            ),
        ]));
        let log = Arc::new(Mutex::new(Vec::<RequestLog>::new()));
        let addr = stub_server(routes, log.clone()).await;

        let dir = tempdir().unwrap();
        let store = Store::open(&dir.path().join("sessions.db")).unwrap();
        store
            .insert_skill_repo("owner/acme", "github.com", Some("main"))
            .await
            .unwrap();
        let client = client_at(addr, None);

        let sha = fetch_and_index(&client, &store, "owner/acme", Some("main"))
            .await
            .unwrap();
        assert_eq!(sha, "sha1234");

        // The frontmatter name is advertised; the address still comes from the
        // directory-based package root.
        let ads = store.advertised_skill_packages().await.unwrap();
        assert_eq!(ads.len(), 1);
        assert_eq!(ads[0].address, "github.com/owner/acme/skills/alpha");
        assert_eq!(ads[0].name, "alpha-tool");
        assert_eq!(ads[0].description, "Does alpha");
    }
}
