use crate::config::ShellConfig;
use nix::pty::{forkpty, ForkptyResult, Winsize};
use nix::unistd::{execvp, Pid};
use std::ffi::{CStr, CString};
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

/// The short device name (e.g. `"ttys003"`) of the pty's slave side, for
/// display in the status bar. `ptsname` works on the master fd of any
/// BSD-style pty pair, not just ones opened via `posix_openpt`, so this is
/// safe to call on the fd `forkpty` handed back. Not thread-safe (the
/// non-reentrant libc call reuses a static buffer), but this is only ever
/// called from the main thread while constructing a `Tab`, never from a
/// pty reader thread.
pub fn tty_name(master: BorrowedFd) -> Option<String> {
    // Safety: `master` is a valid, open pty master fd.
    let ptr = unsafe { libc::ptsname(master.as_raw_fd()) };
    if ptr.is_null() {
        return None;
    }
    // Safety: a non-null return is a valid NUL-terminated C string owned by
    // libc's static buffer, live until the next `ptsname` call.
    let path = unsafe { CStr::from_ptr(ptr) }.to_str().ok()?;
    Some(path.rsplit('/').next().unwrap_or(path).to_string())
}

/// Fork a new pty and exec the user's shell as a login shell in the child.
///
/// # Safety / ordering
/// The very first call must happen before any other threads exist (i.e.
/// before winit initializes AppKit) -- `main()` spawns the first tab's
/// shell that early for exactly this reason. Later calls (opening a new
/// tab) necessarily run with reader threads alive; strictly speaking only
/// async-signal-safe calls are allowed in a forked child then, and
/// `setenv`/`execvp` both allocate. In practice the fork-to-exec window is
/// a handful of instructions and every parent-side thread here is either
/// parked in `read(2)` or the event loop rather than mid-`malloc`, so the
/// deadlock window is vanishingly small -- but if a new tab ever hangs
/// before exec, this is the place to suspect (the fix would be
/// `posix_spawn` plus manual pty plumbing).
/// A UTF-8 locale to hand the shell, or `None` when the environment
/// already specifies one (in which case the user's choice wins).
///
/// Resolved once and cached: it shells out, which must happen in the
/// parent -- between `fork` and `exec` only async-signal-safe calls are
/// allowed, and spawning a process is emphatically not one.
fn utf8_locale() -> Option<&'static str> {
    static LOCALE: std::sync::OnceLock<Option<String>> = std::sync::OnceLock::new();
    LOCALE.get_or_init(resolve_utf8_locale).as_deref()
}

fn resolve_utf8_locale() -> Option<String> {
    // Anything already set is the user's decision -- a login shell's
    // profile may well set it, and overriding that would be worse than
    // the problem being fixed.
    for var in ["LC_ALL", "LC_CTYPE", "LANG"] {
        if std::env::var_os(var).is_some_and(|v| !v.is_empty()) {
            return None;
        }
    }

    // macOS keeps the user's region here rather than in the environment;
    // it looks like "ja_JP" or "en_US@calendar=gregorian".
    let region = std::process::Command::new("defaults")
        .args(["read", "-g", "AppleLocale"])
        .output()
        .ok()
        .filter(|out| out.status.success())
        .and_then(|out| String::from_utf8(out.stdout).ok())
        .map(|s| s.trim().split('@').next().unwrap_or_default().to_string())
        .filter(|s| !s.is_empty());

    let candidate = region.map(|r| format!("{r}.UTF-8"));
    // A locale the system doesn't actually have makes things worse, not
    // better: setlocale fails and half the toolchain warns about it on
    // every command. `en_US.UTF-8` is present on every macOS install.
    match candidate {
        Some(locale) if locale_exists(&locale) => Some(locale),
        _ => Some("en_US.UTF-8".to_string()),
    }
}

fn locale_exists(locale: &str) -> bool {
    std::process::Command::new("locale")
        .arg("-a")
        .output()
        .ok()
        .filter(|out| out.status.success())
        .and_then(|out| String::from_utf8(out.stdout).ok())
        .is_some_and(|list| list.lines().any(|line| line.eq_ignore_ascii_case(locale)))
}

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

    // Resolve the locale *before* forking so the child's lookup is a
    // cached read. Left to the child, the first call would run
    // subprocesses between fork and exec, which is exactly the
    // async-signal-safety violation this file is otherwise careful about.
    let _ = utf8_locale();

    match unsafe { forkpty(None, None) }.expect("forkpty failed") {
        ForkptyResult::Child => {
            // Force a known-good TERM regardless of whatever the parent
            // process happened to have (Finder-launched apps, or a script
            // that spawned this one, often have no TERM at all) -- an
            // empty/missing TERM leaves the shell's readline without a
            // terminfo entry, so e.g. Ctrl-L's clear-screen binding
            // silently no-ops. This emulator understands roughly xterm's
            // escape sequences, so advertise that.
            // Safety: single-threaded child right after fork, before
            // execvp -- same invariant the rest of this function relies on.
            unsafe { std::env::set_var("TERM", "xterm-256color") };
            // Same problem, different variable: a GUI-launched app
            // inherits launchd's environment, which has no locale at
            // all. Programs that decide their character encoding from
            // the locale then fall back to latin1 -- vim opens a UTF-8
            // file as one character per *byte*, so Japanese shows as
            // mojibake and editing it corrupts it. Every terminal
            // emulator on this platform sets a locale for this reason.
            // Resolved in the parent (see `utf8_locale`), because
            // resolving it here would mean running subprocesses between
            // fork and exec.
            if let Some(locale) = utf8_locale() {
                unsafe { std::env::set_var("LANG", locale) };
            }
            // Start in $HOME rather than whatever cwd this process
            // inherited. Unlike a shell launching a child, there's no
            // meaningful directory to inherit here -- a GUI app launched
            // from Finder/Dock/Spotlight starts with cwd "/" (or
            // occasionally the app bundle's own directory), neither of
            // which is anywhere a user would want a fresh shell to land.
            // Every other terminal emulator defaults new sessions to
            // $HOME for the same reason. Best-effort: if $HOME isn't set
            // or doesn't exist, the shell just keeps whatever cwd it
            // already had.
            if let Ok(home) = std::env::var("HOME") {
                let _ = nix::unistd::chdir(home.as_str());
            }
            let _ = execvp(&shell_c, &argv);
            // execvp only returns on failure.
            std::process::exit(1);
        }
        ForkptyResult::Parent { child, master } => PtyHandle { master, child },
    }
}
