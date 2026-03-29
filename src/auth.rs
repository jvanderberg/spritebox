use serde::{Deserialize, Serialize};
use std::env;
use std::fs;
use std::path::PathBuf;

#[derive(Debug, Serialize, Deserialize)]
struct StoredAuth {
    token: String,
    org: String,
}

/// Return the config directory for spritebox.
fn config_dir() -> Result<PathBuf, String> {
    if let Ok(dir) = env::var("SPRITEBOX_CONFIG_DIR") {
        return Ok(PathBuf::from(dir));
    }

    let home = env::var("HOME")
        .or_else(|_| env::var("USERPROFILE"))
        .map_err(|_| "HOME is not set".to_string())?;

    Ok(PathBuf::from(home).join(".config").join("spritebox"))
}

fn auth_file() -> Result<PathBuf, String> {
    Ok(config_dir()?.join("auth.json"))
}

/// Load the sprites API token. Priority:
/// 1. SPRITEBOX_TOKEN env var
/// 2. SPRITES_TOKEN env var
/// 3. Stored auth file from `spritebox auth login`
pub fn load_token() -> Option<String> {
    // Env vars take priority
    if let Ok(token) = env::var("SPRITEBOX_TOKEN")
        && !token.is_empty()
    {
        return Some(token);
    }
    if let Ok(token) = env::var("SPRITES_TOKEN")
        && !token.is_empty()
    {
        return Some(token);
    }

    // Fall back to stored auth file
    let path = auth_file().ok()?;
    let content = fs::read_to_string(path).ok()?;
    let stored: StoredAuth = serde_json::from_str(&content).ok()?;
    if stored.token.is_empty() {
        None
    } else {
        Some(stored.token)
    }
}

/// Load the stored org name (if any).
pub fn load_org() -> Option<String> {
    let path = auth_file().ok()?;
    let content = fs::read_to_string(path).ok()?;
    let stored: StoredAuth = serde_json::from_str(&content).ok()?;
    Some(stored.org)
}

/// Save a sprites token and org to the auth file.
pub fn save_token(token: &str, org: &str) -> Result<(), String> {
    let path = auth_file()?;
    let parent = path
        .parent()
        .ok_or_else(|| "invalid auth file path".to_string())?;
    fs::create_dir_all(parent).map_err(|e| format!("failed to create config dir: {e}"))?;

    let stored = StoredAuth {
        token: token.to_string(),
        org: org.to_string(),
    };
    let json =
        serde_json::to_string_pretty(&stored).map_err(|e| format!("failed to serialize: {e}"))?;
    fs::write(&path, json).map_err(|e| format!("failed to write {}: {e}", path.display()))?;

    // Restrict permissions on Unix
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&path, fs::Permissions::from_mode(0o600))
            .map_err(|e| format!("failed to set permissions: {e}"))?;
    }

    Ok(())
}

/// Remove stored auth.
pub fn remove_token() -> Result<bool, String> {
    let path = auth_file()?;
    if !path.exists() {
        return Ok(false);
    }
    fs::remove_file(&path).map_err(|e| format!("failed to remove {}: {e}", path.display()))?;
    Ok(true)
}

fn secrets_file() -> Result<PathBuf, String> {
    Ok(config_dir()?.join("secrets.json"))
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct StoredSecrets {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    claude_oauth_token: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    openai_api_key: Option<String>,
}

fn load_secrets() -> StoredSecrets {
    let path = match secrets_file() {
        Ok(p) => p,
        Err(_) => return StoredSecrets::default(),
    };
    let content = match fs::read_to_string(path) {
        Ok(c) => c,
        Err(_) => return StoredSecrets::default(),
    };
    serde_json::from_str(&content).unwrap_or_default()
}

fn save_secrets(secrets: &StoredSecrets) -> Result<(), String> {
    let path = secrets_file()?;
    let parent = path.parent().ok_or("invalid secrets path")?;
    fs::create_dir_all(parent).map_err(|e| format!("failed to create config dir: {e}"))?;
    let json = serde_json::to_string_pretty(secrets).map_err(|e| format!("serialize: {e}"))?;
    fs::write(&path, json).map_err(|e| format!("failed to write {}: {e}", path.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&path, fs::Permissions::from_mode(0o600))
            .map_err(|e| format!("failed to set permissions: {e}"))?;
    }
    Ok(())
}

/// Load the Claude OAuth token for sprites.
/// Priority: CLAUDE_CODE_OAUTH_TOKEN env var → stored secrets.
pub fn load_claude_token() -> Option<String> {
    if let Ok(token) = env::var("CLAUDE_CODE_OAUTH_TOKEN")
        && !token.is_empty()
    {
        return Some(token);
    }
    load_secrets().claude_oauth_token
}

/// Save a Claude OAuth token.
pub fn save_claude_token(token: &str) -> Result<(), String> {
    let mut secrets = load_secrets();
    secrets.claude_oauth_token = Some(token.to_string());
    save_secrets(&secrets)
}

/// Load the OpenAI API key for sprites.
pub fn load_openai_key() -> Option<String> {
    if let Ok(key) = env::var("OPENAI_API_KEY")
        && !key.is_empty()
    {
        return Some(key);
    }
    load_secrets().openai_api_key
}

/// Save an OpenAI API key.
pub fn save_openai_key(key: &str) -> Result<(), String> {
    let mut secrets = load_secrets();
    secrets.openai_api_key = Some(key.to_string());
    save_secrets(&secrets)
}

/// Get a Fly.io macaroon token from the `fly` CLI.
pub fn fly_auth_token() -> Result<String, String> {
    let output = std::process::Command::new("fly")
        .args(["auth", "token"])
        .output()
        .map_err(|e| format!("failed to run `fly auth token`: {e}. Is the Fly CLI installed?"))?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(format!(
            "`fly auth token` failed. Run `fly auth login` first.\n{stderr}"
        ));
    }

    let token = String::from_utf8(output.stdout)
        .map_err(|_| "fly auth token output is not valid UTF-8".to_string())?;
    let trimmed = token.trim();
    if trimmed.is_empty() {
        Err("fly auth token returned empty output".to_string())
    } else {
        Ok(trimmed.to_string())
    }
}
