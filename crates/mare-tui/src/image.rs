//! Images in the terminal: protocol detection, encoding and inline layout.
//!
//! - **protocols**, in order: Kitty graphics, iTerm2 inline images (OSC 1337), sixel; a one-line
//!   text placeholder otherwise;
//! - **detection**: environment first (tmux and Zellij
//!   off, herdr checked before the outer terminal's variables that leak into its panes), then the
//!   startup probe's XTVERSION / DA1 replies (sixel = DA1 attribute 4);
//!   `mare.o.images_protocol` overrides;
//! - **encoding**: decode / scale with `image`; Kitty and iTerm2 get PNG (Kitty chunked 4096
//!   base64 bytes per APC, a stable image id per picture); sixel is quantized
//!   with `quantette` (Wu) and encoded to sixel;
//!   the work runs off the UI thread on at most 2 workers, deduplicated per picture, queued
//!   requests wait and are never dropped ([`super::image_queue`]); until a picture is ready
//!   its final rows are reserved blank (size from
//!   the file header, [`super::image_header`]) and the UI loop is woken when it is ([`ready`]);
//!   a block waits for its pictures at most 5 s before it commits ([`super::image_hold`]);
//!   results are kept in an LRU;
//! - **deletion**: a Kitty image that changes or leaves the live area is deleted by id
//!   ([`kitty_delete`], called from the frame diff); sixel / iTerm2 pictures are cell content
//!   and are overwritten by the redraw;
//! - **layout**: an image takes `rows` × `cols` cells (from the cell's
//!   pixel size, 9×18 when the terminal didn't report it). It's `rows` empty lines; the last
//!   carries the sequence as [`Line::raw`]: move up to the image's first row, draw without moving
//!   the cursor (Kitty `C=1`, iTerm2 `doNotMoveCursor=1`), move back. No save/restore cursor —
//!   the scroll-region commit path already uses DECSC/DECRC;
//! - **safety**: sequences are built only here and registered with the write exit
//!   ([`super::out::allow_image`]); its final check passes exactly these, so the same sequence
//!   kinds from model, tool or file text stay filtered. Pixel and byte caps per image.

use std::sync::{Arc, LazyLock, Mutex, RwLock};

use base64::Engine as _;
use image::DynamicImage;

use super::style::{Line, Span, Theme};

/// Largest decoded image accepted (pixels); bigger ones are scaled down first.
pub const MAX_PIXELS: u64 = 4096 * 4096;
/// Largest encoded payload written for one image (bytes); bigger ones are scaled down further.
pub const MAX_BYTES: usize = 2 * 1024 * 1024;
/// Largest source file read.
pub const MAX_FILE: u64 = 32 * 1024 * 1024;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Default)]
pub enum Protocol {
    Kitty,
    Iterm2,
    Sixel,
    #[default]
    None,
}

impl Protocol {
    pub fn name(self) -> &'static str {
        match self {
            Self::Kitty => "kitty",
            Self::Iterm2 => "iterm2",
            Self::Sixel => "sixel",
            Self::None => "none",
        }
    }
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "kitty" => Some(Self::Kitty),
            "iterm2" => Some(Self::Iterm2),
            "sixel" => Some(Self::Sixel),
            "none" | "off" | "false" => Some(Self::None),
            _ => None,
        }
    }
}

static PROTOCOL: RwLock<Protocol> = RwLock::new(Protocol::None);
/// Cell size in pixels.
static CELL_PX: RwLock<(u32, u32)> = RwLock::new((9, 18));

/// Bumped when the protocol or the cell size changes: renders cached with pictures in them
/// (Markdown, tool-output previews, the streaming answer) hold the old pictures, so their keys
/// carry this and they are built again, queueing the pictures at the new size.
static GENERATION: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

pub fn generation() -> u64 {
    GENERATION.load(std::sync::atomic::Ordering::Acquire)
}

fn changed() {
    GENERATION.fetch_add(1, std::sync::atomic::Ordering::AcqRel);
}

pub fn set_protocol(p: Protocol) {
    if std::mem::replace(&mut *PROTOCOL.write().unwrap_or_else(|e| e.into_inner()), p) != p {
        changed();
    }
}
pub fn protocol() -> Protocol {
    *PROTOCOL.read().unwrap_or_else(|e| e.into_inner())
}
pub fn set_cell_px(w: u32, h: u32) {
    if w > 0 && h > 0 && std::mem::replace(&mut *CELL_PX.write().unwrap_or_else(|e| e.into_inner()), (w, h)) != (w, h) {
        changed();
    }
}
pub fn cell_px() -> (u32, u32) {
    *CELL_PX.read().unwrap_or_else(|e| e.into_inner())
}

/// What the environment says. `Some(None)`: known to
/// have no usable protocol (multiplexers, terminals without one); `None`: unknown — the probe
/// decides.
pub fn detect_env(var: &dyn Fn(&str) -> Option<String>) -> Option<Protocol> {
    let lower = |k: &str| var(k).unwrap_or_default().to_ascii_lowercase();
    let (prog, term) = (lower("TERM_PROGRAM"), lower("TERM"));
    // image protocols are unreliable under tmux; Zellij doesn't pass them
    if var("TMUX").is_some() || term.starts_with("tmux") || var("ZELLIJ").is_some() || term.starts_with("screen") {
        return Some(Protocol::None);
    }
    // herdr supports Kitty graphics; check it before the outer terminal's variables
    if prog == "herdr" || prog == "rex" {
        return Some(Protocol::Kitty);
    }
    if var("KITTY_WINDOW_ID").is_some() || prog == "kitty" || term.contains("kitty") {
        return Some(Protocol::Kitty);
    }
    if prog == "ghostty" || term.contains("ghostty") || var("GHOSTTY_RESOURCES_DIR").is_some() {
        return Some(Protocol::Kitty);
    }
    if var("WEZTERM_PANE").is_some() || prog == "wezterm" {
        return Some(Protocol::Kitty);
    }
    if prog == "warpterminal" || var("WARP_SESSION_ID").is_some() {
        return Some(Protocol::Kitty);
    }
    if var("ITERM_SESSION_ID").is_some() || prog == "iterm.app" {
        return Some(Protocol::Iterm2);
    }
    if prog == "apple_terminal" || prog == "vscode" || prog == "zed" || term == "linux" || term == "dumb" {
        return Some(Protocol::None);
    }
    None
}

/// Environment first, then the probe: XTVERSION names the terminal, DA1
/// attribute 4 means sixel.
pub fn detect(var: &dyn Fn(&str) -> Option<String>, xtversion: Option<&str>, da1: &[u32]) -> Protocol {
    if let Some(p) = detect_env(var) {
        return p;
    }
    let v = xtversion.unwrap_or("").to_ascii_lowercase();
    if ["kitty", "ghostty", "wezterm", "konsole", "warp"].iter().any(|t| v.contains(t)) {
        return Protocol::Kitty;
    }
    if v.contains("iterm2") {
        return Protocol::Iterm2;
    }
    if da1.contains(&4) {
        return Protocol::Sixel;
    }
    Protocol::None
}

// ---- layout -------------------------------------------------------------------------------------

/// Cells an image of `w`×`h` pixels takes, fitted into `max_cols` × `max_rows` keeping its aspect
/// ratio. Never upscales past the image's own pixel size.
pub fn fit(w: u32, h: u32, max_cols: usize, max_rows: usize) -> (usize, usize) {
    fit_in(w, h, max_cols, max_rows, cell_px())
}

fn fit_in(w: u32, h: u32, max_cols: usize, max_rows: usize, (cw, ch): (u32, u32)) -> (usize, usize) {
    let (w, h) = (w.max(1) as f64, h.max(1) as f64);
    let (max_cols, max_rows) = (max_cols.max(1) as f64, max_rows.max(1) as f64);
    let natural_cols = (w / cw as f64).ceil();
    let cols = natural_cols.min(max_cols).max(1.0);
    let mut rows = (cols * cw as f64 * h / (w * ch as f64)).ceil().max(1.0);
    let mut cols = cols;
    if rows > max_rows {
        rows = max_rows;
        cols = (rows * ch as f64 * w / (h * cw as f64)).ceil().clamp(1.0, max_cols);
    }
    (cols as usize, rows as usize)
}

// ---- encoding -----------------------------------------------------------------------------------

/// Kitty graphics: PNG, base64 in 4096-byte chunks, `C=1` (cursor stays),
/// `q=2` (no replies), a stable image id so a redraw replaces the earlier placement.
pub fn encode_kitty(png: &[u8], cols: usize, rows: usize, id: u32) -> Vec<String> {
    const CHUNK: usize = 4096;
    let b64 = base64::engine::general_purpose::STANDARD.encode(png);
    let params = format!("a=T,f=100,q=2,C=1,c={cols},r={rows},i={id},p=1");
    if b64.len() <= CHUNK {
        return vec![format!("\x1b_G{params};{b64}\x1b\\")];
    }
    let mut out = vec![];
    let mut off = 0;
    while off < b64.len() {
        let end = (off + CHUNK).min(b64.len());
        let more = if end < b64.len() { 1 } else { 0 };
        let chunk = &b64[off..end];
        out.push(if off == 0 {
            format!("\x1b_G{params},m={more};{chunk}\x1b\\")
        } else {
            format!("\x1b_Gm={more};{chunk}\x1b\\")
        });
        off = end;
    }
    out
}

/// iTerm2 inline image: OSC 1337 File, size in cells, cursor not moved.
pub fn encode_iterm2(png: &[u8], cols: usize, rows: usize) -> String {
    let b64 = base64::engine::general_purpose::STANDARD.encode(png);
    format!(
        "\x1b]1337;File=inline=1;size={};width={cols};height={rows};preserveAspectRatio=1;doNotMoveCursor=1:{b64}\x07",
        png.len()
    )
}

/// Sixel, after Yazi `yazi-adapter/src/drivers/sixel.rs` (MIT): Wu quantization by `quantette`
/// (256 colors, index 0 transparent when the image has alpha), one run-length line per pixel row.
pub fn encode_sixel(img: &DynamicImage) -> Option<String> {
    use quantette::PaletteSize;
    use quantette::deps::palette::{Srgb, cast::ComponentsAs};
    use quantette::wu::{BinnerU8x3, WuU8x3};
    use std::fmt::Write as _;
    if img.width() == 0 || img.height() == 0 {
        return None;
    }
    let alpha = img.color().has_alpha();
    let rgb = img.to_rgb8();
    let rgba = img.to_rgba8();
    let colors: &[Srgb<u8>] = rgb.as_raw()[..(rgb.pixels().len() * 3)].components_as();
    let wu = WuU8x3::run_slice(colors, BinnerU8x3::rgb()).ok()?;
    let map = wu.color_map(PaletteSize::try_from(256u16 - alpha as u16).ok()?);
    let indices = {
        use quantette::color_map::IndexedColorMap as _;
        map.map_to_indices(colors)
    };
    let palette = map.into_palette().into_vec();
    let (w, h) = (img.width(), img.height());
    let mut s = String::with_capacity(indices.len());
    let _ = write!(s, "\x1bP9;1q\"1;1;{w};{h}");
    for (i, c) in palette.iter().enumerate() {
        let _ = write!(
            s,
            "#{};2;{};{};{}",
            i + alpha as usize,
            c.red as u16 * 100 / 255,
            c.green as u16 * 100 / 255,
            c.blue as u16 * 100 / 255
        );
    }
    for y in 0..h {
        let ch = (b'?' + (1 << (y % 6))) as char;
        let (mut last, mut repeat) = (0u8, 0usize);
        let flush = |s: &mut String, last: u8, repeat: usize| {
            if repeat > 1 {
                let _ = write!(s, "#{last}!{repeat}{ch}");
            } else {
                let _ = write!(s, "#{last}{ch}");
            }
        };
        for x in 0..w {
            let idx =
                if alpha && rgba.get_pixel(x, y)[3] == 0 { 0 } else { indices[(y * w + x) as usize] + alpha as u8 };
            if idx == last || repeat == 0 {
                (last, repeat) = (idx, repeat + 1);
                continue;
            }
            flush(&mut s, last, repeat);
            (last, repeat) = (idx, 1);
        }
        flush(&mut s, last, repeat);
        s.push('$');
        if y % 6 == 5 {
            s.push('-');
        }
    }
    s.push_str("\x1b\\");
    Some(s)
}

fn png_bytes(img: &DynamicImage) -> Option<Vec<u8>> {
    let mut out = std::io::Cursor::new(Vec::new());
    img.write_to(&mut out, image::ImageFormat::Png).ok()?;
    Some(out.into_inner())
}

/// FNV-1a: a stable id per picture (Kitty `i=`), 1..2^24.
fn image_id(bytes: &[u8]) -> u32 {
    let mut h: u32 = 0x811c_9dc5;
    for b in bytes {
        h ^= *b as u32;
        h = h.wrapping_mul(0x0100_0193);
    }
    (h & 0x00ff_ffff).max(1)
}

/// Kitty image ids placed by a row's raw sequence (the `i=` of each `a=T` command).
pub fn kitty_ids(raw: &str) -> impl Iterator<Item = u32> + '_ {
    raw.split("\x1b_G").skip(1).filter_map(|cmd| {
        let params = cmd.split(';').next()?;
        if !params.split(',').any(|p| p == "a=T") {
            return None;
        }
        params.split(',').find_map(|p| p.strip_prefix("i=")?.parse().ok())
    })
}

/// Delete the placement of a Kitty image that left the live area or changed there (by id; `q=2`
/// keeps the terminal quiet). Registered with the write exit like every core image sequence.
pub fn kitty_delete(id: u32) -> String {
    let s = format!("\x1b_Ga=d,d=i,i={id},p=1,q=2\x1b\\");
    super::out::allow_image(s.as_bytes());
    s
}

// ---- source → lines -----------------------------------------------------------------------------

/// Where an image comes from.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum Source {
    Path(String),
    Bytes(Arc<[u8]>),
    Resource(mare_shared::image_resource::Reference),
}

fn path_label(p: &str) -> String {
    std::path::Path::new(p).file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_else(|| p.to_string())
}

impl Source {
    fn label(&self) -> String {
        match self {
            Self::Path(p) => path_label(p),
            Self::Resource(_) => "stored image".into(),
            Self::Bytes(b) => format!("{} bytes", b.len()),
        }
    }
    fn load(&self) -> Result<DynamicImage, String> {
        let bytes: Vec<u8> = match self {
            Self::Path(p) => {
                use std::io::Read as _;
                let file = std::fs::File::open(p).map_err(|e| e.to_string())?;
                let mut buf = Vec::new();
                file.take(MAX_FILE + 1).read_to_end(&mut buf).map_err(|e| e.to_string())?;
                if buf.len() as u64 > MAX_FILE {
                    return Err("too large".into());
                }
                buf
            }
            Self::Bytes(b) => {
                if b.len() as u64 > MAX_FILE {
                    return Err("too large".into());
                }
                b.to_vec()
            }
            Self::Resource(reference) => {
                let b = reference.load()?;
                if b.len() as u64 > MAX_FILE {
                    return Err("too large".into());
                }
                b.to_vec()
            }
        };
        let reader =
            image::ImageReader::new(std::io::Cursor::new(&bytes)).with_guessed_format().map_err(|e| e.to_string())?;
        let (w, h) = reader.into_dimensions().map_err(|e| e.to_string())?;
        if let Self::Resource(reference) = self
            && (w, h) != (reference.width, reference.height)
        {
            return Err("image resource dimensions changed".into());
        }
        if w as u64 * h as u64 > MAX_PIXELS * 4 {
            return Err(format!("{w}×{h} is too large"));
        }
        image::load_from_memory(&bytes).map_err(|e| e.to_string())
    }
}

/// Metadata fingerprint for a local path image so in-place overwrites and recovered partial
/// writes invalidate cached dimensions and encoded payloads.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub(crate) struct FileStamp {
    pub(crate) len: u64,
    modified: Option<std::time::SystemTime>,
    #[cfg(unix)]
    ino: u64,
    #[cfg(unix)]
    ctime: i64,
    #[cfg(unix)]
    ctime_nsec: i64,
}

pub(crate) fn source_stamp(src: &Source) -> Option<FileStamp> {
    let Source::Path(p) = src else {
        return None;
    };
    let meta = std::fs::metadata(p).ok()?;
    #[cfg(unix)]
    let (ino, ctime, ctime_nsec) = {
        use std::os::unix::fs::MetadataExt as _;
        (meta.ino(), meta.ctime(), meta.ctime_nsec())
    };
    Some(FileStamp {
        len: meta.len(),
        modified: meta.modified().ok(),
        #[cfg(unix)]
        ino,
        #[cfg(unix)]
        ctime,
        #[cfg(unix)]
        ctime_nsec,
    })
}

pub(crate) fn path_sample_hash(path: &str) -> Option<u64> {
    use std::hash::{Hash as _, Hasher as _};
    use std::io::{Read as _, Seek as _, SeekFrom};
    let mut file = std::fs::File::open(path).ok()?;
    let len = file.metadata().ok()?.len();
    let mut h = rustc_hash::FxHasher::default();
    len.hash(&mut h);
    let mut head = [0u8; 64];
    let n = file.read(&mut head).ok()?;
    head[..n].hash(&mut h);
    if len > 64 {
        let _ = file.seek(SeekFrom::End(-(len.min(64) as i64)));
        let mut tail = [0u8; 64];
        let m = file.read(&mut tail).ok()?;
        tail[..m].hash(&mut h);
    }
    Some(h.finish())
}

/// Encoded image for one (source, file stamp, size, protocol, cell size): cached, since nodes re-render every frame.
#[derive(Clone)]
struct Encoded {
    rows: usize,
    seqs: Arc<[String]>,
}

/// One prepared picture: source, optional local file metadata stamp, size limits, protocol and
/// cell size — the id the cache and the preprocessing queue deduplicate by.
type CacheKey = (Source, Option<FileStamp>, usize, usize, Protocol, (u32, u32));
type Done = Result<Encoded, String>;
#[derive(Clone)]
struct CacheEntry {
    done: Done,
    /// Content head/tail sample hash recorded when a local `Source::Path` failed to decode, so an
    /// in-place overwrite within the same filesystem timestamp tick still invalidates the cached `Err`.
    err_sample: Option<u64>,
}
static CACHE: Mutex<Option<lru::LruCache<CacheKey, CacheEntry>>> = Mutex::new(None);
const CACHE_MAX: std::num::NonZeroUsize = std::num::NonZeroUsize::new(32).unwrap();

fn cached(key: &CacheKey) -> Option<Done> {
    let entry = CACHE.lock().unwrap_or_else(|e| e.into_inner()).as_mut().and_then(|c| c.get(key).cloned())?;
    if entry.done.is_err()
        && let Source::Path(p) = &key.0
        && entry.err_sample != path_sample_hash(p)
    {
        return None;
    }
    Some(entry.done)
}

// ---- preprocessing --
//
// The draw path never decodes: a picture that isn't in the cache is queued and keeps its final
// rows blank until a worker has prepared it; the worker stores it and wakes the UI loop
// ([`ready`]), whose next frame draws it. Every key carries the cell size and the size limits, so
// a resize that changes them queues the new size instead of reusing an old encoding.

/// Pictures prepared at once.
const WORKERS: usize = 2;

static QUEUE: LazyLock<super::image_queue::Queue<CacheKey, Done>> = LazyLock::new(|| {
    super::image_queue::Queue::new(
        WORKERS,
        |key| std::panic::catch_unwind(|| prepare(key)).unwrap_or_else(|_| Err("decode failed".into())),
        |key, res| {
            // If a local file already changed while we failed to decode a partial write, skip
            // caching the stale Err so the next frame immediately re-queues the updated file.
            if res.is_err() && matches!(key.0, Source::Path(_)) && source_stamp(&key.0) != key.1 {
                ready_notify().notify_one();
                return;
            }
            // least recently used out (a picture on screen is hit every frame and stays)
            let mut guard = CACHE.lock().unwrap_or_else(|e| e.into_inner());
            let cache = guard.get_or_insert_with(|| lru::LruCache::new(CACHE_MAX));
            if matches!(key.0, Source::Path(_)) {
                let stale: Vec<CacheKey> = cache
                    .iter()
                    .filter(|(k, _)| {
                        k.0 == key.0 && k.1 != key.1 && k.2 == key.2 && k.3 == key.3 && k.4 == key.4 && k.5 == key.5
                    })
                    .map(|(k, _)| k.clone())
                    .collect();
                for old in stale {
                    cache.pop(&old);
                }
            }
            let err_sample = if res.is_err() {
                match &key.0 {
                    Source::Path(p) => path_sample_hash(p),
                    _ => None,
                }
            } else {
                None
            };
            cache.put(key.clone(), CacheEntry { done: res.clone(), err_sample });
            drop(guard);
            ready_notify().notify_one();
        },
    )
});

static REPLAY_PENDING: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
pub fn take_replay_pending() -> bool {
    REPLAY_PENDING.swap(false, std::sync::atomic::Ordering::AcqRel)
}

static READY: std::sync::OnceLock<tokio::sync::Notify> = std::sync::OnceLock::new();

fn ready_notify() -> &'static tokio::sync::Notify {
    READY.get_or_init(tokio::sync::Notify::new)
}

/// Resolves when a queued picture has been prepared: the UI loop draws a frame (a wake-up that
/// arrives while nobody waits is kept for the next wait, so none is lost).
pub async fn ready() {
    ready_notify().notified().await
}

thread_local! {
    static WINDOW:std::cell::Cell<Option<(usize,usize)>>=const {std::cell::Cell::new(None)};
    static BASE_ROW:std::cell::Cell<usize>=const {std::cell::Cell::new(0)};
    static SUPPRESSED:std::cell::Cell<u64>=const {std::cell::Cell::new(0)};
}
pub fn layout_pass_active() -> bool {
    MEASURE_ONLY.with(|v| v.get()) || WINDOW.with(|v| v.get().is_some())
}
pub fn suppressed() -> u64 {
    SUPPRESSED.with(|v| v.get())
}
pub fn window(start: usize, end: usize) -> Window {
    Window(WINDOW.with(|v| v.replace(Some((start, end)))), BASE_ROW.with(|v| v.replace(0)))
}
pub struct Window(Option<(usize, usize)>, usize);
impl Drop for Window {
    fn drop(&mut self) {
        WINDOW.with(|v| v.set(self.0));
        BASE_ROW.with(|v| v.set(self.1));
    }
}
pub fn row_offset(rows: usize) -> RowOffset {
    RowOffset(BASE_ROW.with(|v| v.replace(v.get().saturating_add(rows))))
}
pub struct RowOffset(usize);
impl Drop for RowOffset {
    fn drop(&mut self) {
        BASE_ROW.with(|v| v.set(self.0));
    }
}

/// Layout-only passes reserve rows from known dimensions without IO, queueing or protocol bytes.
pub fn measure_only() -> MeasureOnly {
    MeasureOnly(MEASURE_ONLY.with(|v| v.replace(true)))
}
pub struct MeasureOnly(bool);
impl Drop for MeasureOnly {
    fn drop(&mut self) {
        MEASURE_ONLY.with(|v| v.set(self.0));
    }
}
thread_local! {static MEASURE_ONLY:std::cell::Cell<bool>=const {std::cell::Cell::new(false)};}

thread_local! {
    /// Reserved areas this thread drew for pictures still being prepared.
    static WAITED: std::cell::Cell<u64> = const { std::cell::Cell::new(0) };
    /// Text placeholders drawn instead of a picture not ready yet (see [`degrade`]).
    static DEGRADED: std::cell::Cell<u64> = const { std::cell::Cell::new(0) };
    /// Rendering here waits for the picture instead (see [`blocking`]).
    static BLOCKING: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
    /// Rendering here draws a picture not ready yet as its text placeholder (see [`degrade`]).
    static DEGRADE: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

/// Reserved areas drawn on this thread for pictures not prepared yet. A render that raised it
/// has blank rows where a picture will be: don't commit it to the scrollback.
pub fn waited() -> u64 {
    WAITED.with(|w| w.get())
}

/// Renders on this thread that drew a picture not ready yet (reserved rows or, under
/// [`degrade`], text): their output changes once it's ready, so caches don't keep it.
pub fn unsettled() -> u64 {
    WAITED.with(|w| w.get()) + DEGRADED.with(|d| d.get())
}

/// While the guard lives, a picture that isn't ready is drawn as its one-line text placeholder
/// (the commit of a block that waited too long, [`super::image_hold`]); it stays queued.
#[must_use]
pub fn degrade() -> Degrade {
    Degrade(DEGRADE.with(|d| d.replace(true)))
}

/// See [`degrade`]; dropping it restores the previous mode.
pub struct Degrade(bool);

impl Drop for Degrade {
    fn drop(&mut self) {
        DEGRADE.with(|d| d.set(self.0));
    }
}

/// While the guard lives, this thread's renders wait for their pictures (through the same
/// queue) instead of drawing placeholders: for output that can't be drawn again, the history
/// rebuilt into the scrollback after a resize.
#[must_use]
pub fn blocking() -> Blocking {
    Blocking(BLOCKING.with(|b| b.replace(true)))
}

/// See [`blocking`]; dropping it restores the previous mode.
pub struct Blocking(bool);

impl Drop for Blocking {
    fn drop(&mut self) {
        BLOCKING.with(|b| b.set(self.0));
    }
}

/// The prepared picture, or `None` while it is queued (then it is queued now, at most once).
fn encode(src: &Source, max_cols: usize, max_rows: usize, proto: Protocol) -> Option<Done> {
    let key = (src.clone(), source_stamp(src), max_cols, max_rows, proto, cell_px());
    if let Some(v) = cached(&key) {
        return Some(v);
    }
    let job = QUEUE.submit(key);
    if BLOCKING.with(|b| b.get()) && !matches!(src, Source::Resource(_)) {
        return Some(job.wait());
    }
    // Resource loaders need the Lua event loop; never block that thread during replay.
    let result = job.get();
    if result.is_none() && BLOCKING.with(|b| b.get()) && matches!(src, Source::Resource(_)) {
        REPLAY_PENDING.store(true, std::sync::atomic::Ordering::Release);
    }
    result
}

/// Cache-only failure observation for a node's complete source fallback; never performs IO.
pub fn failed(src: &Source, max_cols: usize, max_rows: usize, width: usize) -> bool {
    let (mc, mr, proto, px) = (max_cols.min(width).max(1), max_rows.max(1), protocol(), cell_px());
    if !matches!(src, Source::Path(_)) {
        return matches!(cached(&(src.clone(), None, mc, mr, proto, px)), Some(Err(_)));
    }
    CACHE.lock().unwrap_or_else(|e| e.into_inner()).as_ref().is_some_and(|cache| {
        cache
            .iter()
            .find(|(k, _)| &k.0 == src && k.2 == mc && k.3 == mr && k.4 == proto && k.5 == px)
            .is_some_and(|(_, entry)| entry.done.is_err())
    })
}

fn prepare(key: &CacheKey) -> Done {
    let (src, _stamp, max_cols, max_rows, proto, (cw, ch)) = key;
    let (max_cols, max_rows, proto, cw, ch) = (*max_cols, *max_rows, *proto, *cw, *ch);
    (|| {
        #[cfg(test)]
        tests::gate(src);
        let img = src.load()?;
        // the same layout the reserved area had (from the header's size)
        let (cols, rows) = fit_in(img.width(), img.height(), max_cols, max_rows, (cw, ch));
        // pixels the cells hold: never send more than that (and never more than the cap)
        let mut tw = (cols as u32 * cw).min(img.width());
        let mut th = (rows as u32 * ch).min(img.height());
        let mut out = None;
        let mut final_rows = rows;
        for _ in 0..4 {
            let scaled = if tw < img.width() || th < img.height() {
                img.resize(tw, th, image::imageops::Triangle)
            } else {
                img.clone()
            };
            let seqs: Vec<String> = match proto {
                Protocol::Kitty => {
                    let png = png_bytes(&scaled).ok_or("png encode failed")?;
                    encode_kitty(&png, cols, rows, image_id(&png))
                }
                Protocol::Iterm2 => vec![encode_iterm2(&png_bytes(&scaled).ok_or("png encode failed")?, cols, rows)],
                Protocol::Sixel => vec![encode_sixel(&scaled).ok_or("sixel encode failed")?],
                Protocol::None => vec![],
            };
            if seqs.iter().map(String::len).sum::<usize>() <= MAX_BYTES {
                if proto == Protocol::Sixel {
                    final_rows = (scaled.height().div_ceil(ch) as usize).max(1);
                }
                out = Some(seqs);
                break;
            }
            // over the byte cap: halve the pixels and try again
            tw = (tw / 2).max(1);
            th = (th / 2).max(1);
        }
        let seqs = out.ok_or("too large to send")?;
        Ok(Encoded { rows: final_rows, seqs: seqs.into() })
    })()
}

/// Narrowest room (cells) a picture is drawn in; narrower gets the text placeholder.
pub const MIN_COLS: usize = 4;

/// The text shown when there's no protocol or the image can't be loaded.
pub fn placeholder(src: &Source, note: Option<&str>, width: usize, theme: &Theme) -> Vec<Line> {
    placeholder_with_label(src, None, note, width, theme)
}

/// Like [`placeholder`], allowing an optional path hint to supply the display label for
/// in-memory image bytes.
pub fn placeholder_with_label(
    src: &Source,
    path_hint: Option<&str>,
    note: Option<&str>,
    width: usize,
    theme: &Theme,
) -> Vec<Line> {
    let label = path_hint.filter(|p| !p.is_empty()).map(path_label).unwrap_or_else(|| src.label());
    let mut t = format!("[image: {label}");
    if let Some((w, h)) = super::image_header::dimensions(src) {
        use std::fmt::Write as _;
        let _ = write!(t, " {w}×{h}");
    }
    if let Some(n) = note {
        t.push_str(" — ");
        t.push_str(n);
    }
    t.push(']');
    vec![super::style::truncate_line(&Line::new(vec![Span::styled(t, theme.fg("md.image"))]), width)]
}

/// An image as layout lines (`mare.ui.image`, Markdown images, the `images` plugin).
pub fn render(src: &Source, max_rows: usize, max_cols: usize, width: usize, theme: &Theme) -> Vec<Line> {
    render_inner(src, max_rows, max_cols, width, theme, None, None)
}

/// A stable image occurrence, independent of the shared source/PNG encoding cache. Reusing a
/// Kitty content id would delete every other placement when that data is retransmitted.
pub fn render_with_id(
    src: &Source,
    max_rows: usize,
    max_cols: usize,
    width: usize,
    theme: &Theme,
    image_id: u32,
) -> Vec<Line> {
    render_inner(src, max_rows, max_cols, width, theme, Some(image_id), None)
}

/// Like [`render_with_id`], carrying an optional path hint for placeholder display when `src`
/// holds decoded bytes.
pub fn render_with_label(
    src: &Source,
    max_rows: usize,
    max_cols: usize,
    width: usize,
    theme: &Theme,
    image_id: u32,
    path_hint: Option<&str>,
) -> Vec<Line> {
    render_inner(src, max_rows, max_cols, width, theme, Some(image_id), path_hint)
}

fn render_inner(
    src: &Source,
    max_rows: usize,
    max_cols: usize,
    width: usize,
    theme: &Theme,
    occurrence: Option<u32>,
    path_hint: Option<&str>,
) -> Vec<Line> {
    let proto = protocol();
    if matches!(src,Source::Resource(reference) if reference.resolver.closed()) {
        return placeholder_with_label(src, path_hint, Some("image resolver closed"), width, theme);
    }
    let (max_cols, max_rows) = (max_cols.min(width).max(1), max_rows.max(1));
    if proto == Protocol::None || max_cols < MIN_COLS {
        return placeholder_with_label(src, path_hint, None, width, theme);
    }
    // the size from the header (no decode): unreadable means it can't be drawn
    let Some((w, h)) = super::image_header::dimensions(src) else {
        return placeholder_with_label(src, path_hint, None, width, theme);
    };
    if w as u64 * h as u64 > MAX_PIXELS * 4 {
        return placeholder_with_label(src, path_hint, Some(&format!("{w}×{h} is too large")), width, theme);
    }
    let rows = fit_in(w, h, max_cols, max_rows, cell_px()).1;
    let outside = WINDOW.with(|window| {
        window.get().is_some_and(|(start, end)| {
            BASE_ROW.with(|base| base.get() >= end || base.get().saturating_add(rows) <= start)
        })
    });
    if MEASURE_ONLY.with(|v| v.get()) || outside {
        SUPPRESSED.with(|v| v.set(v.get() + 1));
        return vec![Line::empty(); rows];
    }
    let enc = match encode(src, max_cols, max_rows, proto) {
        Some(Ok(e)) => e,
        Some(Err(e)) => return placeholder_with_label(src, path_hint, Some(&e), width, theme),
        None if DEGRADE.with(|d| d.get()) => {
            DEGRADED.with(|d| d.set(d.get() + 1));
            return placeholder_with_label(src, path_hint, None, width, theme);
        }
        None => {
            // still being prepared: its final rows, blank, so nothing moves when it arrives
            //
            WAITED.with(|w| w.set(w.get() + 1));
            return vec![Line::empty(); fit_in(w, h, max_cols, max_rows, cell_px()).1];
        }
    };
    finish_encoded(&enc, proto, occurrence)
}

fn finish_encoded(enc: &Encoded, proto: Protocol, occurrence: Option<u32>) -> Vec<Line> {
    use std::fmt::Write;
    let up = enc.rows.saturating_sub(1);
    let mut raw = String::from("\r");
    if up > 0 {
        let _ = write!(raw, "\x1b[{up}A");
    }
    for sequence in enc.seqs.iter() {
        let located = if proto == Protocol::Kitty
            && let Some(id) = occurrence
            && let Some((header, data)) = sequence.split_once(';')
            && header.starts_with("\x1b_Ga=T,")
        {
            let header = header
                .split(',')
                .map(|field| if field.starts_with("i=") { format!("i={id}") } else { field.to_owned() })
                .collect::<Vec<_>>()
                .join(",");
            std::borrow::Cow::Owned(format!("{header};{data}"))
        } else {
            std::borrow::Cow::Borrowed(sequence.as_str())
        };
        super::out::allow_image(located.as_bytes());
        raw.push_str(&located);
    }
    match proto {
        // Sixel cursor position depends on whether the final graphics newline (`-`) fired.
        Protocol::Sixel => {
            if up > 0 || enc.seqs.last().is_some_and(|s| s.ends_with("-\x1b\\")) {
                raw.push_str("\r\x1b[1A");
            } else {
                raw.push('\r');
            }
        }
        _ if up > 0 => {
            let _ = write!(raw, "\x1b[{up}B");
        }
        _ => {}
    }
    let mut lines = vec![Line::empty(); enc.rows];
    if let Some(last) = lines.last_mut() {
        last.raw = Some(raw.into());
    }
    lines
}

/// Tests that change the global protocol / cell size hold this.
#[cfg(test)]
pub(crate) static TEST_LOCK: Mutex<()> = Mutex::new(());

#[cfg(test)]
mod tests;
