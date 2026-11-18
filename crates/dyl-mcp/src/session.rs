//! 会话管理 — 每个 Session 持有一个独立的 Engine 实例

use std::collections::HashMap;
use std::sync::Arc;

use crossbeam_channel::Receiver;
use dyl_core::contract::{Command, CommandEnvelope, EventEnvelope, SCHEMA_VERSION};
use dyl_engine::{ControlDiag, Engine};
use parking_lot::Mutex;
use serde::Serialize;

use crate::trace_buffer;

const MAX_SESSIONS: usize = 8;
const MAX_EVENTS_PER_DRAIN: usize = 100;

pub struct Session {
    pub engine: Engine,
    pub events: Mutex<Receiver<EventEnvelope>>,
    pub trace_buffer: Arc<trace_buffer::TraceBuffer>,
}

#[derive(Debug, Clone, Serialize)]
pub struct SessionInfo {
    pub session_id: String,
    pub state: String,
    pub source: Option<String>,
    pub position_ms: u64,
    pub duration_ms: u64,
}

pub struct SessionManager {
    sessions: Mutex<HashMap<String, Arc<Session>>>,
    next_id: Mutex<u64>,
}

impl SessionManager {
    pub fn new() -> Self {
        Self {
            sessions: Mutex::new(HashMap::new()),
            next_id: Mutex::new(1),
        }
    }

    pub fn create(&self, source: &str) -> Result<serde_json::Value, String> {
        let count = self.sessions.lock().len();
        if count >= MAX_SESSIONS {
            return Err(format!(
                "session limit reached ({MAX_SESSIONS}); destroy an existing session first"
            ));
        }

        let (engine, events) = Engine::new().map_err(|e| format!("engine create: {e}"))?;

        let trace_buffer = Arc::new(trace_buffer::TraceBuffer::new(1024));
        trace_buffer::install(trace_buffer.clone());

        let envelope = CommandEnvelope {
            schema: SCHEMA_VERSION,
            request_id: None,
            command: Command::LoadFile {
                url: source.to_string(),
            },
        };
        engine
            .command(envelope)
            .map_err(|e| format!("load_file: {e}"))?;

        let id = {
            let mut next = self.next_id.lock();
            let v = *next;
            *next += 1;
            format!("sess-{v}")
        };

        let session = Arc::new(Session {
            engine,
            events: Mutex::new(events),
            trace_buffer,
        });

        let snap = session.engine.snapshot();

        self.sessions.lock().insert(id.clone(), session);

        Ok(serde_json::json!({
            "session_id": id,
            "state": format!("{:?}", snap.state),
            "source": snap.source,
            "note": "File loading is asynchronous. Use dyl_events or dyl_snapshot to confirm load result."
        }))
    }

    pub fn command(&self, id: &str, command: Command) -> Result<u64, String> {
        let session = {
            let sessions = self.sessions.lock();
            sessions
                .get(id)
                .cloned()
                .ok_or_else(|| format!("session {id} not found"))?
        };
        let envelope = CommandEnvelope {
            schema: SCHEMA_VERSION,
            request_id: None,
            command,
        };
        session
            .engine
            .command(envelope)
            .map_err(|e| format!("command: {e}"))
    }

    pub fn snapshot(&self, id: &str) -> Result<serde_json::Value, String> {
        let session = {
            let sessions = self.sessions.lock();
            sessions
                .get(id)
                .cloned()
                .ok_or_else(|| format!("session {id} not found"))?
        };
        let snap = session.engine.snapshot();
        serde_json::to_value(snap.as_ref()).map_err(|e| format!("snapshot serialize: {e}"))
    }

    pub fn inspect(&self, id: &str) -> Result<ControlDiag, String> {
        let session = {
            let sessions = self.sessions.lock();
            sessions
                .get(id)
                .cloned()
                .ok_or_else(|| format!("session {id} not found"))?
        };
        session
            .engine
            .inspect()
            .map_err(|e| format!("inspect: {e}"))
    }

    pub fn drain_events(&self, id: &str) -> Result<Vec<serde_json::Value>, String> {
        let session = {
            let sessions = self.sessions.lock();
            sessions
                .get(id)
                .cloned()
                .ok_or_else(|| format!("session {id} not found"))?
        };
        let mut out = Vec::new();
        let mut last_position: Option<u64> = None;
        let events = session.events.lock();
        while let Ok(evt) = events.try_recv() {
            let v = serde_json::to_value(&evt).map_err(|e| format!("event serialize: {e}"))?;
            if v.get("PositionChanged").is_some() {
                if let Some(pos) = v
                    .get("PositionChanged")
                    .and_then(|p| p.get("position_ms"))
                    .and_then(|p| p.as_u64())
                {
                    last_position = Some(pos);
                    continue;
                }
            }
            out.push(v);
            if out.len() >= MAX_EVENTS_PER_DRAIN {
                break;
            }
        }
        if let Some(pos) = last_position {
            out.push(serde_json::json!({
                "PositionChanged": { "position_ms": pos }
            }));
        }
        Ok(out)
    }

    pub fn drain_trace(&self, id: &str, min_level: &str) -> Result<Vec<serde_json::Value>, String> {
        let session = {
            let sessions = self.sessions.lock();
            sessions
                .get(id)
                .cloned()
                .ok_or_else(|| format!("session {id} not found"))?
        };
        Ok(session.trace_buffer.drain(min_level))
    }

    pub fn list(&self) -> Vec<SessionInfo> {
        self.sessions
            .lock()
            .iter()
            .map(|(id, s)| {
                let snap = s.engine.snapshot();
                SessionInfo {
                    session_id: id.clone(),
                    state: format!("{:?}", snap.state),
                    source: snap.source.clone(),
                    position_ms: snap.position_ms,
                    duration_ms: snap.duration_ms,
                }
            })
            .collect()
    }

    pub fn destroy(&self, id: &str) -> Result<(), String> {
        let session = {
            let mut sessions = self.sessions.lock();
            sessions
                .remove(id)
                .ok_or_else(|| format!("session {id} not found"))?
        };
        trace_buffer::uninstall(&session.trace_buffer);
        Ok(())
    }
}

impl Default for SessionManager {
    fn default() -> Self {
        Self::new()
    }
}
