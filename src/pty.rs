use crate::config::ShellConfig;
use nix::pty::{forkpty, ForkptyResult, Winsize};
use nix::unistd::{execvp, Pid};
use std::ffi::CString;
use std::os::fd::{AsRawFd, BorrowedFd, OwnedFd};

pub struct PtyHandle {
    pub master: OwnedFd,
    pub child: Pid,
}

// TIOCSWINSZ is a pre-built ioctl request constant (not assembled from a
// group char + sequence number), so this uses the "_bad" variant of the
// macro, matching the pattern portable-pty uses internally.
nix::ioctl_write_ptr_bad!(set_window_size, libc::TIOCSWINSZ, Winsize);

/// Inform the pty (and therefore the shell/programs inside it, via
/// `SIGWINCH`) of the terminal's current size in character cells.
pub fn resize(fd: BorrowedFd, cols: u16, rows: u16) {
    let ws = Winsize {
        ws_row: rows,
        ws_col: cols,
        ws_xpixel: 0,
        ws_ypixel: 0,
    };
    // Safety: `fd` is a valid, open pty master descriptor for the lifetime
    // of this call, and `ws` is a valid, fully-initialized Winsize.
    let _ = unsafe { set_window_size(fd.as_raw_fd(), &ws) };
}

/// Fork a new pty and exec the user's shell as a login shell in the child.
///
/// # Safety / ordering
/// Must be called before any other threads exist in this process (i.e.
/// before winit initializes AppKit). `forkpty` only allows async-signal-safe
/// calls in the child until `execve`, and `execvp` allocates internally to
/// build its argv array; that's only sound here because no other thread can
/// be holding the allocator lock at fork time.
pub fn spawn_shell(shell: &ShellConfig) -> PtyHandle {
    let shell_path = shell
        .command
        .clone()
        .or_else(|| std::env::var("SHELL").ok())
        .unwrap_or_else(|| "/bin/zsh".to_string());
    let shell_name = shell_path.rsplit('/').next().unwrap_or(&shell_path).to_string();
    let shell_c = CString::new(shell_path.clone()).expect("shell path contains a NUL byte");
    // Prefix argv[0] with '-' to make the shell start as a login shell, so
    // profile files (.zprofile, .bash_profile, etc.) are sourced, matching
    // the behavior of Terminal.app/iTerm2.
    let arg0 = CString::new(format!("-{shell_name}")).expect("shell name contains a NUL byte");
    let mut argv = vec![arg0];
    for arg in &shell.args {
        argv.push(CString::new(arg.as_str()).expect("shell arg contains a NUL byte"));
    }

    match unsafe { forkpty(None, None) }.expect("forkpty failed") {
        ForkptyResult::Child => {
            let _ = execvp(&shell_c, &argv);
            // execvp only returns on failure.
            std::process::exit(1);
        }
        ForkptyResult::Parent { child, master } => PtyHandle { master, child },
    }
}
