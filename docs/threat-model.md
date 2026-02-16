# Flock Threat Model

This document analyzes security threats to Flock repositories and the mitigations in place to address them.

## Overview

Flock is designed for environments where AI agents are first-class participants in version control workflows. This introduces unique threat vectors beyond traditional human-operated VCS systems. The security architecture relies on cryptographic primitives, event sourcing immutability, and human-in-the-loop quality gates.

### Core Security Properties

- **Event Integrity**: Ed25519 signatures and BLAKE3 hash chains ensure events cannot be tampered with undetectably
- **Snapshot Verification**: Merkle roots provide cryptographic proof of snapshot integrity
- **Key Protection**: AES-256-GCM encryption with Argon2 key derivation protects signing keys at rest
- **Auditability**: Append-only event log enables complete history reconstruction and audit trails
- **Causal Consistency**: Parent-child event chains are validated to prevent history rewriting
- **Quality Gates**: Human approval required for high-risk operations

## Threat Actors

### Compromised AI Agent

**Description**: An AI agent's credentials or execution environment is compromised by an attacker.

**Capabilities**:
- Read repository contents and event history
- Create new events (checkpoints, explorations)
- Access file snapshots
- Submit changes for approval

**Limitations**:
- Cannot forge events without the signing key
- Cannot bypass quality gates that require human approval
- Cannot modify historical events due to hash chain
- Cannot access encrypted signing keys without passphrase

**Impact**: Medium to High - Attacker can inject malicious code changes but must pass review gates.

### Malicious Agent

**Description**: An intentionally malicious agent with legitimate access to the repository.

**Capabilities**:
- All capabilities of a legitimate agent
- Deliberately inject backdoors, vulnerabilities, or data exfiltration code
- Create plausible-looking commits to evade detection
- Exploit semantic merge logic to hide changes

**Limitations**:
- Subject to same cryptographic constraints as compromised agent
- Human review gates can catch suspicious changes
- Audit trail records all actions for forensic analysis

**Impact**: High - Sophisticated attacks may bypass automated detection.

### Insider Human Operator

**Description**: Legitimate human user with repository access who becomes malicious.

**Capabilities**:
- Full repository access including signing keys
- Can approve their own changes if they control quality gates
- Can extract and exfiltrate signing keys
- Can create backups with full history

**Limitations**:
- Actions are signed and logged in event history
- Cannot modify past events without detection
- Key theft is evident if keys are re-encrypted

**Impact**: Critical - Full repository compromise possible.

### External Attacker

**Description**: Unauthorized party attempting to compromise the repository.

**Capabilities**:
- Network-level attacks on remote sync
- Filesystem access if host is compromised
- Supply chain attacks via dependencies
- Social engineering against users

**Limitations**:
- No signing key access without key theft
- Cannot forge signatures
- Tampered events fail validation
- Need physical or network access to repository

**Impact**: Low to Critical depending on access level achieved.

## Attack Surfaces

### Event Log Tampering

**Threat**: Attacker modifies the append-only event log to alter history, remove evidence, or inject malicious events.

**Attack Vectors**:
- Direct filesystem modification of `event-log/events.jsonl`
- Truncation to remove recent events
- Insertion of forged events
- Reordering of events to break causal chain

**Mitigations**:
- **BLAKE3 Hash Chain**: Each event includes hash of previous event, making tampering evident
- **Ed25519 Signatures**: Events are cryptographically signed; forgery requires private key
- **Merkle Roots**: Snapshot integrity verified via merkle tree roots in checkpoint events
- **Causal Validation**: Event replay validates parent-child relationships
- **Audit Trail**: `fl audit-trail` command detects anomalies, signature failures, chain breaks

**Residual Risk**: Attacker with signing key can append valid-looking malicious events. Mitigation requires human review and anomaly detection.

### Signing Key Theft

**Threat**: Attacker steals the Ed25519 private key to forge events or impersonate agents.

**Attack Vectors**:
- Filesystem access to `.flock/keys/ed25519.sk`
- Memory dump of running process holding decrypted key
- Brute force attack on key encryption passphrase
- Keylogger capturing passphrase during unlock

**Mitigations**:
- **AES-256-GCM Encryption**: Keys encrypted at rest with strong cipher
- **Argon2 Key Derivation**: Passphrase converted to encryption key via memory-hard KDF (resists brute force)
- **Filesystem Permissions**: Key files should be readable only by owner (mode 600)
- **Short-lived Key Unlocking**: Keys decrypted only when needed for signing

**Residual Risk**: Passphrase compromise or memory access during key usage. Recommend hardware security modules (HSM) or key management services for high-security environments.

### Snapshot Corruption

**Threat**: File snapshots under `.flock/snapshots/<uuid>/` are corrupted or replaced with malicious versions.

**Attack Vectors**:
- Direct filesystem modification of snapshot files
- Bit rot or storage media failure
- Malicious replacement of snapshot directory

**Mitigations**:
- **Merkle Roots**: Checkpoint events store cryptographic merkle root of snapshot tree
- **Snapshot Validation**: `fl fsck` verifies merkle root matches actual snapshot contents
- **Content-Addressable Storage**: Snapshots named by UUID prevent collisions
- **Backup Integrity**: `fl backup` creates tar.gz archives with manifest for validation

**Residual Risk**: Silent corruption between checkpoints. Recommend periodic `fl fsck` runs and backup verification.

### Unauthorized Repository Access

**Threat**: Unauthorized users or agents gain read or write access to repository.

**Attack Vectors**:
- Filesystem permission misconfiguration
- Shared credentials among agents
- Network access to remote sync endpoint without authentication
- Container escape or privilege escalation

**Mitigations**:
- **Filesystem ACLs**: Repository directories should restrict access to authorized users
- **Advisory Locks**: `fl lock` prevents concurrent modifications (cooperative, not enforced)
- **Authentication on Remote Sync**: Client-side auth tokens required for push/pull (Section 10b)
- **Per-Agent Keys**: Each agent should have unique signing key for attribution

**Residual Risk**: Filesystem-level access control is OS-dependent. Recommend mandatory access control (MAC) systems like SELinux or AppArmor for high-security deployments.

### Dependency Supply Chain

**Threat**: Malicious code introduced via compromised Rust dependencies (crates).

**Attack Vectors**:
- Compromised crates.io package
- Typosquatting on dependency names
- Malicious code in transitive dependencies
- Build script attacks during compilation

**Mitigations**:
- **Cargo.lock Pinning**: Dependency versions locked for reproducible builds
- **Minimal Dependencies**: Flock uses few external crates, reducing attack surface
- **No Async Runtime**: Synchronous design avoids tokio/async-std complexity
- **Code Review**: Dependencies reviewed during updates

**Residual Risk**: Zero-day exploits in dependencies. Recommend dependency scanning tools (cargo-audit, cargo-deny) and staying current with security advisories.

### Semantic Merge Exploitation

**Threat**: Attacker crafts malicious code that appears safe to semantic merge but introduces vulnerabilities.

**Attack Vectors**:
- Exploit parser blind spots in tree-sitter grammars
- Hide malicious logic in style-only changes
- Craft changes that merge cleanly but break semantics
- Unicode homoglyph attacks in identifiers

**Mitigations**:
- **Conflict Classification**: Semantic merge identifies divergent edits, signature changes, concurrent additions
- **Risk Levels**: Changes tagged as Low/Medium/High risk based on impact
- **Quality Gates**: High-risk merges require human approval
- **Fallback to Text Diff**: Unsupported languages use conservative text-based merge

**Residual Risk**: Semantic analysis is best-effort. Sophisticated attacks may evade detection. Recommend code review for all agent-generated changes.

## Agent-Specific Threats

### Excessive Permissions

**Threat**: Agents granted broader repository access than needed, increasing blast radius of compromise.

**Example**: Agent with read-only needs has write access and can create malicious checkpoints.

**Mitigation**:
- Principle of least privilege: Grant minimal necessary permissions
- Role-based access control (RBAC) for multi-agent environments
- Separate signing keys per agent for attribution
- Quality gates that require human approval for sensitive operations

### Prompt Injection

**Threat**: Attacker manipulates agent via crafted prompts in commit messages, file contents, or metadata.

**Example**: Commit message contains instructions that override agent's safety guidelines.

**Mitigation**:
- Agent instruction hygiene: Separate user input from system prompts
- Content filtering on event metadata
- Human review of agent-generated events via quality gates
- Audit trail analysis for anomalous patterns

### Agent Collusion

**Threat**: Multiple compromised agents coordinate to bypass review gates or hide malicious activity.

**Example**: Agent A creates malicious checkpoint, Agent B approves it in quality gate.

**Mitigation**:
- Require human approval for high-risk operations
- Audit trail correlation across multiple agent actions
- Anomaly detection for unusual approval patterns
- Segregation of duties: approval authority separate from authorship

## Recommended External Controls

Beyond Flock's built-in security features, organizations should implement:

### Infrastructure Security

- **Filesystem Encryption**: Encrypt `.flock` directory at rest (e.g., LUKS, FileVault)
- **Backup Encryption**: Encrypt backup archives with separate key from repository
- **Network Security**: TLS for remote sync, mutual TLS for agent authentication
- **Access Logging**: OS-level audit logs for filesystem access to `.flock/`

### Operational Security

- **Key Rotation**: Periodic rotation of Ed25519 signing keys
- **Passphrase Policy**: Strong passphrases for key encryption, consider hardware tokens
- **Incident Response**: Runbook for compromised keys, corrupted repos (see disaster-recovery.md)
- **Monitoring**: Automated alerts on audit trail anomalies, signature failures, fsck errors

### Agent Security

- **Sandboxing**: Run agents in containers or VMs with limited filesystem access
- **Rate Limiting**: Throttle agent operations to detect and limit runaway behavior
- **Behavioral Analysis**: Monitor agent actions for deviations from baseline
- **Kill Switch**: Ability to revoke agent access instantly if compromise detected

### Code Review

- **Mandatory Human Review**: All agent-generated code reviewed before merge
- **Pair Review**: High-risk changes reviewed by multiple humans
- **Automated Scanning**: SAST/DAST tools on agent commits
- **Canary Deployments**: Test agent changes in isolated environments first

## Limitations and Future Work

### Current Limitations

- **No Access Control in Flock**: Filesystem permissions only; no built-in user/role management
- **Single-Machine Focus**: Remote sync is experimental; no distributed consensus
- **Cooperative Locking**: Advisory locks can be ignored by malicious actors
- **Key Management**: Manual key handling; no integration with HSM or KMS

### Planned Security Enhancements

- **Multi-signature Events**: Require approval from N-of-M signers for critical operations
- **Immutable Event Log**: Append-only storage backend (e.g., S3 object lock, WORM media)
- **Hardware Key Support**: Integration with Yubikey, TPM for signing key protection
- **Anomaly Detection**: ML-based detection of unusual agent behavior patterns
- **Zero-Knowledge Proofs**: Prove properties of snapshots without revealing contents

## Conclusion

Flock's security model prioritizes **detectability over prevention**. The append-only event log with cryptographic integrity ensures that malicious actions leave indelible evidence. Combined with human-in-the-loop quality gates and comprehensive audit trails, Flock provides a strong foundation for secure AI-human collaboration on code.

However, security is a shared responsibility. Organizations must implement appropriate external controls around key management, infrastructure security, and agent oversight to fully realize Flock's security benefits.

For disaster recovery procedures related to security incidents, see `docs/disaster-recovery.md`.
