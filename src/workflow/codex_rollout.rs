//! Shared session identity and opening verification for Codex rollouts.
use super::messages::body_hash;
use serde_json::Value;
use std::{
    fs::File,
    io::{self, BufRead, BufReader, Read},
    path::Path,
};
use uuid::Uuid;

pub(super) fn id(path: &Path) -> Option<Uuid> {
    let filename = path
        .file_name()?
        .to_str()?
        .strip_prefix("rollout-")?
        .strip_suffix(".jsonl")?;
    let id = filename.get(filename.len().checked_sub(36)?..)?;
    let prefix = &filename[..filename.len() - 36];
    (prefix.is_empty() || prefix.ends_with('-')).then_some(())?;
    let uuid = Uuid::parse_str(id).ok()?;
    (uuid.to_string() == id).then_some(uuid)
}

pub(super) fn opening_matches(
    file: &File,
    limit: u64,
    id: Uuid,
    cwd: &str,
    brief_hash: Option<&str>,
    allow_vscode: bool,
) -> io::Result<bool> {
    let mut reader = BufReader::new(file).take(limit);
    let mut line = Vec::new();
    reader.read_until(b'\n', &mut line)?;
    let Some(meta) = record(&line) else {
        return Ok(false);
    };
    let payload = &meta["payload"];
    if meta["type"] != "session_meta"
        || payload["id"] != id.to_string()
        || payload["cwd"] != cwd
        || !(payload["source"] == "cli" || (allow_vscode && payload["source"] == "vscode"))
        || !payload["forked_from_id"].is_null()
        || !payload["parent_thread_id"].is_null()
    {
        return Ok(false);
    }
    let Some(brief_hash) = brief_hash else {
        return Ok(true);
    };
    loop {
        line.clear();
        reader.read_until(b'\n', &mut line)?;
        let Some(record) = record(&line) else {
            return Ok(false);
        };
        let item = &record["payload"];
        if record["type"] == "event_msg" && item["type"] == "user_message" {
            return Ok(item["message"]
                .as_str()
                .is_some_and(|text| body_hash(text) == brief_hash));
        }
        if record["type"] == "response_item" && item["type"] == "message" && item["role"] == "user"
        {
            if let Some(text) = item["content"][0]["text"].as_str() {
                if text.starts_with("<environment_context>")
                    || text.starts_with("# AGENTS.md instructions")
                    || text.starts_with("<user_instructions>")
                {
                    continue;
                }
                return Ok(body_hash(text) == brief_hash);
            }
        }
    }
}

pub(super) fn record(line: &[u8]) -> Option<Value> {
    line.ends_with(b"\n")
        .then(|| serde_json::from_slice(line).ok())
        .flatten()
}
