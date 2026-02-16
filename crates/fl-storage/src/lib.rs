pub mod chunking;
pub mod compaction;
pub mod content_store;
pub mod credential_store;
pub mod event;
pub mod event_log;
pub mod event_log_migration;
pub mod file_index;
pub mod layout;
pub mod materialized_state;
pub mod refs;
pub mod remote_config;
pub mod remote_protocol;
pub mod segmented_event_log;
pub mod segmented_refs;
pub mod snapshot_store;
pub mod ws_protocol;

pub use chunking::{Chunk, ChunkConfig, chunk_data};
pub use compaction::{CompactionReport, compact_event_log};
pub use content_store::ContentStore;
pub use event::{
    ApiCallRecord, CURRENT_EVENT_SCHEMA_VERSION, CheckpointEvent, ConflictAction,
    ConflictResolutionEvent, DecisionAction, DecisionEvent, Event, EventKind, EventRecord,
    ExplorationAction, ExplorationEvent, GateAction, GateCondition, GateEvent, GatePolicy,
    GitBridgeAction, GitBridgeEvent, HookEvent, IntelligenceAction, IntelligenceEvent,
    LockAction, LockEvent, NotifyConfig, PresenceAction,
    PresenceEvent, RebaseEvent, RemoteSyncAction, RemoteSyncEvent, ResourceUsageEvent,
    SessionAction, SessionEvent, SubscriptionAction, SubscriptionEvent, SubscriptionFilter,
    TaskAction, TaskEvent, UndoEvent, UndoMode, compute_event_hash, event_signing_payload,
};
pub use event_log::EventLog;
pub use event_log_migration::{MigrationReport as EventLogMigrationReport, migrate_to_segmented};
pub use file_index::{BlockRef, FileEntry, FileIndex, SnapshotIndex};
pub use layout::{
    CONFIG_FILE, EVENT_LOG_DIR, EVENT_LOG_FILE, EVENT_LOG_INDEX_FILE, EVENT_LOG_SEGMENTS_DIR,
    FLOCK_DIR, KEY_DIR, MATERIALIZED_STATES_DIR, REFS_DIR, REFS_FILE, SECRETS_CONFIG_FILE,
    HOOKS_CONFIG_FILE, SEGMENTED_REFS_DIR, SIGNING_KEY_FILE, SNAPSHOT_DIR, STORE_BLOCKS_DIR,
    STORE_DIR, STORE_INDEX_DIR,
};
pub use materialized_state::MaterializedStateStore;
pub use refs::{
    CURRENT_REFS_SCHEMA_VERSION, RefKind, RefRecord, RefStore, RepoRef, WorkspaceRefConfig,
};
pub use remote_config::{
    RoostEntry, RoostsConfig, add_roost, find_roost, find_roost_mut, load_roosts, remove_roost,
    save_roosts,
};
pub use remote_protocol::{
    BlockNeedRequest, BlockNeedResponse, BlockPayload, BlockUploadRequest, BlockUploadResponse,
    EventPullRequest, EventPullResponse, EventPushRequest, EventPushResponse, PullReport,
    PushReport, RemoteScheme, RemoteUrl, SnapshotNeedRequest, SnapshotNeedResponse,
    SshAuthChallengeRequest, SshAuthChallengeResponse, SshAuthVerifyRequest, SshAuthVerifyResponse,
    TokenLoginRequest, TokenLoginResponse,
};
pub use segmented_event_log::SegmentedEventLog;
pub use segmented_refs::SegmentedRefStore;
pub use snapshot_store::{IncrementalSnapshotReport, build_incremental_index, store_snapshot_incremental};
pub use ws_protocol::{
    WsClientMessage, WsServerMessage, WsSubscribeRequest,
};
pub use credential_store::{
    AuthMethod, CredentialEntry, CredentialStore, credentials_path, resolve_token,
};
