use std::collections::HashSet;
use serde::{Deserialize, Serialize};

/// Request from editor plugin to Flock daemon
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "type", content = "data")]
pub enum EditorRequest {
    Initialize {
        root_path: String,
        capabilities: Vec<String>,
    },
    FileOpened {
        path: String,
    },
    FileClosed {
        path: String,
    },
    CursorAtSymbol {
        path: String,
        symbol: String,
        line: u32,
    },
    GetPresence,
    GetWarnings {
        path: Option<String>,
    },
    Shutdown,
}

/// Notification from Flock daemon to editor plugin
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "type", content = "data")]
pub enum EditorNotification {
    Initialized {
        version: String,
        capabilities: Vec<String>,
    },
    PresenceOverlay {
        entries: Vec<PresenceEntry>,
    },
    HeadsUpWarning {
        actor: String,
        symbol: String,
        path: String,
        action: String,
    },
    ConflictForecastWarning {
        symbol: String,
        path: String,
        local_change: String,
        remote_change: String,
        remote_actor: String,
    },
    GhostText {
        path: String,
        line: u32,
        text: String,
        actor: String,
    },
    TaskReady {
        task_id: String,
        title: String,
    },
    Error {
        message: String,
    },
}

/// Presence entry for an actor in the workspace
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct PresenceEntry {
    pub actor: String,
    pub file: String,
    pub symbol: Option<String>,
    pub intent: Option<String>,
}

/// Manages editor protocol session state
#[derive(Debug)]
pub struct EditorSession {
    open_files: HashSet<String>,
    cursor_symbol: Option<(String, String)>,
}

impl EditorSession {
    /// Create a new editor session
    pub fn new() -> Self {
        Self {
            open_files: HashSet::new(),
            cursor_symbol: None,
        }
    }

    /// Handle an editor request and return an optional notification
    pub fn handle_request(&mut self, request: &EditorRequest) -> Option<EditorNotification> {
        match request {
            EditorRequest::Initialize { .. } => {
                Some(EditorNotification::Initialized {
                    version: "0.1.0".to_string(),
                    capabilities: vec![
                        "presence".to_string(),
                        "heads_up".to_string(),
                        "conflict_forecast".to_string(),
                        "tasks".to_string(),
                    ],
                })
            }
            EditorRequest::FileOpened { path } => {
                self.open_files.insert(path.clone());
                None
            }
            EditorRequest::FileClosed { path } => {
                self.open_files.remove(path);
                None
            }
            EditorRequest::CursorAtSymbol { path, symbol, .. } => {
                self.cursor_symbol = Some((path.clone(), symbol.clone()));
                None
            }
            EditorRequest::GetPresence => {
                // Actual data comes from WebSocket
                Some(EditorNotification::PresenceOverlay {
                    entries: Vec::new(),
                })
            }
            EditorRequest::GetWarnings { .. } => {
                // Actual data comes from WebSocket
                None
            }
            EditorRequest::Shutdown => None,
        }
    }

    /// Check if a file is currently open
    pub fn is_file_open(&self, path: &str) -> bool {
        self.open_files.contains(path)
    }

    /// Check if notifications should be sent for a given path
    /// Returns true if the path matches any open file (exact or prefix match)
    pub fn should_notify_for_path(&self, path: &str) -> bool {
        for open_file in &self.open_files {
            if open_file == path || open_file.starts_with(path) || path.starts_with(open_file) {
                return true;
            }
        }
        false
    }
}

impl Default for EditorSession {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_editor_request_serde_roundtrip() {
        let requests = vec![
            EditorRequest::Initialize {
                root_path: "/path/to/repo".to_string(),
                capabilities: vec!["presence".to_string(), "tasks".to_string()],
            },
            EditorRequest::FileOpened {
                path: "src/main.rs".to_string(),
            },
            EditorRequest::FileClosed {
                path: "src/lib.rs".to_string(),
            },
            EditorRequest::CursorAtSymbol {
                path: "src/main.rs".to_string(),
                symbol: "main".to_string(),
                line: 42,
            },
            EditorRequest::GetPresence,
            EditorRequest::GetWarnings {
                path: Some("src/main.rs".to_string()),
            },
            EditorRequest::GetWarnings { path: None },
            EditorRequest::Shutdown,
        ];

        for request in requests {
            let json = serde_json::to_string(&request).unwrap();
            let deserialized: EditorRequest = serde_json::from_str(&json).unwrap();
            assert_eq!(request, deserialized);
        }
    }

    #[test]
    fn test_editor_notification_serde_roundtrip() {
        let notifications = vec![
            EditorNotification::Initialized {
                version: "0.1.0".to_string(),
                capabilities: vec!["presence".to_string()],
            },
            EditorNotification::PresenceOverlay {
                entries: vec![
                    PresenceEntry {
                        actor: "alice".to_string(),
                        file: "src/main.rs".to_string(),
                        symbol: Some("main".to_string()),
                        intent: Some("editing".to_string()),
                    },
                ],
            },
            EditorNotification::HeadsUpWarning {
                actor: "bob".to_string(),
                symbol: "calculate".to_string(),
                path: "src/math.rs".to_string(),
                action: "modifying".to_string(),
            },
            EditorNotification::ConflictForecastWarning {
                symbol: "process_data".to_string(),
                path: "src/processor.rs".to_string(),
                local_change: "signature change".to_string(),
                remote_change: "signature change".to_string(),
                remote_actor: "charlie".to_string(),
            },
            EditorNotification::GhostText {
                path: "src/lib.rs".to_string(),
                line: 10,
                text: "// ghost comment".to_string(),
                actor: "dave".to_string(),
            },
            EditorNotification::TaskReady {
                task_id: "task-123".to_string(),
                title: "Implement feature X".to_string(),
            },
            EditorNotification::Error {
                message: "Connection lost".to_string(),
            },
        ];

        for notification in notifications {
            let json = serde_json::to_string(&notification).unwrap();
            let deserialized: EditorNotification = serde_json::from_str(&json).unwrap();
            assert_eq!(notification, deserialized);
        }
    }

    #[test]
    fn test_presence_entry_serde_roundtrip() {
        let entry = PresenceEntry {
            actor: "alice".to_string(),
            file: "src/main.rs".to_string(),
            symbol: Some("main".to_string()),
            intent: Some("editing".to_string()),
        };

        let json = serde_json::to_string(&entry).unwrap();
        let deserialized: PresenceEntry = serde_json::from_str(&json).unwrap();
        assert_eq!(entry, deserialized);
    }

    #[test]
    fn test_editor_session_file_tracking() {
        let mut session = EditorSession::new();

        // Initially no files open
        assert!(!session.is_file_open("src/main.rs"));

        // Open a file
        let response = session.handle_request(&EditorRequest::FileOpened {
            path: "src/main.rs".to_string(),
        });
        assert!(response.is_none());
        assert!(session.is_file_open("src/main.rs"));

        // Open another file
        session.handle_request(&EditorRequest::FileOpened {
            path: "src/lib.rs".to_string(),
        });
        assert!(session.is_file_open("src/lib.rs"));

        // Close a file
        session.handle_request(&EditorRequest::FileClosed {
            path: "src/main.rs".to_string(),
        });
        assert!(!session.is_file_open("src/main.rs"));
        assert!(session.is_file_open("src/lib.rs"));
    }

    #[test]
    fn test_editor_session_cursor_tracking() {
        let mut session = EditorSession::new();

        // Initially no cursor symbol
        assert!(session.cursor_symbol.is_none());

        // Update cursor position
        session.handle_request(&EditorRequest::CursorAtSymbol {
            path: "src/main.rs".to_string(),
            symbol: "main".to_string(),
            line: 10,
        });
        assert_eq!(
            session.cursor_symbol,
            Some(("src/main.rs".to_string(), "main".to_string()))
        );

        // Update to different symbol
        session.handle_request(&EditorRequest::CursorAtSymbol {
            path: "src/lib.rs".to_string(),
            symbol: "process".to_string(),
            line: 42,
        });
        assert_eq!(
            session.cursor_symbol,
            Some(("src/lib.rs".to_string(), "process".to_string()))
        );
    }

    #[test]
    fn test_editor_session_initialize() {
        let mut session = EditorSession::new();

        let response = session.handle_request(&EditorRequest::Initialize {
            root_path: "/path/to/repo".to_string(),
            capabilities: vec!["presence".to_string()],
        });

        assert!(response.is_some());
        if let Some(EditorNotification::Initialized { version, capabilities }) = response {
            assert_eq!(version, "0.1.0");
            assert_eq!(capabilities, vec![
                "presence".to_string(),
                "heads_up".to_string(),
                "conflict_forecast".to_string(),
                "tasks".to_string(),
            ]);
        } else {
            panic!("Expected Initialized notification");
        }
    }

    #[test]
    fn test_editor_session_get_presence() {
        let mut session = EditorSession::new();

        let response = session.handle_request(&EditorRequest::GetPresence);

        assert!(response.is_some());
        if let Some(EditorNotification::PresenceOverlay { entries }) = response {
            assert!(entries.is_empty());
        } else {
            panic!("Expected PresenceOverlay notification");
        }
    }

    #[test]
    fn test_should_notify_for_path() {
        let mut session = EditorSession::new();

        // Open some files
        session.handle_request(&EditorRequest::FileOpened {
            path: "src/main.rs".to_string(),
        });
        session.handle_request(&EditorRequest::FileOpened {
            path: "src/lib/mod.rs".to_string(),
        });

        // Exact match
        assert!(session.should_notify_for_path("src/main.rs"));

        // Prefix match (open file is prefix of path)
        assert!(session.should_notify_for_path("src/lib/mod.rs"));

        // Prefix match (path is prefix of open file)
        assert!(session.should_notify_for_path("src/lib"));

        // No match
        assert!(!session.should_notify_for_path("tests/test.rs"));
        assert!(!session.should_notify_for_path("README.md"));
    }

    #[test]
    fn test_editor_session_shutdown() {
        let mut session = EditorSession::new();

        let response = session.handle_request(&EditorRequest::Shutdown);
        assert!(response.is_none());
    }

    #[test]
    fn test_editor_session_get_warnings() {
        let mut session = EditorSession::new();

        // With path
        let response = session.handle_request(&EditorRequest::GetWarnings {
            path: Some("src/main.rs".to_string()),
        });
        assert!(response.is_none());

        // Without path
        let response = session.handle_request(&EditorRequest::GetWarnings { path: None });
        assert!(response.is_none());
    }
}
