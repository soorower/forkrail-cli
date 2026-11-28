//! What a deployment's record says of where it came from: who deployed it, and the commit the
//! project folder was at. Both are read with the `git` command and are best effort: without git,
//! a checkout or a GitHub remote the field is left out, and the deploy goes on.

use std::path::Path;
use std::process::{Command, Stdio};

use crate::state::GitCommit;

/// Who is deploying: git's `user.name` as the project folder sees it (the repository's own, else
/// the user's), else the OS user's name. Never an email address.
pub fn deployer(dir: &Path) -> Option<String> {
    git(dir, &["config", "user.name"]).or_else(os_user)
}

fn os_user() -> Option<String> {
    ["USER", "LOGNAME", "USERNAME"].iter().filter_map(|key| std::env::var(key).ok()).map(|v| v.trim().to_owned()).find(|v| !v.is_empty())
}

/// The commit `dir` is at, if it's in a checkout whose `origin` is a GitHub repository. A folder
/// inside a repository that ignores it (a build's output, another project kept beside the code)
/// isn't that repository's, so it has no commit.
pub fn commit(dir: &Path) -> Option<GitCommit> {
    if git(dir, &["rev-parse", "--is-inside-work-tree"])? != "true" || ignored(dir) {
        return None;
    }
    let repo = github_repo(&git(dir, &["remote", "get-url", "origin"])?)?;
    // The abbreviated and whole hashes, the author, then the message as it was written. No
    // commits yet: no commit.
    let head = git(dir, &["log", "-1", "--format=%h%n%H%n%an%n%B"])?;
    let mut lines = head.lines();
    let sha = lines.next()?.to_owned();
    let full_sha = lines.next().map(str::to_owned);
    let author = lines.next().map(|a| a.trim().to_owned()).filter(|a| !a.is_empty());
    let message = lines.next().unwrap_or_default().trim().to_owned();
    // A branch's name, or `HEAD` when a commit is checked out rather than a branch.
    let branch = git(dir, &["rev-parse", "--abbrev-ref", "HEAD"])?;
    Some(GitCommit { repo, branch, sha, message, full_sha, author })
}

/// `owner/name` from a GitHub remote's URL, in the forms git takes: `https://github.com/owner/name.git`,
/// `git@github.com:owner/name.git`, `ssh://git@github.com/owner/name`. Only the two names are
/// kept, so a token in the URL goes no further. `None` for any other host.
fn github_repo(url: &str) -> Option<String> {
    let url = url.trim();
    let (authority, path) = match url.split_once("://") {
        Some((scheme, rest)) => {
            if !["https", "http", "ssh", "git", "git+ssh"].contains(&scheme.to_ascii_lowercase().as_str()) {
                return None;
            }
            rest.split_once('/')?
        }
        // scp-like: `[user@]host:path`.
        None => url.split_once(':')?,
    };
    let host = authority.rsplit_once('@').map_or(authority, |(_, host)| host);
    let host = host.split_once(':').map_or(host, |(host, _port)| host).to_ascii_lowercase();
    // `ssh.github.com` is GitHub's SSH over port 443.
    if !["github.com", "www.github.com", "ssh.github.com"].contains(&host.as_str()) {
        return None;
    }
    let path = path.trim_matches('/');
    let (owner, name) = path.strip_suffix(".git").unwrap_or(path).split_once('/')?;
    let valid = |part: &str| {
        !part.is_empty() && part != "." && part != ".." && part.bytes().all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.'))
    };
    (valid(owner) && valid(name)).then(|| format!("{owner}/{name}"))
}

/// What `git ARGS` prints when run in `dir`, trimmed, if git is there, succeeds and prints anything.
fn git(dir: &Path, args: &[&str]) -> Option<String> {
    let out = Command::new("git").arg("-C").arg(dir).args(args).stdin(Stdio::null()).stderr(Stdio::null()).output().ok()?;
    let text = String::from_utf8_lossy(&out.stdout).trim().to_owned();
    (out.status.success() && !text.is_empty()).then_some(text)
}

/// Whether the enclosing repository ignores `dir` itself.
fn ignored(dir: &Path) -> bool {
    Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(["check-ignore", "--quiet", "."])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok_and(|status| status.success())
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::*;

    #[test]
    fn github_remotes() {
        for url in [
            "https://github.com/soorower/forkrail.git",
            "https://github.com/soorower/forkrail",
            "https://github.com/soorower/forkrail/",
            "https://GitHub.com/soorower/forkrail.git",
            "git@github.com:soorower/forkrail.git",
            "git@github.com:soorower/forkrail",
            "ssh://git@github.com/soorower/forkrail.git",
            "ssh://git@ssh.github.com:443/soorower/forkrail.git",
            "git://github.com/soorower/forkrail.git",
            // A token in the URL stays out of the record.
            "https://x-access-token:ghp_secret@github.com/soorower/forkrail.git\n",
        ] {
            assert_eq!(github_repo(url).as_deref(), Some("soorower/forkrail"), "{url}");
        }
        assert_eq!(github_repo("git@github.com:vercel/next.js.git").as_deref(), Some("vercel/next.js"));
        for url in [
            "https://gitlab.com/owner/name.git",
            "git@bitbucket.org:owner/name.git",
            "https://github.example.com/owner/name.git",
            "https://notgithub.com/owner/name",
            // An SSH host alias: which host it is, only the user's SSH config says.
            "git@github-work:owner/name.git",
            "https://github.com/owner",
            "https://github.com/owner/name/tree/main",
            "https://github.com//name",
            "/srv/git/name.git",
            "../name",
            "file:///srv/git/owner/name.git",
            "",
        ] {
            assert_eq!(github_repo(url), None, "{url}");
        }
    }

    /// Run git in `dir` with settings of its own, whatever the machine's are (a signing key, hooks,
    /// another default branch).
    fn run(dir: &Path, args: &[&str]) -> bool {
        Command::new("git")
            .arg("-C")
            .arg(dir)
            .args(["-c", "commit.gpgsign=false", "-c", "core.hooksPath=/dev/null", "-c", "user.email=ada@example.com"])
            .args(args)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .is_ok_and(|status| status.success())
    }

    fn temp_dir() -> PathBuf {
        let dir = std::env::temp_dir().join(format!("forkrail-git-{}", forkrail_common::random_hex(6)));
        std::fs::create_dir_all(&dir).unwrap();
        // Through symlinks, as `deploy` hands it over (macOS's /var is one).
        dir.canonicalize().unwrap()
    }

    #[test]
    fn commit_of_a_checkout() {
        let dir = temp_dir();
        // Not a checkout: no commit. (The deployer is still someone: git's user, or the OS's.)
        assert_eq!(commit(&dir), None);
        if !run(&dir, &["init", "--quiet", "--initial-branch=main"]) {
            eprintln!("skipped: no git here");
            return;
        }
        assert!(run(&dir, &["config", "user.name", "Ada Lovelace"]));
        assert_eq!(deployer(&dir).as_deref(), Some("Ada Lovelace"));
        // A checkout without a remote, then one without a commit.
        assert_eq!(commit(&dir), None);
        assert!(run(&dir, &["remote", "add", "origin", "git@github.com:ada/engine.git"]));
        assert_eq!(commit(&dir), None);
        std::fs::create_dir_all(dir.join("api")).unwrap();
        std::fs::write(dir.join("api/main.py"), "").unwrap();
        std::fs::write(dir.join(".gitignore"), "/.output/\n").unwrap();
        assert!(run(&dir, &["add", "."]));
        assert!(run(&dir, &["commit", "--quiet", "-m", "Add the mill\n\nIt grinds numbers.\nSlowly."]));

        let at = commit(&dir).unwrap();
        assert_eq!((at.repo.as_str(), at.branch.as_str(), at.message.as_str()), ("ada/engine", "main", "Add the mill"));
        assert!(at.sha.len() >= 7 && at.sha.bytes().all(|b| b.is_ascii_hexdigit()), "{}", at.sha);
        let full = at.full_sha.as_deref().unwrap();
        assert!(full.len() == 40 && full.starts_with(&at.sha), "{full}");
        assert_eq!(at.author.as_deref(), Some("Ada Lovelace"));
        // A project in a folder of the repository is at the repository's commit; one the
        // repository ignores isn't part of it.
        assert_eq!(commit(&dir.join("api")), Some(at.clone()));
        std::fs::create_dir_all(dir.join(".output/server")).unwrap();
        assert_eq!(commit(&dir.join(".output")), None);
        assert_eq!(commit(&dir.join(".output/server")), None);

        // On another branch, and then on no branch.
        assert!(run(&dir, &["checkout", "--quiet", "-b", "feature/logs"]));
        assert_eq!(commit(&dir).unwrap().branch, "feature/logs");
        assert!(run(&dir, &["checkout", "--quiet", "--detach"]));
        assert_eq!(commit(&dir), Some(GitCommit { branch: "HEAD".into(), ..at.clone() }));

        // A remote that isn't GitHub.
        assert!(run(&dir, &["remote", "set-url", "origin", "https://gitlab.com/ada/engine.git"]));
        assert_eq!(commit(&dir), None);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
