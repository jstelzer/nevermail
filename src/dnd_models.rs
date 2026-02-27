use std::borrow::Cow;

use cosmic::iced::clipboard::mime::{AllowedMimeTypes, AsMimeTypes};

/// External file drop data (text/uri-list from file managers).
#[derive(Debug, Clone)]
pub struct DraggedFiles(pub String);

impl AllowedMimeTypes for DraggedFiles {
    fn allowed() -> Cow<'static, [String]> {
        Cow::Owned(vec!["text/uri-list".to_string()])
    }
}

impl TryFrom<(Vec<u8>, String)> for DraggedFiles {
    type Error = String;
    fn try_from((bytes, _mime): (Vec<u8>, String)) -> Result<Self, Self::Error> {
        String::from_utf8(bytes)
            .map(DraggedFiles)
            .map_err(|e| e.to_string())
    }
}

/// Internal message drag data for message-to-folder moves.
#[derive(Debug, Clone)]
pub struct DraggedMessage {
    pub envelope_hash: u64,
    pub source_mailbox: u64,
}

const NEVERLIGHT_MAIL_MIME: &str = "application/x-neverlight-mail-message";

impl AsMimeTypes for DraggedMessage {
    fn available(&self) -> Cow<'static, [String]> {
        Cow::Owned(vec![NEVERLIGHT_MAIL_MIME.to_string()])
    }

    fn as_bytes(&self, mime_type: &str) -> Option<Cow<'static, [u8]>> {
        if mime_type == NEVERLIGHT_MAIL_MIME {
            let s = format!("{}:{}", self.envelope_hash, self.source_mailbox);
            Some(Cow::Owned(s.into_bytes()))
        } else {
            None
        }
    }
}

impl AllowedMimeTypes for DraggedMessage {
    fn allowed() -> Cow<'static, [String]> {
        Cow::Owned(vec![NEVERLIGHT_MAIL_MIME.to_string()])
    }
}

impl TryFrom<(Vec<u8>, String)> for DraggedMessage {
    type Error = String;
    fn try_from((bytes, _mime): (Vec<u8>, String)) -> Result<Self, Self::Error> {
        let s = String::from_utf8(bytes).map_err(|e| e.to_string())?;
        let (a, b) = s.split_once(':').ok_or("missing ':' separator")?;
        Ok(DraggedMessage {
            envelope_hash: a.parse().map_err(|e: std::num::ParseIntError| e.to_string())?,
            source_mailbox: b.parse().map_err(|e: std::num::ParseIntError| e.to_string())?,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // -- DraggedFiles --

    #[test]
    fn dragged_files_allowed_mime() {
        let allowed = DraggedFiles::allowed();
        assert_eq!(allowed.as_ref(), &["text/uri-list"]);
    }

    #[test]
    fn dragged_files_try_from_valid() {
        let data = b"file:///home/user/doc.pdf\n".to_vec();
        let files = DraggedFiles::try_from((data, "text/uri-list".into())).unwrap();
        assert!(files.0.contains("file:///home/user/doc.pdf"));
    }

    #[test]
    fn dragged_files_try_from_invalid_utf8() {
        let data = vec![0xFF, 0xFE];
        assert!(DraggedFiles::try_from((data, "text/uri-list".into())).is_err());
    }

    // -- DraggedMessage --

    #[test]
    fn dragged_message_roundtrip() {
        let msg = DraggedMessage {
            envelope_hash: 12345,
            source_mailbox: 67890,
        };

        // Serialize
        let available = msg.available();
        assert_eq!(available.as_ref(), &[NEVERLIGHT_MAIL_MIME]);
        let bytes = msg.as_bytes(NEVERLIGHT_MAIL_MIME).unwrap();
        assert_eq!(bytes.as_ref(), b"12345:67890");

        // Deserialize
        let parsed =
            DraggedMessage::try_from((bytes.into_owned(), NEVERLIGHT_MAIL_MIME.into())).unwrap();
        assert_eq!(parsed.envelope_hash, 12345);
        assert_eq!(parsed.source_mailbox, 67890);
    }

    #[test]
    fn dragged_message_as_bytes_wrong_mime() {
        let msg = DraggedMessage {
            envelope_hash: 1,
            source_mailbox: 2,
        };
        assert!(msg.as_bytes("text/plain").is_none());
    }

    #[test]
    fn dragged_message_try_from_missing_separator() {
        let data = b"12345".to_vec();
        assert!(DraggedMessage::try_from((data, NEVERLIGHT_MAIL_MIME.into())).is_err());
    }

    #[test]
    fn dragged_message_try_from_non_numeric() {
        let data = b"abc:def".to_vec();
        assert!(DraggedMessage::try_from((data, NEVERLIGHT_MAIL_MIME.into())).is_err());
    }

    #[test]
    fn dragged_message_try_from_large_values() {
        let max = u64::MAX;
        let data = format!("{max}:{max}").into_bytes();
        let parsed = DraggedMessage::try_from((data, NEVERLIGHT_MAIL_MIME.into())).unwrap();
        assert_eq!(parsed.envelope_hash, max);
        assert_eq!(parsed.source_mailbox, max);
    }

    #[test]
    fn dragged_message_allowed_mime() {
        let allowed = DraggedMessage::allowed();
        assert_eq!(allowed.as_ref(), &[NEVERLIGHT_MAIL_MIME]);
    }
}
