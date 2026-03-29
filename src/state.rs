/// Derive a sprite name from the instance selector (repo+branch, name, or base).
pub fn sprite_name(
    name: Option<&str>,
    repo: Option<&str>,
    branch: Option<&str>,
) -> Result<String, String> {
    match (name, repo, branch) {
        (Some(name), None, None) => {
            let id = slugify(name);
            if id.is_empty() {
                Err("instance name must contain at least one alphanumeric character".to_string())
            } else {
                Ok(id)
            }
        }
        (None, Some(repo), Some(branch)) => {
            let repo_slug = slugify(&repo_basename(repo));
            let branch_slug = slugify(branch);
            if repo_slug.is_empty() || branch_slug.is_empty() {
                return Err(
                    "repo and branch must contain at least one alphanumeric character".to_string(),
                );
            }
            Ok(format!("{repo_slug}-{branch_slug}"))
        }
        (Some(_), _, _) => Err("--name cannot be combined with --repo/--branch".to_string()),
        (None, Some(_), None) | (None, None, Some(_)) => {
            Err("--repo and --branch must be provided together".to_string())
        }
        (None, None, None) => Err("provide --repo/--branch or --name".to_string()),
    }
}

fn repo_basename(repo: &str) -> String {
    let trimmed = repo.trim_end_matches('/');
    trimmed
        .rsplit(['/', ':'])
        .next()
        .unwrap_or(trimmed)
        .trim_end_matches(".git")
        .to_string()
}

fn slugify(value: &str) -> String {
    let mut slug = String::new();
    let mut previous_dash = false;

    for ch in value.chars() {
        let normalized = if ch.is_ascii_alphanumeric() {
            ch.to_ascii_lowercase()
        } else {
            '-'
        };
        if normalized == '-' {
            if !previous_dash {
                slug.push(normalized);
                previous_dash = true;
            }
        } else {
            slug.push(normalized);
            previous_dash = false;
        }
    }

    slug.trim_matches('-').chars().take(48).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn repo_branch_produces_slug() {
        let name = sprite_name(
            None,
            Some("git@github.com:org/my-repo.git"),
            Some("feature/cool-thing"),
        )
        .unwrap();
        assert_eq!(name, "my-repo-feature-cool-thing");
    }

    #[test]
    fn standalone_name_slugifies() {
        let name = sprite_name(Some("My Tools Box"), None, None).unwrap();
        assert_eq!(name, "my-tools-box");
    }

    #[test]
    fn name_cannot_combine_with_repo() {
        assert!(sprite_name(Some("foo"), Some("repo"), Some("branch")).is_err());
    }

    #[test]
    fn repo_without_branch_fails() {
        assert!(sprite_name(None, Some("repo"), None).is_err());
    }

    #[test]
    fn no_args_fails() {
        assert!(sprite_name(None, None, None).is_err());
    }

    #[test]
    fn slug_truncates_at_48_chars() {
        let long_name = "a".repeat(100);
        let name = sprite_name(Some(&long_name), None, None).unwrap();
        assert_eq!(name.len(), 48);
    }
}
