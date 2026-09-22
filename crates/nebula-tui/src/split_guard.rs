//! The SPLIT GUARD — paths a project says must travel in a change of their
//! own, and whether a checkout is currently mixing them with anything else.
//!
//! Some files carry a deploy step the code around them does not: a database
//! migration ships, and is deployed, on its own, before (expand) or after
//! (contract) the code that uses it. A branch that quietly grew both is a
//! rule broken long before anyone opens the pull request, and the cost of
//! noticing late is a split-and-reorder of work already written.
//!
//! So a project names the paths that must stand alone (`isolate_paths`) and
//! the WORKTREES PANEL badges any checkout whose change touches both those
//! and anything else. Like the dirty-file count and the pull request rows,
//! this is the TUI's own git, off the render path on the blocking pool; the
//! DAEMON's only git polling stays the structural WORKTREE SYNC.

use crate::git_diff::{changed_files, run_git};
use std::path::Path;

/// What the guard found in one checkout.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Verdict {
    /// Changed paths matching the project's `isolate_paths`.
    pub isolated: Vec<String>,
    /// Everything else that changed.
    pub other: Vec<String>,
    /// Whether the base ref resolved. False means only the working tree was
    /// read — no `origin`, an unfetched base, a fresh checkout — so a clean
    /// verdict is "nothing seen yet", not "nothing there".
    pub base_known: bool,
}

impl Verdict {
    /// The badge condition: the change carries isolated paths *and*
    /// something else, so it cannot ship as one PR.
    pub fn is_split(&self) -> bool {
        !self.isolated.is_empty() && !self.other.is_empty()
    }
}

/// Classify `paths` against the project's isolate patterns.
pub fn classify(paths: impl IntoIterator<Item = String>, patterns: &[String]) -> Verdict {
    let mut v = Verdict::default();
    for path in paths {
        if patterns.iter().any(|p| matches(p, &path)) {
            v.isolated.push(path);
        } else {
            v.other.push(path);
        }
    }
    v.isolated.sort();
    v.isolated.dedup();
    v.other.sort();
    v.other.dedup();
    v
}

/// Every path this checkout's change touches: what the branch committed
/// since it left `base`, plus what is uncommitted on top. The two together
/// are what a pull request from here would carry, which is the thing the
/// rule is about — a migration already committed is just as much a split as
/// one still dirty in the tree.
pub fn changed_paths(root: &Path, base: &str) -> Result<(Vec<String>, bool), String> {
    let mut paths = Vec::new();
    // `--merge-base` diffs against where the branch left the base rather
    // than against the base's tip, so commits that landed on main in the
    // meantime are not counted as this branch's work.
    let committed = run_git(root, &["diff", "--name-only", "--merge-base", base, "HEAD"])?;
    let base_known = committed.status.success();
    if base_known {
        paths.extend(
            String::from_utf8_lossy(&committed.stdout)
                .lines()
                .filter(|l| !l.trim().is_empty())
                .map(str::to_string),
        );
    }
    paths.extend(changed_files(root)?.into_iter().map(|f| f.path));
    Ok((paths, base_known))
}

/// The guard's answer for one checkout. No patterns means the project never
/// asked for the guard, and the read is skipped entirely.
pub fn verdict(root: &Path, base: &str, patterns: &[String]) -> Result<Verdict, String> {
    if patterns.is_empty() {
        return Ok(Verdict::default());
    }
    let (paths, base_known) = changed_paths(root, base)?;
    let mut v = classify(paths, patterns);
    v.base_known = base_known;
    Ok(v)
}

/// Glob match over a repo-relative path. `*` spans one segment, `**` spans
/// any number, and a pattern with no wildcard at all matches that path or
/// anything under it — so `prisma/migrations` needs no `/**` to mean the
/// directory, which is how everyone writes it the first time.
pub fn matches(pattern: &str, path: &str) -> bool {
    let pattern = pattern.trim();
    if pattern.is_empty() {
        return false;
    }
    if !pattern.contains(['*', '?']) {
        let p = pattern.trim_end_matches('/');
        return path == p || path.starts_with(&format!("{p}/"));
    }
    match_here(pattern.as_bytes(), path.as_bytes())
}

/// Backtracking matcher: `**` crosses `/`, `*` does not, `?` is one
/// non-`/` byte. Patterns are short and paths are one line of git output,
/// so the simple form is fast enough and has no dependency behind it.
fn match_here(p: &[u8], s: &[u8]) -> bool {
    if p.is_empty() {
        return s.is_empty();
    }
    if p.starts_with(b"**") {
        let rest = &p[2..];
        // `**/x` should also match a bare `x` at the root.
        let skipped = rest.strip_prefix(b"/").unwrap_or(rest);
        if match_here(skipped, s) {
            return true;
        }
        for i in 0..s.len() {
            if match_here(rest, &s[i..]) || match_here(skipped, &s[i + 1..]) {
                return true;
            }
        }
        return false;
    }
    if p[0] == b'*' {
        let rest = &p[1..];
        if match_here(rest, s) {
            return true;
        }
        for i in 0..s.len() {
            if s[i] == b'/' {
                break;
            }
            if match_here(rest, &s[i + 1..]) {
                return true;
            }
        }
        return false;
    }
    if !s.is_empty() && (p[0] == b'?' && s[0] != b'/' || p[0] == s[0]) {
        return match_here(&p[1..], &s[1..]);
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;
    use std::process::Command;

    fn git(repo: &PathBuf, args: &[&str]) {
        let out = Command::new("git")
            .arg("-C")
            .arg(repo)
            .args(args)
            .output()
            .expect("run git");
        assert!(
            out.status.success(),
            "git {args:?}: {}",
            String::from_utf8_lossy(&out.stderr)
        );
    }

    fn make_repo(dir: &tempfile::TempDir) -> PathBuf {
        let repo = dir.path().join("repo");
        std::fs::create_dir(&repo).unwrap();
        git(&repo, &["init", "-b", "main"]);
        git(&repo, &["config", "user.email", "t@t"]);
        git(&repo, &["config", "user.name", "t"]);
        std::fs::write(repo.join("README.md"), "x").unwrap();
        git(&repo, &["add", "-A"]);
        git(&repo, &["commit", "-m", "init"]);
        repo
    }

    fn write(repo: &Path, rel: &str, body: &str) {
        let p = repo.join(rel);
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(p, body).unwrap();
    }

    const MIGRATIONS: &str = "prisma/migrations";

    #[test]
    fn a_bare_prefix_matches_the_directory_and_everything_under_it() {
        assert!(matches(
            MIGRATIONS,
            "prisma/migrations/20240101_x/migration.sql"
        ));
        assert!(matches(MIGRATIONS, "prisma/migrations"));
        assert!(matches("prisma/migrations/", "prisma/migrations/a.sql"));
        assert!(!matches(MIGRATIONS, "prisma/schema.prisma"));
        // A sibling whose name merely starts the same is not under it.
        assert!(!matches(MIGRATIONS, "prisma/migrations-old/a.sql"));
    }

    #[test]
    fn star_spans_one_segment_and_doublestar_spans_many() {
        assert!(matches(
            "prisma/migrations/**",
            "prisma/migrations/a/b/c.sql"
        ));
        assert!(matches("**/*.sql", "prisma/migrations/a/up.sql"));
        assert!(matches("**/*.sql", "up.sql"), "**/ also matches the root");
        assert!(matches("src/*.ts", "src/main.ts"));
        assert!(!matches("src/*.ts", "src/deep/main.ts"), "* stops at /");
        assert!(!matches("**/*.sql", "prisma/schema.prisma"));
        assert!(!matches("", "anything"));
    }

    #[test]
    fn classify_splits_the_named_paths_from_the_rest() {
        let patterns = vec![MIGRATIONS.to_string()];
        let v = classify(
            [
                "prisma/migrations/1/migration.sql".to_string(),
                "src/orders/orders.service.ts".to_string(),
            ],
            &patterns,
        );
        assert_eq!(v.isolated, ["prisma/migrations/1/migration.sql"]);
        assert_eq!(v.other, ["src/orders/orders.service.ts"]);
        assert!(v.is_split());
    }

    #[test]
    fn a_change_of_only_isolated_paths_is_not_a_split() {
        let patterns = vec![MIGRATIONS.to_string()];
        let v = classify(["prisma/migrations/1/migration.sql".to_string()], &patterns);
        assert!(
            !v.is_split(),
            "a migration alone is exactly what the rule wants"
        );
        let v = classify(["src/a.ts".to_string()], &patterns);
        assert!(!v.is_split(), "code alone is fine too");
    }

    /// The point of `--merge-base`: a migration committed earlier on the
    /// branch counts, and a commit that landed on the base meanwhile does
    /// not get blamed on this branch.
    #[test]
    fn committed_branch_work_counts_not_the_bases_own_commits() {
        let dir = tempfile::tempdir().unwrap();
        let repo = make_repo(&dir);
        git(&repo, &["checkout", "-qb", "dzt-1-feature"]);
        write(&repo, "prisma/migrations/1/migration.sql", "ALTER TABLE t;");
        git(&repo, &["add", "-A"]);
        git(&repo, &["commit", "-qm", "migration"]);

        // main moves on underneath, with a file this branch never touched.
        git(&repo, &["checkout", "-q", "main"]);
        write(&repo, "unrelated.ts", "x");
        git(&repo, &["add", "-A"]);
        git(&repo, &["commit", "-qm", "other work"]);
        git(&repo, &["checkout", "-q", "dzt-1-feature"]);

        let patterns = vec![MIGRATIONS.to_string()];
        let v = verdict(&repo, "main", &patterns).unwrap();
        assert!(v.base_known);
        assert_eq!(v.isolated, ["prisma/migrations/1/migration.sql"]);
        assert!(
            v.other.is_empty(),
            "main's own commit is not ours: {:?}",
            v.other
        );
        assert!(!v.is_split());

        // Now the code lands on the branch, uncommitted: that is the split.
        write(&repo, "src/orders.service.ts", "x");
        let v = verdict(&repo, "main", &patterns).unwrap();
        assert!(v.is_split(), "committed migration + dirty code: {v:?}");
        assert_eq!(v.other, ["src/orders.service.ts"]);
    }

    /// No patterns, no read: a project that never asked for the guard pays
    /// nothing and is never badged.
    #[test]
    fn no_patterns_means_no_verdict() {
        let dir = tempfile::tempdir().unwrap();
        let repo = make_repo(&dir);
        let v = verdict(&repo, "main", &[]).unwrap();
        assert_eq!(v, Verdict::default());
        assert!(!v.is_split());
    }

    /// An unresolvable base (no origin, never fetched) still reads the
    /// working tree, and says the branch half is missing rather than
    /// claiming the checkout is clean.
    #[test]
    fn an_unknown_base_falls_back_to_the_working_tree() {
        let dir = tempfile::tempdir().unwrap();
        let repo = make_repo(&dir);
        write(&repo, "prisma/migrations/1/migration.sql", "ALTER TABLE t;");
        write(&repo, "src/a.ts", "x");
        let patterns = vec![MIGRATIONS.to_string()];
        let v = verdict(&repo, "origin/nope", &patterns).unwrap();
        assert!(!v.base_known, "the base did not resolve");
        assert!(v.is_split(), "the working tree alone already shows it");
    }
}
