//! The model behind the file-tree sidebar: a directory rooted at the
//! focused pane's cwd, flattened into the list of rows actually visible
//! right now (i.e. the root's entries, plus the contents of every
//! directory the user has expanded).
//!
//! Kept free of any rendering or event-loop concerns so it can be tested
//! against a real temp directory without a window.

use std::collections::HashSet;
use std::path::{Path, PathBuf};

/// Stop descending past this depth. Expanding a directory tree deep
/// enough to hit this is already unreadable in a sidebar, and it bounds
/// the work done by a single rebuild.
const MAX_DEPTH: usize = 12;
/// Hard cap on rows built in one pass, so pointing the tree at something
/// enormous (`/nix/store`, a node_modules) can't stall a redraw.
const MAX_ROWS: usize = 5000;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Row {
    pub path: PathBuf,
    /// Display name -- see `display_name` for why it isn't just the
    /// file name verbatim.
    pub name: String,
    /// Nesting level below the root (root's own children are 0).
    pub depth: usize,
    pub is_dir: bool,
    pub expanded: bool,
}

pub struct FileTree {
    root: PathBuf,
    /// Absolute paths of expanded directories. Absolute (rather than
    /// relative to `root`) so expansion state survives the root moving
    /// as the user `cd`s around and comes back.
    expanded: HashSet<PathBuf>,
    rows: Vec<Row>,
    /// First visible row index -- the sidebar's scroll position.
    pub scroll: usize,
    show_hidden: bool,
}

impl FileTree {
    pub fn new() -> Self {
        FileTree {
            root: PathBuf::new(),
            expanded: HashSet::new(),
            rows: Vec::new(),
            scroll: 0,
            show_hidden: false,
        }
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    pub fn rows(&self) -> &[Row] {
        &self.rows
    }

    pub fn show_hidden(&self) -> bool {
        self.show_hidden
    }

    /// Finder's Cmd+Shift+. equivalent.
    pub fn toggle_hidden(&mut self) {
        self.show_hidden = !self.show_hidden;
        self.rebuild();
    }

    /// Point the tree at a new directory, rebuilding it. A no-op when
    /// already rooted there, so this is cheap to call on every status
    /// refresh.
    pub fn set_root(&mut self, root: &Path) {
        if self.root == root {
            return;
        }
        self.root = root.to_path_buf();
        self.scroll = 0;
        self.rebuild();
    }

    /// Expand or collapse `path`, then rebuild. No-op for files.
    pub fn toggle(&mut self, path: &Path) {
        if !path.is_dir() {
            return;
        }
        if !self.expanded.remove(path) {
            self.expanded.insert(path.to_path_buf());
        }
        self.rebuild();
    }

    /// Re-read the filesystem into `rows`, keeping expansion state.
    /// Directories that have since disappeared simply contribute nothing.
    pub fn rebuild(&mut self) {
        self.rows.clear();
        if self.root.as_os_str().is_empty() {
            return;
        }
        let root = self.root.clone();
        self.push_dir(&root, 0);
        self.scroll = self.scroll.min(self.rows.len().saturating_sub(1));
    }

    fn push_dir(&mut self, dir: &Path, depth: usize) {
        if depth >= MAX_DEPTH || self.rows.len() >= MAX_ROWS {
            return;
        }
        let Ok(read_dir) = std::fs::read_dir(dir) else {
            return;
        };

        let mut entries: Vec<(PathBuf, String, bool)> = Vec::new();
        for entry in read_dir.flatten() {
            let name = entry.file_name().to_string_lossy().into_owned();
            // Dotfiles are hidden by default, like `ls` and Finder --
            // showing `.git` in every repo would bury the interesting
            // entries. Cmd+Shift+. reveals them, same as Finder.
            if !self.show_hidden && name.starts_with('.') {
                continue;
            }
            let path = entry.path();
            // `file_type()` doesn't follow symlinks, so a symlinked
            // directory would otherwise sort and behave as a file.
            let is_dir = match entry.file_type() {
                Ok(ft) if ft.is_symlink() => path.is_dir(),
                Ok(ft) => ft.is_dir(),
                Err(_) => false,
            };
            entries.push((path, name, is_dir));
        }
        // Directories first, then case-insensitive by name -- the
        // ordering every file browser uses.
        entries.sort_by(|a, b| b.2.cmp(&a.2).then_with(|| a.1.to_lowercase().cmp(&b.1.to_lowercase())));

        for (path, name, is_dir) in entries {
            if self.rows.len() >= MAX_ROWS {
                return;
            }
            let expanded = is_dir && self.expanded.contains(&path);
            self.rows.push(Row {
                name: display_name(&name),
                path: path.clone(),
                depth,
                is_dir,
                expanded,
            });
            if expanded {
                self.push_dir(&path, depth + 1);
            }
        }
    }
}

/// Replace characters the glyph atlas can't draw (it rasterizes printable
/// ASCII only) with `?`. Without this a file named entirely in Japanese
/// would render as a blank row that's still clickable -- a visible `?`
/// run is a worse-looking but honest stand-in until the atlas grows
/// beyond ASCII.
fn display_name(name: &str) -> String {
    name.chars()
        .map(|c| if c.is_ascii_graphic() || c == ' ' { c } else { '?' })
        .collect()
}

/// Quote `path` so a shell receives it as a single literal argument --
/// clicking a file inserts its path at the prompt, and names with spaces
/// or quotes must not turn into multiple words or break out into extra
/// commands. Single quotes disable every shell metacharacter; an embedded
/// single quote is escaped the standard way, by closing the quoted run
/// (`'\''`) around it.
pub fn shell_quote(path: &str) -> String {
    if !path.is_empty() && path.chars().all(|c| c.is_alphanumeric() || "_./-@%+=:,".contains(c)) {
        return path.to_string();
    }
    format!("'{}'", path.replace('\'', r"'\''"))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A throwaway directory tree under the system temp dir, removed when
    /// the test ends.
    struct TempTree(PathBuf);

    impl TempTree {
        fn new(tag: &str) -> Self {
            let dir = std::env::temp_dir().join(format!("keterm-filetree-test-{tag}"));
            let _ = std::fs::remove_dir_all(&dir);
            std::fs::create_dir_all(&dir).unwrap();
            TempTree(dir)
        }
        fn dir(&self, rel: &str) -> PathBuf {
            let p = self.0.join(rel);
            std::fs::create_dir_all(&p).unwrap();
            p
        }
        fn file(&self, rel: &str) {
            std::fs::write(self.0.join(rel), b"x").unwrap();
        }
    }

    impl Drop for TempTree {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn lists_directories_first_then_files_and_hides_dotfiles() {
        let t = TempTree::new("sort");
        t.dir("zeta");
        t.dir("alpha");
        t.file("b.txt");
        t.file("A.txt");
        t.file(".hidden");

        let mut tree = FileTree::new();
        tree.set_root(&t.0);
        let names: Vec<&str> = tree.rows().iter().map(|r| r.name.as_str()).collect();
        assert_eq!(names, vec!["alpha", "zeta", "A.txt", "b.txt"]);
        assert!(tree.rows()[0].is_dir);
        assert!(!tree.rows()[2].is_dir);
    }

    #[test]
    fn hidden_entries_appear_only_once_revealed() {
        let t = TempTree::new("hidden");
        t.file("visible.txt");
        t.file(".secret");

        let mut tree = FileTree::new();
        tree.set_root(&t.0);
        assert_eq!(tree.rows().len(), 1);

        tree.toggle_hidden();
        let names: Vec<&str> = tree.rows().iter().map(|r| r.name.as_str()).collect();
        assert_eq!(names, vec![".secret", "visible.txt"]);

        tree.toggle_hidden();
        assert_eq!(tree.rows().len(), 1);
    }

    #[test]
    fn expanding_a_directory_inlines_its_children_one_level_deeper() {
        let t = TempTree::new("expand");
        let sub = t.dir("sub");
        std::fs::write(sub.join("inner.txt"), b"x").unwrap();
        t.file("top.txt");

        let mut tree = FileTree::new();
        tree.set_root(&t.0);
        assert_eq!(tree.rows().len(), 2, "collapsed: just sub/ and top.txt");

        tree.toggle(&sub);
        let rows: Vec<(&str, usize)> = tree.rows().iter().map(|r| (r.name.as_str(), r.depth)).collect();
        assert_eq!(rows, vec![("sub", 0), ("inner.txt", 1), ("top.txt", 0)]);
        assert!(tree.rows()[0].expanded);

        tree.toggle(&sub);
        assert_eq!(tree.rows().len(), 2, "collapsing puts it back");
    }

    #[test]
    fn expansion_survives_re_rooting_elsewhere_and_back() {
        let t = TempTree::new("reroot");
        let sub = t.dir("sub");
        std::fs::write(sub.join("inner.txt"), b"x").unwrap();
        let other = t.dir("other");

        let mut tree = FileTree::new();
        tree.set_root(&t.0);
        tree.toggle(&sub);
        assert_eq!(tree.rows().len(), 3);

        tree.set_root(&other);
        assert_eq!(tree.rows().len(), 0);

        tree.set_root(&t.0);
        assert_eq!(tree.rows().len(), 3, "sub/ is still expanded");
    }

    #[test]
    fn unreadable_root_yields_no_rows_instead_of_failing() {
        let mut tree = FileTree::new();
        tree.set_root(Path::new("/definitely/not/a/real/path"));
        assert!(tree.rows().is_empty());
    }

    #[test]
    fn non_ascii_names_become_visible_placeholders() {
        // Until the atlas covers more than ASCII, a name that would draw
        // as nothing at all is worse than one that draws as '?'.
        assert_eq!(display_name("日本語.txt"), "???.txt");
        assert_eq!(display_name("plain.txt"), "plain.txt");
    }

    #[test]
    fn shell_quoting_protects_spaces_and_quotes() {
        assert_eq!(shell_quote("src/main.rs"), "src/main.rs");
        assert_eq!(shell_quote("my file.txt"), "'my file.txt'");
        assert_eq!(shell_quote("it's.txt"), r"'it'\''s.txt'");
        // The case that matters: a name crafted to break out of the quotes.
        assert_eq!(shell_quote("a';rm -rf /;'b"), r"'a'\'';rm -rf /;'\''b'");
    }
}
