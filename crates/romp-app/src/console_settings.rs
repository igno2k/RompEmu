use crate::cores::{
    choosable_platforms, core_for, cores_for_platform, default_options, uses_vulkan, CoreChoices,
};
use std::collections::BTreeMap;

pub const STORE_KEY: &str = "console_settings";

pub struct Choice {
    pub label: &'static str,
    pub value: &'static str,
}

pub struct Setting {
    pub key: &'static str,
    pub label: &'static str,
    pub detail: &'static str,
    pub choices: &'static [Choice],
    pub default: usize,
}

pub struct Console {
    pub name: &'static str,
    pub platform: &'static str,
    pub core: &'static str,
    /// Settings for an emulator's Vulkan renderer only apply where it draws through Vulkan.
    pub vulkan: bool,
    pub settings: &'static [Setting],
}

const fn choice(label: &'static str, value: &'static str) -> Choice {
    Choice { label, value }
}

const RESOLUTION_DETAIL: &str = "Higher is sharper and needs a faster computer.";
const ANISOTROPIC_DETAIL: &str = "Keeps textures sharp on floors and walls seen at an angle.";
const WIDESCREEN_DETAIL: &str = "Shows a wider picture in games that have a widescreen patch.";

static CONSOLES: &[Console] = &[
    Console {
        name: "PlayStation 2",
        platform: "ps2",
        core: "armsx2",
        vulkan: true,
        settings: &[
            Setting {
                key: "armsx2_upscale",
                label: "Resolution",
                detail: RESOLUTION_DETAIL,
                choices: &[
                    choice("Native", "1x"),
                    choice("2x", "2x"),
                    choice("3x", "3x"),
                    choice("4x", "4x"),
                ],
                default: 2,
            },
            Setting {
                key: "armsx2_anisotropic_filtering",
                label: "Anisotropic filtering",
                detail: ANISOTROPIC_DETAIL,
                choices: &[
                    choice("Off", "0"),
                    choice("2x", "2"),
                    choice("4x", "4"),
                    choice("8x", "8"),
                    choice("16x", "16"),
                ],
                default: 4,
            },
            Setting {
                key: "armsx2_widescreen_patches",
                label: "Widescreen",
                detail: WIDESCREEN_DETAIL,
                choices: &[choice("Off", "disabled"), choice("On", "enabled")],
                default: 0,
            },
        ],
    },
    Console {
        name: "PlayStation 2",
        platform: "ps2",
        core: "pcsx2",
        vulkan: false,
        settings: &[
            Setting {
                key: "pcsx2_upscale_multiplier",
                label: "Resolution",
                detail: RESOLUTION_DETAIL,
                choices: &[choice("Native", "1x (Native)"), choice("2x", "2x")],
                default: 0,
            },
            Setting {
                key: "pcsx2_anisotropic_filtering",
                label: "Anisotropic filtering",
                detail: ANISOTROPIC_DETAIL,
                choices: &[
                    choice("Off", "disabled"),
                    choice("2x", "2x"),
                    choice("4x", "4x"),
                    choice("8x", "8x"),
                    choice("16x", "16x"),
                ],
                default: 4,
            },
            Setting {
                key: "pcsx2_widescreen_hint",
                label: "Widescreen",
                detail: WIDESCREEN_DETAIL,
                choices: &[choice("Off", "disabled"), choice("On", "enabled (16:9)")],
                default: 0,
            },
        ],
    },
    Console {
        name: "Nintendo 64",
        platform: "n64",
        core: "mupen64plus_next",
        vulkan: true,
        settings: &[Setting {
            key: "mupen64plus-parallel-rdp-upscaling",
            label: "Resolution",
            detail: RESOLUTION_DETAIL,
            choices: &[
                choice("Native", "1x"),
                choice("2x", "2x"),
                choice("4x", "4x"),
            ],
            default: 1,
        }],
    },
    Console {
        name: "GameCube and Wii",
        platform: "ngc",
        core: "dolphin",
        vulkan: true,
        settings: &[Setting {
            key: "dolphin_efb_scale",
            label: "Resolution",
            detail: RESOLUTION_DETAIL,
            choices: &[
                choice("Native", "1"),
                choice("2x (720p)", "2"),
                choice("3x (1080p)", "3"),
            ],
            default: 1,
        }],
    },
    Console {
        name: "PlayStation",
        platform: "psx",
        core: "mednafen_psx_hw",
        vulkan: true,
        settings: &[Setting {
            key: "beetle_psx_hw_internal_resolution",
            label: "Resolution",
            detail: RESOLUTION_DETAIL,
            choices: &[
                choice("Native", "1x(native)"),
                choice("2x", "2x"),
                choice("4x", "4x"),
            ],
            default: 1,
        }],
    },
    Console {
        name: "PlayStation",
        platform: "psx",
        core: "swanstation",
        vulkan: true,
        settings: &[Setting {
            key: "swanstation_GPU_ResolutionScale",
            label: "Resolution",
            detail: RESOLUTION_DETAIL,
            choices: &[choice("Native", "1"), choice("2x", "2"), choice("4x", "4")],
            default: 1,
        }],
    },
    Console {
        name: "PSP",
        platform: "psp",
        core: "ppsspp",
        vulkan: true,
        settings: &[Setting {
            key: "ppsspp_internal_resolution",
            label: "Resolution",
            detail: RESOLUTION_DETAIL,
            choices: &[
                choice("Native", "480x272"),
                choice("2x", "960x544"),
                choice("3x", "1440x816"),
                choice("4x", "1920x1088"),
            ],
            default: 2,
        }],
    },
    Console {
        name: "Dreamcast",
        platform: "dc",
        core: "flycast",
        vulkan: true,
        settings: &[Setting {
            key: "reicast_internal_resolution",
            label: "Resolution",
            detail: RESOLUTION_DETAIL,
            choices: &[
                choice("Native", "640x480"),
                choice("2x", "1280x960"),
                choice("3x", "1920x1440"),
            ],
            default: 1,
        }],
    },
];

fn applies(console: &Console) -> bool {
    !console.vulkan || uses_vulkan(console.core)
}

/// The consoles whose emulator on this computer, as chosen, has settings.
pub fn consoles(cores: &CoreChoices) -> impl Iterator<Item = &'static Console> + '_ {
    CONSOLES.iter().filter(move |c| {
        core_for(c.platform, cores).is_some_and(|core| core.id == c.core) && applies(c)
    })
}

/// Setting keys for a system's emulator start with this, followed by the platform slug.
pub const EMULATOR_KEY: &str = "emulator:";

const SYSTEM_NAMES: [(&str, &str); 6] = [
    ("psx", "PlayStation"),
    ("gb", "Game Boy"),
    ("gbc", "Game Boy Color"),
    ("nes", "NES"),
    ("famicom", "Famicom"),
    ("fds", "Famicom Disk System"),
];

pub struct EmulatorChoice {
    pub key: String,
    pub system: &'static str,
    pub choices: Vec<&'static str>,
    pub current: usize,
}

/// A row per system that can play with more than one emulator, listing them default first.
pub fn emulator_choices(cores: &CoreChoices) -> Vec<EmulatorChoice> {
    choosable_platforms()
        .into_iter()
        .map(|slug| {
            let options = cores_for_platform(slug);
            let current = core_for(slug, cores)
                .and_then(|chosen| options.iter().position(|c| c.id == chosen.id))
                .unwrap_or(0);
            EmulatorChoice {
                key: format!("{EMULATOR_KEY}{slug}"),
                system: SYSTEM_NAMES
                    .iter()
                    .find(|(s, _)| *s == slug)
                    .map_or(slug, |(_, name)| name),
                choices: options.iter().map(|c| c.name).collect(),
                current,
            }
        })
        .collect()
}

/// The platform slug an emulator setting key is for.
pub fn emulator_platform(key: &str) -> Option<&str> {
    key.strip_prefix(EMULATOR_KEY)
}

pub type Chosen = BTreeMap<String, String>;

pub fn chosen_from_json(text: Option<&str>) -> Chosen {
    text.and_then(|t| serde_json::from_str(t).ok())
        .unwrap_or_default()
}

pub fn selected(setting: &Setting, chosen: &Chosen) -> usize {
    chosen
        .get(setting.key)
        .and_then(|v| setting.choices.iter().position(|c| c.value == v))
        .unwrap_or(setting.default)
}

/// Sets `key` to its choice at `index`, returning false for an unknown setting or choice.
pub fn choose(chosen: &mut Chosen, key: &str, index: usize) -> bool {
    let Some(setting) = CONSOLES
        .iter()
        .flat_map(|c| c.settings)
        .find(|s| s.key == key)
    else {
        return false;
    };
    let Some(choice) = setting.choices.get(index) else {
        return false;
    };
    chosen.insert(key.to_string(), choice.value.to_string());
    true
}

/// The core options a game starts with: the core's own defaults, then its console settings.
pub fn core_options(core_id: &str, chosen: &Chosen) -> Vec<(String, String)> {
    let mut options = default_options(core_id);
    for setting in CONSOLES
        .iter()
        .filter(|c| c.core == core_id && applies(c))
        .flat_map(|c| c.settings)
    {
        let value = setting.choices[selected(setting, chosen)].value.to_string();
        match options.iter_mut().find(|(k, _)| k == setting.key) {
            Some(existing) => existing.1 = value,
            None => options.push((setting.key.to_string(), value)),
        }
    }
    options
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_are_valid_choices() {
        for setting in CONSOLES.iter().flat_map(|c| c.settings) {
            assert!(setting.default < setting.choices.len(), "{}", setting.key);
        }
    }

    // The largest picture each console draws natively, which the resolution choices scale.
    fn native(core: &str) -> (u32, u32) {
        match core {
            "armsx2" | "pcsx2" => (640, 512),
            "dolphin" => (640, 528),
            "ppsspp" => (480, 272),
            _ => (640, 480),
        }
    }

    fn output_size(console: &Console, value: &str) -> (u32, u32) {
        if let Some((w, h)) = value
            .split_once('x')
            .and_then(|(w, h)| Some((w.parse().ok()?, h.parse().ok()?)))
        {
            return (w, h);
        }
        let factor: u32 = value
            .chars()
            .take_while(char::is_ascii_digit)
            .collect::<String>()
            .parse()
            .unwrap();
        let (w, h) = native(console.core);
        (w * factor, h * factor)
    }

    #[test]
    fn every_resolution_choice_fits_the_frame_buffer() {
        for console in CONSOLES {
            let (max_w, max_h) = if console.vulkan {
                (romp_proto::frame::MAX_W, romp_proto::frame::MAX_H)
            } else {
                (1920, 1080)
            };
            let setting = &console.settings[0];
            assert_eq!(setting.label, "Resolution", "{}", console.core);
            for choice in setting.choices {
                let (w, h) = output_size(console, choice.value);
                assert!(w <= max_w, "{} {} is {w} wide", console.core, choice.value);
                if console.vulkan {
                    assert!(h <= max_h, "{} {} is {h} tall", console.core, choice.value);
                }
            }
        }
    }

    #[test]
    fn vulkan_settings_only_show_where_the_emulator_uses_vulkan() {
        let listed: Vec<_> = consoles(&CoreChoices::new()).map(|c| c.core).collect();
        assert_eq!(listed.contains(&"dolphin"), cfg!(target_os = "macos"));
        assert_eq!(
            listed.contains(&"mupen64plus_next"),
            cfg!(target_os = "macos")
        );
        let n64 = core_options("mupen64plus_next", &Chosen::new());
        let upscaling = n64
            .iter()
            .any(|(k, _)| k == "mupen64plus-parallel-rdp-upscaling");
        assert_eq!(upscaling, cfg!(target_os = "macos"));
    }

    fn ps2() -> Option<&'static Console> {
        consoles(&CoreChoices::new()).find(|c| c.platform == "ps2")
    }

    #[test]
    fn a_chosen_setting_overrides_its_default() {
        let Some(console) = ps2() else { return };
        let resolution = &console.settings[0];
        let mut chosen = Chosen::new();
        let default = resolution.choices[resolution.default].value;
        assert!(
            core_options(console.core, &chosen).contains(&(resolution.key.into(), default.into()))
        );

        assert!(choose(&mut chosen, resolution.key, 0));
        let native = resolution.choices[0].value;
        assert!(
            core_options(console.core, &chosen).contains(&(resolution.key.into(), native.into()))
        );
        if console.core == "armsx2" {
            assert!(core_options(console.core, &chosen)
                .contains(&("armsx2_renderer".into(), "Vulkan".into())));
        }
    }

    #[test]
    fn unknown_or_stale_choices_fall_back_safely() {
        let mut chosen = Chosen::new();
        assert!(!choose(&mut chosen, "armsx2_upscale", 9));
        assert!(!choose(&mut chosen, "nope", 0));
        assert!(chosen.is_empty());
        assert_eq!(chosen_from_json(Some("not json")), Chosen::new());
        let Some(console) = ps2() else { return };
        let resolution = &console.settings[0];
        chosen.insert(resolution.key.into(), "99x".into());
        let default = resolution.choices[resolution.default].value;
        assert!(
            core_options(console.core, &chosen).contains(&(resolution.key.into(), default.into()))
        );
    }

    #[test]
    fn only_the_emulators_this_computer_uses_are_listed() {
        for console in consoles(&CoreChoices::new()) {
            assert_eq!(
                crate::cores::core_for_platform(console.platform)
                    .unwrap()
                    .id,
                console.core
            );
        }
        assert_eq!(
            consoles(&CoreChoices::new())
                .filter(|c| c.platform == "ps2")
                .count(),
            1,
            "one PS2 emulator per computer"
        );
    }

    #[test]
    fn the_chosen_emulator_brings_its_own_settings() {
        let mut cores = CoreChoices::new();
        let psx = |cores: &CoreChoices| -> Vec<&str> {
            consoles(cores)
                .filter(|c| c.platform == "psx")
                .map(|c| c.core)
                .collect()
        };
        let default = crate::cores::core_for_platform("psx").unwrap().id;
        let other = cores_for_platform("psx")
            .into_iter()
            .find(|c| c.id != default)
            .unwrap()
            .id;
        cores.insert("psx".into(), other.into());
        let listed = psx(&cores);
        assert!(!listed.contains(&default));
        assert_eq!(listed.contains(&other), uses_vulkan(other));
        assert!(psx(&CoreChoices::new()).iter().all(|c| *c == default));

        let swanstation = core_options("swanstation", &Chosen::new());
        let scale = swanstation
            .iter()
            .find(|(k, _)| k == "swanstation_GPU_ResolutionScale")
            .map(|(_, v)| v.as_str());
        assert_eq!(scale, uses_vulkan("swanstation").then_some("2"));
        assert!(swanstation.contains(&(
            "swanstation_MemoryCards_Card1Type".into(),
            "Libretro".into()
        )));
    }

    #[test]
    fn every_system_with_a_choice_of_emulators_is_named_and_shows_its_choice() {
        let rows = emulator_choices(&CoreChoices::new());
        assert_eq!(rows.len(), choosable_platforms().len());
        for row in &rows {
            let slug = emulator_platform(&row.key).unwrap();
            assert!(
                SYSTEM_NAMES.iter().any(|(s, _)| *s == slug),
                "{slug} has no name"
            );
            assert!(row.choices.len() > 1, "{slug}");
            assert_eq!(row.current, 0, "{slug} starts on its default");
        }
        let mut cores = CoreChoices::new();
        assert!(crate::cores::choose_core(&mut cores, "gb", 1));
        let gb = emulator_choices(&cores)
            .into_iter()
            .find(|r| r.key == "emulator:gb")
            .unwrap();
        assert_eq!(gb.current, 1);
        assert_eq!(gb.system, "Game Boy");
        assert_eq!(emulator_platform("armsx2_upscale"), None);
    }
}
