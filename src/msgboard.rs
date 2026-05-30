use crate::state;
use serde::{Deserialize, Serialize};
use std::fs;
use std::path::PathBuf;

// ---------------------------------------------------------------------------
// Types
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BranchEntry {
    pub branch: String,
    pub sprite_name: String,
    pub leader: bool,
    pub joined_at: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Message {
    pub id: u64,
    pub from_branch: String,
    pub sprite_name: String,
    pub timestamp: String,
    pub body: String,
    /// true if posted via leader-post
    #[serde(default)]
    pub leader_msg: bool,
    /// If set, only this branch sees the message in list/new results.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub to: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct Cursor {
    last_seen_id: u64,
}

// ---------------------------------------------------------------------------
// Board — all operations on a single repo's message board
// ---------------------------------------------------------------------------

pub struct Board {
    dir: PathBuf,
}

impl Board {
    /// Open (or create) the message board for a repo.
    /// `repo` is the raw repo URL; the slug is derived automatically.
    pub fn open(repo: &str) -> Self {
        let repo_slug = state::slugify(&state::repo_basename(repo));
        let dir = std::env::temp_dir()
            .join("spritebox-msgboard")
            .join(&repo_slug);
        fs::create_dir_all(dir.join("cursors")).ok();
        Self { dir }
    }

    // -- Registration -------------------------------------------------------

    pub fn register(&self, branch: &str, sprite_name: &str) -> Result<Vec<BranchEntry>, String> {
        self.with_lock("branches.json", |path| {
            let mut branches: Vec<BranchEntry> = read_json(path);
            if branches.iter().any(|b| b.branch == branch) {
                // Already registered — update sprite name in case it changed
                for b in &mut branches {
                    if b.branch == branch {
                        b.sprite_name = sprite_name.to_string();
                    }
                }
            } else {
                let is_first = branches.is_empty();
                branches.push(BranchEntry {
                    branch: branch.to_string(),
                    sprite_name: sprite_name.to_string(),
                    leader: is_first,
                    joined_at: now_iso(),
                });
            }
            write_json(path, &branches);
            branches
        })
    }

    // -- Posting (all messages are direct — no broadcasts) --------------------

    /// Send a direct message to a specific branch.
    pub fn send(
        &self,
        branch: &str,
        sprite_name: &str,
        to: &str,
        body: &str,
    ) -> Result<Message, String> {
        self.append_message(branch, sprite_name, body, false, to.to_string())
    }

    /// Send a leader message as individual DMs to every other registered branch.
    pub fn leader_post(
        &self,
        branch: &str,
        sprite_name: &str,
        body: &str,
    ) -> Result<Vec<Message>, String> {
        let branches = self.list_branches();
        let targets: Vec<String> = branches
            .iter()
            .filter(|b| b.branch != branch)
            .map(|b| b.branch.clone())
            .collect();
        if targets.is_empty() {
            return Ok(Vec::new());
        }
        let mut sent = Vec::new();
        for target in &targets {
            let msg = self.append_message(branch, sprite_name, body, true, target.clone())?;
            sent.push(msg);
        }
        Ok(sent)
    }

    /// Send a direct message to the current leader.
    pub fn send_to_leader(
        &self,
        branch: &str,
        sprite_name: &str,
        body: &str,
    ) -> Result<Message, String> {
        let branches = self.list_branches();
        let leader = branches.iter().find(|b| b.leader)
            .ok_or("no leader registered on the board")?;
        let leader_branch = leader.branch.clone();
        self.append_message(branch, sprite_name, body, false, leader_branch)
    }

    fn append_message(
        &self,
        branch: &str,
        sprite_name: &str,
        body: &str,
        leader_msg: bool,
        to: String,
    ) -> Result<Message, String> {
        self.with_lock("messages.json", |path| {
            let mut messages: Vec<Message> = read_json(path);
            let id = messages.last().map_or(1, |m| m.id + 1);
            let msg = Message {
                id,
                from_branch: branch.to_string(),
                sprite_name: sprite_name.to_string(),
                timestamp: now_iso(),
                body: body.to_string(),
                leader_msg,
                to: Some(to.clone()),
            };
            messages.push(msg.clone());
            write_json(path, &messages);
            msg
        })
    }

    // -- Reading ------------------------------------------------------------

    pub fn list_branches(&self) -> Vec<BranchEntry> {
        read_json(&self.dir.join("branches.json"))
    }

    fn all_messages(&self) -> Vec<Message> {
        read_json(&self.dir.join("messages.json"))
    }

    /// All messages visible to this branch (filters out DMs to other branches).
    pub fn list_all(&self, branch: &str) -> Vec<Message> {
        self.all_messages()
            .into_iter()
            .filter(|m| visible_to(m, branch))
            .collect()
    }

    pub fn list_new(&self, branch: &str) -> Vec<Message> {
        let cursor = self.read_cursor(branch);
        let messages: Vec<Message> = self.all_messages()
            .into_iter()
            .filter(|m| m.id > cursor && visible_to(m, branch))
            .collect();
        if let Some(last) = messages.last() {
            self.write_cursor(branch, last.id);
        }
        messages
    }

    /// New messages from leader since this branch's cursor. Updates cursor.
    pub fn list_from_leader_new(&self, branch: &str) -> Vec<Message> {
        let cursor = self.read_cursor(branch);
        let branches = self.list_branches();
        let leader = branches.iter().find(|b| b.leader);
        match leader {
            Some(leader) => {
                let leader_branch = leader.branch.clone();
                let messages: Vec<Message> = self.all_messages()
                    .into_iter()
                    .filter(|m| m.from_branch == leader_branch && m.id > cursor && visible_to(m, branch))
                    .collect();
                if let Some(last) = messages.last() {
                    self.write_cursor(branch, last.id);
                }
                messages
            }
            None => Vec::new(),
        }
    }

    // -- Leader management --------------------------------------------------

    pub fn set_leader(&self, branch: &str) -> Result<Vec<BranchEntry>, String> {
        self.with_lock("branches.json", |path| {
            let mut branches: Vec<BranchEntry> = read_json(path);
            for b in &mut branches {
                b.leader = b.branch == branch;
            }
            write_json(path, &branches);
            branches
        })
    }

    // -- Cursor management --------------------------------------------------

    fn read_cursor(&self, branch: &str) -> u64 {
        let path = self.cursor_path(branch);
        read_json::<Cursor>(&path).last_seen_id
    }

    fn write_cursor(&self, branch: &str, id: u64) {
        let path = self.cursor_path(branch);
        write_json(&path, &Cursor { last_seen_id: id });
    }

    fn cursor_path(&self, branch: &str) -> PathBuf {
        self.dir
            .join("cursors")
            .join(format!("{}.json", state::slugify(branch)))
    }

    // -- File locking -------------------------------------------------------

    fn with_lock<T, F>(&self, filename: &str, f: F) -> Result<T, String>
    where
        F: FnOnce(&PathBuf) -> T,
    {
        let path = self.dir.join(filename);
        let lock_path = self.dir.join(format!("{filename}.lock"));

        let lock_file = fs::OpenOptions::new()
            .create(true)
            .write(true)
            .truncate(false)
            .open(&lock_path)
            .map_err(|e| format!("failed to open lock file: {e}"))?;

        flock_exclusive(&lock_file)?;
        let result = f(&path);
        flock_unlock(&lock_file)?;

        Ok(result)
    }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// A message is visible to `branch` if it has no `to` field (broadcast),
/// or if `to` matches `branch`, or if the sender is `branch`.
fn visible_to(msg: &Message, branch: &str) -> bool {
    match &msg.to {
        None => true,
        Some(to) => to == branch || msg.from_branch == branch,
    }
}

fn read_json<T: for<'de> Deserialize<'de> + Default>(path: &PathBuf) -> T {
    fs::read_to_string(path)
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default()
}

fn write_json<T: Serialize>(path: &PathBuf, value: &T) {
    if let Ok(json) = serde_json::to_string_pretty(value) {
        let _ = fs::write(path, json);
    }
}

fn now_iso() -> String {
    let output = std::process::Command::new("date")
        .args(["-u", "+%Y-%m-%dT%H:%M:%SZ"])
        .output();
    match output {
        Ok(o) if o.status.success() => {
            String::from_utf8_lossy(&o.stdout).trim().to_string()
        }
        _ => "unknown".to_string(),
    }
}

// ---------------------------------------------------------------------------
// flock via std Command (avoids libc dependency)
// ---------------------------------------------------------------------------

fn flock_exclusive(file: &fs::File) -> Result<(), String> {
    #[cfg(unix)]
    {
        use std::os::unix::io::AsRawFd;
        // LOCK_EX = 2
        let rc = unsafe { flock_raw(file.as_raw_fd(), 2) };
        if rc != 0 {
            return Err(format!("flock failed: {}", std::io::Error::last_os_error()));
        }
    }
    Ok(())
}

fn flock_unlock(file: &fs::File) -> Result<(), String> {
    #[cfg(unix)]
    {
        use std::os::unix::io::AsRawFd;
        // LOCK_UN = 8
        let rc = unsafe { flock_raw(file.as_raw_fd(), 8) };
        if rc != 0 {
            return Err(format!("flock unlock failed: {}", std::io::Error::last_os_error()));
        }
    }
    Ok(())
}

#[cfg(unix)]
unsafe fn flock_raw(fd: i32, operation: i32) -> i32 {
    unsafe extern "C" {
        fn flock(fd: i32, operation: i32) -> i32;
    }
    unsafe { flock(fd, operation) }
}

// ---------------------------------------------------------------------------
// Cursor Default impl
// ---------------------------------------------------------------------------

impl Default for Cursor {
    fn default() -> Self {
        Self { last_seen_id: 0 }
    }
}
