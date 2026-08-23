//! Test-only scaffolding: a throwaway directory, a scriptable fake model CLI,
//! and a throwaway git repository.
//!
//! The point is to exercise the code that touches the outside world — process
//! spawning, stdin piping, git — without reaching the network or the user's
//! real database. Everything lands in a uniquely named temp directory that is
//! deleted when its guard drops, so tests still run in parallel.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU32, Ordering};

use crate::settings::ModelSlot;

static COUNTER: AtomicU32 = AtomicU32::new(0);

/// A directory that deletes itself. Named by process id plus a counter so
/// concurrent tests — and concurrent `cargo test` runs — never collide.
pub struct TempDir {
    path: PathBuf,
}

impl TempDir {
    pub fn new(tag: &str) -> TempDir {
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        let path = std::env::temp_dir()
            .join(format!("cra_test_{}_{}_{}", std::process::id(), tag, n));
        let _ = std::fs::remove_dir_all(&path);
        std::fs::create_dir_all(&path).expect("create temp dir");
        // The model command template is whitespace-tokenized, so a fake CLI
        // living under a path with spaces could not be expressed in one.
        assert!(
            !path.to_string_lossy().contains(' '),
            "temp dir has spaces in it: {}",
            path.display()
        );
        TempDir { path }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.path);
    }
}

/// A stand-in for a model CLI: a real executable on disk that records the
/// argv and stdin it was handed, then prints a canned reply and exits with a
/// chosen status. Spawning it goes through exactly the same path as a real
/// CLI — `resolve_program`, argv building, the stdin pipe, the timeout poll.
pub struct FakeCli {
    program: PathBuf,
    argv_log: PathBuf,
    stdin_log: PathBuf,
}

#[derive(Default)]
pub struct FakeCliSpec<'a> {
    /// Printed verbatim on stdout.
    pub reply: &'a str,
    /// Process exit code.
    pub exit_code: i32,
    /// Seconds to stall before replying, for exercising the timeout.
    pub delay_secs: u32,
}

impl FakeCli {
    /// Write the fake CLI into `dir`. On Windows it is deliberately a `.cmd`
    /// shim — the same shape as an npm-installed CLI, which is what broke
    /// argument passing and what `resolve_program` has to find via PATHEXT.
    pub fn new(dir: &TempDir, name: &str, spec: FakeCliSpec) -> FakeCli {
        let base = dir.path();
        let argv_log = base.join(format!("{name}.argv"));
        let stdin_log = base.join(format!("{name}.stdin"));
        let reply_file = base.join(format!("{name}.reply"));
        std::fs::write(&reply_file, spec.reply).expect("write reply");

        let (program, script) = Self::script(name, base, &argv_log, &stdin_log, &reply_file, &spec);
        std::fs::write(&program, script).expect("write fake cli");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&program, std::fs::Permissions::from_mode(0o755))
                .expect("chmod fake cli");
        }
        FakeCli { program, argv_log, stdin_log }
    }

    #[cfg(windows)]
    fn script(
        name: &str,
        base: &Path,
        argv_log: &Path,
        stdin_log: &Path,
        reply_file: &Path,
        spec: &FakeCliSpec,
    ) -> (PathBuf, String) {
        // `findstr "^"` matches every line, so it copies stdin through; with a
        // null stdin it simply produces an empty file.
        let delay = if spec.delay_secs > 0 {
            format!("ping -n {} 127.0.0.1 > NUL\r\n", spec.delay_secs + 1)
        } else {
            String::new()
        };
        let script = format!(
            "@echo off\r\n\
             > \"{argv}\" echo %*\r\n\
             findstr \"^\" > \"{stdin}\"\r\n\
             {delay}\
             type \"{reply}\"\r\n\
             exit /b {code}\r\n",
            argv = argv_log.display(),
            stdin = stdin_log.display(),
            reply = reply_file.display(),
            code = spec.exit_code,
        );
        (base.join(format!("{name}.cmd")), script)
    }

    #[cfg(not(windows))]
    fn script(
        name: &str,
        base: &Path,
        argv_log: &Path,
        stdin_log: &Path,
        reply_file: &Path,
        spec: &FakeCliSpec,
    ) -> (PathBuf, String) {
        let delay = if spec.delay_secs > 0 {
            format!("sleep {}\n", spec.delay_secs)
        } else {
            String::new()
        };
        let script = format!(
            "#!/bin/sh\n\
             echo \"$@\" > '{argv}'\n\
             cat > '{stdin}'\n\
             {delay}\
             cat '{reply}'\n\
             exit {code}\n",
            argv = argv_log.display(),
            stdin = stdin_log.display(),
            reply = reply_file.display(),
            code = spec.exit_code,
        );
        (base.join(name), script)
    }

    /// The command template that invokes this fake, with the prompt on stdin.
    pub fn command(&self) -> String {
        self.program.to_string_lossy().to_string()
    }

    /// A slot wired to this fake. `extra` is appended to the command template,
    /// so a test can add `{prompt}` or `{session}` as needed.
    pub fn slot(&self, extra: &str) -> ModelSlot {
        let command = if extra.is_empty() {
            self.command()
        } else {
            format!("{} {extra}", self.command())
        };
        ModelSlot {
            name: "fake".into(),
            command,
            coauthor: "Fake <fake@example.com>".into(),
            enabled: true,
            model: String::new(),
            model_flag: "--model".into(),
            effort: String::new(),
            effort_flag: "--effort".into(),
            resume_command: String::new(),
            session_key: String::new(),
        }
    }

    /// What the process was handed on the command line.
    pub fn argv_seen(&self) -> String {
        std::fs::read_to_string(&self.argv_log).unwrap_or_default()
    }

    /// What the process was handed on stdin.
    pub fn stdin_seen(&self) -> String {
        std::fs::read_to_string(&self.stdin_log).unwrap_or_default()
    }
}

/// A real git repository in a temp directory, so the git plumbing can be
/// tested against the binary it actually shells out to.
pub struct TempRepo {
    dir: TempDir,
}

impl TempRepo {
    pub fn new(tag: &str) -> TempRepo {
        let dir = TempDir::new(tag);
        let repo = TempRepo { dir };
        repo.git(&["init", "--initial-branch=main"]);
        // Identity and signing are per-repo so the test never depends on (or
        // disturbs) the machine's global git configuration.
        repo.git(&["config", "user.email", "test@example.com"]);
        repo.git(&["config", "user.name", "Test"]);
        repo.git(&["config", "commit.gpgsign", "false"]);
        repo
    }

    pub fn path(&self) -> String {
        self.dir.path().to_string_lossy().to_string()
    }

    pub fn git(&self, args: &[&str]) -> String {
        let out = crate::gitio::hidden_command("git")
            .args(args)
            .current_dir(self.dir.path())
            .output()
            .unwrap_or_else(|e| panic!("git {args:?}: {e}"));
        assert!(
            out.status.success(),
            "git {args:?} failed: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        String::from_utf8_lossy(&out.stdout).to_string()
    }

    pub fn write(&self, rel: &str, contents: &str) {
        let path = self.dir.path().join(rel);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).expect("create parent dir");
        }
        std::fs::write(path, contents).expect("write file");
    }

    pub fn read(&self, rel: &str) -> String {
        std::fs::read_to_string(self.dir.path().join(rel)).expect("read file")
    }

    pub fn commit(&self, message: &str) {
        self.git(&["add", "-A"]);
        self.git(&["commit", "-m", message]);
    }
}
