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
const MAX_ENCODED_BYTES: usize = 1_500_000;

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

pub fn is_image_path(p: &Path) -> bool {
    p.extension().and_then(|e| e.to_str()).is_some_and(|e| IMAGE_EXTS.contains(&e.to_ascii_lowercase().as_str()))
}

fn is_opaque(img: &DynamicImage) -> bool {
    if !img.color().has_alpha() {
        return true;
    }
    match img {
        DynamicImage::ImageRgba8(i) => i.pixels().all(|p| p.0[3] == u8::MAX),
        DynamicImage::ImageLumaA8(i) => i.pixels().all(|p| p.0[1] == u8::MAX),
        DynamicImage::ImageRgba16(i) => i.pixels().all(|p| p.0[3] == u16::MAX),
        DynamicImage::ImageLumaA16(i) => i.pixels().all(|p| p.0[1] == u16::MAX),
        DynamicImage::ImageRgba32F(i) => i.pixels().all(|p| p.0[3] >= 1.0),
        _ => img.to_rgba8().pixels().all(|p| p.0[3] == u8::MAX),
    }
}

/// Downscale so the longest side is ≤ `max_dim`. For opaque images, pick the smaller of PNG
/// and JPEG so photographic inputs are not expanded while crisp diagrams stay PNG; for
/// transparent inputs, always preserve alpha as PNG (downscaling further if needed to stay
/// within `MAX_ENCODED_BYTES`).
pub fn encode(img: DynamicImage, max_dim: u32) -> anyhow::Result<Encoded> {
    let mut img =
        if img.width().max(img.height()) > max_dim { img.resize(max_dim, max_dim, FilterType::Lanczos3) } else { img };
    let mut buf = Vec::new();
    img.write_to(&mut Cursor::new(&mut buf), ImageFormat::Png)?;
    let mut mime = "image/png";
    if is_opaque(&img) {
        let mut jbuf = Vec::new();
        DynamicImage::ImageRgb8(img.to_rgb8()).write_to(&mut Cursor::new(&mut jbuf), ImageFormat::Jpeg)?;
        if jbuf.len() < buf.len() {
            buf = jbuf;
            mime = "image/jpeg";
        }
    } else {
        while buf.len() > MAX_ENCODED_BYTES {
            let cur_max = img.width().max(img.height());
            if cur_max <= 1 {
                anyhow::bail!("encoded transparent PNG ({} bytes) exceeds {MAX_ENCODED_BYTES} byte limit", buf.len());
            }
            let scale = ((MAX_ENCODED_BYTES as f64) / (buf.len() as f64)).sqrt() * 0.9;
            let next_max = ((cur_max as f64) * scale).floor().clamp(1.0, (cur_max - 1) as f64) as u32;
            img = img.resize(next_max, next_max, FilterType::Lanczos3);
            buf.clear();
            img.write_to(&mut Cursor::new(&mut buf), ImageFormat::Png)?;
        }
    }
    Ok(Encoded { data: STANDARD.encode(&buf), mime, width: img.width(), height: img.height() })
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
    fn repro_265_photographic_jpeg_not_expanded_to_larger_png() {
        let (width, height) = (640u32, 480u32);
        let mut rgb = image::RgbImage::new(width, height);
        for (x, y, px) in rgb.enumerate_pixels_mut() {
            let h = x.wrapping_mul(2_654_435_761) ^ y.wrapping_mul(2_246_822_519);
            let r = (((x * 3 + y * 5) ^ (h >> 16)) & 0xff) as u8;
            let g = (((x * 7 + y * 2) ^ (h >> 8)) & 0xff) as u8;
            let b = (((x * 11 + y * 13) ^ h) & 0xff) as u8;
            *px = image::Rgb([r, g, b]);
        }
        let mut jpeg_bytes = Vec::new();
        DynamicImage::ImageRgb8(rgb).write_to(&mut Cursor::new(&mut jpeg_bytes), ImageFormat::Jpeg).unwrap();
        let encoded = load_bytes(&jpeg_bytes, 2000).unwrap();
        let out_bytes = STANDARD.decode(&encoded.data).unwrap();
        assert!(
            out_bytes.len() <= jpeg_bytes.len() * 2,
            "opaque photographic JPEG ({} bytes) expanded to {} bytes as {}",
            jpeg_bytes.len(),
            out_bytes.len(),
            encoded.mime,
        );
        assert_eq!(encoded.mime, "image/jpeg");
    }

    #[test]
    fn transparent_png_preserves_alpha_and_flat_opaque_stays_png() {
        // Noisy RGBA image with non-opaque alpha: must keep PNG so alpha is not lost.
        let mut rgba = image::RgbaImage::new(256, 256);
        for (x, y, px) in rgba.enumerate_pixels_mut() {
            let h = x.wrapping_mul(2_654_435_761) ^ y.wrapping_mul(2_246_822_519);
            let alpha = if (x + y) % 17 == 0 { 128 } else { 255 };
            *px = image::Rgba([((h >> 16) & 0xff) as u8, ((h >> 8) & 0xff) as u8, (h & 0xff) as u8, alpha]);
        }
        let mut png_bytes = Vec::new();
        DynamicImage::ImageRgba8(rgba).write_to(&mut Cursor::new(&mut png_bytes), ImageFormat::Png).unwrap();
        let encoded_alpha = load_bytes(&png_bytes, 2000).unwrap();
        assert_eq!(encoded_alpha.mime, "image/png", "transparent input must preserve alpha as PNG");
        let decoded = image::load_from_memory(&STANDARD.decode(&encoded_alpha.data).unwrap()).unwrap();
        assert!(decoded.color().has_alpha(), "decoded output must retain alpha channel");
        assert_eq!(decoded.to_rgba8().get_pixel(0, 0).0[3], 128);

        // Flat opaque diagram: PNG compresses smaller than JPEG and stays lossless PNG.
        let flat = image::RgbImage::from_pixel(512, 512, image::Rgb([30, 40, 50]));
        let encoded_flat = encode(DynamicImage::ImageRgb8(flat), 2000).unwrap();
        assert_eq!(encoded_flat.mime, "image/png", "flat opaque image should pick smaller PNG");
    }

    #[test]
    fn oversized_transparent_png_preserves_alpha_within_byte_limit() {
        // Construct a noisy semi-transparent RGBA image whose unscaled PNG exceeds 1.5 MiB.
        let mut rgba = image::RgbaImage::new(1000, 1000);
        for (x, y, px) in rgba.enumerate_pixels_mut() {
            let h = x.wrapping_mul(2_654_435_761) ^ y.wrapping_mul(2_246_822_519) ^ (x.wrapping_mul(y));
            let alpha = 64 + ((h >> 24) & 0x7f) as u8;
            *px = image::Rgba([((h >> 16) & 0xff) as u8, ((h >> 8) & 0xff) as u8, (h & 0xff) as u8, alpha]);
        }
        let img = DynamicImage::ImageRgba8(rgba);
        let mut raw_png = Vec::new();
        img.write_to(&mut Cursor::new(&mut raw_png), ImageFormat::Png).unwrap();
        assert!(
            raw_png.len() > MAX_ENCODED_BYTES,
            "test precondition: raw semi-transparent PNG must exceed {MAX_ENCODED_BYTES} bytes (got {})",
            raw_png.len()
        );

        let encoded = encode(img, 2000).unwrap();
        assert_eq!(encoded.mime, "image/png", "oversized transparent PNG must never drop alpha to JPEG");
        let out_bytes = STANDARD.decode(&encoded.data).unwrap();
        assert!(
            out_bytes.len() <= MAX_ENCODED_BYTES,
            "downscaled transparent PNG must stay <= {MAX_ENCODED_BYTES} bytes (got {})",
            out_bytes.len()
        );
        let decoded = image::load_from_memory(&out_bytes).unwrap();
        assert_eq!((decoded.width(), decoded.height()), (encoded.width, encoded.height));
        assert!(decoded.color().has_alpha(), "decoded output must retain alpha channel");
        assert!(
            decoded.to_rgba8().pixels().any(|p| p.0[3] < u8::MAX),
            "decoded output must retain semi-transparent pixels"
        );
    }
}
