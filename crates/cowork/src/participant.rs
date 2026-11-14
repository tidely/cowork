use uuid::Uuid;

/// Identifies one participant of a thread.
///
/// The host assigns collaborators a fresh id every time they join. The
/// generated name and color are derived from an id deterministically, so every
/// client renders a participant identically. Profiles name the id to derive
/// them from, so a participant looks the same in every thread. They are purely
/// cosmetic: names can collide and must never be used as identity.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub(crate) struct ParticipantId(Uuid);

const ADJECTIVES: [&str; 64] = [
    "Amber", "Ashen", "Azure", "Bold", "Brave", "Breezy", "Bright", "Brisk", "Calm", "Clever",
    "Coral", "Cosmic", "Crimson", "Curious", "Dapper", "Dawn", "Deft", "Dusky", "Eager", "Early",
    "Fleet", "Frosty", "Gentle", "Gilded", "Glad", "Golden", "Hazel", "Humble", "Indigo", "Jade",
    "Jolly", "Keen", "Kind", "Lively", "Lucky", "Lunar", "Merry", "Misty", "Mossy", "Nimble",
    "Noble", "Ochre", "Olive", "Plucky", "Polar", "Quick", "Quiet", "Rapid", "Rosy", "Rusty",
    "Sage", "Scarlet", "Silver", "Sly", "Snowy", "Solar", "Steady", "Sunny", "Swift", "Tidy",
    "Topaz", "Velvet", "Witty", "Zesty",
];

const ANIMALS: [&str; 64] = [
    "Badger", "Beaver", "Bison", "Bobcat", "Condor", "Crane", "Cricket", "Dingo", "Dolphin",
    "Dove", "Eagle", "Egret", "Falcon", "Ferret", "Finch", "Fox", "Gecko", "Gibbon", "Goose",
    "Heron", "Ibis", "Impala", "Jackal", "Jaguar", "Koala", "Lemur", "Lark", "Llama", "Lynx",
    "Marmot", "Marten", "Mink", "Moose", "Narwhal", "Newt", "Ocelot", "Orca", "Osprey", "Otter",
    "Owl", "Panda", "Panther", "Pelican", "Puffin", "Quail", "Rabbit", "Raven", "Robin", "Salmon",
    "Seal", "Sparrow", "Stoat", "Swan", "Tapir", "Tern", "Tiger", "Toucan", "Turtle", "Viper",
    "Walrus", "Weasel", "Wolf", "Wren", "Yak",
];

/// Identity colors, behind white initials and cursor name tags, and drawn on
/// the page as carets, comment borders, and highlights: each one reaches the
/// 4.5:1 contrast small text needs against white, and the 3:1 that marks
/// need against the dark theme's background (and so against white too).
const COLORS: [u32; 12] = [
    0xd43e25, 0xa9631e, 0x8d721b, 0x5f7e2c, 0x328354, 0x268181, 0x2276c9, 0x576ae0, 0x865dd0,
    0xac4bc0, 0xcc3c84, 0x837264,
];

impl ParticipantId {
    pub(crate) fn new() -> Self {
        Self(Uuid::new_v4())
    }

    pub(crate) fn from_bytes(bytes: uuid::Bytes) -> Self {
        Self(Uuid::from_bytes(bytes))
    }

    pub(crate) fn into_bytes(self) -> uuid::Bytes {
        self.0.into_bytes()
    }

    pub(crate) fn from_uuid(uuid: Uuid) -> Self {
        Self(uuid)
    }

    pub(crate) fn as_uuid(self) -> Uuid {
        self.0
    }

    pub(crate) fn display_name(self) -> String {
        let [adjective, animal] = self.words();
        format!("{adjective} {animal}")
    }

    pub(crate) fn initials(self) -> String {
        self.words()
            .iter()
            .filter_map(|word| word.chars().next())
            .collect()
    }

    pub(crate) fn color(self) -> u32 {
        COLORS[(self.hash(2) % COLORS.len() as u64) as usize]
    }

    fn words(self) -> [&'static str; 2] {
        [
            ADJECTIVES[(self.hash(0) % ADJECTIVES.len() as u64) as usize],
            ANIMALS[(self.hash(1) % ANIMALS.len() as u64) as usize],
        ]
    }

    /// Mixes every bit of the id into an independent value per `salt`.
    ///
    /// Implemented here rather than with `std::hash` so that the result can
    /// never differ between builds, platforms, or versions of the standard
    /// library, which would give one participant different names on
    /// different clients.
    fn hash(self, salt: u64) -> u64 {
        let value = self.0.as_u128();
        splitmix64((value as u64) ^ splitmix64((value >> 64) as u64 ^ salt))
    }
}

fn splitmix64(mut value: u64) -> u64 {
    value = value.wrapping_add(0x9e37_79b9_7f4a_7c15);
    value = (value ^ (value >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
    value = (value ^ (value >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
    value ^ (value >> 31)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// WCAG 2's relative luminance of an sRGB color.
    fn luminance(color: u32) -> f64 {
        let channel = |shift: u32| {
            let value = f64::from((color >> shift) & 0xff) / 255.;
            if value <= 0.03928 {
                value / 12.92
            } else {
                ((value + 0.055) / 1.055).powf(2.4)
            }
        };
        0.2126 * channel(16) + 0.7152 * channel(8) + 0.0722 * channel(0)
    }

    #[test]
    fn identity_colors_stay_legible_in_both_roles() {
        let theme: serde_json::Value =
            serde_json::from_str(include_str!("../themes/cowork-dark.json")).unwrap();
        let background = theme["colors"]["background"].as_str().unwrap();
        let background = u32::from_str_radix(background.trim_start_matches('#'), 16).unwrap();
        for color in COLORS {
            let on_white = 1.05 / (luminance(color) + 0.05);
            assert!(on_white >= 4.5, "white on {color:06x} is {on_white:.2}:1");
            let on_dark = (luminance(color) + 0.05) / (luminance(background) + 0.05);
            assert!(
                on_dark >= 3.,
                "{color:06x} on the dark theme is {on_dark:.2}:1"
            );
        }
    }

    #[test]
    fn names_and_colors_are_derived_deterministically() {
        let id = ParticipantId::from_bytes([7; 16]);
        let copy = ParticipantId::from_bytes(id.into_bytes());

        assert_eq!(id.display_name(), copy.display_name());
        assert_eq!(id.color(), copy.color());
        // Pinned so that a change to the derivation, which would make old and
        // new clients disagree on names, is caught deliberately.
        assert_eq!(id.display_name(), "Mossy Crane");
    }

    #[test]
    fn ids_spread_across_names() {
        let names = (0..64)
            .map(|_| ParticipantId::new().display_name())
            .collect::<std::collections::HashSet<_>>();

        // 4096 combinations; 64 random ids colliding this often would mean
        // the derivation ignores most of the id.
        assert!(names.len() > 48, "only {} distinct names", names.len());
    }
}
