//! PS2 saves in the shape RomM's other clients share them in.
//!
//! RomM holds a PS2 game's in-game save in the `autosave` slot, tagged `pcsx2`, as a zip whose
//! roots are the game's own save folders (`BASLUS-20439Futurama/…`), the shape Argosy, RomM's
//! reference client, uploads from Android's PCSX2 forks and the RomMix fork uploads from
//! RetroDECK. So Romp syncs a game's folders rather than a whole memory card, and which folders
//! are the game's is decided by RomM's `save_target` for the game (see [`rule`]).
//!
//! On the computer, each emulator keeps the folders where it reads them:
//!
//! - ARMSX2 (macOS) is PCSX2 with a libretro frontend and keeps PCSX2's folder memory cards:
//!   a directory at the card's path is a folder card (`FileMcd_SetType`,
//!   pcsx2/SIO/Memcard/MemoryCardFile.cpp:621-640 in RompEmu/ARMSX2, branch romp), filtered to
//!   the running game's serial and GameDB `memcardFilters` (`FileMcd_EmuOpen` sets filtering on
//!   at :643-653; `FolderMemoryCard::AddFolder`, MemoryCardFolder.cpp:457-500). The libretro
//!   frontend puts the cards in the save folder it is given
//!   (pcsx2-libretro/Main.cpp:244-251), so each game gets its own
//!   `pcsx2/memcards/Mcd001.ps2/`, and a pull or push is folders moved or read ([`folder`]).
//! - LRPS2 (x86_64 Windows and Linux) only knows memory card images: its card types are
//!   `Empty` and `File` (pcsx2/Config.h:188-193 in libretro/ps2), and it names each game's card
//!   after the game in the save folder (libretro/main.cpp:2718-2733, pcsx2/VMManager.cpp:303-305).
//!   There Romp reads the game's folders out of the image and writes them into it ([`card`]),
//!   as Ludo, the RomM organisation's Steam Deck client, does for the same core.
//!
//! Neither direction ever moves a zip into a card or an image up as a zip: what is uploaded is
//! always a zip Romp wrote of the game's folders, and what is downloaded is unpacked into
//! folders, or refused with the reason.

pub mod archive;
pub mod card;
pub mod folder;
pub mod gamedb;
pub mod meta;
pub mod rule;

pub use gamedb::GameDb;
pub use rule::UnitRule;
use std::collections::BTreeMap;

/// One save folder's files, by name.
pub type Files = BTreeMap<String, Vec<u8>>;
/// A game's save folders, by name: what one PS2 save zip holds.
pub type Unit = BTreeMap<String, Files>;

/// The emulator tag PS2 saves carry on RomM, which Romp has always uploaded them under.
pub const TAG: &str = "pcsx2";
/// The tags Argosy uploads PS2 saves under from Android's PCSX2 forks, whose folder cards are
/// PCSX2's, so a save one of them wrote is the same format.
pub const ALSO_ACCEPTED: [&str; 3] = ["armsx2", "nethersx2", "aethersx2"];

/// Whether a save RomM holds under `tag` is one Romp can put on a PS2 card. A save without a
/// tag is judged by what it holds.
pub fn accepts_tag(tag: Option<&str>) -> bool {
    tag.map(str::trim)
        .filter(|t| !t.is_empty())
        .is_none_or(|t| {
            std::iter::once(TAG)
                .chain(ALSO_ACCEPTED)
                .any(|ok| ok.eq_ignore_ascii_case(t))
        })
}

/// Where an emulator keeps a game's PS2 saves.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Layout {
    /// A PCSX2 folder memory card.
    Folder,
    /// A memory card image.
    Image,
}

/// The game's folders in a save downloaded from RomM: a PS2 save zip, or a memory card image
/// as Romp uploaded before it synced folders, from which only the game's folders are taken.
/// Anything else, or a save holding none of the game's folders, is refused with the reason.
pub fn decode(bytes: &[u8], rule: &UnitRule) -> Result<archive::Download, String> {
    let download = if archive::is_zip(bytes) {
        let (download, left_out) = archive::read(bytes, rule)?;
        if !left_out.is_empty() {
            tracing::info!(
                "left out of {}'s PS2 save zip, not its own: {}",
                rule.key(),
                left_out.join(", ")
            );
        }
        download
    } else if bytes.starts_with(card::MAGIC) {
        let unit = card::read_unit(bytes, &|n| rule.owns(n))
            .map_err(|e| format!("the server's memory card can't be read: {e}"))?;
        archive::Download {
            unit,
            ..Default::default()
        }
    } else {
        return Err(
            "the server's save for this game is neither a PS2 save zip nor a memory card".into(),
        );
    };
    // Shared folders alone are another game's saves, not this one's.
    if !download.unit.keys().any(|n| rule.is_own(n)) {
        return Err(format!(
            "the server's save holds no save folders for {}",
            rule.key()
        ));
    }
    Ok(download)
}

#[cfg(test)]
mod tests {
    use super::*;
    use archive::tests::{foreign_zip, futurama_like};

    fn rule() -> UnitRule {
        UnitRule::new("SLUS-20152", &GameDb::empty()).unwrap()
    }

    #[test]
    fn argosys_tags_are_accepted_and_others_refused() {
        for tag in [
            None,
            Some("pcsx2"),
            Some("PCSX2"),
            Some("armsx2"),
            Some("nethersx2"),
            Some("aethersx2"),
            Some(""),
        ] {
            assert!(accepts_tag(tag), "{tag:?}");
        }
        for tag in ["play", "retroarch", "duckstation"] {
            assert!(!accepts_tag(Some(tag)), "{tag}");
        }
    }

    #[test]
    fn a_zip_and_an_old_card_upload_both_give_the_games_folders() {
        let unit = futurama_like();
        assert_eq!(
            decode(&archive::build(&unit).unwrap(), &rule())
                .unwrap()
                .unit,
            unit
        );
        let mut with_another = unit.clone();
        with_another.insert(
            "BASLUS-21693XX".into(),
            Files::from([("a".into(), b"b".to_vec())]),
        );
        let image = card::image_of(&with_another, card::tod(0));
        assert_eq!(decode(&image, &rule()).unwrap().unit, unit);
    }

    #[test]
    fn another_games_shared_folders_alone_are_not_this_games_save() {
        let gt4 = card::tests::gt4();
        let only_gt3 = foreign_zip(&[("BASCUS-97102GT3/garage", b"GT3's")]);
        let err = decode(&only_gt3, &gt4).unwrap_err();
        assert!(err.contains("no save folders"), "{err}");
    }

    #[test]
    fn what_is_not_a_ps2_save_is_refused() {
        assert!(decode(b"game.srm contents", &rule()).is_err());
        let other = foreign_zip(&[("BASLUS-21693XX/save", b"another game")]);
        let err = decode(&other, &rule()).unwrap_err();
        assert!(err.contains("no save folders"), "{err}");
    }
}
