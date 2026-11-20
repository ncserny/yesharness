//! What's in a channel's project folder: Git/GitHub, AGENTS.md, TASKS.md, docs/.
//! Read straight from the files (no `git` needed); refreshed by the heartbeat every minute.

use std::path::{Path, PathBuf};

#[derive(Clone, Debug, Default, PartialEq)]
pub struct ProjectInfo {
    pub exists: bool,
    /// Inside a Git repository.
    pub git: bool,
    pub branch: Option<String>,
    /// "owner/repo" when the origin remote is on GitHub.
    pub github: Option<String>,
    /// Non-empty lines in AGENTS.md (None = no file, Some(0) = empty file).
    pub agents_md: Option<usize>,
    pub agents_path: Option<PathBuf>,
    /// (open, done) checkboxes in TASKS.md, None = no file.
    pub tasks: Option<(usize, usize)>,
    pub tasks_path: Option<PathBuf>,
    /// Files in docs/ (or doc/).
    pub docs: usize,
    pub docs_path: Option<PathBuf>,
}

pub fn inspect(folder: &Path) -> ProjectInfo {
    let mut p = ProjectInfo { exists: folder.is_dir(), ..Default::default() };
    if !p.exists {
        return p;
    }
    if let Some(git_dir) = find_git_dir(folder) {
        p.git = true;
        p.branch = std::fs::read_to_string(git_dir.join("HEAD"))
            .ok()
            .and_then(|h| h.trim().strip_prefix("ref: refs/heads/").map(String::from));
        // Worktrees keep the config in the main repository ("commondir").
        let common = std::fs::read_to_string(git_dir.join("commondir")).ok().map(|c| git_dir.join(c.trim())).unwrap_or(git_dir);
        if let Ok(cfg) = std::fs::read_to_string(common.join("config")) {
            p.github = origin_url(&cfg).and_then(|u| github_repo(&u));
        }
    }
    let file = |name: &str| -> Option<PathBuf> {
        std::fs::read_dir(folder).ok()?.flatten().map(|e| e.path()).find(|f| {
            f.is_file() && f.file_name().is_some_and(|n| n.to_string_lossy().eq_ignore_ascii_case(name))
        })
    };
    if let Some(f) = file("AGENTS.md") {
        let text = std::fs::read_to_string(&f).unwrap_or_default();
        p.agents_md = Some(text.lines().filter(|l| !l.trim().is_empty()).count());
        p.agents_path = Some(f);
    }
    if let Some(f) = file("TASKS.md") {
        let text = std::fs::read_to_string(&f).unwrap_or_default();
        let (mut open, mut done) = (0, 0);
        for l in text.lines() {
            let l = l.trim_start().trim_start_matches(['-', '*', '+']).trim_start();
            if l.starts_with("[ ]") {
                open += 1;
            } else if l.starts_with("[x]") || l.starts_with("[X]") {
                done += 1;
            }
        }
        p.tasks = Some((open, done));
        p.tasks_path = Some(f);
    }
    for d in ["docs", "doc", "Docs"] {
        let dir = folder.join(d);
        if dir.is_dir() {
            p.docs = count_files(&dir, 0);
            p.docs_path = Some(dir);
            break;
        }
    }
    p
}

/// The repository's .git directory for `folder` or any parent (stops at the home folder).
fn find_git_dir(folder: &Path) -> Option<PathBuf> {
    let home = dirs::home_dir();
    let mut dir = Some(folder);
    while let Some(d) = dir {
        let g = d.join(".git");
        if g.is_dir() {
            return Some(g);
        }
        if g.is_file() {
            // Worktree / submodule: "gitdir: <path>"
            let text = std::fs::read_to_string(&g).ok()?;
            let target = text.trim().strip_prefix("gitdir:")?.trim();
            let path = PathBuf::from(target);
            return Some(if path.is_absolute() { path } else { d.join(path) });
        }
        if home.as_deref() == Some(d) {
            break;
        }
        dir = d.parent();
    }
    None
}

fn origin_url(config: &str) -> Option<String> {
    let mut in_origin = false;
    let mut first_url = None;
    for line in config.lines() {
        let l = line.trim();
        if l.starts_with('[') {
            in_origin = l == "[remote \"origin\"]";
            continue;
        }
        if let Some(v) = l.strip_prefix("url").map(str::trim).and_then(|r| r.strip_prefix('=')) {
            let v = v.trim().to_string();
            if in_origin {
                return Some(v);
            }
            first_url.get_or_insert(v);
        }
    }
    first_url
}

/// git@github.com:owner/repo.git, https://github.com/owner/repo(.git), ssh://git@github.com/owner/repo
pub fn github_repo(url: &str) -> Option<String> {
    let i = url.find("github.com")? + "github.com".len();
    let rest = url[i..].trim_start_matches([':', '/']);
    let rest = rest.trim_end_matches('/').trim_end_matches(".git");
    let mut parts = rest.split('/');
    let (owner, repo) = (parts.next()?, parts.next()?);
    (!owner.is_empty() && !repo.is_empty()).then(|| format!("{owner}/{repo}"))
}

fn count_files(dir: &Path, depth: usize) -> usize {
    if depth > 5 {
        return 0;
    }
    let Ok(entries) = std::fs::read_dir(dir) else { return 0 };
    let mut n = 0;
    for e in entries.flatten() {
        let name = e.file_name();
        if name.to_string_lossy().starts_with('.') {
            continue;
        }
        let path = e.path();
        if path.is_dir() {
            n += count_files(&path, depth + 1);
        } else {
            n += 1;
        }
        if n > 999 {
            break;
        }
    }
    n
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_github_remotes() {
        assert_eq!(github_repo("git@github.com:acme/yesharness.git").as_deref(), Some("acme/yesharness"));
        assert_eq!(github_repo("https://github.com/acme/site").as_deref(), Some("acme/site"));
        assert_eq!(github_repo("ssh://git@github.com/acme/site.git/").as_deref(), Some("acme/site"));
        assert_eq!(github_repo("https://gitlab.com/acme/site"), None);
    }

    #[test]
    fn inspects_a_project_folder() {
        let dir = std::env::temp_dir().join(format!("yh-proj-{}", crate::model::new_id()));
        std::fs::create_dir_all(dir.join(".git")).unwrap();
        std::fs::create_dir_all(dir.join("docs/api")).unwrap();
        std::fs::write(dir.join(".git/HEAD"), "ref: refs/heads/main\n").unwrap();
        std::fs::write(
            dir.join(".git/config"),
            "[core]\n\tbare = false\n[remote \"origin\"]\n\turl = git@github.com:acme/rocket.git\n\tfetch = +refs/heads/*:refs/remotes/origin/*\n",
        )
        .unwrap();
        std::fs::write(dir.join("AGENTS.md"), "# Agents\n\nUse pnpm.\n").unwrap();
        std::fs::write(dir.join("TASKS.md"), "- [ ] one\n- [x] two\n- [ ] three\n").unwrap();
        std::fs::write(dir.join("docs/a.md"), "a").unwrap();
        std::fs::write(dir.join("docs/api/b.md"), "b").unwrap();
        let p = inspect(&dir);
        assert!(p.git);
        assert_eq!(p.branch.as_deref(), Some("main"));
        assert_eq!(p.github.as_deref(), Some("acme/rocket"));
        assert_eq!(p.agents_md, Some(2));
        assert_eq!(p.tasks, Some((2, 1)));
        assert_eq!(p.docs, 2);

        std::fs::write(dir.join("AGENTS.md"), "  \n\n").unwrap();
        assert_eq!(inspect(&dir).agents_md, Some(0), "empty AGENTS.md");
        let _ = std::fs::remove_dir_all(dir);
    }
}
