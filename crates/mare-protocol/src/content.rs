//! Unified media content types (`Attachment`, `ContentPart`) and audio/video detection.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

/// File extensions treated as video inputs when `kind` / `mime` are omitted.
pub const VIDEO_EXTENSIONS: &[&str] = &["mp4", "mov", "webm", "mkv", "avi", "m4v", "mpeg", "mpg"];

/// File extensions treated as audio inputs when `kind` / `mime` are omitted.
pub const AUDIO_EXTENSIONS: &[&str] = &["wav", "mp3", "ogg", "opus", "flac", "m4a", "aac", "pcm"];

/// Unsupported media capability kind detected on an input attachment or content part.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MediaKind {
    Audio,
    Video,
}

impl MediaKind {
    pub fn capability(self) -> &'static str {
        match self {
            Self::Audio => "audio",
            Self::Video => "video",
        }
    }
}

/// Explicit attachment kind (`file`, `image`, `video`, or `audio`).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum AttachmentKind {
    File,
    Image,
    Video,
    Audio,
}

/// A file (text is inlined into the prompt), an image (path, URL, or base64 data), or a
/// video/audio attachment (recognised and explicitly rejected with `unsupported`).
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct Attachment {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub kind: Option<AttachmentKind>,
    #[serde(default)]
    pub path: Option<String>,
    /// base64 media data (with `mime`)
    #[serde(default)]
    pub data: Option<String>,
    #[serde(default)]
    pub mime: Option<String>,
    /// media URL or `data:` URI
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
    /// optional display name
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
}

fn has_extension(path: &str, exts: &[&str]) -> bool {
    std::path::Path::new(path)
        .extension()
        .and_then(|e| e.to_str())
        .is_some_and(|e| exts.contains(&e.to_ascii_lowercase().as_str()))
}

fn detect_media(mime: Option<&str>, url: Option<&str>, path: Option<&str>, check_ext: bool) -> Option<MediaKind> {
    if let Some(m) = mime.map(str::to_ascii_lowercase) {
        if m.starts_with("video/") {
            return Some(MediaKind::Video);
        }
        if m.starts_with("audio/") {
            return Some(MediaKind::Audio);
        }
    }
    if let Some(u) = url.map(str::to_ascii_lowercase) {
        if u.starts_with("data:video/") {
            return Some(MediaKind::Video);
        }
        if u.starts_with("data:audio/") {
            return Some(MediaKind::Audio);
        }
    }
    if check_ext && let Some(p) = path {
        if has_extension(p, VIDEO_EXTENSIONS) {
            return Some(MediaKind::Video);
        }
        if has_extension(p, AUDIO_EXTENSIONS) {
            return Some(MediaKind::Audio);
        }
    }
    None
}

impl Attachment {
    /// Classify whether this attachment represents an audio or video input.
    pub fn media_kind(&self) -> Option<MediaKind> {
        match self.kind {
            Some(AttachmentKind::Video) => return Some(MediaKind::Video),
            Some(AttachmentKind::Audio) => return Some(MediaKind::Audio),
            _ => {}
        }
        detect_media(self.mime.as_deref(), self.url.as_deref(), self.path.as_deref(), self.kind.is_none())
    }

    /// Whether this attachment represents a video input.
    pub fn is_video(&self) -> bool {
        self.media_kind() == Some(MediaKind::Video)
    }

    /// Whether this attachment represents an audio input.
    pub fn is_audio(&self) -> bool {
        self.media_kind() == Some(MediaKind::Audio)
    }
}

/// Unified multimodal prompt/message content part alongside file and image attachments.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ContentPart {
    Text {
        text: String,
    },
    Image {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        data: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        mime: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        url: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        path: Option<String>,
    },
    Video {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        data: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        mime: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        url: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        path: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        name: Option<String>,
    },
    Audio {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        data: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        mime: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        url: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        path: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        name: Option<String>,
    },
    File {
        path: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        mime: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        name: Option<String>,
    },
}

impl ContentPart {
    /// Classify whether this content part carries an unsupported audio or video input,
    /// including via extension, MIME, `data:` URI, or base64+MIME on `image`/`file` variants.
    pub fn media_kind(&self) -> Option<MediaKind> {
        match self {
            Self::Text { .. } => None,
            Self::Video { .. } => Some(MediaKind::Video),
            Self::Audio { .. } => Some(MediaKind::Audio),
            Self::Image { mime, url, path, .. } => detect_media(mime.as_deref(), url.as_deref(), path.as_deref(), true),
            Self::File { path, mime, .. } => detect_media(mime.as_deref(), None, Some(path.as_str()), true),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn attachment_detection_covers_audio_and_video_across_forms() {
        assert!(Attachment { kind: Some(AttachmentKind::Video), ..Default::default() }.is_video());
        assert!(Attachment { mime: Some("video/mp4".into()), ..Default::default() }.is_video());
        assert!(Attachment { url: Some("data:video/webm;base64,AAAA".into()), ..Default::default() }.is_video());
        assert!(Attachment { path: Some("clip.MP4".into()), ..Default::default() }.is_video());
        assert!(Attachment { path: Some("demo.webm".into()), ..Default::default() }.is_video());

        assert!(Attachment { kind: Some(AttachmentKind::Audio), ..Default::default() }.is_audio());
        assert!(Attachment { mime: Some("audio/wav".into()), ..Default::default() }.is_audio());
        assert!(Attachment { url: Some("data:audio/wav;base64,AAAA".into()), ..Default::default() }.is_audio());
        assert!(Attachment { path: Some("clip.MP3".into()), ..Default::default() }.is_audio());
        assert!(
            Attachment { data: Some("AAAA".into()), mime: Some("audio/ogg".into()), ..Default::default() }.is_audio()
        );

        assert!(
            !Attachment { mime: Some("image/png".into()), data: Some("AA==".into()), ..Default::default() }.is_video()
        );
        assert!(!Attachment { path: Some("notes.txt".into()), ..Default::default() }.is_video());
        assert!(
            !Attachment { kind: Some(AttachmentKind::File), path: Some("clip.mp4".into()), ..Default::default() }
                .is_video()
        );
    }

    #[test]
    fn content_part_detection_covers_audio_and_video_across_forms() {
        let cases = [
            (MediaKind::Video, json!({ "type": "video", "url": "https://example.com/clip.mp4" })),
            (MediaKind::Video, json!({ "type": "file", "path": "clip.mp4" })),
            (MediaKind::Video, json!({ "type": "file", "path": "blob.bin", "mime": "video/mp4" })),
            (MediaKind::Video, json!({ "type": "image", "url": "data:video/webm;base64,AAAA" })),
            (MediaKind::Video, json!({ "type": "image", "data": "AAAA", "mime": "video/mp4" })),
            (MediaKind::Audio, json!({ "type": "audio", "path": "note.txt" })),
            (MediaKind::Audio, json!({ "type": "file", "path": "clip.mp3" })),
            (MediaKind::Audio, json!({ "type": "file", "path": "blob.bin", "mime": "audio/wav" })),
            (MediaKind::Audio, json!({ "type": "image", "url": "data:audio/wav;base64,AAAA" })),
            (MediaKind::Audio, json!({ "type": "image", "data": "AAAA", "mime": "audio/ogg" })),
        ];
        for (expected, raw) in cases {
            let part: ContentPart = serde_json::from_value(raw.clone()).unwrap();
            assert_eq!(part.media_kind(), Some(expected), "{raw}");
        }
        let img: ContentPart =
            serde_json::from_value(json!({ "type": "image", "mime": "image/png", "data": "AA==" })).unwrap();
        assert_eq!(img.media_kind(), None);
    }
}
