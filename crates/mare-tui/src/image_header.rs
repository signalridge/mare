//! A picture's pixel size from its file header, without decoding it: the draw path
//! reserves the picture's final rows from it while the preprocessing queue prepares the picture.

use std::sync::Mutex;

use super::image::{FileStamp, MAX_FILE, Source, source_stamp};

type SizeKey = (Source, Option<FileStamp>);
type SizeCache = lru::LruCache<SizeKey, (u32, u32)>;

/// Sizes read so far (a live picture is drawn every frame until it's ready): 256 sources.
static SIZES: Mutex<Option<SizeCache>> = Mutex::new(None);
const SIZES_MAX: std::num::NonZeroUsize = std::num::NonZeroUsize::new(256).unwrap();

/// Width and height in pixels, or `None` when the header can't be read (missing file, unknown
/// or corrupt format, a file over the size cap). Only the first bytes are read.
pub fn dimensions(src: &Source) -> Option<(u32, u32)> {
    let stamp = match src {
        Source::Path(_) => Some(source_stamp(src)?),
        _ => None,
    };
    let key = (src.clone(), stamp);
    if let Some(d) = SIZES.lock().unwrap_or_else(|e| e.into_inner()).as_mut().and_then(|c| c.get(&key).copied()) {
        return Some(d);
    }
    let d = match src {
        Source::Resource(reference) => (reference.width, reference.height),
        Source::Path(p) => {
            if stamp.is_some_and(|s| s.len > MAX_FILE) {
                return None;
            }
            image::ImageReader::open(p).ok()?.with_guessed_format().ok()?.into_dimensions().ok()?
        }
        Source::Bytes(b) => {
            image::ImageReader::new(std::io::Cursor::new(&b[..])).with_guessed_format().ok()?.into_dimensions().ok()?
        }
    };
    SIZES.lock().unwrap_or_else(|e| e.into_inner()).get_or_insert_with(|| lru::LruCache::new(SIZES_MAX)).put(key, d);
    Some(d)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_header_is_enough_and_garbage_has_no_size() {
        let mut img = image::RgbaImage::new(7, 3);
        img.put_pixel(0, 0, image::Rgba([1, 2, 3, 255]));
        let mut png = std::io::Cursor::new(Vec::new());
        image::DynamicImage::ImageRgba8(img).write_to(&mut png, image::ImageFormat::Png).unwrap();
        let mut png = png.into_inner();
        // the pixel data is broken (it wouldn't decode): the size is still known
        let n = png.len();
        for b in &mut png[n - 20..n - 12] {
            *b ^= 0xff;
        }
        let broken: Source = Source::Bytes(png.into());
        assert_eq!(dimensions(&broken), Some((7, 3)));
        assert!(
            image::load_from_memory(match &broken {
                Source::Bytes(b) => b,
                Source::Path(_) | Source::Resource(_) => unreachable!(),
            })
            .is_err()
        );
        assert_eq!(dimensions(&Source::Bytes(b"not a picture at all".to_vec().into())), None);
        assert_eq!(dimensions(&Source::Path("/no/such/picture.png".into())), None);
    }
}
