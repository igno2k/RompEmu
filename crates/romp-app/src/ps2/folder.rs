//! A PCSX2 folder memory card: a directory holding `_pcsx2_superblock` and one folder per save.
//!
//! ARMSX2 picks the card type by what is at the card's path, a directory being a folder card
//! (`FileMcd_SetType`, pcsx2/SIO/Memcard/MemoryCardFile.cpp), and reads the card formatted
//! only when `_pcsx2_superblock` holds a whole formatted first block
//! (`FolderMemoryCard::LoadMemoryCardData`). Everything else on the card is files, so a pull
//! is folders moved into place and a push is folders read, byte for byte.
//!
//! A pull is back up, stage, swap. The game's folders are copied to the backup first, and
//! nothing changes if that fails. The new folders are written beside the card, not inside it,
//! where the emulator would list a half-written one, then renamed into place one by one, the
//! old one aside first; if any rename fails, every step already taken is undone.

use super::archive;
use super::card::{self, Card, SUPERBLOCK_BLOCK};
use super::rule::{is_card_file, UnitRule};
use super::{Files, Unit};
use std::io;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

pub const SUPERBLOCK: &str = "_pcsx2_superblock";

/// Makes a folder for the backups of one change, on first use.
pub type BackupDir<'a> = &'a mut dyn FnMut() -> io::Result<PathBuf>;

/// What `prepare` found or did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Prepared {
    Ready,
    Created,
    /// A folder card without a formatted superblock, which ARMSX2 would show as unformatted.
    Formatted,
    /// A memory card image turned into a folder card, the image kept in `backup`.
    Migrated {
        backup: PathBuf,
        folders: usize,
    },
}

/// The first block of a freshly formatted 8 MB card.
pub fn fresh_superblock() -> Vec<u8> {
    let mut block = vec![0xFF; SUPERBLOCK_BLOCK];
    block[..card::PAGE].copy_from_slice(&card::formatted_superblock_page());
    block
}

fn is_formatted(superblock: &[u8]) -> bool {
    superblock.len() >= SUPERBLOCK_BLOCK && superblock.starts_with(card::MAGIC)
}

fn sibling(card: &Path, what: &str) -> PathBuf {
    let name = card
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    card.with_file_name(format!(".{name}.romp-{what}"))
}

fn remove_any(path: &Path) -> io::Result<()> {
    match std::fs::symlink_metadata(path) {
        Ok(m) if m.is_dir() => std::fs::remove_dir_all(path),
        Ok(_) => std::fs::remove_file(path),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(e),
    }
}

/// Removes a staging folder a failed step left, saying so where even that fails.
fn discard(path: &Path) {
    if let Err(e) = remove_any(path) {
        tracing::warn!("could not remove {}: {e}", path.display());
    }
}

fn write_atomically(path: &Path, bytes: &[u8]) -> io::Result<()> {
    let tmp = path.with_extension("romp-tmp");
    std::fs::write(&tmp, bytes)?;
    std::fs::rename(&tmp, path)
}

fn io_error(what: &str, path: &Path) -> impl Fn(io::Error) -> String {
    let (what, path) = (what.to_string(), path.display().to_string());
    move |e| format!("could not {what} {path}: {e}")
}

fn write_folder(dir: &Path, files: &Files, modified: Option<i64>) -> io::Result<()> {
    std::fs::create_dir_all(dir)?;
    for (name, data) in files {
        let path = dir.join(name);
        std::fs::write(&path, data)?;
        if let Some(secs) = modified.and_then(|s| u64::try_from(s).ok()) {
            let when = SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(secs);
            let set = std::fs::File::options()
                .write(true)
                .open(&path)
                .and_then(|f| f.set_modified(when));
            if let Err(e) = set {
                tracing::warn!("could not date {}: {e}", path.display());
            }
        }
    }
    Ok(())
}

/// Gets the folder card at `card` ready for ARMSX2: made where there is none, so ARMSX2 does
/// not create an image in its place; given a formatted superblock where it has none; and, once,
/// converted from the memory card image Romp kept before, with the image backed up first.
pub fn prepare(card_path: &Path, backup: BackupDir) -> Result<Prepared, String> {
    match std::fs::metadata(card_path) {
        Ok(m) if m.is_dir() => {
            let path = card_path.join(SUPERBLOCK);
            let current = match std::fs::read(&path) {
                Ok(bytes) => Some(bytes),
                Err(e) if e.kind() == io::ErrorKind::NotFound => None,
                Err(e) => return Err(io_error("read", &path)(e)),
            };
            if current.as_deref().is_some_and(is_formatted) {
                return Ok(Prepared::Ready);
            }
            if let Some(old) = current.filter(|b| !b.is_empty()) {
                let kept = backup().map_err(io_error("back up", &path))?;
                std::fs::write(kept.join(SUPERBLOCK), old).map_err(io_error("back up", &path))?;
            }
            write_atomically(&path, &fresh_superblock()).map_err(io_error("write", &path))?;
            Ok(Prepared::Formatted)
        }
        Ok(_) => migrate(card_path, backup),
        Err(e) if e.kind() == io::ErrorKind::NotFound => {
            let staging = sibling(card_path, "new");
            let create = || -> io::Result<()> {
                remove_any(&staging)?;
                std::fs::create_dir_all(&staging)?;
                std::fs::write(staging.join(SUPERBLOCK), fresh_superblock())?;
                std::fs::rename(&staging, card_path)
            };
            create().map_err(io_error("create the memory card", card_path))?;
            Ok(Prepared::Created)
        }
        Err(e) => Err(io_error("read", card_path)(e)),
    }
}

/// Turns a memory card image into a folder card holding every folder the image held.
fn migrate(card_path: &Path, backup: BackupDir) -> Result<Prepared, String> {
    let bytes = std::fs::read(card_path).map_err(io_error("read", card_path))?;
    let unreadable = |e: card::CardError| {
        format!(
            "the memory card {} can't be read ({e}), so it was left as it is",
            card_path.display()
        )
    };
    let (superblock, folders) = if card::is_blank(&bytes) {
        (fresh_superblock(), Vec::new())
    } else if archive::is_zip(&bytes) {
        // What an earlier Romp wrote when it put a server's save zip where the card goes:
        // no card at all. It is kept in the backup, and the game's saves come down again.
        tracing::warn!(
            "{} holds a save zip rather than a memory card; it goes to the backup and the \
             game starts with an empty folder card",
            card_path.display()
        );
        (fresh_superblock(), Vec::new())
    } else {
        let image = Card::parse(bytes).map_err(unreadable)?;
        let mut folders = Vec::new();
        for folder in image.folders().map_err(unreadable)? {
            if !card::valid_name(folder.name()) {
                return Err(unreadable(card::CardError(format!(
                    "{:?} can't be a folder on this computer",
                    folder.name()
                ))));
            }
            let files = image.files(&folder).map_err(unreadable)?;
            folders.push((folder.name().to_string(), files, folder.modified()));
        }
        (image.superblock_block(), folders)
    };
    let kept = backup().map_err(io_error("back up", card_path))?;
    let name = card_path.file_name().unwrap_or_default();
    std::fs::copy(card_path, kept.join(name)).map_err(io_error("back up", card_path))?;

    let staging = sibling(card_path, "new");
    let build = || -> io::Result<()> {
        remove_any(&staging)?;
        std::fs::create_dir_all(&staging)?;
        std::fs::write(staging.join(SUPERBLOCK), &superblock)?;
        for (name, files, modified) in &folders {
            write_folder(&staging.join(name), files, *modified)?;
        }
        Ok(())
    };
    if let Err(e) = build() {
        discard(&staging);
        return Err(io_error("convert", card_path)(e));
    }
    let expected: Unit = folders
        .iter()
        .map(|(name, files, _)| (name.clone(), files.clone()))
        .collect();
    if read_folders(&staging, &|_| true).map(|(unit, _)| unit) != Ok(expected) {
        discard(&staging);
        return Err(format!(
            "the memory card {} did not read back after converting it, so it was left as it is",
            card_path.display()
        ));
    }
    let aside = sibling(card_path, "old");
    let swap = || -> io::Result<()> {
        remove_any(&aside)?;
        std::fs::rename(card_path, &aside)?;
        if let Err(e) = std::fs::rename(&staging, card_path) {
            if let Err(back) = std::fs::rename(&aside, card_path) {
                tracing::error!(
                    "could not put the memory card back from {}: {back}",
                    aside.display()
                );
            }
            return Err(e);
        }
        Ok(())
    };
    swap().map_err(io_error("convert", card_path))?;
    discard(&aside);
    Ok(Prepared::Migrated {
        backup: kept,
        folders: folders.len(),
    })
}

/// The folders of a folder card `owns` claims, with their files, and when the newest of them
/// changed. PCSX2's `_pcsx2_*` files are not part of any save and are left out.
pub fn read_folders(
    card_path: &Path,
    owns: &dyn Fn(&str) -> bool,
) -> Result<(Unit, Option<SystemTime>), String> {
    let mut unit = Unit::new();
    let mut newest: Option<SystemTime> = None;
    let mut note = |m: &std::fs::Metadata| {
        if let Ok(t) = m.modified() {
            newest = Some(newest.map_or(t, |n| n.max(t)));
        }
    };
    let entries = std::fs::read_dir(card_path).map_err(io_error("read", card_path))?;
    for entry in entries {
        let entry = entry.map_err(io_error("read", card_path))?;
        let name = entry.file_name().to_string_lossy().into_owned();
        let meta = std::fs::metadata(entry.path()).map_err(io_error("read", &entry.path()))?;
        if is_card_file(&name) || !meta.is_dir() || !owns(&name) {
            continue;
        }
        if !card::valid_name(&name) {
            return Err(format!(
                "the save folder {name:?} can't go into a PS2 save zip"
            ));
        }
        note(&meta);
        let mut files = Files::new();
        for file in std::fs::read_dir(entry.path()).map_err(io_error("read", &entry.path()))? {
            let file = file.map_err(io_error("read", &entry.path()))?;
            let file_name = file.file_name().to_string_lossy().into_owned();
            if is_card_file(&file_name) {
                continue;
            }
            let meta = std::fs::metadata(file.path()).map_err(io_error("read", &file.path()))?;
            if meta.is_dir() {
                return Err(format!(
                    "{name}/{file_name} is a folder; PS2 saves have none"
                ));
            }
            if !card::valid_name(&file_name) {
                return Err(format!("{name}/{file_name:?} can't go into a PS2 save zip"));
            }
            note(&meta);
            let data = std::fs::read(file.path()).map_err(io_error("read", &file.path()))?;
            files.insert(file_name, data);
        }
        unit.insert(name, files);
    }
    Ok((unit, newest))
}

/// A folder, or a file in one folder, named twice apart from case.
fn case_clash(unit: &Unit) -> Option<String> {
    let mut folders = std::collections::HashSet::new();
    for (name, files) in unit {
        if !folders.insert(name.to_ascii_lowercase()) {
            return Some(name.clone());
        }
        let mut seen = std::collections::HashSet::new();
        if let Some(file) = files.keys().find(|f| !seen.insert(f.to_ascii_lowercase())) {
            return Some(format!("{name}/{file}"));
        }
    }
    None
}

fn copy_folder(from: &Path, to: &Path) -> io::Result<()> {
    std::fs::create_dir_all(to)?;
    for entry in std::fs::read_dir(from)? {
        let entry = entry?;
        if entry.file_type()?.is_dir() {
            copy_folder(&entry.path(), &to.join(entry.file_name()))?;
        } else {
            std::fs::copy(entry.path(), to.join(entry.file_name()))?;
        }
    }
    Ok(())
}

/// Replaces the game's folders on the card with `unit`, all or nothing: the game's folders the
/// unit no longer has are removed too, as the unit is the game's whole save. Every other
/// folder, and PCSX2's own files, are left alone. Returns the folders removed.
pub fn apply(
    card_path: &Path,
    unit: &Unit,
    rule: &UnitRule,
    backup: BackupDir,
) -> Result<Vec<String>, String> {
    apply_with(card_path, unit, rule, backup, &|from, to| {
        std::fs::rename(from, to)
    })
}

/// A rename, replaceable in a test to fail where a disk might.
type Move<'a> = &'a dyn Fn(&Path, &Path) -> io::Result<()>;

fn apply_with(
    card_path: &Path,
    unit: &Unit,
    rule: &UnitRule,
    backup: BackupDir,
    rename: Move,
) -> Result<Vec<String>, String> {
    if let Some(stray) = unit.keys().find(|n| !rule.owns(n) || !card::valid_name(n)) {
        return Err(format!("{stray} is not a save folder of {}", rule.key()));
    }
    if let Some(clash) = case_clash(unit) {
        return Err(format!(
            "the save holds {clash} twice, in different upper and lower case, which a Mac's \
             disk can't keep apart"
        ));
    }
    prepare(card_path, backup)?;
    let local: Vec<String> = read_folders(card_path, &|n| rule.owns(n))?
        .0
        .into_keys()
        .collect();
    if !local.is_empty() {
        let kept = backup().map_err(io_error("back up", card_path))?;
        let into = kept.join(card_path.file_name().unwrap_or_default());
        for name in &local {
            copy_folder(&card_path.join(name), &into.join(name))
                .map_err(io_error("back up", &card_path.join(name)))?;
        }
    }

    let staging = sibling(card_path, "pull");
    let aside = sibling(card_path, "aside");
    let stage = || -> io::Result<()> {
        remove_any(&staging)?;
        remove_any(&aside)?;
        std::fs::create_dir_all(&aside)?;
        for (name, files) in unit {
            write_folder(&staging.join(name), files, None)?;
        }
        Ok(())
    };
    if let Err(e) = stage() {
        discard(&staging);
        return Err(io_error("stage the saves for", card_path)(e));
    }

    let leaving: Vec<&String> = local.iter().filter(|n| !unit.contains_key(*n)).collect();
    // Each step: the folder's name, whether an old one went aside, whether the new one is in.
    let mut done: Vec<(String, bool, bool)> = Vec::new();
    let mut swap = || -> io::Result<()> {
        for name in unit.keys() {
            let had = local.contains(name);
            if had {
                rename(&card_path.join(name), &aside.join(name))?;
            }
            done.push((name.clone(), had, false));
            rename(&staging.join(name), &card_path.join(name))?;
            done.last_mut().expect("a step").2 = true;
        }
        for name in &leaving {
            rename(&card_path.join(name), &aside.join(name))?;
            done.push(((*name).clone(), true, false));
        }
        Ok(())
    };
    let mut result = swap();
    // Read back through the same code a push reads with, and undo everything if it differs.
    if result.is_ok() {
        match read_folders(card_path, &|n| rule.owns(n)) {
            Ok((now, _)) if now == *unit => {}
            Ok(_) => result = Err(io::Error::other("the saves did not read back as written")),
            Err(e) => result = Err(io::Error::other(e)),
        }
    }
    let mut aside_disposable = true;
    if let Err(e) = &result {
        tracing::error!("could not swap pulled PS2 saves into place, undoing: {e}");
        for (name, had, placed) in done.iter().rev() {
            let undo = || -> io::Result<()> {
                if *placed {
                    remove_any(&card_path.join(name))?;
                }
                if *had {
                    std::fs::rename(aside.join(name), card_path.join(name))?;
                }
                Ok(())
            };
            if let Err(e) = undo() {
                aside_disposable = false;
                tracing::error!(
                    "could not put {name} back; the old copy is in {} and the backup: {e}",
                    aside.display()
                );
            }
        }
    }
    discard(&staging);
    if aside_disposable {
        discard(&aside);
    }
    if let Err(e) = result {
        return Err(io_error("swap the saves into", card_path)(e));
    }
    Ok(leaving.into_iter().cloned().collect())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ps2::archive::tests::unit;
    use crate::ps2::GameDb;

    fn rule() -> UnitRule {
        UnitRule::new("SLUS-20152", &GameDb::empty()).unwrap()
    }

    struct Backups {
        root: PathBuf,
        made: Vec<PathBuf>,
    }

    impl Backups {
        fn new(root: &Path) -> Self {
            Backups {
                root: root.to_path_buf(),
                made: Vec::new(),
            }
        }

        fn make(&mut self) -> io::Result<PathBuf> {
            let dir = self.root.join(format!("b{}", self.made.len()));
            std::fs::create_dir_all(&dir)?;
            self.made.push(dir.clone());
            Ok(dir)
        }
    }

    fn tree(dir: &Path) -> Vec<(String, Vec<u8>)> {
        let mut out = Vec::new();
        let mut stack = vec![dir.to_path_buf()];
        while let Some(d) = stack.pop() {
            for e in std::fs::read_dir(&d).unwrap().flatten() {
                if e.file_type().unwrap().is_dir() {
                    stack.push(e.path());
                } else {
                    let rel = e
                        .path()
                        .strip_prefix(dir)
                        .unwrap()
                        .to_string_lossy()
                        .into_owned();
                    out.push((rel, std::fs::read(e.path()).unwrap()));
                }
            }
        }
        out.sort();
        out
    }

    fn plant(card: &Path, entries: &[(&str, &[u8])]) {
        for (name, data) in entries {
            let path = card.join(name);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, data).unwrap();
        }
    }

    #[test]
    fn a_missing_card_is_created_formatted() {
        let dir = tempfile::tempdir().unwrap();
        let card = dir.path().join("memcards/Mcd001.ps2");
        std::fs::create_dir_all(card.parent().unwrap()).unwrap();
        let mut backups = Backups::new(&dir.path().join("backup"));
        assert_eq!(
            prepare(&card, &mut || backups.make()).unwrap(),
            Prepared::Created
        );
        let superblock = std::fs::read(card.join(SUPERBLOCK)).unwrap();
        assert_eq!(superblock.len(), SUPERBLOCK_BLOCK);
        assert_eq!(
            superblock[0x16], 0x6F,
            "what PCSX2 checks for a formatted card"
        );
        assert_eq!(
            prepare(&card, &mut || backups.make()).unwrap(),
            Prepared::Ready
        );
        assert!(backups.made.is_empty());
    }

    #[test]
    fn an_unformatted_superblock_is_replaced_and_kept() {
        // Argosy and PCSX2 create a folder card with an empty superblock.
        let dir = tempfile::tempdir().unwrap();
        let card = dir.path().join("Mcd001.ps2");
        plant(&card, &[(SUPERBLOCK, b""), ("BASLUS-20152AC04/save", b"s")]);
        let mut backups = Backups::new(&dir.path().join("backup"));
        assert_eq!(
            prepare(&card, &mut || backups.make()).unwrap(),
            Prepared::Formatted
        );
        assert!(is_formatted(&std::fs::read(card.join(SUPERBLOCK)).unwrap()));
        assert!(backups.made.is_empty(), "an empty file needs no backup");
        std::fs::write(card.join(SUPERBLOCK), b"short").unwrap();
        assert_eq!(
            prepare(&card, &mut || backups.make()).unwrap(),
            Prepared::Formatted
        );
        assert_eq!(
            std::fs::read(backups.made[0].join(SUPERBLOCK)).unwrap(),
            b"short"
        );
        assert_eq!(
            std::fs::read(card.join("BASLUS-20152AC04/save")).unwrap(),
            b"s"
        );
    }

    #[test]
    fn a_memory_card_image_is_converted_once_with_a_backup() {
        let dir = tempfile::tempdir().unwrap();
        let card = dir.path().join("Mcd001.ps2");
        let when = card::tod(1_790_769_600);
        let saves = unit(&[
            ("BASLUS-20152AC04", "icon.sys", b"icon"),
            ("BASLUS-20152AC04", "save", &[3u8; 2500]),
            ("BADATA-SYSTEM", "history", b"h"),
        ]);
        let image = card::apply_unit(None, &saves, &|_| true, when).unwrap();
        std::fs::write(&card, &image).unwrap();
        let mut backups = Backups::new(&dir.path().join("backup"));

        let prepared = prepare(&card, &mut || backups.make()).unwrap();
        assert_eq!(
            prepared,
            Prepared::Migrated {
                backup: backups.made[0].clone(),
                folders: 2
            }
        );
        assert_eq!(
            std::fs::read(backups.made[0].join("Mcd001.ps2")).unwrap(),
            image
        );
        assert!(card.is_dir());
        let superblock = std::fs::read(card.join(SUPERBLOCK)).unwrap();
        assert_eq!(superblock, Card::parse(image).unwrap().superblock_block());
        assert_eq!(read_folders(&card, &|_| true).unwrap().0, saves);
        let modified = std::fs::metadata(card.join("BASLUS-20152AC04/save"))
            .unwrap()
            .modified()
            .unwrap();
        assert_eq!(
            modified,
            SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(1_790_769_600)
        );
        assert_eq!(
            prepare(&card, &mut || backups.make()).unwrap(),
            Prepared::Ready
        );
        assert_eq!(backups.made.len(), 1);
        let leftovers: Vec<_> = std::fs::read_dir(dir.path())
            .unwrap()
            .flatten()
            .map(|e| e.file_name())
            .collect();
        assert_eq!(
            leftovers.len(),
            2,
            "the card and the backups, nothing staged: {leftovers:?}"
        );
    }

    #[test]
    fn an_unreadable_image_is_left_as_it_is() {
        let dir = tempfile::tempdir().unwrap();
        let card = dir.path().join("Mcd001.ps2");
        std::fs::write(&card, b"garbage, neither a card nor a zip").unwrap();
        let mut backups = Backups::new(&dir.path().join("backup"));
        let err = prepare(&card, &mut || backups.make()).unwrap_err();
        assert!(err.contains("left as it is"), "{err}");
        assert_eq!(
            std::fs::read(&card).unwrap(),
            b"garbage, neither a card nor a zip"
        );
        assert!(backups.made.is_empty());
    }

    #[test]
    fn a_zip_where_the_card_goes_is_kept_and_replaced_by_an_empty_card() {
        let dir = tempfile::tempdir().unwrap();
        let card = dir.path().join("Mcd001.ps2");
        let zip = archive::build(&unit(&[("BASLUS-20152AC04", "save", b"s")])).unwrap();
        std::fs::write(&card, &zip).unwrap();
        let mut backups = Backups::new(&dir.path().join("backup"));
        assert!(matches!(
            prepare(&card, &mut || backups.make()).unwrap(),
            Prepared::Migrated { folders: 0, .. }
        ));
        assert_eq!(
            std::fs::read(backups.made[0].join("Mcd001.ps2")).unwrap(),
            zip
        );
        assert!(read_folders(&card, &|_| true).unwrap().0.is_empty());
    }

    #[test]
    fn names_that_differ_only_in_case_are_refused() {
        let dir = tempfile::tempdir().unwrap();
        let card = dir.path().join("Mcd001.ps2");
        let mut backups = Backups::new(&dir.path().join("backup"));
        for clash in [
            unit(&[
                ("BASLUS-20152AC04", "save", b"a"),
                ("baslus-20152ac04", "save", b"b"),
            ]),
            unit(&[
                ("BASLUS-20152AC04", "SAVE", b"a"),
                ("BASLUS-20152AC04", "save", b"b"),
            ]),
        ] {
            let err = apply(&card, &clash, &rule(), &mut || backups.make()).unwrap_err();
            assert!(err.contains("upper and lower case"), "{err}");
        }
        assert!(!card.exists());
    }

    #[test]
    fn a_pull_that_does_not_read_back_is_undone() {
        let dir = tempfile::tempdir().unwrap();
        let card = dir.path().join("Mcd001.ps2");
        std::fs::create_dir_all(&card).unwrap();
        std::fs::write(card.join(SUPERBLOCK), fresh_superblock()).unwrap();
        plant(&card, &[("BASLUS-20152AC04/save", b"old")]);
        let before = tree(&card);
        let incoming = unit(&[("BASLUS-20152AC04", "save", b"new")]);
        // A disk that lands the folder but not quite as it was written.
        let rename = |from: &Path, to: &Path| {
            std::fs::rename(from, to)?;
            if to.parent() == Some(card.as_path()) {
                std::fs::write(to.join("save"), b"garbled")?;
            }
            Ok(())
        };
        let mut backups = Backups::new(&dir.path().join("backup"));
        let err =
            apply_with(&card, &incoming, &rule(), &mut || backups.make(), &rename).unwrap_err();
        assert!(err.contains("did not read back"), "{err}");
        assert_eq!(tree(&card), before);
    }

    #[test]
    fn a_blank_image_becomes_an_empty_folder_card() {
        let dir = tempfile::tempdir().unwrap();
        let card = dir.path().join("Mcd001.ps2");
        std::fs::write(&card, vec![0xFF; card::IMAGE_SIZE]).unwrap();
        let mut backups = Backups::new(&dir.path().join("backup"));
        assert!(matches!(
            prepare(&card, &mut || backups.make()).unwrap(),
            Prepared::Migrated { folders: 0, .. }
        ));
        assert!(is_formatted(&std::fs::read(card.join(SUPERBLOCK)).unwrap()));
    }

    #[test]
    fn a_pull_replaces_only_the_games_folders() {
        let dir = tempfile::tempdir().unwrap();
        let card = dir.path().join("Mcd001.ps2");
        std::fs::create_dir_all(&card).unwrap();
        std::fs::write(card.join(SUPERBLOCK), fresh_superblock()).unwrap();
        plant(
            &card,
            &[
                ("_pcsx2_index", b"root index"),
                ("BASLUS-20152AC04/save", b"old"),
                ("BASLUS-20152AC04/_pcsx2_index", b"old index"),
                ("BASLUS-20152OLD/save", b"gone on the other device"),
                ("BASLUS-21693XX/save", b"another game"),
                ("BADATA-SYSTEM/history", b"the console's"),
            ],
        );
        let others_before: Vec<_> = tree(&card)
            .into_iter()
            .filter(|(n, _)| !n.starts_with("BASLUS-20152"))
            .collect();
        let mut backups = Backups::new(&dir.path().join("backup"));
        let incoming = unit(&[
            ("BASLUS-20152AC04", "save", b"new"),
            ("BASLUS-20152SYS", "icon.sys", b"icon"),
        ]);
        let removed = apply(&card, &incoming, &rule(), &mut || backups.make()).unwrap();
        assert_eq!(removed, ["BASLUS-20152OLD"]);
        assert_eq!(
            read_folders(&card, &|n| rule().owns(n)).unwrap().0,
            incoming
        );
        let others_after: Vec<_> = tree(&card)
            .into_iter()
            .filter(|(n, _)| !n.starts_with("BASLUS-20152"))
            .collect();
        assert_eq!(others_after, others_before);
        let kept = backups.made[0].join("Mcd001.ps2");
        assert_eq!(
            std::fs::read(kept.join("BASLUS-20152AC04/save")).unwrap(),
            b"old"
        );
        assert_eq!(
            std::fs::read(kept.join("BASLUS-20152OLD/save")).unwrap(),
            b"gone on the other device"
        );
        let names: Vec<_> = std::fs::read_dir(dir.path())
            .unwrap()
            .flatten()
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .collect();
        assert!(!names.iter().any(|n| n.contains("romp-")), "{names:?}");
    }

    #[test]
    fn a_pull_that_cannot_back_up_changes_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let card = dir.path().join("Mcd001.ps2");
        std::fs::create_dir_all(&card).unwrap();
        std::fs::write(card.join(SUPERBLOCK), fresh_superblock()).unwrap();
        plant(&card, &[("BASLUS-20152AC04/save", b"old")]);
        let before = tree(&card);
        let incoming = unit(&[("BASLUS-20152AC04", "save", b"new")]);
        let err = apply(&card, &incoming, &rule(), &mut || {
            Err(io::Error::other("disk full"))
        })
        .unwrap_err();
        assert!(err.contains("disk full"), "{err}");
        assert_eq!(tree(&card), before);
    }

    #[test]
    fn a_folder_that_is_not_the_games_is_never_written() {
        let dir = tempfile::tempdir().unwrap();
        let card = dir.path().join("Mcd001.ps2");
        let mut backups = Backups::new(&dir.path().join("backup"));
        let incoming = unit(&[("BADATA-SYSTEM", "history", b"x")]);
        assert!(apply(&card, &incoming, &rule(), &mut || backups.make()).is_err());
        assert!(!card.exists());
    }

    #[test]
    fn a_failed_swap_puts_everything_back() {
        let dir = tempfile::tempdir().unwrap();
        let card = dir.path().join("Mcd001.ps2");
        std::fs::create_dir_all(&card).unwrap();
        std::fs::write(card.join(SUPERBLOCK), fresh_superblock()).unwrap();
        plant(
            &card,
            &[
                ("BASLUS-20152AC04/save", b"old"),
                ("BASLUS-20152SYS/s", b"sys"),
                ("BASLUS-20152X/gone", b"x"),
            ],
        );
        let before = tree(&card);
        let incoming = unit(&[
            ("BASLUS-20152AC04", "save", b"new"),
            ("BASLUS-20152SYS", "s", b"new"),
        ]);
        for fail_at in 1..=5 {
            let calls = std::cell::Cell::new(0);
            let rename = |from: &Path, to: &Path| {
                calls.set(calls.get() + 1);
                if calls.get() == fail_at {
                    return Err(io::Error::other("the disk went away"));
                }
                std::fs::rename(from, to)
            };
            let mut backups = Backups::new(&dir.path().join(format!("backup{fail_at}")));
            let err =
                apply_with(&card, &incoming, &rule(), &mut || backups.make(), &rename).unwrap_err();
            assert!(err.contains("the disk went away"), "{err}");
            assert_eq!(tree(&card), before, "failing rename {fail_at}");
            let names: Vec<_> = std::fs::read_dir(dir.path())
                .unwrap()
                .flatten()
                .map(|e| e.file_name().to_string_lossy().into_owned())
                .collect();
            assert!(!names.iter().any(|n| n.contains("romp-")), "{names:?}");
        }
    }
}
