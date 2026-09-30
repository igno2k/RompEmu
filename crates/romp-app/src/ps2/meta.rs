//! PCSX2's metadata files in a folder card's save folders, which are part of the save.
//!
//! A folder card keeps a save as host files, and a host file has no PS2 mode, attributes or
//! PS2 name. Where those are not the defaults, PCSX2 keeps the card's raw 512-byte directory
//! entry beside the save: `<FOLDER>/_pcsx2_meta_directory` for the folder, and
//! `<FOLDER>/_pcsx2_meta/<file>` for a file. It writes one exactly when the entry's name had to
//! be cleaned for the host, or its mode is not the default, or it has attributes
//! (`FolderMemoryCard::FlushFileEntries` and `FileAccessHelper::WriteMetadata`,
//! pcsx2/SIO/Memcard/MemoryCardFolder.cpp:1262-1288 and :2040-2072 in RompEmu/ARMSX2), and
//! removes it otherwise. Loading a card, it takes the entry from the file, then sets the
//! entry's length and first cluster itself (`AddFolder`, :515-535; `AddFile`, :587-625). A
//! copy-protected save (`BASLUS-20314-TS2-OPT`, mode 0x842F) is one.
//!
//! So a metadata file carries the save's real PS2 name, mode, attributes and dates, and Romp
//! carries it, but in one canonical form, so that the same save hashes alike from a folder card
//! and from a memory card image: the raw entry with its length set from the save and its
//! cluster cleared, the two fields that describe where the save sat on one card and that PCSX2
//! replaces anyway, and none at all where PCSX2 would keep none. `_pcsx2_index`, PCSX2's file
//! order and dates for the host files, is not part of the save.

use super::Files;

pub const INDEX: &str = "_pcsx2_index";
pub const DIR_META: &str = "_pcsx2_meta_directory";
pub const FILE_META: &str = "_pcsx2_meta";
pub const ENTRY: usize = 512;
/// The mode PCSX2 gives a save folder without metadata (`MemoryCardFileEntry::DefaultDirMode`).
pub const DIR_MODE: u16 = 0x8427;
/// The mode PCSX2 gives a file without metadata (`MemoryCardFileEntry::DefaultFileMode`).
pub const FILE_MODE: u16 = 0x8497;
/// Where the name starts in an entry; PCSX2 reads a shorter metadata file as having none.
const NAME_AT: usize = 0x40;
const SHORTEST: usize = 0x60;

/// Where a file's metadata is kept in a save folder, as a key of [`Files`].
pub fn file_meta_key(file: &str) -> String {
    format!("{FILE_META}/{file}")
}

/// Whether a key of [`Files`] is metadata rather than a file of the save.
pub fn is_meta(key: &str) -> bool {
    key == DIR_META || key.starts_with("_pcsx2_meta/")
}

/// A card name as PCSX2 names the host file for it (`CleanMemcardFilename`,
/// MemoryCardFolder.cpp:2219-2262): `\ % : | " < >` become `_`, and so do dots and spaces at
/// the end, which Windows would drop.
pub fn clean(name: &[u8]) -> Vec<u8> {
    let mut out: Vec<u8> = name
        .iter()
        .map(|&b| if b"\\%:|\"<>".contains(&b) { b'_' } else { b })
        .collect();
    for b in out.iter_mut().rev() {
        if *b == b' ' || *b == b'.' {
            *b = b'_';
        } else {
            break;
        }
    }
    out
}

fn u16_at(raw: &[u8], at: usize) -> u16 {
    u16::from_le_bytes([raw[at], raw[at + 1]])
}

fn u32_at(raw: &[u8], at: usize) -> u32 {
    u32::from_le_bytes([raw[at], raw[at + 1], raw[at + 2], raw[at + 3]])
}

/// The entry PCSX2 loads from a metadata file for the host file `host`: the file's bytes, the
/// rest zero, and the host name where the file is too short to hold one.
pub fn loaded(raw: &[u8], host: &str) -> Vec<u8> {
    let mut entry = raw[..raw.len().min(ENTRY)].to_vec();
    entry.resize(ENTRY, 0);
    if raw.len() < SHORTEST {
        entry[NAME_AT..NAME_AT + 32].fill(0);
        let name = host.as_bytes();
        entry[NAME_AT..NAME_AT + name.len().min(31)].copy_from_slice(&name[..name.len().min(31)]);
    }
    entry
}

/// The PS2 name in an entry.
pub fn real_name(entry: &[u8]) -> &[u8] {
    let field = &entry[NAME_AT..NAME_AT + 32];
    let end = field.iter().position(|&b| b == 0).unwrap_or(field.len());
    &field[..end]
}

/// Whether PCSX2 keeps a metadata file for an entry: its name or its folder's needed
/// cleaning, or its mode or attributes are not the defaults.
pub fn is_nonstandard(entry: &[u8], default_mode: u16, folder_cleaned: bool) -> bool {
    let name = real_name(entry);
    folder_cleaned
        || clean(name) != name
        || u16_at(entry, 0) != default_mode
        || u32_at(entry, 0x20) != 0
}

/// A metadata file in Romp's form: the loaded entry with its length set and cluster cleared.
pub fn canonical_entry(entry: &[u8], length: u32) -> Vec<u8> {
    let mut out = entry.to_vec();
    out[4..8].copy_from_slice(&length.to_le_bytes());
    out[16..20].fill(0);
    out
}

/// A save folder's files with their metadata in Romp's form: each metadata file PCSX2 would
/// keep as a canonical entry, and those it would not, or that describe no file of the save,
/// left out.
pub fn canonical(folder: &str, files: &Files) -> Files {
    let mut out: Files = files
        .iter()
        .filter(|(key, _)| !is_meta(key))
        .map(|(k, v)| (k.clone(), v.clone()))
        .collect();
    let data_count = out.len() as u32;
    let mut folder_cleaned = false;
    if let Some(raw) = files.get(DIR_META) {
        let entry = loaded(raw, folder);
        folder_cleaned = clean(real_name(&entry)) != real_name(&entry);
        if is_nonstandard(&entry, DIR_MODE, false) {
            out.insert(DIR_META.into(), canonical_entry(&entry, 2 + data_count));
        }
    }
    for (key, raw) in files {
        let Some(file) = key.strip_prefix("_pcsx2_meta/") else {
            continue;
        };
        let Some(data) = files.get(file).filter(|_| !is_meta(file)) else {
            continue;
        };
        let entry = loaded(raw, file);
        if is_nonstandard(&entry, FILE_MODE, folder_cleaned) {
            out.insert(key.clone(), canonical_entry(&entry, data.len() as u32));
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(name: &[u8], mode: u16, attr: u32, length: u32, cluster: u32) -> Vec<u8> {
        let mut raw = vec![0u8; ENTRY];
        raw[0..2].copy_from_slice(&mode.to_le_bytes());
        raw[4..8].copy_from_slice(&length.to_le_bytes());
        raw[8..16].copy_from_slice(&[0, 40, 25, 1, 5, 2, 231, 7]);
        raw[16..20].copy_from_slice(&cluster.to_le_bytes());
        raw[0x20..0x24].copy_from_slice(&attr.to_le_bytes());
        raw[NAME_AT..NAME_AT + name.len()].copy_from_slice(name);
        raw
    }

    #[test]
    fn names_are_cleaned_as_pcsx2_cleans_them() {
        assert_eq!(clean(b"BASLUS-20314-TS2-OPT"), b"BASLUS-20314-TS2-OPT");
        assert_eq!(clean(b"A:B<C>|\"%\\"), b"A_B_C_____");
        assert_eq!(clean(b"save. ."), b"save___");
        assert_eq!(clean(b"a.b"), b"a.b");
    }

    #[test]
    fn metadata_is_kept_only_where_pcsx2_keeps_it_and_in_one_form() {
        let data = [("icon.sys", 964usize), ("save", 3)];
        let mut files: Files = data
            .iter()
            .map(|(n, len)| ((*n).to_string(), vec![1u8; *len]))
            .collect();
        // A copy-protected folder, as PCSX2 wrote it on the real card: its cluster there, 750.
        let protected = entry(b"BASLUS-20314-TS2-OPT", 0x842F, 0, 5, 750);
        files.insert(DIR_META.into(), protected.clone());
        // A file whose entry is the default is no metadata at all; one that is not stays.
        files.insert(
            file_meta_key("icon.sys"),
            entry(b"icon.sys", FILE_MODE, 0, 1, 9),
        );
        files.insert(file_meta_key("save"), entry(b"save", FILE_MODE, 0x10, 1, 9));
        files.insert(file_meta_key("gone"), entry(b"gone", 0x8417, 0, 1, 9));
        let canon = canonical("BASLUS-20314-TS2-OPT", &files);
        let dir = &canon[DIR_META];
        assert_eq!(dir.len(), ENTRY);
        assert_eq!(u16_at(dir, 0), 0x842F);
        assert_eq!(u32_at(dir, 4), 4, "two dot entries and the two files");
        assert_eq!(u32_at(dir, 16), 0, "the cluster on one card is cleared");
        assert_eq!(&dir[8..16], &protected[8..16], "the dates stay");
        assert!(!canon.contains_key(&file_meta_key("icon.sys")));
        assert!(!canon.contains_key(&file_meta_key("gone")));
        assert_eq!(u32_at(&canon[&file_meta_key("save")], 4), 3);
        let again = canonical("BASLUS-20314-TS2-OPT", &canon);
        assert_eq!(again, canon, "canonical is a fixed point");
    }

    #[test]
    fn a_cleaned_folder_name_keeps_metadata_for_every_file() {
        let mut files = Files::from([("f".to_string(), b"x".to_vec())]);
        files.insert(DIR_META.into(), entry(b"A:B", DIR_MODE, 0, 3, 1));
        files.insert(file_meta_key("f"), entry(b"f", FILE_MODE, 0, 1, 1));
        let canon = canonical("A_B", &files);
        assert_eq!(real_name(&canon[DIR_META]), b"A:B");
        assert!(canon.contains_key(&file_meta_key("f")));
    }

    #[test]
    fn a_short_metadata_file_takes_the_host_name() {
        let raw = entry(b"ignored", 0x842F, 0, 0, 0)[..0x20].to_vec();
        let loaded = loaded(&raw, "BASLUS-1");
        assert_eq!(real_name(&loaded), b"BASLUS-1");
        assert_eq!(u16_at(&loaded, 0), 0x842F);
    }
}
