use std::{collections::HashMap, fmt, ops::Range, sync::Arc};

use uuid::Uuid;
use yrs::{Any, Number};

macro_rules! uuid_id {
    ($(#[$meta:meta])* $name:ident) => {
        $(#[$meta])*
        #[derive(Copy, Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
        pub struct $name(Uuid);

        impl $name {
            /// A fresh random (v4) id.
            #[allow(clippy::new_without_default)] // A random `Default` would be surprising.
            pub fn new() -> Self {
                Self(Uuid::new_v4())
            }

            pub fn from_uuid(uuid: Uuid) -> Self {
                Self(uuid)
            }

            pub fn as_uuid(&self) -> Uuid {
                self.0
            }
        }

        impl fmt::Display for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                fmt::Display::fmt(&self.0.hyphenated(), f)
            }
        }
    };
}

uuid_id!(
    /// Identifies a prompt block or comment within a draft.
    ItemId
);
uuid_id!(
    /// Identifies an attachment. The file bytes live outside the draft document.
    AttachmentId
);

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum AttachmentKind {
    Text,
    Png,
    Jpeg,
}

impl AttachmentKind {
    fn as_str(self) -> &'static str {
        match self {
            Self::Text => "text",
            Self::Png => "png",
            Self::Jpeg => "jpeg",
        }
    }

    fn parse(s: &str) -> Option<Self> {
        match s {
            "text" => Some(Self::Text),
            "png" => Some(Self::Png),
            "jpeg" => Some(Self::Jpeg),
            _ => None,
        }
    }
}

/// Metadata for a file attached to a prompt block.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AttachmentRecord {
    pub id: AttachmentId,
    pub name: String,
    pub kind: AttachmentKind,
    pub size: u64,
    pub creator: Uuid,
}

impl AttachmentRecord {
    /// The exact keys of a stored record.
    pub(crate) const FIELDS: [&'static str; 5] = ["id", "name", "kind", "size", "creator"];

    pub(crate) fn to_any(&self) -> Any {
        any_map([
            ("id", Any::from(self.id.to_string())),
            ("name", Any::from(self.name.as_str())),
            ("kind", Any::from(self.kind.as_str())),
            ("size", uint_any(self.size)),
            ("creator", Any::from(self.creator.to_string())),
        ])
    }

    pub(crate) fn from_any(any: &Any) -> Option<Self> {
        let Any::Map(map) = any else { return None };
        Some(Self {
            id: Self::id_of(any)?,
            name: get_str(map, "name")?.to_owned(),
            kind: AttachmentKind::parse(get_str(map, "kind")?)?,
            size: get_uint(map, "size")?,
            creator: get_uuid(map, "creator")?,
        })
    }

    /// Reads only the id, so records that are otherwise malformed can still be found.
    pub(crate) fn id_of(any: &Any) -> Option<AttachmentId> {
        let Any::Map(map) = any else { return None };
        get_uuid(map, "id").map(AttachmentId)
    }
}

/// The part of a chat message a comment refers to.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CommentTarget {
    pub message_id: Uuid,
    /// A byte range in the target message's markdown source.
    pub range: Range<usize>,
    /// The quoted text, kept so the comment stays meaningful on its own.
    pub quote: String,
}

impl CommentTarget {
    /// The exact keys of a stored target.
    pub(crate) const FIELDS: [&'static str; 4] = ["message_id", "start", "end", "quote"];

    pub(crate) fn to_any(&self) -> Any {
        any_map([
            ("message_id", Any::from(self.message_id.to_string())),
            ("start", uint_any(self.range.start as u64)),
            ("end", uint_any(self.range.end as u64)),
            ("quote", Any::from(self.quote.as_str())),
        ])
    }

    pub(crate) fn from_any(any: &Any) -> Option<Self> {
        let Any::Map(map) = any else { return None };
        let start = usize::try_from(get_uint(map, "start")?).ok()?;
        let end = usize::try_from(get_uint(map, "end")?).ok()?;
        if start > end {
            return None;
        }
        Some(Self {
            message_id: get_uuid(map, "message_id")?,
            range: start..end,
            quote: get_str(map, "quote")?.to_owned(),
        })
    }
}

/// A snapshot of one draft item.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DraftItem {
    pub id: ItemId,
    pub creator: Uuid,
    pub body: String,
    pub kind: DraftItemKind,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum DraftItemKind {
    Prompt { attachments: Vec<AttachmentRecord> },
    Comment { target: CommentTarget },
}

impl DraftItem {
    /// No non-whitespace body text and, for prompts, no attachments.
    pub fn is_empty(&self) -> bool {
        let no_attachments = match &self.kind {
            DraftItemKind::Prompt { attachments } => attachments.is_empty(),
            DraftItemKind::Comment { .. } => true,
        };
        no_attachments && self.body.trim().is_empty()
    }

    pub fn is_prompt(&self) -> bool {
        matches!(self.kind, DraftItemKind::Prompt { .. })
    }

    pub fn is_comment(&self) -> bool {
        matches!(self.kind, DraftItemKind::Comment { .. })
    }
}

/// Whether `any` is a map with exactly the keys in `fields`.
pub(crate) fn has_exact_fields(any: &Any, fields: &[&str]) -> bool {
    let Any::Map(map) = any else { return false };
    map.len() == fields.len() && fields.iter().all(|field| map.contains_key(*field))
}

fn any_map<const N: usize>(fields: [(&str, Any); N]) -> Any {
    let map: HashMap<String, Any> = fields
        .into_iter()
        .map(|(key, value)| (key.to_owned(), value))
        .collect();
    Any::Map(Arc::new(map))
}

/// Integers are stored as `Any::Number`. Values beyond `i64::MAX` are clamped (and lib0 encodes
/// anything past 2^53 as a float); neither is reachable for byte sizes or offsets in practice.
fn uint_any(value: u64) -> Any {
    Any::Number(Number::Int(i64::try_from(value).unwrap_or(i64::MAX)))
}

fn get_str<'a>(map: &'a HashMap<String, Any>, key: &str) -> Option<&'a str> {
    match map.get(key)? {
        Any::String(s) => Some(s),
        _ => None,
    }
}

fn get_uuid(map: &HashMap<String, Any>, key: &str) -> Option<Uuid> {
    Uuid::parse_str(get_str(map, key)?).ok()
}

/// Accepts integral floats too, since lib0 decodes large integers as floats.
fn get_uint(map: &HashMap<String, Any>, key: &str) -> Option<u64> {
    match map.get(key)? {
        Any::Number(n) => u64::try_from(n.as_i64()?).ok(),
        _ => None,
    }
}
