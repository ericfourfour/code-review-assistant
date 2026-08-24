//! Repository discovery for the picker: a bounded scan of the home folder
//! for local clones, and `gh repo list` for the GitHub account. Both run on
//! background threads and stream results one repository at a time; the merged
//! result is cached in the database so the picker opens populated and only
//! rescans when the cache has gone stale or the user asks.

use std::collections::VecDeque;
use std::path::Path;

use serde::{Deserialize, Serialize};

use crate::db::Db;
use crate::gitio;

/// How deep below the scan root a clone can sit and still be found. Home
/// layouts are shallow (`~/proj`, `~/code/proj`, `~/dev/work/proj`); deeper
/// than this, scanning the whole profile costs more than it finds.
pub const MAX_DEPTH: usize = 3;

/// Directory names never worth scanning into: package caches and build output
/// large enough to dominate the scan. A directory on this list is still
/// offered if it is itself a clone — the name only stops descent.
const SKIP_DIRS: &[&str] = &[
    "node_modules",
    "target",
    "vendor",
    "venv",
    "__pycache__",
    "AppData",
    "Application Data",
    "Library",
    "site-packages",
    "bin",
    "obj",
    "Windows",
    "Program Files",
];

#[derive(Clone, Serialize, Deserialize)]
pub struct DiscoveredRepo {
    pub name: String,
    /// Local checkout, when one exists.
    pub path: Option<String>,
    /// `owner/name` on GitHub — from the listing, or a clone's origin URL.
    pub slug: Option<String>,
    /// Unix seconds of the newest activity visible without spawning a
    /// process: `.git` bookkeeping mtimes locally, `updatedAt` from gh.
    pub last_update: i64,
}

impl DiscoveredRepo {
    /// The identity exclusions are keyed on: the path of a local clone, the
    /// slug of a repository that only exists remotely.
    pub fn key(&self) -> &str {
        self.path.as_deref().or(self.slug.as_deref()).unwrap_or("")
    }
}

/// Fold a newly found repository into the list. A local clone and its GitHub
/// listing are the same repository, matched by slug; two hits on the same
/// path are one. Each side keeps the fields it knows and the newest activity
/// wins, so a merged row reads "local checkout, updated whenever either side
/// last saw motion".
pub fn merge(list: &mut Vec<DiscoveredRepo>, r: DiscoveredRepo) {
    for have in list.iter_mut() {
        let same_path = have.path.is_some() && have.path == r.path;
        let same_slug = have.slug.is_some() && have.slug == r.slug;
        if same_path || same_slug {
            if have.path.is_none() {
                have.path = r.path;
            }
            if have.slug.is_none() {
                have.slug = r.slug;
            }
            have.last_update = have.last_update.max(r.last_update);
            return;
        }
    }
    list.push(r);
}

pub fn is_excluded(excludes: &[String], repo: &DiscoveredRepo) -> bool {
    excludes.iter().any(|e| {
        Some(e.as_str()) == repo.path.as_deref() || Some(e.as_str()) == repo.slug.as_deref()
    })
}

/// Scan `root` breadth-first for clones, calling `emit` for each one
/// found — the caller streams them to the UI as they arrive. A directory that
/// contains `.git` is a result, not a place to keep digging: nested repos
/// (submodules, vendored clones) belong to their parent's review.
pub fn scan_local(root: &Path, max_depth: usize, mut emit: impl FnMut(DiscoveredRepo)) {
    let mut queue: VecDeque<(std::path::PathBuf, usize)> = VecDeque::new();
    queue.push_back((root.to_path_buf(), 0));
    while let Some((dir, depth)) = queue.pop_front() {
        if depth > 0 && dir.join(".git").exists() {
            emit(from_local(&dir));
            continue;
        }
        if depth >= max_depth {
            continue;
        }
        let Ok(entries) = std::fs::read_dir(&dir) else { continue };
        for entry in entries.flatten() {
            let Ok(ft) = entry.file_type() else { continue };
            // Symlinks (and junctions, which read_dir also reports as
            // symlinks) are skipped: following them risks cycles and
            // double-counts whatever they point at.
            if !ft.is_dir() || ft.is_symlink() {
                continue;
            }
            let name = entry.file_name().to_string_lossy().to_string();
            let skip = name.starts_with('.')
                || SKIP_DIRS.iter().any(|s| s.eq_ignore_ascii_case(&name));
            // A skip-listed or hidden name still gets in when it is itself a
            // clone (dotfiles repos live in dot-directories), it just is not
            // scanned for more.
            if skip && !entry.path().join(".git").exists() {
                continue;
            }
            queue.push_back((entry.path(), depth + 1));
        }
    }
}

fn from_local(dir: &Path) -> DiscoveredRepo {
    let git = dir.join(".git");
    // Newest of the bookkeeping files git touches on commit, checkout and
    // fetch — activity without the cost of spawning `git log` per repo. A
    // worktree-style `.git` *file* has only its own mtime to offer.
    let mut last = 0i64;
    let probes: Vec<std::path::PathBuf> = if git.is_dir() {
        ["HEAD", "index", "FETCH_HEAD"].iter().map(|p| git.join(p)).collect()
    } else {
        vec![git.clone()]
    };
    for p in probes {
        if let Ok(md) = std::fs::metadata(&p) {
            if let Ok(mtime) = md.modified() {
                if let Ok(d) = mtime.duration_since(std::time::UNIX_EPOCH) {
                    last = last.max(d.as_secs() as i64);
                }
            }
        }
    }
    DiscoveredRepo {
        name: dir.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_default(),
        path: Some(dir.to_string_lossy().to_string()),
        slug: origin_slug(dir),
        last_update: last,
    }
}

/// The `owner/name` of a clone's GitHub origin, read straight out of
/// `.git/config` — no subprocess, because this runs once per discovered repo.
/// `None` for non-GitHub origins and worktree-style `.git` files.
fn origin_slug(dir: &Path) -> Option<String> {
    let cfg = std::fs::read_to_string(dir.join(".git").join("config")).ok()?;
    let mut in_origin = false;
    for line in cfg.lines() {
        let line = line.trim();
        if line.starts_with('[') {
            in_origin = line.replace(' ', "") == "[remote\"origin\"]";
        } else if in_origin && line.starts_with("url") {
            return slug_from_url(line.splitn(2, '=').nth(1)?.trim());
        }
    }
    None
}

fn slug_from_url(url: &str) -> Option<String> {
    let rest = url.split("github.com").nth(1)?;
    let rest = rest.trim_start_matches([':', '/']).trim_end_matches('/');
    let rest = rest.strip_suffix(".git").unwrap_or(rest);
    let mut parts = rest.split('/');
    let owner = parts.next().filter(|s| !s.is_empty())?;
    let name = parts.next().filter(|s| !s.is_empty())?;
    Some(format!("{owner}/{name}"))
}

/// The user's repositories as gh knows them. One blocking call returning the
/// whole page — gh has no streaming mode — so the caller runs it on a thread
/// beside the local scan, not after it.
pub fn list_github(gh: &str, limit: usize) -> Result<Vec<DiscoveredRepo>, String> {
    let home = dirs::home_dir().unwrap_or_else(|| std::path::PathBuf::from("."));
    let out = gitio::run(
        &home.to_string_lossy(),
        gh,
        &["repo", "list", "--limit", &limit.to_string(), "--json", "nameWithOwner,updatedAt"],
    )?;
    #[derive(Deserialize)]
    struct Row {
        #[serde(rename = "nameWithOwner")]
        name_with_owner: String,
        #[serde(rename = "updatedAt")]
        updated_at: String,
    }
    let rows: Vec<Row> = serde_json::from_str(&out).map_err(|e| format!("parse gh output: {e}"))?;
    Ok(rows
        .into_iter()
        .map(|r| {
            let name =
                r.name_with_owner.rsplit('/').next().unwrap_or(&r.name_with_owner).to_string();
            let last_update = chrono::DateTime::parse_from_rfc3339(&r.updated_at)
                .map(|t| t.timestamp())
                .unwrap_or(0);
            DiscoveredRepo { name, path: None, slug: Some(r.name_with_owner), last_update }
        })
        .collect())
}

// -- cache -------------------------------------------------------------------

const CACHE_KEY: &str = "repo_cache";

#[derive(Default, Serialize, Deserialize)]
pub struct RepoCache {
    pub fetched_at: i64,
    pub repos: Vec<DiscoveredRepo>,
}

pub fn load_cache(db: &Db) -> RepoCache {
    db.get_setting(CACHE_KEY).and_then(|v| serde_json::from_str(&v).ok()).unwrap_or_default()
}

pub fn save_cache(db: &Db, repos: &[DiscoveredRepo], fetched_at: i64) {
    if let Ok(json) = serde_json::to_string(&RepoCache { fetched_at, repos: repos.to_vec() }) {
        db.set_setting(CACHE_KEY, &json);
    }
}

/// "3d", "5mo" — compact enough for a table column.
pub fn age_label(now: i64, ts: i64) -> String {
    if ts <= 0 {
        return "?".into();
    }
    let d = (now - ts).max(0);
    if d < 3600 {
        format!("{}m", d / 60)
    } else if d < 86_400 {
        format!("{}h", d / 3600)
    } else if d < 30 * 86_400 {
        format!("{}d", d / 86_400)
    } else if d < 365 * 86_400 {
        format!("{}mo", d / (30 * 86_400))
    } else {
        format!("{}y", d / (365 * 86_400))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testkit::TempDir;

    #[test]
    fn slugs_come_out_of_every_github_url_shape() {
        for url in [
            "https://github.com/eric/proj.git",
            "https://github.com/eric/proj",
            "git@github.com:eric/proj.git",
            "ssh://git@github.com/eric/proj/",
        ] {
            assert_eq!(slug_from_url(url).as_deref(), Some("eric/proj"), "{url}");
        }
        assert_eq!(slug_from_url("https://gitlab.com/eric/proj.git"), None);
        assert_eq!(slug_from_url("https://github.com/eric"), None);
    }

    #[test]
    fn a_clone_and_its_listing_merge_into_one_row() {
        let mut list = Vec::new();
        merge(
            &mut list,
            DiscoveredRepo {
                name: "proj".into(),
                path: Some("/home/e/proj".into()),
                slug: Some("eric/proj".into()),
                last_update: 100,
            },
        );
        merge(
            &mut list,
            DiscoveredRepo {
                name: "proj".into(),
                path: None,
                slug: Some("eric/proj".into()),
                last_update: 500,
            },
        );
        merge(
            &mut list,
            DiscoveredRepo {
                name: "other".into(),
                path: None,
                slug: Some("eric/other".into()),
                last_update: 50,
            },
        );
        assert_eq!(list.len(), 2);
        assert_eq!(list[0].path.as_deref(), Some("/home/e/proj"));
        assert_eq!(list[0].last_update, 500, "newest activity wins");
        // Two remote-only rows with different slugs stay separate even
        // though both have no path.
        assert_eq!(list[1].slug.as_deref(), Some("eric/other"));
    }

    #[test]
    fn the_scan_finds_nested_clones_and_stops_at_repo_and_skip_boundaries() {
        let dir = TempDir::new("discover-scan");
        let root = dir.path();
        let mk = |rel: &str| std::fs::create_dir_all(root.join(rel)).unwrap();
        mk("proj-a/.git");
        mk("code/proj-b/.git");
        // inside a repo: never offered on its own
        mk("proj-a/vendored/.git");
        // under a skip-listed name: never reached
        mk("node_modules/dep/.git");
        // a skip-listed name that is itself a clone is still offered
        mk("target/.git");
        // too deep
        mk("a/b/c/proj-d/.git");

        let mut found = Vec::new();
        scan_local(root, MAX_DEPTH, |r| found.push(r.name.clone()));
        found.sort();
        assert_eq!(found, ["proj-a", "proj-b", "target"]);
    }

    #[test]
    fn origin_slug_reads_the_config_without_git() {
        let dir = TempDir::new("discover-slug");
        let git = dir.path().join("repo/.git");
        std::fs::create_dir_all(&git).unwrap();
        std::fs::write(
            git.join("config"),
            "[core]\n\tbare = false\n[remote \"origin\"]\n\turl = git@github.com:eric/repo.git\n\tfetch = +refs/heads/*:refs/remotes/origin/*\n",
        )
        .unwrap();
        assert_eq!(origin_slug(&dir.path().join("repo")).as_deref(), Some("eric/repo"));
    }

    #[test]
    fn ages_read_like_a_human_wrote_them() {
        let now = 1_000_000_000;
        assert_eq!(age_label(now, now - 120), "2m");
        assert_eq!(age_label(now, now - 7200), "2h");
        assert_eq!(age_label(now, now - 3 * 86_400), "3d");
        assert_eq!(age_label(now, now - 60 * 86_400), "2mo");
        assert_eq!(age_label(now, now - 800 * 86_400), "2y");
        assert_eq!(age_label(now, 0), "?");
    }
}
