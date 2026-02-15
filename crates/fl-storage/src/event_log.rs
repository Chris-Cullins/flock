use std::fs::{self, File, OpenOptions};
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};

use crate::event::Event;
use crate::layout::EVENT_LOG_FILE;

#[derive(Debug, Clone)]
pub struct EventLog {
    path: PathBuf,
}

impl EventLog {
    pub fn for_root(root: &Path) -> Self {
        Self {
            path: root.join(EVENT_LOG_FILE),
        }
    }

    pub fn ensure_exists(&self) -> Result<()> {
        if let Some(parent) = self.path.parent() {
            fs::create_dir_all(parent).with_context(|| {
                format!("failed to create event-log directory {}", parent.display())
            })?;
        }

        if !self.path.exists() {
            File::create(&self.path)
                .with_context(|| format!("failed to create {}", self.path.display()))?;
        }

        Ok(())
    }

    pub fn read_all(&self) -> Result<Vec<Event>> {
        let file = File::open(&self.path)
            .with_context(|| format!("failed to open {}", self.path.display()))?;
        let reader = BufReader::new(file);

        let mut events = Vec::new();
        for line in reader.lines() {
            let line =
                line.with_context(|| format!("failed to read line in {}", self.path.display()))?;
            let trimmed = line.trim();
            if trimmed.is_empty() {
                continue;
            }
            let event = serde_json::from_str::<Event>(trimmed)
                .with_context(|| format!("failed to parse event JSON: {}", trimmed))?;
            events.push(event);
        }

        Ok(events)
    }

    pub fn append(&self, event: &Event) -> Result<()> {
        let mut file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.path)
            .with_context(|| format!("failed to open {} for append", self.path.display()))?;

        let line = serde_json::to_string(event).context("failed to serialize event")?;
        writeln!(file, "{}", line).context("failed to append event to log")?;
        Ok(())
    }
}
