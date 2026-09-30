//! The `memcardFilters` of PCSX2's GameDB (`GameIndex.yaml`).
//!
//! ARMSX2 reads its GameDB from `<system>/pcsx2/resources/GameIndex.yaml`
//! (`GameDatabase::initDatabase`, pcsx2/GameDatabase.cpp in RompEmu/ARMSX2), but the core
//! Romp downloads ships only the library, so that file is there only where someone put it.
//! LRPS2 compiles its copy into the core (pcsx2/CMakeLists.txt, `GameDatabaseBuiltin.cpp`),
//! where Romp can't read it. So Romp carries the filters itself, cut from ARMSX2's copy (see
//! `memcard_filters.yaml`), and prefers the installed file where there is one, since that is
//! what the emulator filters by.
//!
//! Only the lines Romp needs are read, with a parser for the plain shape GameIndex.yaml keeps:
//! a serial at the start of a line, `memcardFilters:` under it, one `- "SERIAL"` per line.

use super::rule::normalize;
use std::borrow::Cow;
use std::collections::HashMap;
use std::path::Path;
use std::sync::OnceLock;

/// Where ARMSX2 reads its GameDB, relative to the libretro system folder.
pub const INSTALLED: &str = "pcsx2/resources/GameIndex.yaml";

const BUNDLED: &str = include_str!("memcard_filters.yaml");

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct GameDb {
    /// Filters by the game's normalized serial (`SLUS20152`).
    filters: HashMap<String, Vec<String>>,
}

/// A line without its `#` comment, quotes respected.
fn without_comment(line: &str) -> &str {
    let mut quote = None;
    let mut previous = ' ';
    for (i, c) in line.char_indices() {
        match quote {
            Some(q) if c == q => quote = None,
            Some(_) => {}
            None if c == '"' || c == '\'' => quote = Some(c),
            None if c == '#' && previous.is_whitespace() => return &line[..i],
            None => {}
        }
        previous = c;
    }
    line
}

impl GameDb {
    #[cfg(test)]
    pub fn empty() -> Self {
        Self::default()
    }

    pub fn parse(text: &str) -> Self {
        let mut filters: HashMap<String, Vec<String>> = HashMap::new();
        let mut serial: Option<String> = None;
        let mut in_filters = false;
        for line in text.lines() {
            let content = without_comment(line);
            let trimmed = content.trim();
            if trimmed.is_empty() {
                continue;
            }
            if !line.starts_with([' ', '\t']) {
                in_filters = false;
                serial = trimmed
                    .strip_suffix(':')
                    .map(|s| normalize(s.trim().trim_matches('"')))
                    .filter(|s| !s.is_empty());
                continue;
            }
            if trimmed == "memcardFilters:" {
                in_filters = serial.is_some();
                continue;
            }
            if in_filters {
                if let (Some(item), Some(serial)) = (trimmed.strip_prefix('-'), &serial) {
                    let value = item.trim().trim_matches('"').trim_matches('\'').trim();
                    if !value.is_empty() {
                        filters
                            .entry(serial.clone())
                            .or_default()
                            .push(value.to_string());
                    }
                    continue;
                }
                in_filters = false;
            }
        }
        GameDb { filters }
    }

    /// The filters Romp carries, cut from ARMSX2's GameDB.
    pub fn bundled() -> &'static GameDb {
        static BUNDLE: OnceLock<GameDb> = OnceLock::new();
        BUNDLE.get_or_init(|| GameDb::parse(BUNDLED))
    }

    /// The GameDB installed in the libretro system folder, or the bundled one where there is
    /// none or it can't be read. Where neither has any filters, a game's saves are its serial's
    /// alone, which is said in the log.
    pub fn load(system_dir: Option<&Path>) -> Cow<'static, GameDb> {
        if let Some(path) = system_dir.map(|dir| dir.join(INSTALLED)) {
            match std::fs::read_to_string(&path) {
                Ok(text) => {
                    let installed = GameDb::parse(&text);
                    if !installed.is_empty() {
                        return Cow::Owned(installed);
                    }
                    tracing::warn!(
                        "{} has no memcardFilters, using Romp's own copy",
                        path.display()
                    );
                }
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                Err(e) => tracing::warn!(
                    "could not read {}: {e}; using Romp's own copy",
                    path.display()
                ),
            }
        }
        let bundled = GameDb::bundled();
        if bundled.is_empty() {
            tracing::warn!(
                "no PS2 GameDB memcardFilters could be read; each game's saves are the folders \
                 of its own serial"
            );
        }
        Cow::Borrowed(bundled)
    }

    pub fn is_empty(&self) -> bool {
        self.filters.is_empty()
    }

    /// The filters listed for a normalized serial, `SLUS20152`.
    pub fn filters(&self, serial: &str) -> &[String] {
        self.filters.get(serial).map_or(&[], Vec::as_slice)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_the_filters_of_each_serial() {
        let db = GameDb::parse(
            "# comment\nSLUS-20152:\n  name: \"Ace # Combat\"\n  memcardFilters: # Reads AC4 saves.\n    - \"SLUS-20152\"\n    - 'SLUS-20851' # AC5\n  gsHWFixes:\n    - x\nSLUS-20066:\n  name: \"Half-Life\"\n",
        );
        assert_eq!(db.filters("SLUS20152"), ["SLUS-20152", "SLUS-20851"]);
        assert!(db.filters("SLUS20066").is_empty());
        assert!(GameDb::parse("not: [yaml").is_empty());
    }

    #[test]
    fn the_bundled_filters_are_readable() {
        let db = GameDb::bundled();
        assert_eq!(db.filters.len(), 421);
        assert!(db.filters("SCUS97328").contains(&"SCUS-97102".to_string()));
    }

    #[test]
    fn an_installed_gamedb_wins_and_a_broken_one_falls_back() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(*GameDb::load(Some(dir.path())), *GameDb::bundled());
        assert_eq!(*GameDb::load(None), *GameDb::bundled());
        let file = dir.path().join(INSTALLED);
        std::fs::create_dir_all(file.parent().unwrap()).unwrap();
        std::fs::write(
            &file,
            "SLUS-20066:\n  memcardFilters:\n    - \"SLUS-20066\"\n",
        )
        .unwrap();
        assert_eq!(
            GameDb::load(Some(dir.path())).filters("SLUS20066"),
            ["SLUS-20066"]
        );
        std::fs::write(&file, [0xFFu8, 0xFE, 0x00]).unwrap();
        assert_eq!(*GameDb::load(Some(dir.path())), *GameDb::bundled());
    }
}
