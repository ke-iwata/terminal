use crate::pty::{self, PtyHandle};
use nix::unistd::Pid;
use std::os::fd::OwnedFd;
use std::sync::Arc;

use crate::config::ShellConfig;
use crate::term::Term;

/// One terminal session: its own pty, shell process, and screen/scrollback
/// state. The window holds a `Vec<Tab>` and renders/feeds input to whichever
/// one is active; the others keep running (and buffering output into their
/// own `Term`) in the background exactly like a real terminal's tabs do.
pub struct Tab {
    /// Stable identity, independent of position in the tab strip -- closing
    /// an earlier tab must not confuse in-flight pty reader events tagged
    /// with a later tab's id. Never reused within one run of the app.
    pub id: u64,
    pub term: Term,
    pub pty_master: Arc<OwnedFd>,
    pub pty_child: Pid,
    /// Bumped when this tab's shell is restarted (see `App::restart_shell`),
    /// so stale reader-thread events from a just-replaced shell session are
    /// told apart from the current one -- mirrors the single-session scheme
    /// this replaced.
    pub pty_generation: u64,
    pub scroll_offset: usize,
    pub shell_name: String,
    pub tty_name: String,
    /// What the tab strip shows for this tab: the foreground process's name
    /// while one is running (e.g. "vim"), the shell's own name otherwise.
    /// Refreshed opportunistically on redraw, not on every keystroke.
    pub title: String,
}

impl Tab {
    /// Spawn a fresh shell and wrap it in a new tab. Does *not* start the
    /// pty reader thread -- the caller does that once it can route the
    /// resulting bytes (see `App::spawn_pty_reader`).
    pub fn spawn(id: u64, shell: &ShellConfig, cols: usize, rows: usize, scrollback_lines: usize) -> Tab {
        let handle = pty::spawn_shell(shell);
        Tab::from_handle(id, handle, shell, cols, rows, scrollback_lines)
    }

    /// Wrap an already-spawned `PtyHandle` in a new tab, without forking a
    /// shell itself. Needed for the very first tab: `pty::spawn_shell` must
    /// run before the winit event loop exists (see its doc comment), so
    /// `main()` calls it directly and hands the result here rather than
    /// going through `Tab::spawn`.
    pub fn from_handle(id: u64, handle: PtyHandle, shell: &ShellConfig, cols: usize, rows: usize, scrollback_lines: usize) -> Tab {
        let PtyHandle { master, child } = handle;
        let master = Arc::new(master);
        pty::resize(std::os::fd::AsFd::as_fd(&*master), cols as u16, rows as u16);

        let shell_path = shell.command.clone().or_else(|| std::env::var("SHELL").ok()).unwrap_or_else(|| "/bin/zsh".to_string());
        let shell_name = shell_path.rsplit('/').next().unwrap_or(&shell_path).to_string();
        let tty_name = pty::tty_name(std::os::fd::AsFd::as_fd(&*master)).unwrap_or_default();

        Tab {
            id,
            term: Term::new(cols, rows, scrollback_lines),
            pty_master: master,
            pty_child: child,
            pty_generation: 0,
            scroll_offset: 0,
            shell_name: shell_name.clone(),
            tty_name,
            title: shell_name,
        }
    }
}
