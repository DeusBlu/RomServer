//! Client platforms (`assets/platforms.toml`): where each keeps ROMs, media and saves.

use std::collections::{BTreeMap, HashMap};
use std::path::PathBuf;
use std::sync::OnceLock;

use anyhow::{Context, Result, bail};
use serde::Deserialize;

const BUNDLED: &str = include_str!("../assets/platforms.toml");

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PlatformFile {
    pub emulators: BTreeMap<String, EmulatorSlots>,
    pub media_groups: MediaGroups,
    pub layouts: BTreeMap<String, Layout>,
    pub platforms: BTreeMap<String, Platform>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MediaGroups {
    /// System or ES-DE platform → the group it belongs to, where ES-DE keeps them apart.
    #[serde(default)]
    pub join: BTreeMap<String, String>,
    /// ES-DE platforms that lump different systems together; their members stay separate.
    #[serde(default)]
    pub separate: Vec<String>,
}

/// system id → media group, from the bundled emulator catalog (before the adjustments here).
fn catalog_media_groups() -> &'static HashMap<String, String> {
    static MAP: OnceLock<HashMap<String, String>> = OnceLock::new();
    MAP.get_or_init(|| {
        crate::emulators::bundled()
            .map(|c| {
                c.systems
                    .into_iter()
                    .map(|s| (s.id, s.media_group))
                    .collect()
            })
            .unwrap_or_default()
    })
}

/// The media folder a system shares with every other name for the same platform.
pub fn media_group(system: &str) -> String {
    let groups = &bundled().media_groups;
    if let Some(group) = groups.join.get(system) {
        return group.clone();
    }
    let catalog_group = catalog_media_groups()
        .get(system)
        .map_or(system, String::as_str);
    if groups.separate.iter().any(|s| s == catalog_group) {
        return system.to_string();
    }
    groups
        .join
        .get(catalog_group)
        .cloned()
        .unwrap_or_else(|| catalog_group.to_string())
}

/// Every catalog system sharing a media group, the group's own name included.
pub fn media_group_members(group: &str) -> Vec<String> {
    let mut members: Vec<String> = catalog_media_groups()
        .keys()
        .filter(|s| media_group(s) == group)
        .cloned()
        .collect();
    if !members.iter().any(|m| m == group) {
        members.push(group.to_string());
    }
    members.sort();
    members
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EmulatorSlots {
    /// Sub-folders of the emulator's canonical save folder; "" is the folder itself.
    pub slots: Vec<String>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Layout {
    /// System id → the folder name this frontend expects, where it differs.
    #[serde(default)]
    pub folders: BTreeMap<String, String>,
    /// Top-level view folder for frontends that keep media in its own tree. None: media goes
    /// inside each system's ROM folder.
    #[serde(default)]
    pub media_dir: Option<String>,
    /// Target pattern (`covers/{name}`) → neutral media types, first available wins.
    #[serde(default)]
    pub media: BTreeMap<String, Vec<String>>,
}

/// The library's media types: `media/<system>/<game>/<type>.<ext>`.
pub const MEDIA_TYPES: &[&str] = &[
    "box-front",
    "box-back",
    "box-3d",
    "physical-media",
    "screenshot",
    "title-screen",
    "logo",
    "marquee",
    "fanart",
    "mix-image",
    "video",
    "manual",
];

/// Top-level folder of every view holding the ROM folders; the machine mounts it.
pub const VIEW_ROMS: &str = "roms";

impl Layout {
    /// The folder name this frontend expects for a system.
    pub fn folder<'a>(&'a self, system: &'a str) -> &'a str {
        self.folders.get(system).map_or(system, String::as_str)
    }

    /// A system's ROM folder, relative to the view root.
    pub fn rom_dir(&self, system: &str) -> PathBuf {
        PathBuf::from(VIEW_ROMS).join(self.folder(system))
    }

    /// Where a system's media goes in the view: its own tree, or the ROM folder itself.
    pub fn media_base(&self, system: &str) -> PathBuf {
        match &self.media_dir {
            Some(dir) => PathBuf::from(dir).join(self.folder(system)),
            None => self.rom_dir(system),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Mounter {
    Systemd,
    Batocera,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, serde::Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum RetroArchSort {
    Core,
    Content,
    None,
}

impl RetroArchSort {
    pub const ALL: &[RetroArchSort] = &[
        RetroArchSort::Core,
        RetroArchSort::Content,
        RetroArchSort::None,
    ];

    pub fn id(self) -> &'static str {
        match self {
            RetroArchSort::Core => "core",
            RetroArchSort::Content => "content",
            RetroArchSort::None => "none",
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            RetroArchSort::Core => "Sorted by core name",
            RetroArchSort::Content => "Sorted by content directory (system)",
            RetroArchSort::None => "Not sorted (one folder)",
        }
    }

    pub fn from_id(id: &str) -> Option<Self> {
        Self::ALL.iter().copied().find(|s| s.id() == id)
    }
}

/// emulator id → slot → local path template.
pub type SlotPaths = BTreeMap<String, BTreeMap<String, String>>;

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Platform {
    pub label: String,
    pub layout: String,
    pub mounter: Mounter,
    /// Where systemd units go; `/etc/systemd/system` unless the OS keeps them elsewhere.
    #[serde(default)]
    pub units_dir: Option<String>,
    pub default_home: String,
    pub base: String,
    pub roms: String,
    #[serde(default)]
    pub media: Option<String>,
    /// ES-DE's custom_systems folder, for platforms that run ES-DE.
    #[serde(default)]
    pub esde_custom_systems: Option<String>,
    pub retroarch_saves: String,
    pub retroarch_states: String,
    pub retroarch_sort: RetroArchSort,
    #[serde(default)]
    pub notes: Vec<String>,
    #[serde(default)]
    pub saves: SlotPaths,
    #[serde(default)]
    pub states: SlotPaths,
}

impl Platform {
    pub fn units_dir(&self) -> &str {
        self.units_dir.as_deref().unwrap_or("/etc/systemd/system")
    }
}

impl PlatformFile {
    pub fn platform(&self, id: &str) -> Result<&Platform> {
        self.platforms
            .get(id)
            .with_context(|| format!("unknown platform {id}"))
    }

    pub fn layout(&self, platform: &Platform) -> &Layout {
        &self.layouts[&platform.layout]
    }

    fn check(&self) -> Result<()> {
        for (id, layout) in &self.layouts {
            for (target, types) in &layout.media {
                if !target.contains("{name}") || target.starts_with('/') || target.contains("..") {
                    bail!(
                        "layout {id}: media target {target:?} needs {{name}} and a relative path"
                    );
                }
                if let Some(t) = types.iter().find(|t| !MEDIA_TYPES.contains(&t.as_str())) {
                    bail!("layout {id}: unknown media type {t}");
                }
            }
            if layout.media_dir.as_deref() == Some(VIEW_ROMS) {
                bail!("layout {id}: media_dir can't be the ROM folder");
            }
        }
        for (id, p) in &self.platforms {
            if !self.layouts.contains_key(&p.layout) {
                bail!("platform {id}: unknown layout {}", p.layout);
            }
            let layout = &self.layouts[&p.layout];
            if p.media.is_some() != layout.media_dir.is_some() {
                bail!(
                    "platform {id}: a media mount needs a layout with media_dir, and the other way round"
                );
            }
            for (emulator, slots) in p.saves.iter().chain(&p.states) {
                let known = self
                    .emulators
                    .get(emulator)
                    .map(|e| e.slots.as_slice())
                    .unwrap_or(&[]);
                for slot in slots.keys() {
                    // States are one folder per emulator; saves use the declared slots.
                    if !slot.is_empty() && !known.contains(slot) {
                        bail!("platform {id}: {emulator} has undeclared slot {slot:?}");
                    }
                }
            }
        }
        Ok(())
    }
}

/// The bundled platform data, parsed once.
pub fn bundled() -> &'static PlatformFile {
    static FILE: OnceLock<PlatformFile> = OnceLock::new();
    FILE.get_or_init(|| {
        let file: PlatformFile = toml::from_str(BUNDLED).expect("assets/platforms.toml parses");
        file.check().expect("assets/platforms.toml is consistent");
        file
    })
}

/// Expands `~` and `{base}` in a path template.
pub fn expand(template: &str, home: &str, base: &str) -> PathBuf {
    let base = expand_home(base, home);
    let with_base = template.replace("{base}", &base);
    PathBuf::from(expand_home(&with_base, home))
}

fn expand_home(path: &str, home: &str) -> String {
    match path.strip_prefix('~') {
        Some(rest) if rest.is_empty() || rest.starts_with('/') => format!("{home}{rest}"),
        _ => path.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bundled_platforms_are_consistent_and_use_catalog_ids() {
        let file = bundled();
        let catalog = crate::emulators::bundled().unwrap();
        let ids: Vec<&str> = catalog.emulators.iter().map(|e| e.id.as_str()).collect();
        for emulator in file.emulators.keys() {
            assert!(
                ids.contains(&emulator.as_str()),
                "{emulator} isn't a catalog id"
            );
        }
        for (id, p) in &file.platforms {
            for emulator in p.saves.keys().chain(p.states.keys()) {
                assert!(
                    file.emulators.contains_key(emulator),
                    "{id}: {emulator} has no slots"
                );
            }
        }
        assert!(
            file.platforms.contains_key("es-de"),
            "old settings use frontend = \"es-de\""
        );
    }

    #[test]
    fn platform_names_share_one_media_group() {
        assert_eq!(media_group("tg16"), "pcengine");
        assert_eq!(media_group("pcengine"), "pcengine");
        assert_eq!(media_group("megacd"), "segacd");
        assert_eq!(media_group("sfc"), "snes");
        // ES-DE keeps these apart; they're the same hardware.
        assert_eq!(media_group("megadrive"), "genesis");
        assert_eq!(media_group("megadrivejp"), "genesis");
        assert_eq!(media_group("famicom"), "nes");
        // ES-DE's catch-all scraper platforms don't merge different systems.
        assert_eq!(media_group("naomi"), "naomi");
        assert_eq!(media_group("doom"), "doom");
        assert_eq!(
            media_group_members("genesis"),
            ["genesis", "megadrive", "megadrivejp"]
        );
    }

    #[test]
    fn expands_home_and_base() {
        assert_eq!(
            expand("{base}/saves/psx", "/home/deck", "~/retrodeck"),
            PathBuf::from("/home/deck/retrodeck/saves/psx")
        );
        assert_eq!(
            expand("~/.config/retroarch/saves", "/home/deus", "~"),
            PathBuf::from("/home/deus/.config/retroarch/saves")
        );
        assert_eq!(
            expand("{base}/roms", "/userdata", "/userdata"),
            PathBuf::from("/userdata/roms")
        );
        assert_eq!(expand("~user/x", "/h", "~"), PathBuf::from("~user/x"));
    }
}
