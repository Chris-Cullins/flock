use std::fmt;

use anyhow::{Result, bail};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::event::Event;
use crate::refs::RepoRef;

// ---------------------------------------------------------------------------
// Remote URL
// ---------------------------------------------------------------------------

/// A parsed Flock remote URL.
///
/// Supported schemes:
/// - `flock://host[:port]/owner/repo`  → HTTP(S) transport
/// - `file:///absolute/path`           → local filesystem transport (testing)
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RemoteUrl {
    pub scheme: RemoteScheme,
    pub host: Option<String>,
    pub port: Option<u16>,
    pub path: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum RemoteScheme {
    Flock,
    File,
}

impl RemoteUrl {
    /// Parse a remote URL string.
    ///
    /// Examples:
    /// - `flock://example.com/acme/repo`
    /// - `flock://example.com:8443/acme/repo`
    /// - `file:///tmp/test-roost`
    pub fn parse(url: &str) -> Result<Self> {
        if let Some(rest) = url.strip_prefix("flock://") {
            let (host_port, path) = rest
                .split_once('/')
                .unwrap_or((rest, ""));
            if host_port.is_empty() {
                bail!("flock:// URL must include a host: {url}");
            }
            let (host, port) = if let Some((h, p)) = host_port.split_once(':') {
                let port: u16 = p.parse().map_err(|_| anyhow::anyhow!("invalid port in URL: {url}"))?;
                (h.to_string(), Some(port))
            } else {
                (host_port.to_string(), None)
            };
            let path = format!("/{path}");
            Ok(Self {
                scheme: RemoteScheme::Flock,
                host: Some(host),
                port,
                path,
            })
        } else if let Some(rest) = url.strip_prefix("file://") {
            if rest.is_empty() {
                bail!("file:// URL must include a path: {url}");
            }
            Ok(Self {
                scheme: RemoteScheme::File,
                host: None,
                port: None,
                path: rest.to_string(),
            })
        } else {
            bail!("unsupported remote URL scheme (expected flock:// or file://): {url}");
        }
    }

    /// WebSocket URL for the flock:// scheme.
    ///
    /// `flock://host:port/path` → `wss://host:port/path/ws` (port 443)
    /// `flock://host:8080/path` → `ws://host:8080/path/ws` (non-443 ports)
    pub fn ws_url(&self) -> Result<String> {
        match self.scheme {
            RemoteScheme::Flock => {
                let host = self.host.as_deref().unwrap_or("localhost");
                let port = self.port.unwrap_or(443);
                let scheme = if port == 443 { "wss" } else { "ws" };
                Ok(format!("{scheme}://{host}:{port}{}/ws", self.path))
            }
            RemoteScheme::File => bail!("file:// URLs do not have a WebSocket URL"),
        }
    }

    /// Base HTTP URL for the flock:// scheme.
    pub fn http_base_url(&self) -> Result<String> {
        match self.scheme {
            RemoteScheme::Flock => {
                let host = self.host.as_deref().unwrap_or("localhost");
                let port = self.port.unwrap_or(443);
                let scheme = if port == 443 { "https" } else { "http" };
                Ok(format!("{scheme}://{host}:{port}{}", self.path))
            }
            RemoteScheme::File => bail!("file:// URLs do not have an HTTP base URL"),
        }
    }
}

impl fmt::Display for RemoteUrl {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.scheme {
            RemoteScheme::Flock => {
                let host = self.host.as_deref().unwrap_or("localhost");
                if let Some(port) = self.port {
                    write!(f, "flock://{host}:{port}{}", self.path)
                } else {
                    write!(f, "flock://{host}{}", self.path)
                }
            }
            RemoteScheme::File => write!(f, "file://{}", self.path),
        }
    }
}

// ---------------------------------------------------------------------------
// Event push/pull
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EventPushRequest {
    pub events: Vec<Event>,
    pub refs: Vec<RepoRef>,
    /// The client's latest known event ID on the remote — allows the server
    /// to detect divergence.
    pub last_known_remote_event: Option<Uuid>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EventPushResponse {
    pub accepted: bool,
    /// Number of events accepted.
    pub events_accepted: usize,
    /// If rejected, the server's tip event ID (client must pull first).
    pub server_tip_event: Option<Uuid>,
    /// Human-readable detail on rejection.
    pub detail: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EventPullRequest {
    /// The client's latest event from this remote (pull events after this).
    pub last_known_event: Option<Uuid>,
    /// Optional branch filter.
    pub branch: Option<String>,
    /// Shallow clone: max number of checkpoint events to fetch.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub depth: Option<usize>,
    /// Sparse clone: only fetch blocks for files matching these glob patterns.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sparse_paths: Option<Vec<String>>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EventPullResponse {
    pub events: Vec<Event>,
    pub refs: Vec<RepoRef>,
    /// Block hashes referenced by pulled events that the client may need.
    pub referenced_block_hashes: Vec<String>,
}

// ---------------------------------------------------------------------------
// Block (content) negotiation
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BlockNeedRequest {
    /// Hashes the client wants to upload.
    pub block_hashes: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BlockNeedResponse {
    /// Hashes the server doesn't have yet.
    pub needed_hashes: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BlockUploadRequest {
    pub blocks: Vec<BlockPayload>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BlockPayload {
    pub hash: String,
    /// Base64-encoded block content.
    pub data_base64: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BlockUploadResponse {
    pub accepted: usize,
}

// ---------------------------------------------------------------------------
// Snapshot negotiation
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SnapshotNeedRequest {
    pub snapshot_ids: Vec<Uuid>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SnapshotNeedResponse {
    pub needed_ids: Vec<Uuid>,
}

// ---------------------------------------------------------------------------
// Authentication protocol
// ---------------------------------------------------------------------------

/// Request a login token using a bearer token (e.g. from a web-based OAuth flow
/// or a manually-generated API token).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TokenLoginRequest {
    pub token: String,
}

/// Response from the server after token-based login.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TokenLoginResponse {
    pub success: bool,
    /// Session token to store and use for subsequent requests.
    pub session_token: Option<String>,
    /// Human-readable error message on failure.
    pub error: Option<String>,
    /// Username or identity associated with the token.
    pub identity: Option<String>,
}

/// Step 1 of SSH key challenge-response: client sends its public key.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SshAuthChallengeRequest {
    /// Hex-encoded ed25519 public key.
    pub public_key_hex: String,
}

/// Step 1 response: server sends a nonce to sign.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SshAuthChallengeResponse {
    /// Random nonce (hex-encoded) for the client to sign.
    pub nonce_hex: String,
    /// Whether this public key is recognized by the server.
    pub key_recognized: bool,
}

/// Step 2 of SSH key challenge-response: client sends signed nonce.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SshAuthVerifyRequest {
    /// Hex-encoded ed25519 public key (same as challenge request).
    pub public_key_hex: String,
    /// Hex-encoded signature over the nonce.
    pub signature_hex: String,
    /// The nonce that was signed (for server to verify it's the one it issued).
    pub nonce_hex: String,
}

/// Step 2 response: server verifies signature and returns a session token.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SshAuthVerifyResponse {
    pub success: bool,
    /// Session token to store and use for subsequent requests.
    pub session_token: Option<String>,
    /// Human-readable error message on failure.
    pub error: Option<String>,
    /// Username or identity associated with the key.
    pub identity: Option<String>,
}

// ---------------------------------------------------------------------------
// Block fault (on-demand lazy fetch)
// ---------------------------------------------------------------------------

/// Request blocks that weren't fetched during initial clone (lazy mode).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BlockFaultRequest {
    pub block_hashes: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BlockFaultResponse {
    pub blocks: Vec<BlockPayload>,
}

// ---------------------------------------------------------------------------
// Push / Pull report types (returned by Repo methods)
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
pub struct PushReport {
    pub roost_name: String,
    pub events_pushed: usize,
    pub blocks_uploaded: usize,
    pub rejected: bool,
    pub detail: Option<String>,
}

#[derive(Debug, Clone)]
pub struct PullReport {
    pub roost_name: String,
    pub events_pulled: usize,
    pub blocks_downloaded: usize,
    pub refs_updated: Vec<String>,
    /// True if local had checkpoint(s) that the remote didn't (repos diverged).
    pub diverged: bool,
    /// True if the working directory was NOT updated to the remote HEAD.
    pub workspace_preserved: bool,
}

#[derive(Debug, Clone)]
pub struct CloneReport {
    pub pull: PullReport,
    pub clone_dir: String,
    pub depth: Option<usize>,
    pub sparse_patterns: Vec<String>,
    pub lazy: bool,
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_flock_url() {
        let url = RemoteUrl::parse("flock://example.com/acme/repo").unwrap();
        assert_eq!(url.scheme, RemoteScheme::Flock);
        assert_eq!(url.host.as_deref(), Some("example.com"));
        assert_eq!(url.port, None);
        assert_eq!(url.path, "/acme/repo");
        assert_eq!(url.to_string(), "flock://example.com/acme/repo");
    }

    #[test]
    fn parse_flock_url_with_port() {
        let url = RemoteUrl::parse("flock://example.com:8443/acme/repo").unwrap();
        assert_eq!(url.port, Some(8443));
        assert_eq!(url.to_string(), "flock://example.com:8443/acme/repo");
    }

    #[test]
    fn parse_file_url() {
        let url = RemoteUrl::parse("file:///tmp/test-roost").unwrap();
        assert_eq!(url.scheme, RemoteScheme::File);
        assert_eq!(url.host, None);
        assert_eq!(url.path, "/tmp/test-roost");
        assert_eq!(url.to_string(), "file:///tmp/test-roost");
    }

    #[test]
    fn parse_bad_scheme() {
        assert!(RemoteUrl::parse("https://example.com/repo").is_err());
    }

    #[test]
    fn parse_flock_missing_host() {
        assert!(RemoteUrl::parse("flock:///repo").is_err());
    }

    #[test]
    fn http_base_url() {
        let url = RemoteUrl::parse("flock://example.com/acme/repo").unwrap();
        assert_eq!(url.http_base_url().unwrap(), "https://example.com:443/acme/repo");

        let url = RemoteUrl::parse("flock://localhost:8080/test/repo").unwrap();
        assert_eq!(url.http_base_url().unwrap(), "http://localhost:8080/test/repo");
    }

    #[test]
    fn serde_round_trip_push_request() {
        let req = EventPushRequest {
            events: vec![],
            refs: vec![],
            last_known_remote_event: Some(Uuid::new_v4()),
        };
        let json = serde_json::to_string(&req).unwrap();
        let decoded: EventPushRequest = serde_json::from_str(&json).unwrap();
        assert_eq!(decoded.last_known_remote_event, req.last_known_remote_event);
    }

    #[test]
    fn serde_pull_request_with_depth_and_sparse() {
        let req = EventPullRequest {
            last_known_event: None,
            branch: Some("main".to_string()),
            depth: Some(10),
            sparse_paths: Some(vec!["src/**".to_string()]),
        };
        let json = serde_json::to_string(&req).unwrap();
        let decoded: EventPullRequest = serde_json::from_str(&json).unwrap();
        assert_eq!(decoded.depth, Some(10));
        assert_eq!(decoded.sparse_paths, Some(vec!["src/**".to_string()]));
    }

    #[test]
    fn serde_pull_request_backward_compat() {
        // Old format without depth/sparse should still deserialize
        let json = r#"{"last_known_event":null,"branch":null}"#;
        let decoded: EventPullRequest = serde_json::from_str(json).unwrap();
        assert_eq!(decoded.depth, None);
        assert_eq!(decoded.sparse_paths, None);
    }

    #[test]
    fn serde_block_fault_round_trip() {
        let req = BlockFaultRequest {
            block_hashes: vec!["abc123".to_string(), "def456".to_string()],
        };
        let json = serde_json::to_string(&req).unwrap();
        let decoded: BlockFaultRequest = serde_json::from_str(&json).unwrap();
        assert_eq!(decoded.block_hashes, req.block_hashes);

        let resp = BlockFaultResponse {
            blocks: vec![BlockPayload {
                hash: "abc123".to_string(),
                data_base64: "aGVsbG8=".to_string(),
            }],
        };
        let json = serde_json::to_string(&resp).unwrap();
        let decoded: BlockFaultResponse = serde_json::from_str(&json).unwrap();
        assert_eq!(decoded.blocks.len(), 1);
        assert_eq!(decoded.blocks[0].hash, "abc123");
    }

    #[test]
    fn serde_round_trip_pull_response() {
        let resp = EventPullResponse {
            events: vec![],
            refs: vec![],
            referenced_block_hashes: vec!["abc123".to_string()],
        };
        let json = serde_json::to_string(&resp).unwrap();
        let decoded: EventPullResponse = serde_json::from_str(&json).unwrap();
        assert_eq!(decoded.referenced_block_hashes, resp.referenced_block_hashes);
    }
}
