//! Small helpers backing the tab bar's "running command" labels and the
//! status bar's shell/cwd/git info: process introspection via `sysinfo` and
//! a from-scratch `.git/HEAD` reader (no `git` binary invocation needed).

use nix::unistd::Pid as NixPid;
use std::os::fd::BorrowedFd;
use std::path::{Path, PathBuf};
use sysinfo::{Pid, ProcessRefreshKind, ProcessesToUpdate, System};

/// Process lookups scoped to a couple of specific pids -- `System::new()`
/// starts empty and `ProcessesToUpdate::Some` only queries the listed
/// pids, so none of this ever scans the whole process table.
pub struct ProcInfo {
    sys: System,
}

impl ProcInfo {
    pub fn new() -> Self {
        ProcInfo { sys: System::new() }
    }

    fn refresh(&mut self, pid: NixPid) {
        // Start from an empty table every time instead of refreshing into
        // the previous one: sysinfo only reads a process's NAME when it
        // first learns about that pid, and a pid's name legitimately
        // changes here -- a freshly forked shell is briefly a copy of
        // this binary ("terminal") until execvp swaps in bash, and a
        // shell fork is briefly "bash" until it execs the real command.
        // With a persistent table, whichever name won the race against
        // exec got cached forever, which is exactly how tabs sometimes
        // came up titled "terminal" instead of "bash". A from-scratch
        // query of one pid is far too cheap to matter at the status
        // bar's throttled refresh rate.
        self.sys = System::new();
        let pid = Pid::from_u32(pid.as_raw() as u32);
        self.sys.refresh_processes_specifics(
            ProcessesToUpdate::Some(&[pid]),
            true,
            ProcessRefreshKind::new().with_cwd(sysinfo::UpdateKind::Always).with_exe(sysinfo::UpdateKind::Always),
        );
    }

    fn process_name(&mut self, pid: NixPid) -> Option<String> {
        self.refresh(pid);
        let sys_pid = Pid::from_u32(pid.as_raw() as u32);
        self.sys.process(sys_pid).map(|p| p.name().to_string_lossy().into_owned())
    }

    pub fn process_cwd(&mut self, pid: NixPid) -> Option<PathBuf> {
        self.refresh(pid);
        let sys_pid = Pid::from_u32(pid.as_raw() as u32);
        self.sys.process(sys_pid).and_then(|p| p.cwd()).map(Path::to_path_buf)
    }

    /// The name of whatever's currently in the foreground process group of
    /// `master` (e.g. `"vim"`, `"npm"`), or `None` if that can't be
    /// determined (pty already closed, no permission, etc.). Falls back to
    /// the login shell's own name at the caller's discretion when the
    /// foreground group turns out to just be the shell itself sitting idle
    /// at its prompt.
    pub fn foreground_process_name(&mut self, master: BorrowedFd) -> Option<(NixPid, String)> {
        let pgrp = nix::unistd::tcgetpgrp(master).ok()?;
        let name = self.process_name(pgrp)?;
        Some((pgrp, name))
    }
}

/// Walk up from `dir` looking for a `.git` directory or worktree file, and
/// return the checked-out branch name (or a short hash if detached). `None`
/// if `dir` isn't inside a git repo, or the repo state can't be read.
pub fn git_branch(dir: &Path) -> Option<String> {
    let mut current = dir.to_path_buf();
    loop {
        let git_path = current.join(".git");
        if let Some(head_path) = git_dir_head_path(&git_path) {
            let head = std::fs::read_to_string(head_path).ok()?;
            let head = head.trim();
            return Some(match head.strip_prefix("ref: refs/heads/") {
                Some(branch) => branch.to_string(),
                None => head.get(0..7).unwrap_or(head).to_string(),
            });
        }
        if !current.pop() {
            return None;
        }
    }
}

/// Resolve `<repo>/.git` (a directory for a normal clone, or a file
/// containing `gitdir: <path>` for a worktree) to the path of its `HEAD`
/// file, or `None` if `git_path` doesn't exist / isn't recognizable.
fn git_dir_head_path(git_path: &Path) -> Option<PathBuf> {
    if git_path.is_dir() {
        return Some(git_path.join("HEAD"));
    }
    if git_path.is_file() {
        let contents = std::fs::read_to_string(git_path).ok()?;
        let gitdir = contents.trim().strip_prefix("gitdir: ")?;
        return Some(PathBuf::from(gitdir).join("HEAD"));
    }
    None
}
