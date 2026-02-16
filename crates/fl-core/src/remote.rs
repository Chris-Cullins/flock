use std::fs;

use anyhow::{Context, Result, bail};
use fl_storage::{
    BlockNeedRequest, BlockNeedResponse, BlockUploadRequest, BlockUploadResponse,
    EventLog, EventPullRequest, EventPullResponse, EventPushRequest, EventPushResponse,
    RemoteScheme, RemoteUrl, SnapshotNeedRequest, SnapshotNeedResponse,
    SshAuthChallengeRequest, SshAuthChallengeResponse, SshAuthVerifyRequest, SshAuthVerifyResponse,
    TokenLoginRequest, TokenLoginResponse,
};
use fl_storage::refs::RefStore;
use uuid::Uuid;

// ---------------------------------------------------------------------------
// Transport trait
// ---------------------------------------------------------------------------

/// Trait for communicating with a roost (Flock remote server).
///
/// Two implementations are provided:
/// - `HttpTransport` — talks to a real roost server over HTTP(S)
/// - `LocalFsTransport` — reads/writes a second `.flock` directory on disk
pub trait RoostTransport {
    fn push_events(&self, req: &EventPushRequest) -> Result<EventPushResponse>;
    fn pull_events(&self, req: &EventPullRequest) -> Result<EventPullResponse>;
    fn check_blocks_needed(&self, req: &BlockNeedRequest) -> Result<BlockNeedResponse>;
    fn upload_blocks(&self, req: &BlockUploadRequest) -> Result<BlockUploadResponse>;
    fn download_block(&self, hash: &str) -> Result<Vec<u8>>;
    fn check_snapshots_needed(&self, req: &SnapshotNeedRequest) -> Result<SnapshotNeedResponse>;
    fn upload_snapshot(&self, snapshot_id: Uuid, data: &[u8]) -> Result<()>;
    fn download_snapshot(&self, snapshot_id: Uuid) -> Result<Vec<u8>>;

    // --- Authentication ---

    /// Authenticate with a bearer token and receive a session token.
    fn token_login(&self, req: &TokenLoginRequest) -> Result<TokenLoginResponse>;

    /// SSH key auth step 1: send public key, receive nonce challenge.
    fn ssh_auth_challenge(&self, req: &SshAuthChallengeRequest) -> Result<SshAuthChallengeResponse>;

    /// SSH key auth step 2: send signed nonce, receive session token.
    fn ssh_auth_verify(&self, req: &SshAuthVerifyRequest) -> Result<SshAuthVerifyResponse>;
}

/// Create the right transport for a parsed RemoteUrl.
pub fn transport_for_url(url: &RemoteUrl, token: Option<&str>) -> Result<Box<dyn RoostTransport>> {
    match url.scheme {
        RemoteScheme::Flock => {
            let base = url.http_base_url()?;
            Ok(Box::new(HttpTransport {
                base_url: base,
                token: token.map(String::from),
            }))
        }
        RemoteScheme::File => Ok(Box::new(LocalFsTransport {
            root: url.path.clone().into(),
        })),
    }
}

// ---------------------------------------------------------------------------
// HTTP transport (ureq)
// ---------------------------------------------------------------------------

pub struct HttpTransport {
    base_url: String,
    token: Option<String>,
}

impl HttpTransport {
    fn agent(&self) -> ureq::Agent {
        ureq::AgentBuilder::new().build()
    }

    fn auth_header(&self) -> Option<String> {
        self.token.as_ref().map(|t| format!("Bearer {t}"))
    }

    fn post(&self, path: &str, body: &impl serde::Serialize) -> Result<String> {
        let url = format!("{}{}", self.base_url, path);
        let mut req = self.agent().post(&url);
        if let Some(auth) = self.auth_header() {
            req = req.set("Authorization", &auth);
        }
        let json_val = serde_json::to_value(body)?;
        let resp = req
            .send_json(json_val)
            .context("HTTP request failed")?;
        let text = resp.into_string()?;
        Ok(text)
    }

    fn get(&self, path: &str) -> Result<Vec<u8>> {
        let url = format!("{}{}", self.base_url, path);
        let mut req = self.agent().get(&url);
        if let Some(auth) = self.auth_header() {
            req = req.set("Authorization", &auth);
        }
        let resp = req.call().context("HTTP GET failed")?;
        let mut buf = Vec::new();
        resp.into_reader().read_to_end(&mut buf)?;
        Ok(buf)
    }
}

impl RoostTransport for HttpTransport {
    fn push_events(&self, req: &EventPushRequest) -> Result<EventPushResponse> {
        let text = self.post("/api/v1/events/push", req)?;
        Ok(serde_json::from_str(&text)?)
    }

    fn pull_events(&self, req: &EventPullRequest) -> Result<EventPullResponse> {
        let text = self.post("/api/v1/events/pull", req)?;
        Ok(serde_json::from_str(&text)?)
    }

    fn check_blocks_needed(&self, req: &BlockNeedRequest) -> Result<BlockNeedResponse> {
        let text = self.post("/api/v1/blocks/need", req)?;
        Ok(serde_json::from_str(&text)?)
    }

    fn upload_blocks(&self, req: &BlockUploadRequest) -> Result<BlockUploadResponse> {
        let text = self.post("/api/v1/blocks/upload", req)?;
        Ok(serde_json::from_str(&text)?)
    }

    fn download_block(&self, hash: &str) -> Result<Vec<u8>> {
        let data = self.get(&format!("/api/v1/blocks/{hash}"))?;
        Ok(data)
    }

    fn check_snapshots_needed(&self, req: &SnapshotNeedRequest) -> Result<SnapshotNeedResponse> {
        let text = self.post("/api/v1/snapshots/need", req)?;
        Ok(serde_json::from_str(&text)?)
    }

    fn upload_snapshot(&self, snapshot_id: Uuid, data: &[u8]) -> Result<()> {
        let url = format!("{}/api/v1/snapshots/{snapshot_id}", self.base_url);
        let mut req = self.agent().put(&url).set("Content-Type", "application/octet-stream");
        if let Some(auth) = self.auth_header() {
            req = req.set("Authorization", &auth);
        }
        req.send_bytes(data).context("snapshot upload failed")?;
        Ok(())
    }

    fn download_snapshot(&self, snapshot_id: Uuid) -> Result<Vec<u8>> {
        self.get(&format!("/api/v1/snapshots/{snapshot_id}"))
    }

    fn token_login(&self, req: &TokenLoginRequest) -> Result<TokenLoginResponse> {
        let text = self.post("/api/v1/auth/token", req)?;
        Ok(serde_json::from_str(&text)?)
    }

    fn ssh_auth_challenge(&self, req: &SshAuthChallengeRequest) -> Result<SshAuthChallengeResponse> {
        let text = self.post("/api/v1/auth/ssh/challenge", req)?;
        Ok(serde_json::from_str(&text)?)
    }

    fn ssh_auth_verify(&self, req: &SshAuthVerifyRequest) -> Result<SshAuthVerifyResponse> {
        let text = self.post("/api/v1/auth/ssh/verify", req)?;
        Ok(serde_json::from_str(&text)?)
    }
}

// ---------------------------------------------------------------------------
// Local filesystem transport (for testing without a roost server)
// ---------------------------------------------------------------------------

/// Treats a second `.flock` directory on disk as a "remote". Useful for
/// testing push/pull without needing a running roost server.
pub struct LocalFsTransport {
    root: std::path::PathBuf,
}

impl LocalFsTransport {
    /// Ensure the target directory has the minimum `.flock` structure.
    fn ensure_structure(&self) -> Result<()> {
        let flock = self.root.join(".flock");
        fs::create_dir_all(flock.join("event-log"))?;
        fs::create_dir_all(flock.join("refs"))?;
        fs::create_dir_all(flock.join("snapshots"))?;
        fs::create_dir_all(flock.join("store/blocks"))?;
        Ok(())
    }

    fn event_log(&self) -> EventLog {
        EventLog::for_root(&self.root)
    }

    fn ref_store(&self) -> RefStore {
        RefStore::for_root(&self.root)
    }

    fn blocks_dir(&self) -> std::path::PathBuf {
        self.root.join(".flock/store/blocks")
    }

    fn snapshots_dir(&self) -> std::path::PathBuf {
        self.root.join(".flock/snapshots")
    }
}

impl RoostTransport for LocalFsTransport {
    fn push_events(&self, req: &EventPushRequest) -> Result<EventPushResponse> {
        self.ensure_structure()?;
        let event_log = self.event_log();
        event_log.ensure_exists()?;

        let existing = event_log.read_all()?;
        let server_tip = existing.last().map(|e| e.id);

        // Check that client's view of remote tip matches actual tip.
        if req.last_known_remote_event != server_tip {
            return Ok(EventPushResponse {
                accepted: false,
                events_accepted: 0,
                server_tip_event: server_tip,
                detail: Some("remote has diverged; pull first".to_string()),
            });
        }

        // Append events.
        event_log.append_batch(&req.events)?;

        // Update refs.
        let ref_store = self.ref_store();
        ref_store.ensure_exists()?;
        for r in &req.refs {
            ref_store.upsert(r.clone())?;
        }

        Ok(EventPushResponse {
            accepted: true,
            events_accepted: req.events.len(),
            server_tip_event: req.events.last().map(|e| e.id).or(server_tip),
            detail: None,
        })
    }

    fn pull_events(&self, req: &EventPullRequest) -> Result<EventPullResponse> {
        self.ensure_structure()?;
        let event_log = self.event_log();
        event_log.ensure_exists()?;

        let all_events = event_log.read_all()?;

        // Find events after last_known_event.
        let events = if let Some(last_known) = req.last_known_event {
            let pos = all_events.iter().position(|e| e.id == last_known);
            match pos {
                Some(idx) => all_events[idx + 1..].to_vec(),
                None => all_events, // Client doesn't know any — send all.
            }
        } else {
            all_events
        };

        let ref_store = self.ref_store();
        ref_store.ensure_exists()?;
        let refs = ref_store.read_all()?;

        // Collect block hashes referenced by checkpoint events.
        let block_hashes = Vec::new(); // LocalFs doesn't track blocks granularly

        Ok(EventPullResponse {
            events,
            refs,
            referenced_block_hashes: block_hashes,
        })
    }

    fn check_blocks_needed(&self, req: &BlockNeedRequest) -> Result<BlockNeedResponse> {
        self.ensure_structure()?;
        let blocks_dir = self.blocks_dir();
        let needed: Vec<String> = req
            .block_hashes
            .iter()
            .filter(|h| {
                let prefix = &h[..2];
                let rest = &h[2..];
                !blocks_dir.join(prefix).join(rest).exists()
            })
            .cloned()
            .collect();
        Ok(BlockNeedResponse {
            needed_hashes: needed,
        })
    }

    fn upload_blocks(&self, req: &BlockUploadRequest) -> Result<BlockUploadResponse> {
        self.ensure_structure()?;
        let blocks_dir = self.blocks_dir();
        let mut count = 0;
        for block in &req.blocks {
            let prefix = &block.hash[..2];
            let rest = &block.hash[2..];
            let dir = blocks_dir.join(prefix);
            fs::create_dir_all(&dir)?;
            let data = base64_decode(&block.data_base64)?;
            fs::write(dir.join(rest), &data)?;
            count += 1;
        }
        Ok(BlockUploadResponse { accepted: count })
    }

    fn download_block(&self, hash: &str) -> Result<Vec<u8>> {
        let prefix = &hash[..2];
        let rest = &hash[2..];
        let path = self.blocks_dir().join(prefix).join(rest);
        fs::read(&path).with_context(|| format!("block {} not found on remote", hash))
    }

    fn check_snapshots_needed(&self, req: &SnapshotNeedRequest) -> Result<SnapshotNeedResponse> {
        self.ensure_structure()?;
        let snap_dir = self.snapshots_dir();
        let needed: Vec<Uuid> = req
            .snapshot_ids
            .iter()
            .filter(|id| !snap_dir.join(id.to_string()).exists())
            .copied()
            .collect();
        Ok(SnapshotNeedResponse { needed_ids: needed })
    }

    fn upload_snapshot(&self, snapshot_id: Uuid, data: &[u8]) -> Result<()> {
        self.ensure_structure()?;
        let path = self.snapshots_dir().join(snapshot_id.to_string());
        fs::create_dir_all(&path)?;
        // Data is a tar archive — extract it.
        let cursor = std::io::Cursor::new(data);
        let mut archive = tar::Archive::new(cursor);
        archive.unpack(&path)?;
        Ok(())
    }

    fn download_snapshot(&self, snapshot_id: Uuid) -> Result<Vec<u8>> {
        let path = self.snapshots_dir().join(snapshot_id.to_string());
        if !path.is_dir() {
            bail!("snapshot {} not found on remote", snapshot_id);
        }
        // Pack directory as tar.
        let mut buf = Vec::new();
        {
            let mut builder = tar::Builder::new(&mut buf);
            builder.append_dir_all(".", &path)?;
            builder.finish()?;
        }
        Ok(buf)
    }

    fn token_login(&self, _req: &TokenLoginRequest) -> Result<TokenLoginResponse> {
        // Local filesystem transport doesn't require authentication.
        Ok(TokenLoginResponse {
            success: true,
            session_token: Some("local-fs-no-auth".to_string()),
            error: None,
            identity: Some("local".to_string()),
        })
    }

    fn ssh_auth_challenge(&self, _req: &SshAuthChallengeRequest) -> Result<SshAuthChallengeResponse> {
        Ok(SshAuthChallengeResponse {
            nonce_hex: "00".repeat(32),
            key_recognized: true,
        })
    }

    fn ssh_auth_verify(&self, _req: &SshAuthVerifyRequest) -> Result<SshAuthVerifyResponse> {
        Ok(SshAuthVerifyResponse {
            success: true,
            session_token: Some("local-fs-no-auth".to_string()),
            error: None,
            identity: Some("local".to_string()),
        })
    }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn base64_decode(s: &str) -> Result<Vec<u8>> {
    // Simple base64 decode without pulling in a base64 crate — we use a
    // minimal implementation since blocks are also stored by hash.
    // For now, use a straightforward lookup table approach.
    base64_impl::decode(s)
}

pub fn base64_encode(data: &[u8]) -> String {
    base64_impl::encode(data)
}

mod base64_impl {
    use anyhow::{Result, bail};

    const CHARS: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";

    pub fn encode(data: &[u8]) -> String {
        let mut result = String::with_capacity((data.len() + 2) / 3 * 4);
        for chunk in data.chunks(3) {
            let b0 = chunk[0] as u32;
            let b1 = if chunk.len() > 1 { chunk[1] as u32 } else { 0 };
            let b2 = if chunk.len() > 2 { chunk[2] as u32 } else { 0 };
            let n = (b0 << 16) | (b1 << 8) | b2;
            result.push(CHARS[((n >> 18) & 0x3f) as usize] as char);
            result.push(CHARS[((n >> 12) & 0x3f) as usize] as char);
            if chunk.len() > 1 {
                result.push(CHARS[((n >> 6) & 0x3f) as usize] as char);
            } else {
                result.push('=');
            }
            if chunk.len() > 2 {
                result.push(CHARS[(n & 0x3f) as usize] as char);
            } else {
                result.push('=');
            }
        }
        result
    }

    pub fn decode(s: &str) -> Result<Vec<u8>> {
        let s = s.trim_end_matches('=');
        let mut result = Vec::with_capacity(s.len() * 3 / 4);
        let mut buf: u32 = 0;
        let mut bits: u32 = 0;
        for c in s.bytes() {
            let val = match c {
                b'A'..=b'Z' => c - b'A',
                b'a'..=b'z' => c - b'a' + 26,
                b'0'..=b'9' => c - b'0' + 52,
                b'+' => 62,
                b'/' => 63,
                b'\n' | b'\r' | b' ' => continue,
                _ => bail!("invalid base64 character: {}", c as char),
            };
            buf = (buf << 6) | val as u32;
            bits += 6;
            if bits >= 8 {
                bits -= 8;
                result.push((buf >> bits) as u8);
                buf &= (1 << bits) - 1;
            }
        }
        Ok(result)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn base64_round_trip() {
        let original = b"hello, world! This is a test of base64 encoding.";
        let encoded = base64_encode(original);
        let decoded = base64_decode(&encoded).unwrap();
        assert_eq!(decoded, original);
    }

    #[test]
    fn base64_empty() {
        assert_eq!(base64_encode(b""), "");
        assert_eq!(base64_decode("").unwrap(), b"");
    }

    #[test]
    fn transport_for_file_url() {
        let url = RemoteUrl::parse("file:///tmp/test-roost").unwrap();
        // Should not error — just creates LocalFsTransport
        let _transport = transport_for_url(&url, None).unwrap();
    }

    #[test]
    fn transport_for_flock_url() {
        let url = RemoteUrl::parse("flock://example.com/acme/repo").unwrap();
        let _transport = transport_for_url(&url, Some("test-token")).unwrap();
    }
}
