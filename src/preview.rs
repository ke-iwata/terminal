//! Loading files for the in-terminal preview overlay.
//!
//! Three routes, tried in order of how well they preserve the file:
//!   1. Known image extensions are decoded directly, at full quality.
//!   2. Anything that looks like text is shown as text.
//!   3. Everything else (PDFs, Office documents, video, ...) goes
//!      through QuickLook -- the same thumbnail service Finder's
//!      spacebar preview uses, so anything macOS can preview, this can.
//!
//! All of it is blocking and some of it is slow (QuickLook spawns a
//! helper process), so `App` runs `load` on a worker thread and takes
//! the result back through the event loop -- nothing here touches the
//! window.

use std::path::{Path, PathBuf};

/// Cap on decoded image dimensions. Beyond this we downscale before
/// handing pixels to the GPU: a 12000x8000 photo would otherwise be a
/// ~380MB texture upload to display at a few hundred pixels across.
const MAX_IMAGE_DIM: u32 = 4096;
/// How much of a file to sniff when deciding whether it's text.
const SNIFF_BYTES: usize = 8192;
/// Caps on the text preview itself. This is a look-at-it view, not a
/// pager -- the terminal is right there for the whole file.
const MAX_TEXT_BYTES: u64 = 1 << 20;
const MAX_TEXT_LINES: usize = 5000;
/// Long lines are truncated rather than wrapped: a minified bundle on
/// one 2MB line shouldn't turn into thousands of preview rows.
const MAX_TEXT_LINE_CHARS: usize = 500;
/// Pixel size requested from QuickLook. Generous enough that a PDF page
/// stays readable when the overlay is most of a large window.
const QUICKLOOK_SIZE: u32 = 1600;
/// How long to give QuickLook before giving up and killing it.
///
/// Not optional: `qlmanage -t` does not always terminate. Handed a file
/// no generator claims (a bare `.bin`), it can sit forever rather than
/// exiting with a failure -- waiting on it unbounded would leak a worker
/// thread and leave the overlay on "Loading..." for the rest of the
/// session.
const QUICKLOOK_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(8);

const IMAGE_EXTENSIONS: &[&str] = &["png", "jpg", "jpeg", "gif", "bmp", "webp", "tif", "tiff"];

pub enum Content {
    /// Straight RGBA8, ready for a texture upload.
    Image { pixels: Vec<u8>, width: u32, height: u32 },
    Text(Vec<String>),
}

impl Content {
    /// A short "300x200" / "42 lines" note for the overlay caption.
    pub fn describe(&self) -> String {
        match self {
            Content::Image { width, height, .. } => format!("{width}x{height}"),
            Content::Text(lines) => format!("{} lines", lines.len()),
        }
    }
}

/// Decode `path` for preview, or explain why it can't be. Blocking; call
/// off the event loop.
pub fn load(path: &Path) -> Result<Content, String> {
    let metadata = std::fs::metadata(path).map_err(|e| format!("can't read: {e}"))?;
    if metadata.is_dir() {
        return Err("directories have no preview".to_string());
    }

    let extension = path
        .extension()
        .map(|e| e.to_string_lossy().to_lowercase())
        .unwrap_or_default();

    if IMAGE_EXTENSIONS.contains(&extension.as_str()) {
        return decode_image(&std::fs::read(path).map_err(|e| format!("can't read: {e}"))?);
    }
    if let Some(lines) = read_as_text(path, metadata.len()) {
        return Ok(Content::Text(lines));
    }
    let png = quicklook_png(path).ok_or_else(|| "no preview available for this file type".to_string())?;
    decode_image(&png)
}

fn decode_image(bytes: &[u8]) -> Result<Content, String> {
    let decoded = image::load_from_memory(bytes).map_err(|e| format!("can't decode: {e}"))?;
    // `thumbnail` only ever shrinks -- an image already within the cap
    // passes through at full resolution.
    let decoded = if decoded.width() > MAX_IMAGE_DIM || decoded.height() > MAX_IMAGE_DIM {
        decoded.thumbnail(MAX_IMAGE_DIM, MAX_IMAGE_DIM)
    } else {
        decoded
    };
    let rgba = decoded.to_rgba8();
    let (width, height) = rgba.dimensions();
    if width == 0 || height == 0 {
        return Err("image has no pixels".to_string());
    }
    Ok(Content::Image { pixels: rgba.into_raw(), width, height })
}

/// Read `path` as text, or `None` if it doesn't look like text. The test
/// is the practical one every tool uses: a NUL byte in the first block
/// means binary, and so does anything that isn't valid UTF-8.
fn read_as_text(path: &Path, len: u64) -> Option<Vec<String>> {
    use std::io::Read;

    let mut file = std::fs::File::open(path).ok()?;
    let mut head = vec![0u8; SNIFF_BYTES.min(len.max(1) as usize)];
    let read = file.read(&mut head).ok()?;
    head.truncate(read);
    if head.contains(&0) {
        return None;
    }
    // A truncated multi-byte character at the sniff boundary is not
    // evidence of binary, so only the fully-decodable prefix has to be
    // valid -- `from_utf8`'s error tells us which case we're in.
    if let Err(e) = std::str::from_utf8(&head) {
        let truncated_tail = e.error_len().is_none() && e.valid_up_to() + 4 > head.len();
        if !truncated_tail {
            return None;
        }
    }

    let mut contents = String::new();
    std::fs::File::open(path)
        .ok()?
        .take(MAX_TEXT_BYTES)
        .read_to_string(&mut contents)
        .ok()?;

    Some(
        contents
            .lines()
            .take(MAX_TEXT_LINES)
            .map(|line| {
                // Tabs would collapse to a single column in a monospace
                // layout that has no tab stops of its own.
                let expanded = line.replace('\t', "    ");
                display_text(&expanded)
            })
            .collect(),
    )
}

/// Replace characters the glyph atlas can't draw (printable ASCII only)
/// with `?`, so non-ASCII text shows as visibly-unrenderable rather than
/// as invisible gaps. Same trade-off as the file tree's names.
fn display_text(line: &str) -> String {
    line.chars()
        .take(MAX_TEXT_LINE_CHARS)
        .map(|c| if c.is_ascii_graphic() || c == ' ' { c } else { '?' })
        .collect()
}

/// Render `path` through QuickLook and return the resulting PNG bytes.
/// This is what gives PDFs (and Keynote decks, and .docx, and video
/// posters) a preview without this app knowing anything about those
/// formats: `qlmanage -t` asks the system for exactly the thumbnail
/// Finder would draw.
fn quicklook_png(path: &Path) -> Option<Vec<u8>> {
    // A per-call directory, so reading "the file QuickLook wrote" can't
    // pick up a leftover from an earlier preview. QuickLook names its
    // output after the input (`report.pdf` -> `report.pdf.png`), but
    // reading whatever landed in an empty directory avoids depending on
    // that convention.
    let dir = std::env::temp_dir().join(format!("keterm-preview-{}-{:?}", std::process::id(), std::thread::current().id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).ok()?;

    let child = std::process::Command::new("qlmanage")
        .arg("-t")
        .arg("-s")
        .arg(QUICKLOOK_SIZE.to_string())
        .arg("-o")
        .arg(&dir)
        .arg(path)
        // qlmanage is chatty on both streams even when it succeeds.
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn();

    let succeeded = child.ok().is_some_and(|mut child| match wait_with_timeout(&mut child, QUICKLOOK_TIMEOUT) {
        Some(status) => status.success(),
        None => {
            let _ = child.kill();
            let _ = child.wait();
            false
        }
    });

    // Even a "successful" run can produce nothing (some types have no
    // generator), so the output file is the real test. Read before
    // cleaning up, and clean up on every path -- including the failures,
    // which would otherwise leave a directory per attempt in /tmp.
    let png = succeeded.then(|| {
        let entry = std::fs::read_dir(&dir).ok()?.flatten().next()?;
        std::fs::read(entry.path()).ok()
    });
    let _ = std::fs::remove_dir_all(&dir);
    png.flatten()
}

/// `Child::wait` with a deadline, polled rather than blocking -- `std`
/// has no timed wait. `None` means it was still running when time ran
/// out; the caller decides whether to kill it.
fn wait_with_timeout(child: &mut std::process::Child, timeout: std::time::Duration) -> Option<std::process::ExitStatus> {
    let deadline = std::time::Instant::now() + timeout;
    loop {
        match child.try_wait() {
            Ok(Some(status)) => return Some(status),
            // Already reaped or otherwise unwaitable: nothing to wait for.
            Err(_) => return None,
            Ok(None) => {}
        }
        if std::time::Instant::now() >= deadline {
            return None;
        }
        std::thread::sleep(std::time::Duration::from_millis(25));
    }
}

/// The label shown above a preview: the file's name, since the overlay
/// is about one file at a time.
pub fn title_for(path: &Path) -> String {
    path.file_name()
        .map(|n| display_text(&n.to_string_lossy()))
        .unwrap_or_else(|| path.display().to_string())
}

/// The state of the one preview the overlay can be showing.
pub struct Preview {
    pub path: PathBuf,
    pub state: State,
    /// First visible line of a text preview.
    pub scroll: usize,
}

pub enum State {
    Loading,
    Ready(Content),
    Failed(String),
}

impl Preview {
    pub fn loading(path: PathBuf) -> Self {
        Preview { path, state: State::Loading, scroll: 0 }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct TempDir(PathBuf);

    impl TempDir {
        fn new(tag: &str) -> Self {
            let dir = std::env::temp_dir().join(format!("keterm-preview-test-{tag}"));
            let _ = std::fs::remove_dir_all(&dir);
            std::fs::create_dir_all(&dir).unwrap();
            TempDir(dir)
        }
        fn write(&self, name: &str, bytes: &[u8]) -> PathBuf {
            let path = self.0.join(name);
            std::fs::write(&path, bytes).unwrap();
            path
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn text_files_load_as_text() {
        let t = TempDir::new("text");
        let path = t.write("notes.txt", b"first\nsecond\n");
        match load(&path).unwrap() {
            Content::Text(lines) => assert_eq!(lines, vec!["first", "second"]),
            _ => panic!("expected text"),
        }
    }

    #[test]
    fn tabs_are_expanded_and_long_lines_truncated() {
        let t = TempDir::new("shape");
        let long = "x".repeat(MAX_TEXT_LINE_CHARS + 50);
        let path = t.write("wide.txt", format!("a\tb\n{long}\n").as_bytes());
        match load(&path).unwrap() {
            Content::Text(lines) => {
                assert_eq!(lines[0], "a    b");
                assert_eq!(lines[1].chars().count(), MAX_TEXT_LINE_CHARS);
            }
            _ => panic!("expected text"),
        }
    }

    #[test]
    fn non_ascii_text_becomes_visible_placeholders() {
        // Until the atlas covers more than ASCII, a line that would draw
        // as nothing is worse than one that draws as '?'.
        let t = TempDir::new("utf8");
        let path = t.write("jp.txt", "hello 世界\n".as_bytes());
        match load(&path).unwrap() {
            Content::Text(lines) => assert_eq!(lines, vec!["hello ??"]),
            _ => panic!("expected text"),
        }
    }

    #[test]
    fn binary_files_are_not_treated_as_text() {
        let t = TempDir::new("binary");
        // Tests the sniff directly rather than going through `load`:
        // `load` would fall through to QuickLook, which for a type no
        // generator claims can sit for the full timeout.
        let nul = t.write("blob.bin", &[0x00, 0x01, 0x02, 0xff, 0xfe]);
        assert!(read_as_text(&nul, 5).is_none(), "a NUL byte means binary");

        let invalid_utf8 = t.write("bad.txt", &[0xff, 0xfe, 0x41, 0x42]);
        assert!(read_as_text(&invalid_utf8, 4).is_none(), "invalid UTF-8 means binary");
    }

    #[test]
    fn a_multibyte_character_split_by_the_sniff_boundary_still_reads_as_text() {
        // The sniff reads a fixed prefix, which can land mid-character.
        // That is not evidence of binary, and treating it as such would
        // send a perfectly good UTF-8 file to QuickLook instead.
        let t = TempDir::new("boundary");
        let mut contents = "a".repeat(SNIFF_BYTES - 1);
        contents.push('あ'); // 3 bytes, so only its first lands in the sniff
        let path = t.write("split.txt", contents.as_bytes());
        assert!(read_as_text(&path, contents.len() as u64).is_some());
    }

    #[test]
    fn a_real_png_decodes_to_rgba() {
        let t = TempDir::new("png");
        let mut encoded = Vec::new();
        {
            // 2x1: one opaque red pixel, one opaque blue one.
            let raw: Vec<u8> = vec![255, 0, 0, 255, 0, 0, 255, 255];
            let buffer = image::RgbaImage::from_raw(2, 1, raw).unwrap();
            image::DynamicImage::ImageRgba8(buffer)
                .write_to(&mut std::io::Cursor::new(&mut encoded), image::ImageFormat::Png)
                .unwrap();
        }
        let path = t.write("dot.png", &encoded);
        match load(&path).unwrap() {
            Content::Image { pixels, width, height } => {
                assert_eq!((width, height), (2, 1));
                assert_eq!(&pixels[0..4], &[255, 0, 0, 255]);
                assert_eq!(&pixels[4..8], &[0, 0, 255, 255]);
            }
            _ => panic!("expected an image"),
        }
    }

    #[test]
    fn a_directory_is_reported_rather_than_previewed() {
        let t = TempDir::new("dir");
        assert!(load(&t.0).is_err());
    }

    #[test]
    fn describe_summarizes_each_kind() {
        let image = Content::Image { pixels: vec![0; 4], width: 640, height: 480 };
        assert_eq!(image.describe(), "640x480");
        assert_eq!(Content::Text(vec!["a".into(), "b".into()]).describe(), "2 lines");
    }
}
