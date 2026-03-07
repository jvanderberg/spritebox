use std::env;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{SystemTime, UNIX_EPOCH};

pub fn ensure_checkout(
    checkout_dir: &Path,
    repo: &str,
    branch: &str,
    new_branch: bool,
    from: Option<&str>,
) -> Result<(), String> {
    let needs_clone = !checkout_dir.join(".git").exists();
    if needs_clone {
        run_git(&["clone", repo, "."], checkout_dir)?;
    } else {
        let origin = git_output(&["remote", "get-url", "origin"], checkout_dir)?;
        if origin.trim() != repo {
            return Err(format!(
                "checkout at {} points to origin {} instead of {}",
                checkout_dir.display(),
                origin.trim(),
                repo
            ));
        }
        return Ok(());
    }

    run_git(&["fetch", "origin", "--prune"], checkout_dir)?;

    if new_branch {
        let base_branch = from
            .map(str::to_string)
            .unwrap_or(default_remote_branch(checkout_dir)?);
        let remote_ref = format!("origin/{base_branch}");
        if ref_exists(checkout_dir, &format!("refs/heads/{branch}"))? {
            run_git(&["checkout", branch], checkout_dir)?;
        } else {
            run_git(&["checkout", "-b", branch, &remote_ref], checkout_dir)?;
        }
        return Ok(());
    }

    if ref_exists(checkout_dir, &format!("refs/heads/{branch}"))? {
        run_git(&["checkout", branch], checkout_dir)?;
        return Ok(());
    }

    if ref_exists(checkout_dir, &format!("refs/remotes/origin/{branch}"))? {
        run_git(
            &[
                "checkout",
                "-b",
                branch,
                "--track",
                &format!("origin/{branch}"),
            ],
            checkout_dir,
        )?;
        return Ok(());
    }

    Err(format!(
        "branch {branch} does not exist; rerun with --new-branch to create it"
    ))
}

pub fn list_recent_remote_branches(repo: &str, limit: usize) -> Result<Vec<String>, String> {
    let temp_dir = create_temp_git_dir()?;
    let result = (|| {
        run_git(&["init", "--bare"], &temp_dir)?;
        run_git(&["remote", "add", "origin", repo], &temp_dir)?;
        run_git(
            &[
                "fetch",
                "--quiet",
                "--depth=1",
                "origin",
                "+refs/heads/*:refs/remotes/origin/*",
            ],
            &temp_dir,
        )?;
        let _ = run_git(&["remote", "set-head", "origin", "-a"], &temp_dir);

        let mut branches = git_output(
            &[
                "for-each-ref",
                "--sort=-committerdate",
                "--format=%(refname:lstrip=3)",
                "refs/remotes/origin",
            ],
            &temp_dir,
        )?
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty() && *line != "HEAD")
        .map(str::to_string)
        .collect::<Vec<_>>();

        branches.sort_by_key(|branch| branch == "HEAD");
        branches.dedup();

        if let Ok(default_branch) = default_remote_branch(&temp_dir) {
            if let Some(index) = branches.iter().position(|branch| branch == &default_branch) {
                let branch = branches.remove(index);
                branches.insert(0, branch);
            }
        }

        branches.truncate(limit);
        Ok(branches)
    })();
    let _ = fs::remove_dir_all(&temp_dir);
    result
}

fn default_remote_branch(checkout_dir: &Path) -> Result<String, String> {
    let symbolic_ref = git_output(
        &["symbolic-ref", "--short", "refs/remotes/origin/HEAD"],
        checkout_dir,
    )?;
    symbolic_ref
        .trim()
        .strip_prefix("origin/")
        .map(str::to_string)
        .ok_or_else(|| "could not determine origin default branch".to_string())
}

fn ref_exists(checkout_dir: &Path, reference: &str) -> Result<bool, String> {
    let status = Command::new("git")
        .args(["show-ref", "--verify", "--quiet", reference])
        .current_dir(checkout_dir)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map_err(|err| err.to_string())?;
    Ok(status.success())
}

fn run_git(args: &[&str], checkout_dir: &Path) -> Result<(), String> {
    let status = Command::new("git")
        .args(args)
        .current_dir(checkout_dir)
        .status()
        .map_err(|err| err.to_string())?;

    if status.success() {
        Ok(())
    } else {
        Err(format!("git {} failed", args.join(" ")))
    }
}

fn git_output(args: &[&str], checkout_dir: &Path) -> Result<String, String> {
    let output = Command::new("git")
        .args(args)
        .current_dir(checkout_dir)
        .output()
        .map_err(|err| err.to_string())?;

    if !output.status.success() {
        return Err(format!("git {} failed", args.join(" ")));
    }

    String::from_utf8(output.stdout).map_err(|err| err.to_string())
}

fn create_temp_git_dir() -> Result<PathBuf, String> {
    let base = env::temp_dir();
    let pid = std::process::id();
    for attempt in 0..64u32 {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_err(|err| err.to_string())?
            .as_nanos();
        let path = base.join(format!("yolobox-git-{pid}-{nanos}-{attempt}"));
        match fs::create_dir(&path) {
            Ok(()) => return Ok(path),
            Err(err) if err.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(err) => return Err(err.to_string()),
        }
    }

    Err("failed to create temporary git directory".to_string())
}
