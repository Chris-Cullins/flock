pub const FLOCK_DIR: &str = ".flock";
pub const EVENT_LOG_DIR: &str = ".flock/event-log";
pub const EVENT_LOG_FILE: &str = ".flock/event-log/events.jsonl";
pub const SNAPSHOT_DIR: &str = ".flock/snapshots";
pub const REFS_DIR: &str = ".flock/refs";
pub const REFS_FILE: &str = ".flock/refs/refs.json";
pub const KEY_DIR: &str = ".flock/keys";
pub const SIGNING_KEY_FILE: &str = ".flock/keys/ed25519.sk";
pub const CONFIG_FILE: &str = ".flock/config.toml";
pub const SECRETS_CONFIG_FILE: &str = ".flock/secrets.toml";
pub const MATERIALIZED_STATES_DIR: &str = ".flock/materialized-states";

// Native storage engine paths
pub const STORE_DIR: &str = ".flock/store";
pub const STORE_BLOCKS_DIR: &str = ".flock/store/blocks";
pub const STORE_INDEX_DIR: &str = ".flock/store/index";

// Segmented event log paths
pub const EVENT_LOG_SEGMENTS_DIR: &str = ".flock/event-log/segments";
pub const EVENT_LOG_INDEX_FILE: &str = ".flock/event-log/index.json";

// Segmented refs paths
pub const SEGMENTED_REFS_DIR: &str = ".flock/refs-v2";
