//! The zip a PS2 save travels in between RomM's clients.
//!
//! Its roots are the game's save folders as a PCSX2 folder card holds them,
//! `BASLUS-20152AC04/icon.sys`, without the card's own `_pcsx2_*` files: the shape Argosy, the
//! RomMix fork and bazzite-maint's importer upload. Argosy has also uploaded whole cards, the
//! folders one level down, and a pull takes both.
//!
//! Romp writes it the same way every time, entries in name order, stored, every time the zip
//! epoch (1980-01-01), so an unchanged save makes the same bytes. RomM compares zips by what is
//! in them rather than by their bytes (`hash_zip_contents`), which `content_hash` reproduces.

use super::card::valid_name;
use super::rule::{is_card_file, UnitRule};
use super::{Files, Unit};
use md5::{Digest, Md5};
use std::collections::BTreeSet;
use std::io::{Cursor, Read, Write};

/// The most a PS2 save zip may unpack to. A card holds 8 MB, so a zip declaring more is not
/// one game's saves, or is built to fill the disk it is unpacked on.
const MAX_UNPACKED: u64 = 32 << 20;

pub fn is_zip(bytes: &[u8]) -> bool {
    bytes.starts_with(b"PK\x03\x04")
}

fn md5_hex(bytes: &[u8]) -> String {
    Md5::digest(bytes)
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

/// RomM's `hash_zip_contents` (backend/handler/filesystem/assets_handler.py, RomM 5.3.1): the
/// md5 of one `<name>:<md5 of the entry>` line per file, sorted by name and joined by newlines,
/// folder entries left out.
fn combined_hash(mut lines: Vec<(String, String)>) -> String {
    lines.sort();
    let text = lines
        .iter()
        .map(|(name, md5)| format!("{name}:{md5}"))
        .collect::<Vec<_>>()
        .join("\n");
    md5_hex(text.as_bytes())
}

/// The content hash RomM records for the zip `build` writes of `unit`.
pub fn content_hash(unit: &Unit) -> String {
    combined_hash(
        unit.iter()
            .flat_map(|(folder, files)| {
                files
                    .iter()
                    .map(move |(name, data)| (format!("{folder}/{name}"), md5_hex(data)))
            })
            .collect(),
    )
}

/// The content hash RomM records for any zip, as the tests check `content_hash` against.
#[cfg(test)]
pub fn zip_content_hash(bytes: &[u8]) -> Result<String, String> {
    let mut zip = open(bytes)?;
    let mut lines = Vec::new();
    for i in 0..zip.len() {
        let mut entry = zip.by_index(i).map_err(unreadable)?;
        let name = entry.name().to_string();
        if name.ends_with('/') {
            continue;
        }
        lines.push((name, md5_hex(&read_entry(&mut entry)?)));
    }
    Ok(combined_hash(lines))
}

/// The zip of `unit`: a folder entry, then its files, for each save folder in name order.
pub fn build(unit: &Unit) -> Result<Vec<u8>, String> {
    let failed = |e: zip::result::ZipError| format!("could not write the save zip: {e}");
    let file = zip::write::SimpleFileOptions::default()
        .compression_method(zip::CompressionMethod::Stored)
        .last_modified_time(zip::DateTime::default())
        .unix_permissions(0o644);
    let folder = file.unix_permissions(0o755);
    let mut zip = zip::ZipWriter::new(Cursor::new(Vec::new()));
    for (name, files) in unit {
        zip.add_directory(format!("{name}/"), folder)
            .map_err(failed)?;
        for (file_name, data) in files {
            zip.start_file(format!("{name}/{file_name}"), file)
                .map_err(failed)?;
            zip.write_all(data)
                .map_err(|e| format!("could not write the save zip: {e}"))?;
        }
    }
    Ok(zip.finish().map_err(failed)?.into_inner())
}

fn unreadable(e: zip::result::ZipError) -> String {
    format!("the save zip can't be read: {e}")
}

fn open(bytes: &[u8]) -> Result<zip::ZipArchive<Cursor<&[u8]>>, String> {
    let mut zip = zip::ZipArchive::new(Cursor::new(bytes)).map_err(unreadable)?;
    let mut total = 0u64;
    for i in 0..zip.len() {
        total = total.saturating_add(zip.by_index_raw(i).map_err(unreadable)?.size());
    }
    if total > MAX_UNPACKED {
        return Err(format!(
            "the save zip unpacks to {total} bytes, more than a PS2 memory card holds"
        ));
    }
    Ok(zip)
}

fn read_entry(entry: &mut zip::read::ZipFile<'_, Cursor<&[u8]>>) -> Result<Vec<u8>, String> {
    let mut data = Vec::new();
    entry
        .take(MAX_UNPACKED + 1)
        .read_to_end(&mut data)
        .map_err(|e| format!("the save zip can't be read: {e}"))?;
    if data.len() as u64 > MAX_UNPACKED {
        return Err("the save zip unpacks to more than it says".into());
    }
    Ok(data)
}

/// The game's save folders in a zip, and the names of the roots left out as not the game's.
///
/// Folders `rule` does not claim are left out, never unpacked, whatever else the zip holds;
/// so are PCSX2's own `_pcsx2_*` files, which describe the card they came from. A zip whose
/// roots include none of the game's folders is read one level down, as a whole card.
pub fn read(bytes: &[u8], rule: &UnitRule) -> Result<(Unit, Vec<String>), String> {
    let mut zip = open(bytes)?;
    let mut entries = Vec::new();
    for i in 0..zip.len() {
        let entry = zip.by_index_raw(i).map_err(unreadable)?;
        let name = entry.name().replace('\\', "/");
        let is_dir = name.ends_with('/');
        let parts: Vec<String> = name
            .trim_start_matches('/')
            .split('/')
            .filter(|p| !p.is_empty())
            .map(str::to_string)
            .collect();
        if parts.iter().any(|p| p == "." || p == "..") {
            return Err(format!("the save zip has an unsafe entry, {name}"));
        }
        if parts.is_empty() || parts[0] == "__MACOSX" {
            continue;
        }
        entries.push((i, parts, is_dir));
    }
    let card_rooted = !entries
        .iter()
        .any(|(_, parts, is_dir)| (*is_dir || parts.len() > 1) && rule.owns(&parts[0]));
    let strip = usize::from(card_rooted);

    let mut unit = Unit::new();
    let mut left_out = BTreeSet::new();
    for (i, parts, is_dir) in entries {
        let Some(parts) = parts.get(strip..).filter(|p| !p.is_empty()) else {
            continue;
        };
        let folder = &parts[0];
        if is_card_file(folder) || !rule.owns(folder) {
            left_out.insert(folder.clone());
            continue;
        }
        if !valid_name(folder) {
            return Err(format!(
                "the save zip has a folder a memory card can't hold, {folder}"
            ));
        }
        let files: &mut Files = unit.entry(folder.clone()).or_default();
        if parts.len() == 1 {
            if !is_dir {
                return Err(format!(
                    "the save zip has a file {folder} where a save folder goes"
                ));
            }
            continue;
        }
        if is_card_file(&parts[1]) {
            continue;
        }
        if parts.len() > 2 || is_dir {
            return Err(format!(
                "the save zip has a folder inside {folder}; PS2 saves have none"
            ));
        }
        let file = &parts[1];
        if !valid_name(file) {
            return Err(format!(
                "the save zip has a file a memory card can't hold, {folder}/{file}"
            ));
        }
        let data = read_entry(&mut zip.by_index(i).map_err(unreadable)?)?;
        if files.insert(file.clone(), data).is_some() {
            return Err(format!("the save zip holds {folder}/{file} twice"));
        }
    }
    Ok((unit, left_out.into_iter().collect()))
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::ps2::GameDb;

    pub(crate) fn unit(entries: &[(&str, &str, &[u8])]) -> Unit {
        let mut unit = Unit::new();
        for (folder, file, data) in entries {
            unit.entry((*folder).to_string())
                .or_default()
                .insert((*file).to_string(), data.to_vec());
        }
        unit
    }

    /// The RomMix fork's fixture card (units/fixtures.ts): one game's two folders.
    pub(crate) fn futurama_like() -> Unit {
        unit(&[
            (
                "BASLUS-20152AC04",
                "BASLUS-20152AC04",
                b"progress of the first game",
            ),
            ("BASLUS-20152AC04", "icon.sys", b"icon of the first game"),
            (
                "BASLUS-20152SYS",
                "icon.sys",
                b"system icon of the first game",
            ),
            ("BASLUS-20152SYS", "settings", b"settings of the first game"),
        ])
    }

    fn rule(key: &str) -> UnitRule {
        UnitRule::new(key, &GameDb::empty()).unwrap()
    }

    /// A zip as another client writes one: deflated, dated, and in its own order.
    pub(crate) fn foreign_zip(entries: &[(&str, &[u8])]) -> Vec<u8> {
        let mut zip = zip::ZipWriter::new(Cursor::new(Vec::new()));
        let options = zip::write::SimpleFileOptions::default()
            .compression_method(zip::CompressionMethod::Deflated)
            .last_modified_time(zip::DateTime::from_date_and_time(2026, 9, 30, 12, 0, 0).unwrap());
        for (name, data) in entries {
            if name.ends_with('/') {
                zip.add_directory(*name, options).unwrap();
            } else {
                zip.start_file(*name, options).unwrap();
                zip.write_all(data).unwrap();
            }
        }
        zip.finish().unwrap().into_inner()
    }

    #[test]
    fn the_content_hash_is_romms() {
        // RomM's own hash_zip_contents over this unit, pinned by the RomMix fork (zip.test.ts).
        let expected = "9451d2d60d9692b2abaa142eb0764ea5";
        let unit = futurama_like();
        assert_eq!(content_hash(&unit), expected);
        assert_eq!(zip_content_hash(&build(&unit).unwrap()).unwrap(), expected);
        // However another client compressed, dated and ordered it.
        let other = foreign_zip(&[
            ("BASLUS-20152SYS/settings", b"settings of the first game"),
            ("BASLUS-20152AC04/", b""),
            ("BASLUS-20152AC04/icon.sys", b"icon of the first game"),
            ("BASLUS-20152SYS/icon.sys", b"system icon of the first game"),
            (
                "BASLUS-20152AC04/BASLUS-20152AC04",
                b"progress of the first game",
            ),
        ]);
        assert_eq!(zip_content_hash(&other).unwrap(), expected);
    }

    #[test]
    fn a_zip_is_the_same_bytes_every_time() {
        let unit = futurama_like();
        let zip = build(&unit).unwrap();
        assert_eq!(zip, build(&unit).unwrap());
        let mut archive = zip::ZipArchive::new(Cursor::new(&zip[..])).unwrap();
        let names: Vec<String> = archive.file_names().map(str::to_string).collect();
        assert_eq!(
            names,
            [
                "BASLUS-20152AC04/",
                "BASLUS-20152AC04/BASLUS-20152AC04",
                "BASLUS-20152AC04/icon.sys",
                "BASLUS-20152SYS/",
                "BASLUS-20152SYS/icon.sys",
                "BASLUS-20152SYS/settings"
            ]
        );
        for i in 0..archive.len() {
            let entry = archive.by_index(i).unwrap();
            assert_eq!(entry.compression(), zip::CompressionMethod::Stored);
            assert_eq!(entry.last_modified(), Some(zip::DateTime::default()));
            assert!(!entry.name().starts_with("_pcsx2"));
        }
    }

    #[test]
    fn only_the_games_folders_are_taken() {
        let zip = foreign_zip(&[
            ("BASLUS-20152AC04/icon.sys", b"mine"),
            ("BASLUS-21693XX/icon.sys", b"another game's"),
            ("BADATA-SYSTEM/history", b"the console's"),
            ("_pcsx2_superblock", b"a card's"),
            ("_pcsx2_index", b"a card's"),
            ("BASLUS-20152AC04/_pcsx2_index", b"a card's"),
        ]);
        let (unit, left_out) = read(&zip, &rule("SLUS-20152")).unwrap();
        assert_eq!(
            unit,
            super::tests::unit(&[("BASLUS-20152AC04", "icon.sys", b"mine")])
        );
        assert_eq!(
            left_out,
            [
                "BADATA-SYSTEM",
                "BASLUS-21693XX",
                "_pcsx2_index",
                "_pcsx2_superblock"
            ]
        );
    }

    #[test]
    fn a_whole_card_is_read_one_level_down() {
        let zip = foreign_zip(&[
            ("test/_pcsx2_superblock", b"superblock"),
            ("test/BASLUS-20152AC04/", b""),
            ("test/BASLUS-20152AC04/ace.bin", b"server-save"),
            ("test/BASLUS-21693XX/other.bin", b"not ours"),
        ]);
        let (unit, _) = read(&zip, &rule("BASLUS-20152")).unwrap();
        assert_eq!(
            unit,
            super::tests::unit(&[("BASLUS-20152AC04", "ace.bin", b"server-save")])
        );
    }

    #[test]
    fn unsafe_or_impossible_zips_are_refused() {
        let r = rule("SLUS-20152");
        for entries in [
            vec![("BASLUS-20152AC04/../../evil", &b"x"[..])],
            vec![("BASLUS-20152AC04/sub/deep", &b"x"[..])],
            vec![("BASLUS-20152AC04/a:b", &b"x"[..])],
        ] {
            assert!(read(&foreign_zip(&entries), &r).is_err(), "{entries:?}");
        }
        assert!(read(b"PK\x03\x04 not really", &r).is_err());
        assert!(zip_content_hash(b"nope").is_err());
    }
}
