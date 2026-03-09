use crate::state::Instance;
use std::ffi::OsStr;
use std::fs;
use std::io::Write;
#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;
use std::thread::JoinHandle;
use std::time::Duration;

const REQUEST_POLL_MILLIS: u64 = 250;
const GUEST_BRIDGE_ROOT: &str = "/yolobox";
const GUEST_WORKSPACE_ROOT: &str = "/workspace";
const GUEST_REQUESTS_ROOT: &str = "/yolobox/requests";
const GUEST_INPUTS_ROOT: &str = "/yolobox/inputs";
const GUEST_RESPONSES_ROOT: &str = "/yolobox/responses";
const GUEST_SCRIPTS_ROOT: &str = "/yolobox/scripts";
const GUEST_SKILLS_ROOT: &str = "/yolobox/skills";
const GUEST_ARTIFACTS_ROOT: &str = "/workspace/.artifacts";
const BRIDGE_REQUESTS_TAG: &str = "yolobox-requests";
const BRIDGE_INPUTS_TAG: &str = "yolobox-inputs";
const BRIDGE_RESPONSES_TAG: &str = "yolobox-responses";
const BRIDGE_SCRIPTS_TAG: &str = "yolobox-scripts";
const BRIDGE_SKILLS_TAG: &str = "yolobox-skills";

pub struct HostBridge {
    stop: Arc<AtomicBool>,
    handle: Option<JoinHandle<()>>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ManagedMount {
    pub host_path: PathBuf,
    pub guest_path: PathBuf,
    pub tag: &'static str,
    pub readonly: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ManagedMountSpec {
    pub host_subdir: &'static str,
    pub guest_path: &'static str,
    pub tag: &'static str,
    pub readonly: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum RequestVerb {
    Open,
    Code,
    Finder,
    OpenUrl,
    PasteImage,
}

struct Request {
    verb: RequestVerb,
    target: String,
}

struct Response {
    ok: bool,
    message: String,
}

pub fn managed_mount_specs() -> &'static [ManagedMountSpec] {
    &[
        ManagedMountSpec {
            host_subdir: "requests",
            guest_path: GUEST_REQUESTS_ROOT,
            tag: BRIDGE_REQUESTS_TAG,
            readonly: false,
        },
        ManagedMountSpec {
            host_subdir: "inputs",
            guest_path: GUEST_INPUTS_ROOT,
            tag: BRIDGE_INPUTS_TAG,
            readonly: false,
        },
        ManagedMountSpec {
            host_subdir: "responses",
            guest_path: GUEST_RESPONSES_ROOT,
            tag: BRIDGE_RESPONSES_TAG,
            readonly: true,
        },
        ManagedMountSpec {
            host_subdir: "scripts",
            guest_path: GUEST_SCRIPTS_ROOT,
            tag: BRIDGE_SCRIPTS_TAG,
            readonly: true,
        },
        ManagedMountSpec {
            host_subdir: "skills",
            guest_path: GUEST_SKILLS_ROOT,
            tag: BRIDGE_SKILLS_TAG,
            readonly: true,
        },
    ]
}

pub fn managed_mounts(
    instance: &Instance,
    guest_hostname: Option<&str>,
) -> Result<Vec<ManagedMount>, String> {
    let host_dir = host_bridge_dir(instance);
    initialize_bridge_dir(&host_dir, instance, guest_hostname)?;
    managed_mount_specs()
        .iter()
        .map(|spec| {
            let host_path = host_dir.join(spec.host_subdir);
            crate::state::share_mount(&host_path, Path::new(spec.guest_path)).map(|share| {
                ManagedMount {
                    host_path: share.host_path,
                    guest_path: share.guest_path,
                    tag: spec.tag,
                    readonly: spec.readonly,
                }
            })
        })
        .collect()
}

pub fn start(instance: &Instance) -> Result<HostBridge, String> {
    let host_dir = host_bridge_dir(instance);
    ensure_directory(&host_dir.join("requests"))?;
    ensure_directory(&host_dir.join("responses"))?;

    let stop = Arc::new(AtomicBool::new(false));
    let thread_stop = Arc::clone(&stop);
    let thread_instance = instance.clone();
    let handle = thread::spawn(move || {
        while !thread_stop.load(Ordering::Relaxed) {
            if let Err(err) = process_pending_requests(&thread_instance) {
                eprintln!("yolobox host bridge error for {}: {err}", thread_instance.id);
            }
            thread::sleep(Duration::from_millis(REQUEST_POLL_MILLIS));
        }
    });

    Ok(HostBridge {
        stop,
        handle: Some(handle),
    })
}

impl Drop for HostBridge {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
    }
}

fn host_bridge_dir(instance: &Instance) -> PathBuf {
    instance.instance_dir.join("runtime").join("yolobox")
}

fn initialize_bridge_dir(
    host_dir: &Path,
    instance: &Instance,
    guest_hostname: Option<&str>,
) -> Result<(), String> {
    ensure_directory(&host_dir.join("requests"))?;
    ensure_directory(&host_dir.join("responses"))?;
    ensure_directory(&host_dir.join("inputs"))?;
    populate_scripts(&host_dir.join("scripts"))?;
    populate_default_skills(&host_dir.join("skills"), instance, guest_hostname)
}

fn populate_scripts(scripts_dir: &Path) -> Result<(), String> {
    ensure_directory(scripts_dir)?;
    write_executable_script(
        &scripts_dir.join("yolobox-open"),
        &render_bridge_tool_script("open"),
    )?;
    write_executable_script(
        &scripts_dir.join("yolobox-paste-image"),
        &render_bridge_tool_script("paste-image"),
    )?;
    write_executable_script(
        &scripts_dir.join("yolobox-open-url"),
        &render_bridge_tool_script("open-url"),
    )?;
    write_executable_script(&scripts_dir.join("code"), &render_bridge_tool_script("code"))?;
    write_executable_script(
        &scripts_dir.join("finder"),
        &render_bridge_tool_script("finder"),
    )?;
    Ok(())
}

fn write_executable_script(path: &Path, contents: &str) -> Result<(), String> {
    write_atomic_file(path, contents)?;
    #[cfg(unix)]
    {
        let mut permissions = fs::metadata(path).map_err(|err| err.to_string())?.permissions();
        permissions.set_mode(0o755);
        fs::set_permissions(path, permissions).map_err(|err| err.to_string())?;
    }
    Ok(())
}

fn populate_default_skills(
    skills_dir: &Path,
    instance: &Instance,
    guest_hostname: Option<&str>,
) -> Result<(), String> {
    ensure_directory(skills_dir)?;
    ensure_directory(&skills_dir.join("common"))?;
    ensure_directory(&skills_dir.join("codex"))?;
    ensure_directory(&skills_dir.join("claude"))?;

    let common = render_common_skill(instance, guest_hostname);
    let codex = render_agent_skill("Codex");
    let claude = render_agent_skill("Claude");

    write_atomic_file(&skills_dir.join("common").join("yolobox.md"), &common)?;
    write_atomic_file(&skills_dir.join("codex").join("yolobox.md"), &codex)?;
    write_atomic_file(&skills_dir.join("claude").join("yolobox.md"), &claude)?;
    Ok(())
}

fn ensure_directory(path: &Path) -> Result<(), String> {
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_symlink() || !metadata.is_dir() => {
            fs::remove_file(path).map_err(|err| err.to_string())?;
        }
        Ok(_) => {}
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {}
        Err(err) => return Err(err.to_string()),
    }
    fs::create_dir_all(path).map_err(|err| err.to_string())
}

fn write_atomic_file(path: &Path, contents: &str) -> Result<(), String> {
    let parent = path
        .parent()
        .ok_or_else(|| format!("{} has no parent directory", path.display()))?;
    ensure_directory(parent)?;

    let file_name = path
        .file_name()
        .and_then(|name| name.to_str())
        .ok_or_else(|| format!("invalid file name {}", path.display()))?;
    let temp_path = parent.join(format!(".{file_name}.{}.tmp", std::process::id()));
    let _ = fs::remove_file(&temp_path);

    let mut file = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&temp_path)
        .map_err(|err| err.to_string())?;
    file.write_all(contents.as_bytes())
        .map_err(|err| err.to_string())?;
    drop(file);
    fs::rename(&temp_path, path).map_err(|err| err.to_string())
}

fn render_common_skill(instance: &Instance, guest_hostname: Option<&str>) -> String {
    let host_hint = guest_hostname
        .map(|value| format!("The current mDNS hostname is `{}.local`.\n", value))
        .unwrap_or_default();
    let sample_url = guest_hostname
        .map(|value| format!("For example: `http://{}.local:3000`\n", value))
        .unwrap_or_else(|| "For example: `http://<instance>.local:3000`\n".to_string());

    format!(
        "# yolobox\n\n\
You are running inside a Linux guest launched by `yolobox` on a macOS host.\n\n\
- Repo content is mounted at `/workspace`.\n\
- Host bridge files live under `{bridge_root}`.\n\
- Shared inputs from the host should usually go under `{inputs_root}`.\n\
- Host bridge scripts live under `{scripts_root}`.\n\
- Environment skills are available under `{skills_root}`.\n\
- Use `{scripts_root}/yolobox-open <path>` to request opening a host-visible artifact.\n\
- Use `{scripts_root}/code [path]` to open a shared host-backed directory in VS Code. With no path, it uses the current directory. This works for `/workspace`, `/yolobox`, and explicit shared mounts.\n\
- Use `{scripts_root}/finder [path]` to open a shared host-backed directory in Finder. With no path, it uses the current directory. This works for `/workspace`, `/yolobox`, and explicit shared mounts.\n\
- Use `{scripts_root}/yolobox-open-url <http://instance.local:port/...>` to request opening an mDNS URL on the host.\n\
- Use `{scripts_root}/yolobox-paste-image <path>` to request importing the current host clipboard image.\n\
- Do not use `open`, `xdg-open`, `pbpaste`, `xclip`, or other GUI/clipboard tools to reach the host directly.\n\
- The host cannot reach guest services at `localhost:<port>`.\n\
- Tell the user to open guest services via the instance mDNS hostname instead.\n\
{host_hint}\
{sample_url}\
Artifacts meant for host viewing should normally be written under `{artifacts_root}`.\n\
Imported clipboard images should normally be written under `{inputs_root}`.\n\
Instance name: `{instance_id}`\n",
        bridge_root = GUEST_BRIDGE_ROOT,
        inputs_root = GUEST_INPUTS_ROOT,
        scripts_root = GUEST_SCRIPTS_ROOT,
        skills_root = GUEST_SKILLS_ROOT,
        artifacts_root = GUEST_ARTIFACTS_ROOT,
        instance_id = instance.id,
    )
}

fn render_agent_skill(agent_name: &str) -> String {
    format!(
        "# yolobox for {agent_name}\n\n\
When you need the host to do something, prefer the narrow `yolobox-tools` commands over generic Linux desktop commands.\n\n\
- For host-visible artifacts: `/yolobox/scripts/yolobox-open /workspace/.artifacts/...`\n\
- For shared directories in VS Code: `/yolobox/scripts/code [path]` for `/workspace`, `/yolobox`, or an explicit shared mount\n\
- For shared directories in Finder: `/yolobox/scripts/finder [path]` for `/workspace`, `/yolobox`, or an explicit shared mount\n\
- For guest services on the host browser: `/yolobox/scripts/yolobox-open-url http://<instance>.local:<port>`\n\
- For host clipboard image import: `/yolobox/scripts/yolobox-paste-image /yolobox/inputs/<name>.png`\n\
- For dev servers, give the user an mDNS URL like `http://<instance>.local:<port>` instead of `http://localhost:<port>`\n"
    )
}

fn render_bridge_tool_script(verb: &str) -> String {
    let usage = if verb == "open-url" {
        format!("usage: yolobox-{verb} <http://instance.local:port/...>")
    } else if verb == "code" || verb == "finder" {
        format!("usage: {verb} [directory]")
    } else {
        format!("usage: yolobox-{verb} <absolute-path>")
    };
    let (arg_check, target_setup) = if verb == "open-url" {
        (
            format!(
                "if [ \"$#\" -ne 1 ]; then\n  echo \"{usage}\" >&2\n  exit 2\nfi"
            ),
            r#"target="$1"
case "$target" in
  http://*|https://*) ;;
  *)
    echo "url must start with http:// or https://" >&2
    exit 2
    ;;
esac
"#
            .to_string(),
        )
    } else if verb == "code" || verb == "finder" {
        (
            format!(
                "if [ \"$#\" -gt 1 ]; then\n  echo \"{usage}\" >&2\n  exit 2\nfi"
            ),
            r#"if [ "$#" -eq 0 ]; then
  target="$(pwd -P)"
else
  input="$1"
  case "$input" in
    /*) candidate="$input" ;;
    *) candidate="$(pwd -P)/$input" ;;
  esac
  if [ ! -d "$candidate" ]; then
    echo "directory not found: $input" >&2
    exit 2
  fi
  target="$(cd "$candidate" && pwd -P)"
fi
"#
            .to_string(),
        )
    } else {
        (
            format!(
                "if [ \"$#\" -ne 1 ]; then\n  echo \"{usage}\" >&2\n  exit 2\nfi"
            ),
            r#"target="$1"
case "$target" in
  /*) ;;
  *)
    echo "path must be absolute" >&2
    exit 2
    ;;
esac
"#
            .to_string(),
        )
    };
    let field_name = if verb == "open-url" { "url" } else { "path" };
    format!(
        r#"#!/bin/sh
set -eu

{arg_check}
{target_setup}

bridge_root="{bridge_root}"
requests_dir="$bridge_root/requests"
responses_dir="$bridge_root/responses"
mkdir -p "$requests_dir" "$responses_dir"

tmp_path="$(mktemp "$requests_dir/{tool}.XXXXXX.tmp")"
request_id="$(basename "$tmp_path" .tmp)"
request_path="$requests_dir/$request_id.req"
response_path="$responses_dir/$request_id.response"

cleanup() {{
  rm -f "$tmp_path"
}}
trap cleanup EXIT INT TERM

printf 'verb=%s\n{field_name}=%s\n' "{verb}" "$target" >"$tmp_path"
mv "$tmp_path" "$request_path"
trap - EXIT INT TERM

timeout="${{YOLOBOX_TOOLS_TIMEOUT_SECS:-120}}"
elapsed=0
while [ "$elapsed" -lt "$timeout" ]; do
  if [ -f "$response_path" ]; then
    ok="$(sed -n 's/^ok=//p' "$response_path" | tail -n1)"
    message="$(sed -n 's/^message=//p' "$response_path" | tail -n1)"
    if [ "$ok" = "1" ]; then
      if [ -n "$message" ]; then
        printf '%s\n' "$message"
      fi
      exit 0
    fi
    printf '%s\n' "${{message:-request failed}}" >&2
    exit 1
  fi
  sleep 1
  elapsed=$((elapsed + 1))
done

echo "timed out waiting for host bridge response" >&2
exit 1
"#,
        tool = verb,
        arg_check = arg_check,
        target_setup = target_setup,
        field_name = field_name,
        verb = verb,
        bridge_root = GUEST_BRIDGE_ROOT,
    )
}

fn process_pending_requests(instance: &Instance) -> Result<(), String> {
    let requests_dir = host_bridge_dir(instance).join("requests");
    if !requests_dir.exists() {
        return Ok(());
    }

    for entry in fs::read_dir(&requests_dir).map_err(|err| err.to_string())? {
        let entry = entry.map_err(|err| err.to_string())?;
        let request_path = entry.path();
        if request_path.extension() != Some(OsStr::new("req")) {
            continue;
        }
        process_request_file(instance, &request_path)?;
    }

    Ok(())
}

fn process_request_file(instance: &Instance, request_path: &Path) -> Result<(), String> {
    let processing_path = request_path.with_extension("processing");
    if fs::rename(request_path, &processing_path).is_err() {
        return Ok(());
    }
    if fs::symlink_metadata(&processing_path)
        .map_err(|err| err.to_string())?
        .file_type()
        .is_symlink()
    {
        let _ = fs::remove_file(&processing_path);
        return Ok(());
    }

    let response = match load_request(&processing_path) {
        Ok(request) => handle_request(instance, &request),
        Err(err) => Response {
            ok: false,
            message: err,
        },
    };

    let response_result = write_response(instance, &processing_path, &response);
    let _ = fs::remove_file(&processing_path);
    response_result
}

fn load_request(path: &Path) -> Result<Request, String> {
    let body = fs::read_to_string(path).map_err(|err| err.to_string())?;
    let mut verb = None;
    let mut path_target = None;
    let mut url_target = None;
    for line in body.lines() {
        let Some((key, value)) = line.split_once('=') else {
            continue;
        };
        match key {
            "verb" => {
                verb = Some(match value {
                    "open" => RequestVerb::Open,
                    "code" => RequestVerb::Code,
                    "finder" => RequestVerb::Finder,
                    "open-url" => RequestVerb::OpenUrl,
                    "paste-image" => RequestVerb::PasteImage,
                    other => return Err(format!("unsupported host bridge verb {other}")),
                });
            }
            "path" => path_target = Some(value.to_string()),
            "url" => url_target = Some(value.to_string()),
            _ => {}
        }
    }

    let verb = verb.ok_or_else(|| format!("request {} is missing verb", path.display()))?;
    let target = match verb {
        RequestVerb::Open | RequestVerb::Code | RequestVerb::Finder | RequestVerb::PasteImage => {
            let guest_path =
                path_target.ok_or_else(|| format!("request {} is missing path", path.display()))?;
            if !Path::new(&guest_path).is_absolute() {
                return Err(format!("request path {} must be absolute", guest_path));
            }
            guest_path
        }
        RequestVerb::OpenUrl => {
            let url = url_target.ok_or_else(|| format!("request {} is missing url", path.display()))?;
            if !url.starts_with("http://") && !url.starts_with("https://") {
                return Err(format!("request url {url} must start with http:// or https://"));
            }
            url
        }
    };

    Ok(Request { verb, target })
}

fn handle_request(instance: &Instance, request: &Request) -> Response {
    match request.verb {
        RequestVerb::Open => handle_open_request(instance, Path::new(&request.target)),
        RequestVerb::Code => handle_code_request(instance, Path::new(&request.target)),
        RequestVerb::Finder => handle_finder_request(instance, Path::new(&request.target)),
        RequestVerb::OpenUrl => handle_open_url_request(&request.target),
        RequestVerb::PasteImage => handle_paste_image_request(instance, Path::new(&request.target)),
    }
}

fn handle_open_request(instance: &Instance, guest_path: &Path) -> Response {
    match validate_open_target(instance, guest_path).and_then(open_on_host) {
        Ok(host_path) => Response {
            ok: true,
            message: format!("opened {}", host_path.display()),
        },
        Err(err) => Response {
            ok: false,
            message: err,
        },
    }
}

fn handle_code_request(instance: &Instance, guest_path: &Path) -> Response {
    match validate_shared_directory_target(instance, guest_path)
        .and_then(|host_path| open_code_on_host(instance, host_path))
    {
        Ok(host_path) => Response {
            ok: true,
            message: format!("opened {}", host_path.display()),
        },
        Err(err) => Response {
            ok: false,
            message: err,
        },
    }
}

fn handle_finder_request(instance: &Instance, guest_path: &Path) -> Response {
    match validate_shared_directory_target(instance, guest_path).and_then(open_in_finder_on_host) {
        Ok(host_path) => Response {
            ok: true,
            message: format!("opened {}", host_path.display()),
        },
        Err(err) => Response {
            ok: false,
            message: err,
        },
    }
}

fn handle_open_url_request(url: &str) -> Response {
    match validate_mdns_url(url).and_then(open_url_on_host) {
        Ok(opened_url) => Response {
            ok: true,
            message: format!("opened {opened_url}"),
        },
        Err(err) => Response {
            ok: false,
            message: err,
        },
    }
}

fn handle_paste_image_request(instance: &Instance, guest_path: &Path) -> Response {
    let host_path = match validate_paste_destination(instance, guest_path) {
        Ok(path) => path,
        Err(err) => {
            return Response {
                ok: false,
                message: err,
            };
        }
    };

    match confirm_clipboard_import(&instance.id, guest_path)
        .and_then(|approved| {
            if approved {
                export_clipboard_image(&host_path)?;
                Ok(())
            } else {
                Err("clipboard import denied".to_string())
            }
        }) {
        Ok(()) => Response {
            ok: true,
            message: format!("wrote {}", host_path.display()),
        },
        Err(err) => Response {
            ok: false,
            message: err,
        },
    }
}

fn write_response(instance: &Instance, request_path: &Path, response: &Response) -> Result<(), String> {
    let response_dir = host_bridge_dir(instance).join("responses");
    ensure_directory(&response_dir)?;
    let response_id = request_path
        .file_stem()
        .and_then(|stem| stem.to_str())
        .ok_or_else(|| format!("invalid request filename {}", request_path.display()))?;
    let final_path = response_dir.join(format!("{response_id}.response"));
    let temp_path = response_dir.join(format!("{response_id}.tmp"));
    let _ = fs::remove_file(&temp_path);
    let mut file = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&temp_path)
        .map_err(|err| err.to_string())?;
    writeln!(file, "ok={}", if response.ok { "1" } else { "0" }).map_err(|err| err.to_string())?;
    writeln!(file, "message={}", response.message.replace('\n', " ")).map_err(|err| err.to_string())?;
    fs::rename(temp_path, final_path).map_err(|err| err.to_string())
}

fn validate_open_target(instance: &Instance, guest_path: &Path) -> Result<PathBuf, String> {
    let host_path = map_allowlisted_guest_path(instance, guest_path, &[GUEST_ARTIFACTS_ROOT, GUEST_INPUTS_ROOT])?;
    let canonical_root = canonical_root_for_guest(instance, guest_path)?;
    let canonical_target = fs::canonicalize(&host_path)
        .map_err(|err| format!("failed to resolve {}: {err}", host_path.display()))?;
    if !canonical_target.starts_with(&canonical_root) {
        return Err(format!("{} escapes the allowed root", guest_path.display()));
    }
    if !canonical_target.is_file() {
        return Err(format!("{} is not a file", guest_path.display()));
    }

    match canonical_target
        .extension()
        .and_then(|ext| ext.to_str())
        .map(|ext| ext.to_ascii_lowercase())
        .as_deref()
    {
        Some("html" | "htm" | "svg" | "png" | "jpg" | "jpeg" | "pdf") => Ok(canonical_target),
        _ => Err(format!("{} is not an allowlisted artifact type", guest_path.display())),
    }
}

fn validate_paste_destination(instance: &Instance, guest_path: &Path) -> Result<PathBuf, String> {
    let host_path = map_allowlisted_guest_path(instance, guest_path, &[GUEST_INPUTS_ROOT])?;
    let canonical_root = canonical_root_for_guest(instance, guest_path)?;
    let parent = host_path
        .parent()
        .ok_or_else(|| format!("invalid destination {}", host_path.display()))?;
    ensure_descendant_directory(&canonical_root, parent)?;
    match host_path.extension().and_then(|ext| ext.to_str()) {
        Some(ext) if ext.eq_ignore_ascii_case("png") => {}
        _ => return Err(format!("{} must end in .png", guest_path.display())),
    }
    Ok(host_path)
}

fn validate_shared_directory_target(instance: &Instance, guest_path: &Path) -> Result<PathBuf, String> {
    let (host_root, host_path) = map_shared_directory_guest_path(instance, guest_path)?;
    let canonical_root = fs::canonicalize(&host_root)
        .map_err(|err| format!("failed to resolve {}: {err}", host_root.display()))?;
    let canonical_target = fs::canonicalize(&host_path)
        .map_err(|err| format!("failed to resolve {}: {err}", host_path.display()))?;
    if !canonical_target.starts_with(&canonical_root) {
        return Err(format!("{} escapes the allowed root", guest_path.display()));
    }
    if !canonical_target.is_dir() {
        return Err(format!("{} is not a directory", guest_path.display()));
    }
    Ok(canonical_target)
}

fn ensure_descendant_directory(root: &Path, path: &Path) -> Result<(), String> {
    let relative = path
        .strip_prefix(root)
        .map_err(|_| format!("{} escapes the allowed root {}", path.display(), root.display()))?;
    let mut current = root.to_path_buf();
    for component in relative.components() {
        match component {
            std::path::Component::CurDir => continue,
            std::path::Component::Normal(segment) => current.push(segment),
            _ => return Err(format!("{} escapes the allowed root {}", path.display(), root.display())),
        }

        match fs::symlink_metadata(&current) {
            Ok(metadata) if metadata.file_type().is_symlink() => {
                return Err(format!("{} escapes the allowed root {}", path.display(), root.display()))
            }
            Ok(metadata) if metadata.is_dir() => {}
            Ok(_) => return Err(format!("{} is not a directory", current.display())),
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
                fs::create_dir(&current).map_err(|create_err| create_err.to_string())?;
            }
            Err(err) => return Err(err.to_string()),
        }
    }
    Ok(())
}

fn canonical_root_for_guest(instance: &Instance, guest_path: &Path) -> Result<PathBuf, String> {
    let root = if guest_path.starts_with(Path::new(GUEST_ARTIFACTS_ROOT)) {
        instance.checkout_dir.join(".artifacts")
    } else if guest_path.starts_with(Path::new(GUEST_INPUTS_ROOT)) {
        host_bridge_dir(instance).join("inputs")
    } else {
        return Err(format!("{} is not in an allowlisted host bridge path", guest_path.display()));
    };

    fs::create_dir_all(&root).map_err(|err| err.to_string())?;
    fs::canonicalize(root).map_err(|err| err.to_string())
}

fn map_allowlisted_guest_path(
    instance: &Instance,
    guest_path: &Path,
    allowlisted_roots: &[&str],
) -> Result<PathBuf, String> {
    for root in allowlisted_roots {
        let guest_root = Path::new(root);
        if let Ok(relative) = guest_path.strip_prefix(guest_root) {
            let host_root = if *root == GUEST_ARTIFACTS_ROOT {
                instance.checkout_dir.join(".artifacts")
            } else {
                host_bridge_dir(instance).join("inputs")
            };
            return Ok(host_root.join(relative));
        }
    }

    Err(format!("{} is not in an allowlisted host bridge path", guest_path.display()))
}

fn map_shared_directory_guest_path(
    instance: &Instance,
    guest_path: &Path,
) -> Result<(PathBuf, PathBuf), String> {
    let mut mappings = vec![
        (PathBuf::from(GUEST_WORKSPACE_ROOT), instance.checkout_dir.clone()),
        (PathBuf::from(GUEST_BRIDGE_ROOT), host_bridge_dir(instance)),
    ];
    mappings.extend(
        instance
            .shares
            .iter()
            .map(|share| (share.guest_path.clone(), share.host_path.clone())),
    );
    mappings.sort_by(|left, right| {
        right.0.components().count().cmp(&left.0.components().count())
    });

    for (guest_root, host_root) in mappings {
        if let Ok(relative) = guest_path.strip_prefix(&guest_root) {
            return Ok((host_root.clone(), host_root.join(relative)));
        }
    }

    Err(format!(
        "{} is not inside a shared host-backed directory",
        guest_path.display()
    ))
}

fn open_on_host(path: PathBuf) -> Result<PathBuf, String> {
    let mut command = Command::new("open");
    prepare_host_gui_command(&mut command);
    let status = command
        .arg(&path)
        .status()
        .map_err(|err| format!("failed to launch macOS open for {}: {err}", path.display()))?;
    if status.success() {
        Ok(path)
    } else {
        Err(format!("open failed for {}", path.display()))
    }
}

fn open_code_on_host(_instance: &Instance, path: PathBuf) -> Result<PathBuf, String> {
    let status = Command::new("open")
        .arg("-a")
        .arg("Visual Studio Code")
        .arg(&path)
        .arg("--args")
        .arg("--disable-workspace-trust")
        .status()
        .map_err(|err| format!("failed to launch VS Code for {}: {err}", path.display()))?;
    if status.success() {
        Ok(path)
    } else {
        Err(format!("VS Code open failed for {}", path.display()))
    }
}

fn prepare_host_gui_command(command: &mut Command) {
    command.env_clear();
    copy_env_if_present(command, "HOME");
    copy_env_if_present(command, "PATH");
    copy_env_if_present(command, "TMPDIR");
    copy_env_if_present(command, "LANG");
    copy_env_if_present(command, "LC_ALL");
    copy_env_if_present(command, "USER");
    copy_env_if_present(command, "LOGNAME");
    copy_env_if_present(command, "SHELL");
    copy_env_if_present(command, "SSH_AUTH_SOCK");
    if let Ok(home) = std::env::var("HOME") {
        command.current_dir(home);
    }
}

fn copy_env_if_present(command: &mut Command, key: &str) {
    if let Some(value) = std::env::var_os(key) {
        command.env(key, value);
    }
}

fn open_in_finder_on_host(path: PathBuf) -> Result<PathBuf, String> {
    let mut command = Command::new("open");
    prepare_host_gui_command(&mut command);
    let status = command
        .arg(&path)
        .status()
        .map_err(|err| format!("failed to launch Finder for {}: {err}", path.display()))?;
    if status.success() {
        Ok(path)
    } else {
        Err(format!("Finder open failed for {}", path.display()))
    }
}

fn open_url_on_host(url: String) -> Result<String, String> {
    let mut command = Command::new("open");
    prepare_host_gui_command(&mut command);
    let status = command
        .arg(&url)
        .status()
        .map_err(|err| format!("failed to launch macOS open for {url}: {err}"))?;
    if status.success() {
        Ok(url)
    } else {
        Err("open failed for requested url".to_string())
    }
}

fn validate_mdns_url(url: &str) -> Result<String, String> {
    let (scheme, rest) = if let Some(value) = url.strip_prefix("http://") {
        ("http", value)
    } else if let Some(value) = url.strip_prefix("https://") {
        ("https", value)
    } else {
        return Err("url must start with http:// or https://".to_string());
    };
    if rest.is_empty() {
        return Err("url is missing a hostname".to_string());
    }
    let authority_end = rest.find(['/', '?', '#']).unwrap_or(rest.len());
    let authority = &rest[..authority_end];
    if authority.is_empty() || authority.contains('@') || authority.starts_with('[') {
        return Err("url must target an mDNS hostname ending in .local".to_string());
    }
    let host = match authority.rsplit_once(':') {
        Some((host, port))
            if !host.is_empty() && !port.is_empty() && port.chars().all(|ch| ch.is_ascii_digit()) =>
        {
            host
        }
        _ => authority,
    };
    let normalized_host = host.trim_end_matches('.').to_ascii_lowercase();
    if !normalized_host.ends_with(".local") {
        return Err("url must target an mDNS hostname ending in .local".to_string());
    }
    if !normalized_host
        .chars()
        .all(|ch| ch.is_ascii_alphanumeric() || ch == '-' || ch == '.')
    {
        return Err("url host contains unsupported characters".to_string());
    }
    Ok(format!("{scheme}://{rest}"))
}

fn confirm_clipboard_import(instance_id: &str, guest_path: &Path) -> Result<bool, String> {
    let script = r#"on run argv
set instanceName to item 1 of argv
set guestPath to item 2 of argv
try
    set promptText to "yolobox instance " & instanceName & " wants to import the current clipboard image to " & guestPath
    set response to display dialog promptText buttons {"Deny", "Allow"} default button "Allow" with icon caution
    if button returned of response is "Allow" then
        return "allow"
    end if
    return "deny"
on error number -128
    return "deny"
end try
end run"#;
    let output = Command::new("osascript")
        .arg("-e")
        .arg(script)
        .arg(instance_id)
        .arg(guest_path)
        .output()
        .map_err(|err| format!("failed to prompt for clipboard access: {err}"))?;
    if !output.status.success() {
        return Err("clipboard import prompt failed".to_string());
    }

    Ok(String::from_utf8_lossy(&output.stdout).trim() == "allow")
}

fn export_clipboard_image(path: &Path) -> Result<(), String> {
    let script = r#"on run argv
set destPath to item 1 of argv
try
    set imageData to the clipboard as «class PNGf»
on error
    error "Clipboard does not contain a PNG image."
end try
set fileRef to open for access POSIX file destPath with write permission
try
    set eof of fileRef to 0
    write imageData to fileRef
    close access fileRef
on error errMsg number errNum
    try
        close access fileRef
    end try
    error errMsg number errNum
end try
return "ok"
end run"#;
    let output = Command::new("osascript")
        .arg("-e")
        .arg(script)
        .arg(path)
        .output()
        .map_err(|err| format!("failed to read host clipboard image: {err}"))?;
    if output.status.success() {
        Ok(())
    } else {
        let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
        if stderr.is_empty() {
            Err("clipboard image export failed".to_string())
        } else {
            Err(stderr)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{
        GUEST_ARTIFACTS_ROOT, GUEST_INPUTS_ROOT, GUEST_REQUESTS_ROOT, GUEST_RESPONSES_ROOT,
        GUEST_SCRIPTS_ROOT, GUEST_SKILLS_ROOT, Response, canonical_root_for_guest,
        managed_mount_specs, managed_mounts, map_allowlisted_guest_path, render_bridge_tool_script,
        validate_mdns_url, validate_paste_destination, validate_shared_directory_target,
        write_response,
    };
    use crate::ports::PortMapping;
    use crate::state::{Instance, ShareMount};
    use std::fs;
    use std::path::{Path, PathBuf};

    fn sample_instance(root: &PathBuf) -> Instance {
        Instance {
            id: "demo".to_string(),
            repo: None,
            branch: None,
            instance_dir: root.join("instance"),
            base_image_id: "base-1".to_string(),
            base_image_name: "ubuntu".to_string(),
            base_image_path: root.join("base.img"),
            checkout_dir: root.join("checkout"),
            rootfs_path: root.join("branch.img"),
            rootfs_mb: 1024,
            host_port_base: 2200,
            ports: vec![PortMapping { host: 2200, guest: 22 }],
            shares: Vec::new(),
            guest_env: Vec::new(),
            created_unix: 0,
        }
    }

    #[test]
    fn managed_mounts_create_bridge_layout_and_skills() {
        let root = PathBuf::from("/tmp/yolobox-host-bridge-test");
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(root.join("instance").join("runtime")).unwrap();
        fs::create_dir_all(root.join("checkout")).unwrap();
        let mounts = managed_mounts(&sample_instance(&root), Some("demo-host")).unwrap();
        assert_eq!(mounts.len(), 5);
        assert!(mounts.iter().any(|mount| mount.guest_path == PathBuf::from(GUEST_REQUESTS_ROOT) && !mount.readonly));
        assert!(mounts.iter().any(|mount| mount.guest_path == PathBuf::from(GUEST_RESPONSES_ROOT) && mount.readonly));
        assert!(root.join("instance/runtime/yolobox/requests").is_dir());
        assert!(root.join("instance/runtime/yolobox/responses").is_dir());
        assert!(root.join("instance/runtime/yolobox/inputs").is_dir());
        assert!(root.join("instance/runtime/yolobox/scripts").join("code").is_file());
        assert!(root.join("instance/runtime/yolobox/scripts").join("finder").is_file());
        assert!(root.join("instance/runtime/yolobox/scripts").join("yolobox-open").is_file());
        assert!(root.join("instance/runtime/yolobox/scripts").join("yolobox-open-url").is_file());
        assert!(root.join("instance/runtime/yolobox/scripts").join("yolobox-paste-image").is_file());
        assert!(root.join("instance/runtime/yolobox/skills").join("common").join("yolobox.md").is_file());
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn allowlisted_guest_paths_map_to_host_roots() {
        let root = PathBuf::from("/tmp/yolobox-host-bridge-map-test");
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(root.join("instance").join("runtime")).unwrap();
        fs::create_dir_all(root.join("checkout").join(".artifacts")).unwrap();
        let instance = sample_instance(&root);
        managed_mounts(&instance, None).unwrap();

        let artifact = map_allowlisted_guest_path(
            &instance,
            Path::new("/workspace/.artifacts/report/index.html"),
            &[GUEST_ARTIFACTS_ROOT],
        )
        .unwrap();
        assert_eq!(artifact, root.join("checkout").join(".artifacts").join("report").join("index.html"));

        let input = map_allowlisted_guest_path(
            &instance,
            Path::new("/yolobox/inputs/clipboard.png"),
            &[GUEST_INPUTS_ROOT],
        )
        .unwrap();
        assert!(input.ends_with("runtime/yolobox/inputs/clipboard.png"));
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn canonical_root_targets_artifacts_and_inputs() {
        let root = PathBuf::from("/tmp/yolobox-host-bridge-root-test");
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(root.join("instance").join("runtime")).unwrap();
        fs::create_dir_all(root.join("checkout").join(".artifacts")).unwrap();
        let instance = sample_instance(&root);
        managed_mounts(&instance, None).unwrap();

        let artifact_root = canonical_root_for_guest(&instance, Path::new("/workspace/.artifacts/index.html")).unwrap();
        assert!(artifact_root.ends_with("checkout/.artifacts"));
        let inputs_root = canonical_root_for_guest(&instance, Path::new("/yolobox/inputs/test.png")).unwrap();
        assert!(inputs_root.ends_with("runtime/yolobox/inputs"));
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn bridge_tool_script_uses_shared_bridge_dirs() {
        let script = render_bridge_tool_script("paste-image");
        assert!(script.contains("bridge_root=\"/yolobox\""));
        assert!(script.contains("requests_dir=\"$bridge_root/requests\""));
        assert!(script.contains("responses_dir=\"$bridge_root/responses\""));
        assert!(!script.contains("rm -f \"$response_path\""));
    }

    #[test]
    fn open_url_script_requires_http_or_https() {
        let script = render_bridge_tool_script("open-url");
        assert!(script.contains("usage: yolobox-open-url <http://instance.local:port/...>"));
        assert!(script.contains("url must start with http:// or https://"));
        assert!(script.contains("url=%s"));
    }

    #[test]
    fn code_script_defaults_to_pwd_and_accepts_optional_directory() {
        let script = render_bridge_tool_script("code");
        assert!(script.contains("usage: code [directory]"));
        assert!(script.contains("if [ \"$#\" -gt 1 ]; then"));
        assert!(script.contains("target=\"$(pwd -P)\""));
        assert!(script.contains("target=\"$(cd \"$candidate\" && pwd -P)\""));
        assert!(script.contains("path=%s"));
    }

    #[test]
    fn mount_specs_split_ro_and_rw_bridge_paths() {
        let specs = managed_mount_specs();
        assert!(specs.iter().any(|spec| spec.guest_path == GUEST_REQUESTS_ROOT && !spec.readonly));
        assert!(specs.iter().any(|spec| spec.guest_path == GUEST_INPUTS_ROOT && !spec.readonly));
        assert!(specs.iter().any(|spec| spec.guest_path == GUEST_RESPONSES_ROOT && spec.readonly));
        assert!(specs.iter().any(|spec| spec.guest_path == GUEST_SCRIPTS_ROOT && spec.readonly));
        assert!(specs.iter().any(|spec| spec.guest_path == GUEST_SKILLS_ROOT && spec.readonly));
    }

    #[test]
    fn paste_destination_rejects_symlinked_input_parent() {
        let root = PathBuf::from("/tmp/yolobox-host-bridge-symlink-test");
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(root.join("instance").join("runtime")).unwrap();
        fs::create_dir_all(root.join("checkout").join(".artifacts")).unwrap();
        let instance = sample_instance(&root);
        managed_mounts(&instance, None).unwrap();

        let outside = root.join("outside");
        fs::create_dir_all(&outside).unwrap();
        #[cfg(unix)]
        std::os::unix::fs::symlink(&outside, root.join("instance/runtime/yolobox/inputs/escape")).unwrap();

        let result =
            validate_paste_destination(&instance, Path::new("/yolobox/inputs/escape/nested/file.png"));
        assert!(result.is_err());
        assert!(!outside.join("nested").exists());
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn write_response_does_not_follow_stale_temp_symlink() {
        let root = PathBuf::from("/tmp/yolobox-host-bridge-response-test");
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(root.join("instance").join("runtime")).unwrap();
        fs::create_dir_all(root.join("checkout").join(".artifacts")).unwrap();
        let instance = sample_instance(&root);
        managed_mounts(&instance, None).unwrap();

        let poisoned_target = root.join("poisoned.txt");
        fs::write(&poisoned_target, "keep").unwrap();
        #[cfg(unix)]
        std::os::unix::fs::symlink(
            &poisoned_target,
            root.join("instance/runtime/yolobox/responses/demo.tmp"),
        )
        .unwrap();

        write_response(
            &instance,
            Path::new("/tmp/demo.processing"),
            &Response {
                ok: true,
                message: "ok".to_string(),
            },
        )
        .unwrap();

        assert_eq!(fs::read_to_string(&poisoned_target).unwrap(), "keep");
        let response_path = root.join("instance/runtime/yolobox/responses/demo.response");
        assert!(response_path.is_file());
        assert!(fs::read_to_string(response_path).unwrap().contains("ok=1"));
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn mdns_url_validation_accepts_local_hosts_only() {
        assert_eq!(
            validate_mdns_url("http://demo-host.local:3000/path").unwrap(),
            "http://demo-host.local:3000/path"
        );
        assert_eq!(
            validate_mdns_url("https://foo.local").unwrap(),
            "https://foo.local"
        );
        assert!(validate_mdns_url("http://localhost:3000").is_err());
        assert!(validate_mdns_url("http://example.com").is_err());
        assert!(validate_mdns_url("ftp://demo.local/file").is_err());
        assert!(validate_mdns_url("http://user@demo.local").is_err());
    }

    #[test]
    fn shared_directory_validation_allows_workspace_bridge_and_manual_shares() {
        let root = PathBuf::from("/tmp/yolobox-host-bridge-directory-test");
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(root.join("instance").join("runtime")).unwrap();
        fs::create_dir_all(root.join("checkout").join("docs")).unwrap();
        fs::create_dir_all(root.join("manual-share").join("nested")).unwrap();
        let mut instance = sample_instance(&root);
        instance.shares.push(ShareMount {
            host_path: root.join("manual-share"),
            guest_path: PathBuf::from("/mnt/manual"),
        });
        managed_mounts(&instance, None).unwrap();

        assert!(
            validate_shared_directory_target(&instance, Path::new("/workspace/docs"))
                .unwrap()
                .ends_with("checkout/docs")
        );
        assert!(
            validate_shared_directory_target(&instance, Path::new("/yolobox/skills"))
                .unwrap()
                .ends_with("runtime/yolobox/skills")
        );
        assert!(
            validate_shared_directory_target(&instance, Path::new("/mnt/manual/nested"))
                .unwrap()
                .ends_with("manual-share/nested")
        );
        assert!(validate_shared_directory_target(&instance, Path::new("/etc")).is_err());
        let _ = fs::remove_dir_all(&root);
    }
}
