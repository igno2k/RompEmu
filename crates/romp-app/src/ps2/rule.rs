//! Which folders on a PS2 memory card are one game's.
//!
//! A save folder is the region prefix (`BA` America, `BE` Europe, `BI` Japan and Asia), the
//! disc serial, and whatever the game adds: `BASLUS-20152AC04` beside `BASLUS-20152SYS`.
//! RomM names the game by its `save_target`, which may carry the prefix (`BASLUS-20439`) or be
//! the bare serial (`SLUS-20439`), whose third letter says the region. A game owns every
//! folder starting with that stem, as the RomMix fork (`units/keys.ts`, `ps2Stems`) and
//! Argosy's `Ps2FolderHandler` decide it, so the three clients claim the same folders.
//!
//! PCSX2 itself shows a game the folders matching its serial plus the `memcardFilters` its
//! GameDB lists for it (`FolderMemoryCard::AddFolder`, pcsx2/SIO/Memcard/MemoryCardFolder.cpp),
//! so a sequel sees its predecessor's saves; the game's unit takes those folders too. The
//! console's own folders, `B?DATA-SYSTEM` (system configuration), `B?EXEC-SYSTEM` (system
//! updates) and `BWNETCNF` (network settings), are every game's and never part of one, even
//! where PCSX2's filter shows them to every game.

use super::gamedb::GameDb;

/// Letters and digits, upper case: what PCSX2 and the serial databases agree on, so
/// `BASLUS-20152AC04` and `BASLUS_20152AC04` are one save.
pub fn normalize(name: &str) -> String {
    name.chars()
        .filter(char::is_ascii_alphanumeric)
        .map(|c| c.to_ascii_uppercase())
        .collect()
}

fn is_region_prefixed(serial: &str) -> bool {
    let b = serial.as_bytes();
    b.len() >= 9
        && b[0] == b'B'
        && matches!(b[1], b'A' | b'E' | b'I')
        && b[2..6].iter().all(u8::is_ascii_uppercase)
        && b[6..].iter().all(u8::is_ascii_digit)
}

fn is_bare_serial(serial: &str) -> bool {
    let b = serial.as_bytes();
    b.len() >= 7
        && b[..4].iter().all(u8::is_ascii_uppercase)
        && b[4..].iter().all(u8::is_ascii_digit)
}

/// The folder-name prefixes a PS2 game's saves start with, most specific first: the serial
/// with its region prefix, then the bare serial for folders a homebrew tool wrote without one.
/// Empty for a key that is not a serial, which leaves the game with no unit rather than one
/// matching every folder on the card.
pub fn stems(key: &str) -> Vec<String> {
    let serial = normalize(key);
    if is_region_prefixed(&serial) {
        return vec![serial.clone(), serial[2..].to_string()];
    }
    if !is_bare_serial(&serial) {
        return Vec::new();
    }
    let prefix = match serial.as_bytes()[2] {
        b'E' => "BE",
        b'P' | b'J' | b'K' => "BI",
        _ => "BA",
    };
    vec![format!("{prefix}{serial}"), serial]
}

/// The disc serial of a key, without its region prefix, as PCSX2's GameDB names games:
/// normalized, so `BASLUS-20439` and `SLUS-20439` both give `SLUS20439`.
pub fn bare_serial(key: &str) -> Option<String> {
    let serial = normalize(key);
    if is_region_prefixed(&serial) {
        Some(serial[2..].to_string())
    } else {
        is_bare_serial(&serial).then_some(serial)
    }
}

/// The console's own folders, which no game owns.
pub fn is_system_folder(name: &str) -> bool {
    let n = normalize(name);
    let Some(rest) = n.strip_prefix('B') else {
        return false;
    };
    let unprefixed = rest.strip_prefix(['A', 'E', 'I']).unwrap_or(rest);
    [rest, unprefixed].iter().any(|r| {
        r.starts_with("DATASYSTEM") || r.starts_with("EXECSYSTEM") || r.starts_with("WNETCNF")
    })
}

/// PCSX2's own bookkeeping on a folder card (`_pcsx2_superblock`, `_pcsx2_index`,
/// `_pcsx2_meta`, `_pcsx2_meta_directory`), which PCSX2 leaves out of the card it shows
/// (`GetOrderedFiles` skips every `_pcsx2_` name) and which is never part of a save.
pub fn is_card_file(name: &str) -> bool {
    name.starts_with("_pcsx2_")
}

/// The folders one game owns on a card.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UnitRule {
    key: String,
    stems: Vec<String>,
}

impl UnitRule {
    /// The rule for the game RomM names `save_target`, widened by the `memcardFilters` the
    /// GameDB lists for its serial. None where the key is not a PS2 serial.
    pub fn new(save_target: &str, gamedb: &GameDb) -> Option<Self> {
        let mut all = stems(save_target);
        if all.is_empty() {
            return None;
        }
        if let Some(serial) = bare_serial(save_target) {
            for filter in gamedb.filters(&serial) {
                let mut extra = stems(filter);
                // A few filters name a folder rather than a serial (`BISLPM-65286NET`), which
                // PCSX2 matches as it is; the console's own folders stay out regardless.
                let literal = normalize(filter);
                if extra.is_empty() && !is_system_folder(filter) && literal.len() >= 8 {
                    extra.push(literal);
                }
                for stem in extra {
                    if !all.contains(&stem) {
                        all.push(stem);
                    }
                }
            }
        }
        Some(UnitRule {
            key: save_target.trim().to_string(),
            stems: all,
        })
    }

    pub fn key(&self) -> &str {
        &self.key
    }

    /// Whether a folder on the card is this game's.
    pub fn owns(&self, name: &str) -> bool {
        if is_card_file(name) || is_system_folder(name) {
            return false;
        }
        let n = normalize(name);
        self.stems.iter().any(|stem| n.starts_with(stem.as_str()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_bare_serial_gains_the_region_prefix_its_third_letter_names() {
        // The RomMix fork's cases (units.test.ts), so both clients claim alike.
        assert_eq!(stems("SLUS-20152"), ["BASLUS20152", "SLUS20152"]);
        assert_eq!(stems("SLES-50330"), ["BESLES50330", "SLES50330"]);
        assert_eq!(stems("SLPS_250.50"), ["BISLPS25050", "SLPS25050"]);
        assert_eq!(stems("SCKA-20010"), ["BISCKA20010", "SCKA20010"]);
        assert_eq!(stems("SCUS-97328"), ["BASCUS97328", "SCUS97328"]);
        assert_eq!(stems("BASLUS-20152"), ["BASLUS20152", "SLUS20152"]);
        assert_eq!(stems("BASLUS-20439"), ["BASLUS20439", "SLUS20439"]);
        for not_a_serial in ["", "Mcd001", "BA", "Futurama"] {
            assert!(stems(not_a_serial).is_empty(), "{not_a_serial}");
        }
    }

    #[test]
    fn a_game_owns_the_folders_starting_with_its_serial() {
        let rule = UnitRule::new("BASLUS-20439", &GameDb::empty()).unwrap();
        assert!(rule.owns("BASLUS-20439Futurama"));
        assert!(rule.owns("BASLUS_20439SYS"));
        assert!(rule.owns("SLUS-20439"));
        assert!(!rule.owns("BASLUS-20440"));
        assert!(!rule.owns("BASLUS-21693XX"));
        assert!(!rule.owns("_pcsx2_superblock"));
        assert!(!rule.owns("_pcsx2_index"));
        assert!(UnitRule::new("not a serial", &GameDb::empty()).is_none());
    }

    #[test]
    fn system_folders_are_never_a_games() {
        for system in [
            "BADATA-SYSTEM",
            "BEDATA-SYSTEM",
            "BIDATA-SYSTEM",
            "BEEXEC-SYSTEM",
            "BWNETCNF",
            "BIWNETCNF",
        ] {
            assert!(is_system_folder(system), "{system}");
        }
        // Half-Life's own folder ends in SYSTEM, and is Half-Life's.
        for game in ["BASLUS-20066SYSTEM", "BASLUS-20152SYS", "BANETCNF"] {
            assert!(!is_system_folder(game), "{game}");
        }
        let db = GameDb::parse(
            "SLUS-20066:\n  memcardFilters:\n    - \"BADATA-SYSTEM\"\n    - \"BWNETCNF\"\n",
        );
        let rule = UnitRule::new("SLUS-20066", &db).unwrap();
        assert!(rule.owns("BASLUS-20066SYSTEM"));
        assert!(!rule.owns("BADATA-SYSTEM"));
        assert!(!rule.owns("BWNETCNF"));
    }

    #[test]
    fn memcard_filters_widen_the_unit() {
        let db = GameDb::parse(
            "SCUS-97328:\n  name: \"Gran Turismo 4\"\n  memcardFilters:\n    - \"SCUS-97328\"\n    - \"SCUS-97102\" # GT3\n",
        );
        let rule = UnitRule::new("BASCUS-97328", &db).unwrap();
        assert!(rule.owns("BASCUS-97328GT4"));
        assert!(rule.owns("BASCUS-97102GT3"));
        assert!(!rule.owns("BASCUS-97103"));
        let db = GameDb::parse(
            "SLPM-65286:\n  memcardFilters:\n    - \"BISLPM-65286NET\"\n    - \"BWNETCNF\"\n",
        );
        let rule = UnitRule::new("SLPM-65286", &db).unwrap();
        assert!(rule.owns("BISLPM-65286NET"));
        assert!(!rule.owns("BWNETCNF"));
        // Without the GameDB, the serial alone.
        let plain = UnitRule::new("BASCUS-97328", &GameDb::empty()).unwrap();
        assert!(!plain.owns("BASCUS-97102GT3"));
    }
}
