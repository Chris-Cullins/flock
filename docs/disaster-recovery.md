# Flock Disaster Recovery Playbook

This document provides step-by-step procedures for recovering from common disaster scenarios in Flock repositories.

## Overview

Flock's event-sourcing architecture and append-only event log make it resilient to many failure modes. However, disasters can still occur due to hardware failure, human error, security incidents, or software bugs. This playbook helps you recover quickly and minimize data loss.

### General Recovery Principles

1. **Stop the bleeding**: Immediately halt operations that may worsen the situation
2. **Assess damage**: Use `fl fsck` and `fl audit-trail` to understand the scope
3. **Restore from backup**: When possible, restore from known-good backup before attempting repair
4. **Verify integrity**: After recovery, validate repository consistency with `fl fsck`
5. **Document incident**: Record what happened, root cause, and recovery steps for future reference

### Before Disaster Strikes

- Run regular backups with `fl backup`
- Test backup restoration periodically
- Keep backups in multiple locations (local, remote, offsite)
- Maintain separate encryption keys for backups
- Document your backup schedule and retention policy

## Scenario 1: Corrupted Event Log

### Symptoms

- `fl` commands fail with "event log corrupted" or "hash chain validation failed"
- `fl fsck` reports broken event chain or signature verification failures
- `event-log/events.jsonl` contains malformed JSON or truncated lines

### Diagnosis

```bash
# Check event log integrity
fl fsck --verbose

# Examine audit trail for anomalies
fl audit-trail --all
```

### Recovery Steps

#### Option A: Restore from Backup (Recommended)

```bash
# 1. Stop all operations on the corrupted repository
# Move aside the corrupted .flock directory
mv .flock .flock.corrupted-$(date +%Y%m%d-%H%M%S)

# 2. Restore from most recent backup
fl restore /path/to/backup-YYYYMMDD-HHMMSS.tar.gz

# 3. Verify restoration
fl fsck
fl log --limit 10

# 4. If backup is recent enough, you're done
# If there's a gap, see "Manual Recovery" below
```

#### Option B: Manual Event Log Repair

Use this when you don't have a recent backup or only the tail of the log is corrupted.

```bash
# 1. Create backup of corrupted state
cp -r .flock .flock.corrupted-$(date +%Y%m%d-%H%M%S)

# 2. Find the last valid event
# Open event-log/events.jsonl in a text editor
# Manually identify the last line that:
#   - Is valid JSON
#   - Has correct signature
#   - Has parent_id matching previous event's id

# 3. Truncate the log after the last valid event
# If last valid event is line 150:
head -n 150 .flock/event-log/events.jsonl > .flock/event-log/events.jsonl.repaired
mv .flock/event-log/events.jsonl.repaired .flock/event-log/events.jsonl

# 4. Reset refs to point to valid events
# Edit .flock/refs/refs.json
# Ensure all event_id values point to events that exist in the truncated log

# 5. Verify repair
fl fsck
fl log

# 6. Recreate lost work if needed
# Check .flock.corrupted-*/snapshots/ for recent snapshots
# Manually re-checkpoint lost changes
```

#### Option C: Rebuild from Snapshots

If the event log is irrecoverable but snapshots are intact:

```bash
# 1. Initialize fresh repository
mv .flock .flock.corrupted
fl init

# 2. Copy over snapshot data
cp -r .flock.corrupted/snapshots/* .flock/snapshots/

# 3. Create new checkpoint from working directory state
fl checkpoint -m "Rebuilt from snapshots after corruption"

# 4. Manually document the history loss
echo "Repository rebuilt on $(date) due to event log corruption" > RECOVERY-NOTES.txt
fl checkpoint -m "Add recovery notes"
```

### Prevention

- Run `fl backup` on a schedule (daily for active repos, weekly for stable repos)
- Enable filesystem-level backups (e.g., Time Machine, ZFS snapshots)
- Use journaling filesystems (ext4, APFS) to reduce corruption risk
- Avoid force-killing `fl` processes during writes

## Scenario 2: Lost Signing Key

### Symptoms

- `.flock/keys/ed25519.sk` is missing or corrupted
- Cannot create new checkpoints: "signing key not found"
- Forgot passphrase for encrypted signing key

### Diagnosis

```bash
# Check if key file exists
ls -la .flock/keys/

# Try to use key (will prompt for passphrase)
fl checkpoint -m "test"
```

### Recovery Steps

#### If Key is Lost but Repository Intact

```bash
# 1. Generate new signing key
fl init-key --force

# 2. Re-sign existing events with new key (if needed for continuity)
# This is an advanced operation; see "Manual Re-signing" below

# 3. Update remote sync configuration if applicable
# New key requires re-registration with remote server
```

#### If Passphrase is Forgotten

```bash
# Option A: If you have backup of unencrypted key
# Restore from backup taken before encryption
cp /path/to/backup/.flock/keys/ed25519.sk .flock/keys/

# Option B: If no backup exists, generate new key
fl init-key --force

# WARNING: Cannot re-sign old events without original passphrase
# Historical events will have old signature, new events will use new key
# This is detectable in audit trail
```

#### Manual Re-signing (Advanced)

Use this to maintain signature continuity after key recovery.

```bash
# This functionality is not yet implemented in Flock
# Planned for future release (Section 11: Multi-signature)

# Workaround: Keep detailed record of key rotation
echo "Key rotated on $(date): old=<old-key-id> new=<new-key-id>" >> .flock/key-rotation-log.txt
fl checkpoint -m "Record key rotation in repository"
```

### Prevention

- **Backup keys separately**: Store copy of `.flock/keys/` in secure location (encrypted USB, password manager)
- **Passphrase recovery**: Document passphrase in secure vault (1Password, Bitwarden)
- **Multi-key setup**: Generate backup keys for emergency use (requires multi-signature support, planned feature)
- **Key rotation policy**: Rotate keys periodically and test recovery process

## Scenario 3: Committed Secrets

### Symptoms

- API key, password, private key, or other secret accidentally committed to repository
- Secret is in snapshot history, not just current working directory
- Secret may have been synced to remote server

### Diagnosis

```bash
# Search for potential secrets in current state
fl diff --checkpoint HEAD

# Check if secret is in historical snapshots
fl log --all
fl diff --checkpoint <old-checkpoint-id>

# If using git bridge:
git log -p | grep -i "password\|api.?key\|secret"
```

### Recovery Steps

#### Step 1: Immediate Mitigation

```bash
# 1. Rotate the compromised secret immediately
# - Generate new API key/password
# - Revoke old credential at the service provider
# - Update your application configuration

# 2. Remove secret from working directory
# Edit the file to remove the secret
vi path/to/file-with-secret.conf

# 3. Create checkpoint documenting the removal
fl checkpoint -m "Remove committed secret (rotated)"
```

#### Step 2: Scrub Historical References

Flock does not currently support history rewriting (by design - immutability is a security feature). However, you can:

```bash
# Option A: If secret is in recent checkpoints only
# Create new repository branch without the secret
fl branch clean-history
# Manually reconstruct work without secret-containing checkpoints

# Option B: If repository is not yet shared
# Reinitialize and selectively restore clean checkpoints
mv .flock .flock.with-secret
fl init
# Manually copy over files from clean checkpoints in .flock.with-secret/snapshots/

# Option C: Accept the secret is in history, rely on rotation
# If the secret has been rotated and repository access is restricted,
# the risk is mitigated. Document the incident:
echo "Secret committed in checkpoint <id> on $(date), rotated immediately" > SECURITY-INCIDENT.md
fl checkpoint -m "Document security incident and resolution"
```

#### Step 3: Prevention for Future

```bash
# Add secret patterns to .flockignore (similar to .gitignore)
# Flock will warn before checkpointing matched files

cat >> .flockignore <<EOF
# Secrets and credentials
.env
.env.*
**/config/secrets.yml
**/*.pem
**/*.key
**/credentials.json
**/*_rsa
**/*_dsa
**/*_ed25519
EOF

fl checkpoint -m "Add secret patterns to .flockignore"
```

### Prevention

- **Pre-checkpoint hooks**: Add validation that scans for secret patterns before checkpoint
- **Environment variables**: Store secrets in environment, not in repository files
- **Secret management tools**: Use Vault, AWS Secrets Manager, or similar for production secrets
- **.flockignore patterns**: Maintain comprehensive ignore list for secret file patterns
- **Agent instructions**: Configure AI agents to refuse checkpointing files with secret patterns

## Scenario 4: Merge Conflict Resolution

### Symptoms

- `fl merge` reports conflicts that cannot be auto-resolved
- Semantic merge produced `TextFallback` conflicts requiring manual intervention
- Multiple agents made divergent changes to same code region

### Diagnosis

```bash
# View merge conflict details
fl merge branch-name --dry-run

# Examine the specific conflicts
fl diff --checkpoint <base> --checkpoint <branch>

# Check semantic analysis results
fl semantic-diff <base>..<branch>
```

### Recovery Steps

#### Step 1: Understand the Conflict

```bash
# Get the two conflicting checkpoints
fl log --branch main --limit 1
fl log --branch feature-branch --limit 1

# View the divergent changes
fl diff --checkpoint <main-checkpoint> --checkpoint <feature-checkpoint>
```

#### Step 2: Manual Merge Resolution

```bash
# Option A: Accept one side entirely
fl merge branch-name --strategy ours    # Keep current branch
fl merge branch-name --strategy theirs  # Take incoming branch

# Option B: Manual three-way merge
# 1. Create exploration to experiment with merge
fl explore "manual merge of branch-name"

# 2. Manually edit conflicted files in working directory
vi path/to/conflicted-file.rs

# 3. Test the merged result
cargo test  # or appropriate test command

# 4. Checkpoint the resolution
fl checkpoint -m "Manually resolve merge conflicts from branch-name"

# 5. Complete the merge
fl merge branch-name --continue
```

#### Step 3: Semantic Conflict Resolution

For conflicts involving function signature changes:

```bash
# 1. Check signature compatibility
fl semantic-diff <base>..<branch> --format json > conflicts.json

# 2. Identify breaking changes
cat conflicts.json | jq '.changes[] | select(.signature_compatible == false)'

# 3. Update call sites to match new signature
# Edit files to fix signature mismatches

# 4. Re-run semantic merge
fl merge branch-name
```

### Prevention

- **Frequent syncing**: Merge branches regularly to avoid large divergences
- **Communication**: Coordinate with other agents on overlapping work areas
- **Exploration mode**: Use `fl explore` for risky changes before checkpointing
- **Semantic hints**: Add comments or annotations to guide semantic merge engine

## Scenario 5: Total Filesystem Loss

### Symptoms

- Disk failure, ransomware, or accidental deletion of entire working directory
- `.flock` directory and all repository files are gone
- Working directory state is completely lost

### Diagnosis

```bash
# Check if repository still exists
ls -la .flock  # "No such file or directory"

# Check backup availability
ls ~/flock-backups/  # or your backup location
```

### Recovery Steps

#### Step 1: Restore from Remote (If Available)

```bash
# If you have remote sync configured (Section 10)
fl clone <remote-url>
cd <repo-name>

# Verify restoration
fl fsck
fl log
```

#### Step 2: Restore from Local Backup

```bash
# 1. Locate most recent backup
ls -ltr ~/flock-backups/*.tar.gz | tail -1

# 2. Extract to new location
mkdir -p ~/restored-repos/my-project
cd ~/restored-repos/my-project

# 3. Restore backup
fl restore ~/flock-backups/backup-20260215-143022.tar.gz

# 4. Verify restoration
fl fsck
fl log
fl diff --checkpoint HEAD  # Check working directory state
```

#### Step 3: Assess Data Loss

```bash
# Compare backup timestamp to loss event
ls -l ~/flock-backups/backup-20260215-143022.tar.gz
# "Feb 15 14:30"  <- backup time
# Loss occurred: Feb 16 09:00
# Gap: ~19 hours of work potentially lost

# Check for any other data sources
# - CI/CD logs with checkpoint hashes
# - Colleague's cloned copies
# - Cloud storage sync (Dropbox, Google Drive)
```

#### Step 4: Reconstruct Lost Work

```bash
# If you have partial information about lost work:

# 1. Create exploration to reconstruct
fl explore "reconstruct work from Feb 15-16"

# 2. Manually recreate the changes
# Use notes, memory, or external logs to guide reconstruction

# 3. Checkpoint reconstructed work
fl checkpoint -m "Reconstructed work from Feb 15-16 after data loss"

# 4. Document the loss
echo "Data loss incident on 2026-02-16, restored from backup, ~19 hours reconstructed" > RECOVERY-LOG.md
fl checkpoint -m "Document data loss and recovery"
```

### Prevention

#### Backup Strategy

Implement the **3-2-1 backup rule**:

- **3 copies**: Original + 2 backups
- **2 media types**: Local disk + cloud/external drive
- **1 offsite**: Remote location for disaster resilience

#### Automated Backup Schedule

```bash
# Daily local backup (cron job or systemd timer)
# Add to crontab: crontab -e
0 2 * * * cd /path/to/repo && fl backup --output ~/flock-backups/backup-$(date +\%Y\%m\%d-\%H\%M\%S).tar.gz

# Weekly remote sync (if using Roost or similar)
0 3 * * 0 cd /path/to/repo && fl push --remote production

# Monthly verification test
0 4 1 * * /home/user/scripts/test-flock-backup-restore.sh
```

#### Backup Retention Policy

```bash
# Keep backups for:
# - Daily backups: 7 days
# - Weekly backups: 4 weeks
# - Monthly backups: 12 months
# - Yearly backups: 3 years

# Automated cleanup script
#!/bin/bash
# cleanup-old-backups.sh
BACKUP_DIR=~/flock-backups

# Delete daily backups older than 7 days
find $BACKUP_DIR -name "backup-*.tar.gz" -mtime +7 -delete

# Archive weekly backups (every Sunday) to long-term storage
# (Implementation left as exercise)
```

#### Remote Sync Setup

```bash
# Configure remote repository (when Section 10 is complete)
fl remote add origin https://roost.example.com/repos/my-project
fl remote set-auth origin --token <auth-token>

# Push after each significant checkpoint
fl checkpoint -m "Implement feature X"
fl push origin

# Automate push after checkpoint (pre-checkpoint hook, future feature)
```

## Scenario 6: Corrupted Snapshot Files

### Symptoms

- `fl fsck` reports merkle root mismatch for snapshots
- Files in `.flock/snapshots/<uuid>/` are corrupted or missing
- Cannot restore working directory from checkpoint

### Diagnosis

```bash
# Run full integrity check
fl fsck --verbose

# Identify which snapshots are affected
fl log --all
# Note checkpoint IDs with snapshots

# Manually check snapshot directories
ls -la .flock/snapshots/
```

### Recovery Steps

```bash
# 1. Restore from backup
fl restore /path/to/backup.tar.gz

# 2. If only specific snapshots are corrupt, restore selectively
# Extract backup to temporary location
mkdir /tmp/backup-extract
tar -xzf /path/to/backup.tar.gz -C /tmp/backup-extract

# Copy specific snapshot directories
cp -r /tmp/backup-extract/.flock/snapshots/<corrupt-snapshot-id> .flock/snapshots/

# 3. Verify fix
fl fsck

# 4. If snapshots are unrecoverable, recreate checkpoint from current state
fl checkpoint -m "Recreate checkpoint after snapshot corruption"
```

### Prevention

- Run `fl fsck` regularly (weekly)
- Use filesystem with checksumming (ZFS, Btrfs)
- Enable backup verification after creation
- Store backups on different media than primary repository

## Scenario 7: Agent Runaway Behavior

### Symptoms

- Agent creates excessive checkpoints (hundreds in short time)
- Event log grows rapidly and fills disk
- Agent makes unintended or malicious changes

### Immediate Response

```bash
# 1. Kill the agent process immediately
pkill -9 -f "agent-name"

# 2. Revoke agent's access (if using access control)
# Remove agent's signing key
mv .flock/keys/agent-ed25519.sk .flock/keys/agent-ed25519.sk.revoked

# 3. Assess damage
fl audit-trail --all --agent <agent-id>
fl log --limit 100

# 4. Restore from pre-incident state if needed
# Find last known-good checkpoint before runaway
fl log --before "2026-02-16T10:00:00"
fl restore-checkpoint <last-good-checkpoint-id>
```

### Recovery

```bash
# Option A: Cherry-pick good changes from runaway period
fl explore "review agent changes"
# Manually extract useful changes, discard bad ones

# Option B: Revert all agent changes
# Create new checkpoint based on pre-incident state
fl checkout <last-good-checkpoint-id>
fl checkpoint -m "Revert agent runaway changes"

# Document incident
echo "Agent runaway detected $(date), reverted to checkpoint <id>" > INCIDENT-REPORT.md
fl checkpoint -m "Document agent incident"
```

### Prevention

- **Rate limiting**: Throttle agent checkpoint creation (e.g., max 10/hour)
- **Anomaly detection**: Alert on unusual patterns (checkpoint frequency, file volume)
- **Sandboxing**: Run agents in containers with disk quotas
- **Kill switch**: Implement emergency agent shutdown mechanism
- **Quality gates**: Require approval for agent checkpoints

## Appendix: Useful Commands

### Integrity Checking

```bash
fl fsck                           # Basic integrity check
fl fsck --verbose                 # Detailed check with all events
fl audit-trail --all              # Full audit history
fl audit-trail --since "1 day"   # Recent activity
```

### Backup and Restore

```bash
fl backup                                      # Create backup in current dir
fl backup --output /path/backup.tar.gz        # Specify output location
fl restore /path/backup.tar.gz                # Restore from backup
fl restore --verify /path/backup.tar.gz       # Restore and verify integrity
```

### History Inspection

```bash
fl log --all                     # All events in log
fl log --limit 50                # Last 50 events
fl log --before "2026-02-15"     # Events before date
fl log --after "2026-02-14"      # Events after date
fl log --branch main             # Events on specific branch
```

### Working Directory State

```bash
fl diff --checkpoint HEAD        # Changes since last checkpoint
fl status                        # Current working directory status
fl explore "test recovery"       # Start exploration mode for experiments
fl checkpoint -m "message"       # Create checkpoint
```

## Getting Help

If disaster recovery procedures don't resolve your issue:

1. **Check logs**: `.flock/event-log/events.jsonl` for raw event data
2. **Community support**: File issue at github.com/your-org/flock with `fl fsck` output
3. **Expert assistance**: Contact Flock maintainers with details of incident
4. **Professional recovery**: For critical data loss, consider data recovery services for underlying filesystem

## Summary Checklist

Before disaster:

- [ ] Automated daily backups configured
- [ ] Backup restoration tested monthly
- [ ] Offsite backup location configured
- [ ] Signing key backed up securely
- [ ] .flockignore includes secret patterns
- [ ] Agent rate limiting configured
- [ ] Incident response plan documented

After disaster:

- [ ] Stop operations immediately
- [ ] Run `fl fsck` to assess damage
- [ ] Restore from most recent backup
- [ ] Verify restoration with `fl fsck`
- [ ] Reconstruct lost work if needed
- [ ] Document incident and root cause
- [ ] Update prevention measures
- [ ] Test backup/recovery procedures
