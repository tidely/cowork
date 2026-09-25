//! How participants present themselves: display names and profile
//! pictures, and checking the ones peers send.

use std::{path::Path, sync::Arc};

use anyhow::Context as _;
use gpui::SharedString;

use crate::{
    attachments::{MAX_IMAGE_ATTACHMENT_BYTES, format_bytes},
    participant::ParticipantId,
    protocol,
};

/// Profile pictures are cropped to a square of this many pixels, so they stay
/// small and cheap to draw however large the chosen file is.
const PROFILE_PICTURE_PIXELS: u32 = 256;
/// Pictures travel to every participant, so they are sent as JPEG, which
/// keeps a photo this size around 20 KB.
const PROFILE_PICTURE_QUALITY: u8 = 85;
/// Far above what `PROFILE_PICTURE_PIXELS` at `PROFILE_PICTURE_QUALITY`
/// produces; only bounds what a peer can make everyone store.
const MAX_PROFILE_PICTURE_BYTES: usize = 128 * 1024;
/// Shows through where a picture was transparent, since JPEG has no alpha.
const PROFILE_PICTURE_BACKGROUND: [u8; 3] = [0x27, 0x27, 0x2a];
const MAX_DISPLAY_NAME_CHARS: usize = 40;

pub(crate) fn load_profile_picture(path: &Path) -> anyhow::Result<gpui::Image> {
    let name = path
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_else(|| path.display().to_string());
    let len = std::fs::metadata(path)
        .with_context(|| format!("Cannot read {name}"))?
        .len();
    anyhow::ensure!(
        len <= MAX_IMAGE_ATTACHMENT_BYTES,
        "{name} is {}; profile pictures can be at most {}",
        format_bytes(len),
        format_bytes(MAX_IMAGE_ATTACHMENT_BYTES)
    );
    let bytes = std::fs::read(path).with_context(|| format!("Cannot read {name}"))?;
    profile_picture(&bytes).with_context(|| format!("{name} is not a supported image"))
}

/// Center-crops an image to a square and re-encodes it as a JPEG.
pub(crate) fn profile_picture(bytes: &[u8]) -> anyhow::Result<gpui::Image> {
    let square = image::load_from_memory(bytes)?
        .resize_to_fill(
            PROFILE_PICTURE_PIXELS,
            PROFILE_PICTURE_PIXELS,
            image::imageops::FilterType::Lanczos3,
        )
        .into_rgba8();
    let opaque = image::RgbImage::from_fn(square.width(), square.height(), |x, y| {
        let [red, green, blue, alpha] = square.get_pixel(x, y).0;
        let blend = |channel: u8, background: u8| {
            let alpha = u16::from(alpha);
            ((u16::from(channel) * alpha + u16::from(background) * (255 - alpha)) / 255) as u8
        };
        image::Rgb([
            blend(red, PROFILE_PICTURE_BACKGROUND[0]),
            blend(green, PROFILE_PICTURE_BACKGROUND[1]),
            blend(blue, PROFILE_PICTURE_BACKGROUND[2]),
        ])
    });
    let mut jpeg = Vec::new();
    image::codecs::jpeg::JpegEncoder::new_with_quality(&mut jpeg, PROFILE_PICTURE_QUALITY)
        .encode_image(&opaque)?;
    Ok(gpui::Image::from_bytes(gpui::ImageFormat::Jpeg, jpeg))
}

/// Checks a profile a collaborator sent, as [`profile_picture`] and
/// [`display_name_error`] would have made it.
pub(crate) fn validate_profile(profile: &protocol::Profile) -> anyhow::Result<()> {
    if let Some(name) = &profile.name {
        anyhow::ensure!(name.trim() == name, "The profile name is not trimmed.");
        if let Some(error) = display_name_error(name) {
            anyhow::bail!(error);
        }
    }
    if let Some(picture) = &profile.picture {
        anyhow::ensure!(
            picture.len() <= MAX_PROFILE_PICTURE_BYTES,
            "The profile picture is {}; it can be at most {}.",
            format_bytes(picture.len() as u64),
            format_bytes(MAX_PROFILE_PICTURE_BYTES as u64)
        );
        // Limited up front, so a picture claiming to be huge is never
        // decoded.
        let mut limits = image::Limits::default();
        limits.max_image_width = Some(PROFILE_PICTURE_PIXELS);
        limits.max_image_height = Some(PROFILE_PICTURE_PIXELS);
        let mut reader = image::ImageReader::with_format(
            std::io::Cursor::new(picture),
            image::ImageFormat::Jpeg,
        );
        reader.limits(limits);
        let decoded = reader
            .decode()
            .context("The profile picture is not a valid JPEG.")?;
        anyhow::ensure!(
            decoded.width() == PROFILE_PICTURE_PIXELS && decoded.height() == PROFILE_PICTURE_PIXELS,
            "The profile picture is not {PROFILE_PICTURE_PIXELS} pixels square."
        );
    }
    Ok(())
}

/// Why `name` cannot be a display name, if it cannot.
pub(crate) fn display_name_error(name: &str) -> Option<String> {
    let name = name.trim();
    if name.is_empty() {
        Some("Enter a name.".into())
    } else if name.chars().count() > MAX_DISPLAY_NAME_CHARS {
        Some(format!(
            "Names can be at most {MAX_DISPLAY_NAME_CHARS} characters."
        ))
    } else {
        None
    }
}

/// How a participant presents themselves; see [`protocol::Profile`].
#[derive(Clone, Default)]
pub(crate) struct Profile {
    /// Replaces the name derived from the participant id when set.
    pub(crate) name: Option<SharedString>,
    /// Replaces the initials avatar when set.
    pub(crate) picture: Option<Arc<gpui::Image>>,
    /// What the fallback name, initials, and color are derived from; see
    /// [`protocol::Profile::appearance`].
    pub(crate) appearance: Option<ParticipantId>,
}

impl Profile {
    /// The local user's profile before they customize it, which looks the
    /// same in every thread they join.
    pub(crate) fn local(local_participant_id: ParticipantId) -> Self {
        Self {
            appearance: Some(local_participant_id),
            ..Self::default()
        }
    }

    pub(crate) fn to_protocol(&self) -> protocol::Profile {
        protocol::Profile {
            name: self.name.as_ref().map(ToString::to_string),
            picture: self
                .picture
                .as_ref()
                .map(|picture| picture.bytes().to_vec()),
            appearance: self.appearance.map(ParticipantId::into_bytes),
        }
    }

    pub(crate) fn from_protocol(profile: protocol::Profile) -> Self {
        Self {
            name: profile.name.map(Into::into),
            picture: profile
                .picture
                .map(|bytes| Arc::new(gpui::Image::from_bytes(gpui::ImageFormat::Jpeg, bytes))),
            appearance: profile.appearance.map(ParticipantId::from_bytes),
        }
    }
}

/// The id `participant`'s fallback name, initials, and color come from.
pub(crate) fn appearance(participant: ParticipantId, profile: Option<&Profile>) -> ParticipantId {
    profile
        .and_then(|profile| profile.appearance)
        .unwrap_or(participant)
}

/// The name `participant` chose, or the one derived from their appearance.
pub(crate) fn participant_name(
    participant: ParticipantId,
    profile: Option<&Profile>,
) -> SharedString {
    profile
        .and_then(|profile| profile.name.clone())
        .unwrap_or_else(|| appearance(participant, profile).display_name().into())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::encoded_image;

    #[test]
    fn profile_pictures_are_cropped_to_a_square_jpeg_peers_accept() {
        let picture = profile_picture(&encoded_image(7, image::ImageFormat::Bmp))
            .expect("a bmp is a supported image");

        assert_eq!(picture.format(), gpui::ImageFormat::Jpeg);
        let decoded = image::load_from_memory(picture.bytes()).expect("decodes");
        assert_eq!(
            (decoded.width(), decoded.height()),
            (PROFILE_PICTURE_PIXELS, PROFILE_PICTURE_PIXELS)
        );
        let profile = protocol::Profile {
            name: Some("Ada".into()),
            picture: Some(picture.bytes().to_vec()),
            appearance: None,
        };
        validate_profile(&profile).expect("peers accept the pictures this app makes");
        assert!(profile_picture(b"not an image").is_err());
    }

    #[test]
    fn transparent_profile_pictures_get_a_background() {
        let mut png = Vec::new();
        image::DynamicImage::ImageRgba8(image::RgbaImage::new(4, 4))
            .write_to(&mut std::io::Cursor::new(&mut png), image::ImageFormat::Png)
            .expect("encode test image");
        let picture = profile_picture(&png).expect("a png is a supported image");

        let decoded = image::load_from_memory(picture.bytes())
            .expect("decodes")
            .into_rgb8();
        let [red, green, blue] = decoded.get_pixel(128, 128).0;
        for (channel, expected) in [red, green, blue]
            .into_iter()
            .zip(PROFILE_PICTURE_BACKGROUND)
        {
            assert!(
                channel.abs_diff(expected) <= 4,
                "{channel} is not near {expected}"
            );
        }
    }

    #[test]
    fn peers_profiles_are_validated() {
        let valid_picture = profile_picture(&encoded_image(3, image::ImageFormat::Bmp))
            .expect("a bmp is a supported image")
            .bytes()
            .to_vec();
        let small_jpeg = {
            let mut jpeg = Vec::new();
            image::DynamicImage::ImageRgb8(image::RgbImage::new(16, 16))
                .write_to(
                    &mut std::io::Cursor::new(&mut jpeg),
                    image::ImageFormat::Jpeg,
                )
                .expect("encode test image");
            jpeg
        };
        let profile = |name: Option<&str>, picture: Option<Vec<u8>>| protocol::Profile {
            name: name.map(Into::into),
            picture,
            appearance: None,
        };

        assert!(validate_profile(&protocol::Profile::default()).is_ok());
        assert!(validate_profile(&profile(Some("Ada"), Some(valid_picture))).is_ok());
        assert!(validate_profile(&profile(Some(" Ada"), None)).is_err());
        assert!(validate_profile(&profile(Some(""), None)).is_err());
        assert!(
            validate_profile(&profile(
                Some(&"a".repeat(MAX_DISPLAY_NAME_CHARS + 1)),
                None
            ))
            .is_err()
        );
        assert!(validate_profile(&profile(None, Some(small_jpeg))).is_err());
        assert!(
            validate_profile(&profile(
                None,
                Some(encoded_image(3, image::ImageFormat::Png))
            ))
            .is_err()
        );
        assert!(
            validate_profile(&profile(None, Some(vec![0; MAX_PROFILE_PICTURE_BYTES + 1]))).is_err()
        );
    }

    #[test]
    fn display_names_must_be_non_empty_and_short() {
        assert!(display_name_error("  Ada  ").is_none());
        assert!(display_name_error("   ").is_some());
        assert!(display_name_error(&"a".repeat(MAX_DISPLAY_NAME_CHARS)).is_none());
        assert!(display_name_error(&"a".repeat(MAX_DISPLAY_NAME_CHARS + 1)).is_some());
    }
}
