//! `mare.image`: load / clipboard images, downscale and encode for model input.

use std::io::{Cursor, Read};
use std::path::Path;

use base64::Engine;
use base64::engine::general_purpose::STANDARD;
use image::{DynamicImage, ImageFormat, ImageReader, Limits, imageops::FilterType};
use mlua::{Lua, MultiValue, Table, Value};

use mare_shared::lua::nil_err;

pub const IMAGE_EXTS: &[&str] = &["png", "jpg", "jpeg", "gif", "webp"];
const MAX_INPUT_BYTES: usize = 32 * 1024 * 1024;
const MAX_DECODED_PIXELS: u64 = 4096 * 4096;
const MAX_IMAGE_DIM: u32 = 16_384;
const MAX_ALLOC_BYTES: u64 = 256 * 1024 * 1024;
const DEFAULT_MAX_DIM: u32 = 2000;

fn decoder_limits() -> Limits {
    let mut limits = Limits::default();
    limits.max_image_width = Some(MAX_IMAGE_DIM);
    limits.max_image_height = Some(MAX_IMAGE_DIM);
    limits.max_alloc = Some(MAX_ALLOC_BYTES);
    limits
}

/// Decode encoded image bytes only after checking byte length and header dimensions
/// against [`MAX_DECODED_PIXELS`] and decoder allocation limits.
fn decode_limited(bytes: &[u8]) -> anyhow::Result<DynamicImage> {
    if bytes.len() > MAX_INPUT_BYTES {
        anyhow::bail!("image payload exceeds {MAX_INPUT_BYTES} byte limit");
    }
    let mut dim_reader = ImageReader::new(Cursor::new(bytes)).with_guessed_format()?;
    dim_reader.limits(decoder_limits());
    let (w, h) = dim_reader.into_dimensions()?;
    if w as u64 * h as u64 > MAX_DECODED_PIXELS {
        anyhow::bail!("image dimensions {w}x{h} exceed {MAX_DECODED_PIXELS} pixel limit");
    }
    let mut reader = ImageReader::new(Cursor::new(bytes)).with_guessed_format()?;
    reader.limits(decoder_limits());
    Ok(reader.decode()?)
}

pub struct Encoded {
    pub data: String,
    pub mime: &'static str,
    pub width: u32,
    pub height: u32,
}

pub struct ValidatedImage {
    pub data: String,
    pub mime: &'static str,
}

/// Validate base64-encoded image bytes against magic bytes and bounded decode, returning the
/// trimmed base64 alongside the real detected MIME type (`image/png`, `image/jpeg`, `image/gif`,
/// or `image/webp`).
pub fn validate_base64(data_b64: &str) -> Result<ValidatedImage, String> {
    let trimmed = data_b64.trim();
    if trimmed.is_empty() {
        return Err("empty image data".into());
    }
    if trimmed.len() > MAX_INPUT_BYTES.div_ceil(3) * 4 + 4 {
        return Err(format!("image payload exceeds {MAX_INPUT_BYTES} byte limit"));
    }
    let bytes = STANDARD.decode(trimmed).map_err(|e| format!("invalid base64 image data: {e}"))?;
    if bytes.len() > MAX_INPUT_BYTES {
        return Err(format!("image payload exceeds {MAX_INPUT_BYTES} byte limit"));
    }
    let format = image::guess_format(&bytes).map_err(|e| format!("unrecognized image format: {e}"))?;
    let mime = match format {
        ImageFormat::Png => "image/png",
        ImageFormat::Jpeg => "image/jpeg",
        ImageFormat::Gif => "image/gif",
        ImageFormat::WebP => "image/webp",
        other => return Err(format!("unsupported image format {other:?}; expected PNG, JPEG, GIF, or WebP")),
    };
    decode_limited(&bytes).map_err(|e| format!("corrupt or undecodable image: {e}"))?;
    Ok(ValidatedImage { data: trimmed.to_string(), mime })
}

pub fn is_image_path(p: &Path) -> bool {
    p.extension().and_then(|e| e.to_str()).is_some_and(|e| IMAGE_EXTS.contains(&e.to_ascii_lowercase().as_str()))
}

/// Downscale so the longest side is ≤ `max_dim`, encode as PNG (JPEG for large photos).
pub fn encode(img: DynamicImage, max_dim: u32) -> anyhow::Result<Encoded> {
    let img =
        if img.width().max(img.height()) > max_dim { img.resize(max_dim, max_dim, FilterType::Lanczos3) } else { img };
    let (width, height) = (img.width(), img.height());
    let mut buf = Vec::new();
    img.write_to(&mut Cursor::new(&mut buf), ImageFormat::Png)?;
    let mut mime = "image/png";
    if buf.len() > 1_500_000 {
        let mut jbuf = Vec::new();
        DynamicImage::ImageRgb8(img.to_rgb8()).write_to(&mut Cursor::new(&mut jbuf), ImageFormat::Jpeg)?;
        buf = jbuf;
        mime = "image/jpeg";
    }
    Ok(Encoded { data: STANDARD.encode(&buf), mime, width, height })
}

pub fn load_bytes(bytes: &[u8], max_dim: u32) -> anyhow::Result<Encoded> {
    encode(decode_limited(bytes)?, max_dim)
}

pub fn load_path(p: &Path, max_dim: u32) -> anyhow::Result<Encoded> {
    let file = crate::fs::open_no_follow(p, None)?;
    let mut bytes = Vec::new();
    file.take(MAX_INPUT_BYTES as u64 + 1).read_to_end(&mut bytes)?;
    load_bytes(&bytes, max_dim)
}

/// Read an image from the system clipboard (None when the clipboard holds no image).
pub fn from_clipboard(max_dim: u32) -> anyhow::Result<Option<Encoded>> {
    let mut cb = arboard::Clipboard::new()?;
    clipboard_image(cb.get_image(), max_dim)
}

fn clipboard_image(
    result: Result<arboard::ImageData<'_>, arboard::Error>,
    max_dim: u32,
) -> anyhow::Result<Option<Encoded>> {
    let img = match result {
        Ok(img) => img,
        Err(arboard::Error::ContentNotAvailable) => return Ok(None),
        Err(err) => return Err(err.into()),
    };
    let rgba = image::RgbaImage::from_raw(img.width as u32, img.height as u32, img.bytes.into_owned())
        .ok_or_else(|| anyhow::anyhow!("invalid clipboard image"))?;
    Ok(Some(encode(DynamicImage::ImageRgba8(rgba), max_dim)?))
}

fn to_table(lua: &Lua, e: Encoded) -> mlua::Result<Table> {
    let t = lua.create_table()?;
    t.set("type", "image")?;
    t.set("data", e.data)?;
    t.set("mime", e.mime)?;
    t.set("width", e.width)?;
    t.set("height", e.height)?;
    Ok(t)
}

fn clipboard_values(lua: &Lua, result: anyhow::Result<Option<Encoded>>) -> mlua::Result<MultiValue> {
    match result {
        Ok(Some(image)) => Ok(MultiValue::from_vec(vec![Value::Table(to_table(lua, image)?)])),
        Ok(None) => Ok(MultiValue::from_vec(vec![Value::Nil])),
        Err(err) => nil_err(lua, err),
    }
}

pub fn install(lua: &Lua, mare: &Table, cwd: std::path::PathBuf) -> mlua::Result<()> {
    let t = lua.create_table()?;
    // load(path, {max_dim=2000}) -> {type="image", data, mime, width, height} | nil, err  (async)
    t.set(
        "load",
        lua.create_async_function(move |lua, (path, opts): (String, Option<Table>)| {
            let p = if Path::new(&path).is_absolute() { path.into() } else { cwd.join(&path) };
            // Capture the tool's policy before entering the asynchronous blocking operation.
            let checked = crate::exec_ctx::check(&lua, "read", &p.to_string_lossy());
            async move {
                checked?;
                let max = opts.and_then(|o| o.get::<Option<u32>>("max_dim").ok().flatten()).unwrap_or(2000);
                match tokio::task::spawn_blocking(move || load_path(&p, max)).await {
                    Ok(Ok(e)) => Ok(MultiValue::from_vec(vec![Value::Table(to_table(&lua, e)?)])),
                    Ok(Err(e)) => nil_err(&lua, e),
                    Err(e) => nil_err(&lua, e),
                }
            }
        })?,
    )?;
    // clipboard({max_dim}) -> image | nil  (async)
    t.set(
        "clipboard",
        lua.create_async_function(|lua, opts: Option<Table>| async move {
            let max = opts.and_then(|o| o.get::<Option<u32>>("max_dim").ok().flatten()).unwrap_or(2000);
            clipboard_values(
                &lua,
                tokio::task::spawn_blocking(move || from_clipboard(max)).await.unwrap_or_else(|e| Err(e.into())),
            )
        })?,
    )?;
    t.set("is_image", lua.create_function(|_, p: String| Ok(is_image_path(Path::new(&p))))?)?;
    // to_png(base64_data) -> { mime = "image/png" | "image/jpeg", data = base64 } | nil, err  (async)
    t.set(
        "to_png",
        lua.create_async_function(|lua, data_b64: String| async move {
            let trimmed_len = data_b64.trim().len();
            // Reject obviously oversized base64 strings before decoding (~4/3 expansion).
            if trimmed_len > MAX_INPUT_BYTES.div_ceil(3) * 4 + 4 {
                return nil_err(&lua, format!("image payload exceeds {MAX_INPUT_BYTES} byte limit"));
            }
            let bytes = match STANDARD.decode(data_b64.trim()) {
                Ok(b) => b,
                Err(e) => return nil_err(&lua, e),
            };
            match tokio::task::spawn_blocking(move || load_bytes(&bytes, DEFAULT_MAX_DIM)).await {
                Ok(Ok(enc)) => {
                    let out = lua.create_table()?;
                    out.set("mime", enc.mime)?;
                    out.set("data", enc.data)?;
                    Ok(MultiValue::from_vec(vec![Value::Table(out)]))
                }
                Ok(Err(e)) => nil_err(&lua, e),
                Err(e) => nil_err(&lua, e),
            }
        })?,
    )?;
    mare.set("image", t)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use image::ImageEncoder;
    use image::codecs::png::{CompressionType, FilterType as PngFilter, PngEncoder};

    #[test]
    fn clipboard_no_image_is_empty_but_real_errors_propagate() {
        assert!(clipboard_image(Err(arboard::Error::ContentNotAvailable), 2000).unwrap().is_none());
        assert!(clipboard_image(Err(arboard::Error::ClipboardOccupied), 2000).is_err());
    }

    #[test]
    fn clipboard_lua_result_reports_real_errors() {
        let lua = Lua::new();
        let empty = clipboard_values(&lua, Ok(None)).unwrap();
        assert!(empty.front().unwrap().is_nil() && empty.get(1).is_none());
        let error = clipboard_values(&lua, Err(anyhow::anyhow!("clipboard failure"))).unwrap();
        assert!(error.front().unwrap().is_nil());
        assert!(
            matches!(error.get(1), Some(Value::String(message)) if message.to_string_lossy().contains("clipboard failure"))
        );
    }

    fn encode_rgba_png_b64(width: u32, height: u32, noisy: bool) -> String {
        let mut raw = vec![0u8; (width as usize) * (height as usize) * 4];
        if noisy {
            // Smooth per-pixel gradients inside 8x8 blocks: default PNG exceeds 1.5 MiB and triggers
            // JPEG fallback, while JPEG compresses the aligned 8x8 ramps well under 1.5 MiB.
            for (idx, chunk) in raw.as_chunks_mut::<4>().0.iter_mut().enumerate() {
                let x = (idx as u32) % width;
                let y = (idx as u32) / width;
                let bx = x / 8;
                let by = y / 8;
                let h = (bx.wrapping_mul(73_856_093) ^ by.wrapping_mul(19_349_663)).wrapping_mul(83_492_791);
                let base_r = h & 0x7f;
                let base_g = (h >> 8) & 0x7f;
                let base_b = (h >> 16) & 0x7f;
                chunk[0] = (base_r + ((x % 8) * 14) + ((y % 8) * 3)) as u8;
                chunk[1] = (base_g + ((y % 8) * 14) + ((x % 8) * 3)) as u8;
                chunk[2] = (base_b + ((x + y) % 16) * 7) as u8;
                chunk[3] = 255;
            }
        }
        let mut out = Vec::new();
        PngEncoder::new_with_quality(&mut out, CompressionType::Fast, PngFilter::NoFilter)
            .write_image(&raw, width, height, image::ExtendedColorType::Rgba8)
            .unwrap();
        STANDARD.encode(&out)
    }

    #[test]
    fn load_path_rejects_over_pixel_budget() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("large.png");
        std::fs::write(&path, STANDARD.decode(encode_rgba_png_b64(4500, 4500, false)).unwrap()).unwrap();
        assert!(load_path(&path, 2000).is_err());
    }

    #[test]
    fn load_path_rejects_over_byte_budget() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("large.png");
        std::fs::write(&path, STANDARD.decode(encode_rgba_png_b64(1, 1, false)).unwrap()).unwrap();
        std::fs::OpenOptions::new().write(true).open(&path).unwrap().set_len(MAX_INPUT_BYTES as u64 + 1).unwrap();
        assert!(load_path(&path, 2000).is_err());
    }

    #[cfg(unix)]
    #[test]
    fn load_path_rejects_symlink() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("image.png");
        std::fs::write(&path, STANDARD.decode(encode_rgba_png_b64(1, 1, false)).unwrap()).unwrap();
        assert!(load_path(&path, 2000).is_ok());
        let link = dir.path().join("link.png");
        std::os::unix::fs::symlink(&path, &link).unwrap();
        assert!(load_path(&link, 2000).is_err());
    }

    #[tokio::test]
    async fn to_png_rejects_over_pixel_budget() {
        let b64 = encode_rgba_png_b64(4500, 4500, false);
        let lua = Lua::new();
        let mare = lua.create_table().unwrap();
        install(&lua, &mare, std::env::temp_dir()).unwrap();
        lua.globals().set("mare", mare).unwrap();
        lua.globals().set("img_b64", b64).unwrap();
        let (val, err): (Value, Option<String>) =
            lua.load("return mare.image.to_png(img_b64)").eval_async().await.unwrap();
        assert!(val.is_nil(), "expected over-pixel image (> 4096x4096) to be rejected");
        assert!(err.is_some(), "expected error string explaining rejection");
    }

    #[tokio::test]
    async fn to_png_downscales_to_2000_and_switches_large_output_to_jpeg() {
        let wide_b64 = encode_rgba_png_b64(2400, 1200, false);
        let noisy_b64 = encode_rgba_png_b64(2000, 2000, true);
        let lua = Lua::new();
        let mare = lua.create_table().unwrap();
        install(&lua, &mare, std::env::temp_dir()).unwrap();
        lua.globals().set("mare", mare).unwrap();
        lua.globals().set("wide_b64", wide_b64).unwrap();
        lua.globals().set("noisy_b64", noisy_b64).unwrap();

        let (wide_mime, wide_data): (String, String) = lua
            .load("local r = assert(mare.image.to_png(wide_b64)); return r.mime, r.data")
            .eval_async()
            .await
            .unwrap();
        assert_eq!(wide_mime, "image/png");
        let wide_img = image::load_from_memory(&STANDARD.decode(&wide_data).unwrap()).unwrap();
        assert_eq!((wide_img.width(), wide_img.height()), (2000, 1000));

        let (noisy_mime, noisy_data): (String, String) = lua
            .load("local r = assert(mare.image.to_png(noisy_b64)); return r.mime, r.data")
            .eval_async()
            .await
            .unwrap();
        assert_eq!(noisy_mime, "image/jpeg");
        let noisy_bytes = STANDARD.decode(&noisy_data).unwrap();
        assert!(noisy_bytes.len() <= 1_500_000);
        let noisy_img = image::load_from_memory(&noisy_bytes).unwrap();
        assert_eq!((noisy_img.width(), noisy_img.height()), (2000, 2000));
    }

    #[test]
    fn validate_base64_detects_format_and_rejects_corrupt_pixels() {
        let valid_b64 = encode_rgba_png_b64(4, 4, false);
        let validated = validate_base64(&format!("  {valid_b64}\n")).expect("valid PNG must pass validate_base64");
        assert_eq!(validated.mime, "image/png");
        assert_eq!(validated.data, valid_b64);

        // Keep PNG signature + valid IHDR (first 33 bytes), corrupt IDAT pixel bytes.
        let mut corrupt = STANDARD.decode(&valid_b64).unwrap();
        assert!(corrupt.len() > 45);
        for b in &mut corrupt[37..45] {
            *b ^= 0xff;
        }
        let corrupt_b64 = STANDARD.encode(&corrupt);
        assert!(
            validate_base64(&corrupt_b64).is_err(),
            "PNG with valid header/dimensions but corrupt pixel data must be rejected"
        );
        assert!(validate_base64("SGVsbG8=").is_err(), "non-image bytes must be rejected");
        assert!(validate_base64(&encode_rgba_png_b64(4500, 4500, false)).is_err(), "over-pixel PNG must be rejected");
    }
}
