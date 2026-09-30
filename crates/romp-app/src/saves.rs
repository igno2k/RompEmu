use crate::cores::{CoreChoices, CoreInfo};
use crate::ps2;
use crate::romm::client::{Client, Error, SaveUpload};
use crate::romm::types::ClientSave;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

pub fn md5_hex(bytes: &[u8]) -> String {
    use md5::{Digest, Md5};
    Md5::digest(bytes)
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

pub fn file_iso_mtime(path: &Path) -> Option<String> {
    let modified = std::fs::metadata(path).ok()?.modified().ok()?;
    Some(crate::sync::iso_utc(modified))
}

pub const BACKUPS_KEPT: usize = 10;

/// The rotated backups in a save folder, oldest first by their timestamp names.
fn stamped_backups(dir: &Path) -> Vec<(u128, PathBuf)> {
    let Ok(entries) = std::fs::read_dir(dir.join("backup")) else {
        return Vec::new();
    };
    let mut stamped: Vec<(u128, PathBuf)> = entries
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.is_dir())
        .filter_map(|p| {
            let name = p.file_name()?.to_str()?;
            let stamp = name
                .chars()
                .all(|c| c.is_ascii_digit())
                .then(|| name.parse::<u128>().ok())??;
            Some((stamp, p))
        })
        .collect();
    stamped.sort();
    stamped
}

fn new_backup_dir(dir: &Path) -> io::Result<PathBuf> {
    let root = dir.join("backup");
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis();
    // Always newer than the newest backup, even when several are made within a millisecond
    // and pruning freed an older name, so rotation drops the oldest and never the new one.
    let newest = stamped_backups(dir)
        .last()
        .map_or(0, |(stamp, _)| stamp + 1);
    let mut stamp = now.max(newest);
    while root.join(format!("{stamp:015}")).exists() {
        stamp += 1;
    }
    let target = root.join(format!("{stamp:015}"));
    std::fs::create_dir_all(&target)?;
    Ok(target)
}

fn prune_backups(dir: &Path) {
    let stamped = stamped_backups(dir);
    let excess = stamped.len().saturating_sub(BACKUPS_KEPT);
    for (_, old) in &stamped[..excess] {
        let _ = std::fs::remove_dir_all(old);
    }
}

pub fn backup_bytes(dir: &Path, name: &str, bytes: &[u8]) -> io::Result<PathBuf> {
    let target = new_backup_dir(dir)?.join(name);
    std::fs::write(&target, bytes)?;
    prune_backups(dir);
    Ok(target)
}

pub fn backup(dir: &Path, file: &str) -> io::Result<Option<PathBuf>> {
    let source = dir.join(file);
    if !source.exists() {
        return Ok(None);
    }
    let name = Path::new(file).file_name().unwrap_or(file.as_ref());
    let target = new_backup_dir(dir)?.join(name);
    std::fs::copy(&source, &target)?;
    prune_backups(dir);
    Ok(Some(target))
}

fn newest(saves: Vec<crate::romm::types::RemoteSave>) -> Option<crate::romm::types::RemoteSave> {
    saves
        .into_iter()
        .max_by_key(|s| crate::sync::parse_iso(&s.updated_at).unwrap_or(i64::MIN))
}

pub const AUTO_STATE: &str = "auto.state";

pub fn set_aside_auto_state(dir: &Path) -> io::Result<()> {
    let source = dir.join(AUTO_STATE);
    if !source.exists() {
        return Ok(());
    }
    std::fs::rename(&source, new_backup_dir(dir)?.join("auto.state.failed"))?;
    prune_backups(dir);
    Ok(())
}

pub fn replace_file(path: &Path, bytes: &[u8]) -> io::Result<()> {
    static COUNTER: AtomicU32 = AtomicU32::new(0);
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let tmp = path.with_extension(format!(
        "{}-{}.tmp",
        std::process::id(),
        COUNTER.fetch_add(1, Ordering::Relaxed)
    ));
    std::fs::write(&tmp, bytes)?;
    std::fs::rename(&tmp, path)
}

fn clean(part: &str) -> String {
    part.chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                c
            } else {
                '-'
            }
        })
        .collect()
}

pub fn state_remote_name(slot: &str, core_id: &str, core_version: &str) -> String {
    format!(
        "{}.{}.{}.state",
        clean(slot),
        clean(core_id),
        clean(core_version)
    )
}

pub const SRAM_FILE: &str = "game.srm";
pub const SRAM_SLOT: &str = "autosave";
const ARMSX2_CARD: &str = "pcsx2/memcards/Mcd001.ps2";
// Both PS2 emulators write the same memory card format, so a card follows the game between them.
const PS2_CARD_EMULATOR: &str = "pcsx2";

#[derive(Debug, Clone)]
pub struct GameSaves {
    pub rom_id: i64,
    pub dir: PathBuf,
    pub title: String,
    pub emulator: String,
    /// The in-game save, relative to `dir`: save RAM, or a PS2 memory card.
    pub save_file: String,
    pub save_emulator: String,
    /// For a PS2 game, how its memory card is synced: as a zip of the game's save folders.
    pub ps2: Option<Ps2Save>,
}

/// How a PS2 game's saves sync: a zip of its save folders, read from and written to the card
/// its emulator keeps. See [`crate::ps2`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Ps2Save {
    pub layout: ps2::Layout,
    /// RomM's name for the game's saves, its serial, where RomM read one.
    pub save_target: Option<String>,
    /// The name the zip is uploaded under: the ROM's, as the RomMix fork names it.
    pub zip_name: String,
    /// The libretro system folder, where an installed GameDB is looked for.
    pub system_dir: Option<PathBuf>,
}

impl Ps2Save {
    fn new(
        core_id: &str,
        title: &str,
        rom: Option<&Path>,
        save_target: Option<&str>,
    ) -> Option<Self> {
        let layout = match core_id {
            "armsx2" => ps2::Layout::Folder,
            "pcsx2" => ps2::Layout::Image,
            _ => return None,
        };
        let stem = rom
            .and_then(Path::file_stem)
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_else(|| clean_title(title));
        Some(Ps2Save {
            layout,
            save_target: save_target
                .map(str::trim)
                .filter(|t| !t.is_empty())
                .map(String::from),
            zip_name: format!("{stem}.zip"),
            system_dir: None,
        })
    }

    /// Which of the card's folders are the game's, or why that can't be told.
    fn rule(&self) -> Result<ps2::UnitRule, Error> {
        let Some(key) = self.save_target.as_deref() else {
            return Err(Error::Refused(
                "RomM hasn't read this game's serial, so Romp can't tell its PS2 saves from \
                 other games'; rescan the game on the server"
                    .into(),
            ));
        };
        let gamedb = ps2::GameDb::load(self.system_dir.as_deref());
        ps2::UnitRule::new(key, &gamedb).ok_or_else(|| {
            Error::Refused(format!(
                "RomM names this game's saves {key}, which is not a PS2 serial"
            ))
        })
    }
}

/// The file a core keeps its in-game saves in, relative to the game's save folder, and the
/// emulator name it syncs under.
pub fn in_game_save(core_id: &str, dir: &Path, rom: Option<&Path>) -> (String, String) {
    match core_id {
        "armsx2" => (ARMSX2_CARD.into(), PS2_CARD_EMULATOR.into()),
        // PCSX2 names each game's card after the file it was started with.
        "pcsx2" => {
            let card = memory_cards(dir)
                .into_iter()
                .next()
                .or_else(|| {
                    rom?.file_stem()
                        .map(|s| format!("{}.ps2", s.to_string_lossy()))
                })
                .unwrap_or_else(|| "game.ps2".into());
            (card, PS2_CARD_EMULATOR.into())
        }
        _ => (SRAM_FILE.into(), core_id.into()),
    }
}

/// A game's saves as the emulator `core` keeps and syncs them. `save_target` is RomM's name
/// for them, which tells a PS2 game's folders on a memory card from other games'.
pub fn game_saves(
    core: &CoreInfo,
    rom_id: i64,
    dir: PathBuf,
    title: &str,
    rom: Option<&Path>,
    save_target: Option<&str>,
) -> GameSaves {
    let (save_file, save_emulator) = in_game_save(core.id, &dir, rom);
    GameSaves {
        rom_id,
        dir,
        title: title.to_string(),
        emulator: core.id.to_string(),
        save_file,
        save_emulator,
        ps2: Ps2Save::new(core.id, title, rom, save_target),
    }
}

/// What starting a game involves: the emulator it plays with, the saves that emulator syncs,
/// and the emulator it was last played with when that was a different one.
pub struct Launch {
    pub core: &'static CoreInfo,
    pub saves: GameSaves,
    pub previous: Option<&'static CoreInfo>,
}

/// Plans a game's start from its system, the emulators chosen in Settings and the emulator the
/// game was last played with. The launch, its options and the saves it syncs all use `core`.
#[allow(clippy::too_many_arguments)]
pub fn plan_launch(
    platform_slug: &str,
    choices: &CoreChoices,
    last_core: Option<&str>,
    rom_id: i64,
    dir: PathBuf,
    title: &str,
    rom: Option<&Path>,
    save_target: Option<&str>,
) -> Option<Launch> {
    let core = crate::cores::core_for(platform_slug, choices)?;
    let previous = crate::cores::played_core(platform_slug, last_core).filter(|p| p.id != core.id);
    Some(Launch {
        core,
        saves: game_saves(core, rom_id, dir, title, rom, save_target),
        previous,
    })
}

/// Emulators whose save RAM is the same file, byte for byte, so a game's in-game save carries
/// over when it switches between them. Keep this to pairs checked against both cores' source:
/// Beetle PSX HW and SwanStation (memory card 1 as save RAM, "Libretro") both expose memory
/// card 1 as a raw 128 KiB PlayStation memory card.
const SAME_SAVE_RAM: [(&str, &str); 1] = [("mednafen_psx_hw", "swanstation")];

pub fn same_save_ram(a: &str, b: &str) -> bool {
    SAME_SAVE_RAM
        .iter()
        .any(|&(x, y)| (x, y) == (a, b) || (y, x) == (a, b))
}

/// Moves the save states, and unless `keep_save` the in-game save, another emulator left in a
/// game's save folder into a backup of their own, so the next emulator neither loads nor syncs
/// them under its name. The backup is named after that emulator and kept out of the backup
/// rotation.
pub fn set_aside_for_core_switch(
    dir: &Path,
    previous: &str,
    save_file: &str,
    keep_save: bool,
) -> io::Result<Option<PathBuf>> {
    let mut files: Vec<String> = Vec::new();
    if !keep_save {
        files.push(save_file.to_string());
    }
    files.extend(STATE_SLOTS.iter().map(|slot| format!("{slot}.state")));
    // A PS2 folder memory card is a folder, and moves aside the same way.
    files.retain(|f| dir.join(f).exists());
    if files.is_empty() {
        return Ok(None);
    }
    let stamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis();
    let root = dir.join("backup");
    let mut n = 0;
    let target = loop {
        let candidate = root.join(format!("{stamp:015}-{n}-{previous}"));
        if !candidate.exists() {
            break candidate;
        }
        n += 1;
    };
    for file in files {
        let destination = target.join(&file);
        if let Some(parent) = destination.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::rename(dir.join(&file), destination)?;
    }
    Ok(Some(target))
}

/// PS2 memory cards at the top of a save folder, newest first.
fn memory_cards(dir: &Path) -> Vec<String> {
    let mut cards: Vec<(std::time::SystemTime, String)> = std::fs::read_dir(dir)
        .into_iter()
        .flatten()
        .flatten()
        .filter_map(|e| {
            let name = e.file_name().to_str()?.to_string();
            let modified = e.metadata().ok()?.modified().ok()?;
            name.ends_with(".ps2").then_some((modified, name))
        })
        .collect();
    cards.sort_by(|a, b| b.cmp(a));
    cards.into_iter().map(|(_, name)| name).collect()
}

#[derive(Debug, Clone, PartialEq)]
pub struct SramConflict {
    pub save_id: Option<i64>,
    pub server_updated_at: Option<String>,
    pub local_updated_at: Option<String>,
}

#[derive(Debug, Clone, PartialEq)]
pub enum SramOutcome {
    InSync,
    Uploaded,
    Downloaded { backup: Option<PathBuf> },
    Conflict(SramConflict),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Keep {
    Local,
    Server,
}

fn clean_title(title: &str) -> String {
    let clean: String = title
        .chars()
        .map(|c| {
            if c.is_control() || "/\\:|*?\"<>+".contains(c) {
                '-'
            } else {
                c
            }
        })
        .collect();
    clean.trim().to_string()
}

/// The in-game save as it is sent to RomM: its bytes, their content hash, and when it changed.
struct LocalSave {
    bytes: Vec<u8>,
    hash: String,
    updated_at: Option<String>,
}

impl GameSaves {
    fn sram_path(&self) -> PathBuf {
        self.dir.join(&self.save_file)
    }

    fn remote_sram_name(&self) -> String {
        if let Some(ps2) = &self.ps2 {
            return ps2.zip_name.clone();
        }
        let extension = Path::new(&self.save_file)
            .extension()
            .and_then(|e| e.to_str())
            .unwrap_or("srm");
        format!("{}.{extension}", clean_title(&self.title))
    }

    /// The in-game save to send, or None where there is none. A PS2 game's is a zip of its
    /// save folders, never its memory card.
    fn local_save(&self) -> Result<Option<LocalSave>, Error> {
        let path = self.sram_path();
        let Some(ps2) = &self.ps2 else {
            return Ok(std::fs::read(&path).ok().map(|bytes| LocalSave {
                hash: md5_hex(&bytes),
                updated_at: file_iso_mtime(&path),
                bytes,
            }));
        };
        let rule = ps2.rule()?;
        let owns = |name: &str| rule.owns(name);
        let (unit, changed) = match ps2.layout {
            ps2::Layout::Folder => {
                if !path.exists() {
                    return Ok(None);
                }
                if !path.is_dir() {
                    return Err(Error::Refused(format!(
                        "{} is still a memory card image, not a folder card",
                        path.display()
                    )));
                }
                // Freshness is the game's own saves': a shared folder is another game's.
                ps2::folder::read_folders(&path, &owns, &|n| rule.is_own(n))
                    .map_err(Error::Refused)?
            }
            ps2::Layout::Image => {
                let Ok(bytes) = std::fs::read(&path) else {
                    return Ok(None);
                };
                if ps2::archive::is_zip(&bytes) {
                    // An earlier Romp wrote the server's save zip where the card goes. It is
                    // no card, and nothing to upload; a pull backs it up and replaces it.
                    tracing::warn!("{} holds a save zip, not a memory card", path.display());
                    return Ok(None);
                }
                let unit = ps2::card::read_unit(&bytes, &owns).map_err(|e| {
                    Error::Refused(format!(
                        "the memory card {} can't be read: {e}",
                        path.display()
                    ))
                })?;
                let changed = std::fs::metadata(&path).and_then(|m| m.modified()).ok();
                (unit, changed)
            }
        };
        // Shared folders alone are another game's saves, nothing of this one's to send.
        if !unit.keys().any(|n| rule.is_own(n)) {
            return Ok(None);
        }
        Ok(Some(LocalSave {
            bytes: ps2::archive::build(&unit).map_err(Error::Refused)?,
            hash: ps2::archive::content_hash(&unit),
            updated_at: changed.map(crate::sync::iso_utc),
        }))
    }
}

/// A backup folder of its own, kept out of the rotation, for a change made once.
fn kept_backup_dir(dir: &Path, what: &str) -> io::Result<PathBuf> {
    let stamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis();
    let root = dir.join("backup");
    let mut n = 0;
    loop {
        let candidate = root.join(format!("{stamp:015}-{n}-{what}"));
        if !candidate.exists() {
            std::fs::create_dir_all(&candidate)?;
            return Ok(candidate);
        }
        n += 1;
    }
}

/// Gets a game's in-game save ready for its emulator before it starts or syncs. For ARMSX2,
/// that is the game's PCSX2 folder memory card: made where there is none, so ARMSX2 doesn't
/// create an image instead, and converted once from the image Romp kept before, which is kept
/// in a backup of its own.
pub fn prepare_in_game_save(game: &GameSaves) -> Result<(), String> {
    let Some(ps2::Layout::Folder) = game.ps2.as_ref().map(|p| p.layout) else {
        return Ok(());
    };
    let card = game.sram_path();
    if let Some(parent) = card.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|e| format!("could not create {}: {e}", parent.display()))?;
    }
    let mut made: Option<PathBuf> = None;
    let mut backup = || -> io::Result<PathBuf> {
        if let Some(dir) = &made {
            return Ok(dir.clone());
        }
        let dir = kept_backup_dir(&game.dir, "before-folder-card")?;
        made = Some(dir.clone());
        Ok(dir)
    };
    match ps2::folder::prepare(&card, &mut backup)? {
        ps2::folder::Prepared::Ready => {}
        ps2::folder::Prepared::Migrated { backup, folders } => tracing::info!(
            "game {}: converted its PS2 memory card into a folder card with {folders} save \
             folders; the card is kept in {}",
            game.rom_id,
            backup.display()
        ),
        other => tracing::info!("game {}: PS2 folder memory card {other:?}", game.rom_id),
    }
    Ok(())
}

async fn upload_sram(
    client: &Client,
    device_id: &str,
    game: &GameSaves,
    bytes: Vec<u8>,
    session_id: Option<i64>,
    overwrite: bool,
) -> Result<(), Error> {
    if game.ps2.is_some() && !ps2::archive::is_zip(&bytes) {
        return Err(Error::Refused(
            "Romp uploads a PS2 save only as a zip of the game's save folders".into(),
        ));
    }
    let name = game.remote_sram_name();
    client
        .upload_save(SaveUpload {
            rom_id: game.rom_id,
            slot: SRAM_SLOT,
            emulator: &game.save_emulator,
            device_id,
            session_id,
            overwrite,
            file_name: &name,
            bytes,
        })
        .await
        .map(|_| ())
}

/// Puts a downloaded save where the emulator reads it, keeping a backup of what it replaces.
/// For a PS2 game, the download is unpacked into the game's folders on its memory card, and
/// refused, with nothing changed, when it is not a PS2 save of this game. `tag` is the
/// emulator RomM holds the save under, where known.
fn install_download(
    game: &GameSaves,
    bytes: &[u8],
    tag: Option<&str>,
) -> Result<SramOutcome, Error> {
    let io_err = |e: io::Error| Error::Decode(format!("could not write the save: {e}"));
    let Some(ps2) = &game.ps2 else {
        let backup = backup(&game.dir, &game.save_file).map_err(io_err)?;
        replace_file(&game.sram_path(), bytes).map_err(io_err)?;
        return Ok(SramOutcome::Downloaded { backup });
    };
    if !ps2::accepts_tag(tag) {
        return Err(Error::Refused(format!(
            "the server's save for this game was made with {}, not a PS2 emulator Romp can \
             load it into",
            tag.unwrap_or_default()
        )));
    }
    let rule = ps2.rule()?;
    let download = ps2::decode(bytes, &rule).map_err(Error::Refused)?;
    let path = game.sram_path();
    match ps2.layout {
        ps2::Layout::Folder => {
            prepare_in_game_save(game).map_err(Error::Refused)?;
            let mut made: Option<PathBuf> = None;
            let mut backup = || -> io::Result<PathBuf> {
                if let Some(dir) = &made {
                    return Ok(dir.clone());
                }
                let dir = new_backup_dir(&game.dir)?;
                made = Some(dir.clone());
                Ok(dir)
            };
            let removed =
                ps2::folder::apply(&path, &download, &rule, &mut backup).map_err(Error::Refused)?;
            if !removed.is_empty() {
                tracing::info!(
                    "game {}: removed save folders the server's save no longer has: {}",
                    game.rom_id,
                    removed.join(", ")
                );
            }
            prune_backups(&game.dir);
            Ok(SramOutcome::Downloaded {
                backup: made.map(|dir| dir.join(path.file_name().unwrap_or_default())),
            })
        }
        ps2::Layout::Image => {
            // A save zip an earlier Romp wrote where the card goes is no card: it is backed up
            // below and replaced by a freshly formatted one.
            let original = std::fs::read(&path)
                .ok()
                .filter(|bytes| !ps2::archive::is_zip(bytes));
            let now = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map_or(0, |d| d.as_secs() as i64);
            let image = ps2::card::apply_unit(
                original.as_deref(),
                &download.unit,
                &rule,
                ps2::card::tod(now),
            )
            .map_err(|e| {
                Error::Refused(format!(
                    "the save could not be written into the memory card {}, which was \
                         left as it is: {e}",
                    path.display()
                ))
            })?;
            let backup = backup(&game.dir, &game.save_file).map_err(io_err)?;
            replace_file(&path, &image).map_err(io_err)?;
            Ok(SramOutcome::Downloaded { backup })
        }
    }
}

pub async fn sync_sram(
    client: &Client,
    device_id: &str,
    game: &GameSaves,
) -> Result<SramOutcome, Error> {
    prepare_in_game_save(game).map_err(Error::Refused)?;
    let local = game.local_save()?;
    let local_updated_at = local.as_ref().and_then(|l| l.updated_at.clone());
    let saves: Vec<ClientSave> = local
        .as_ref()
        .map(|l| ClientSave {
            rom_id: game.rom_id,
            file_name: game.remote_sram_name(),
            slot: Some(SRAM_SLOT.into()),
            emulator: Some(game.save_emulator.clone()),
            content_hash: Some(l.hash.clone()),
            updated_at: local_updated_at.clone().unwrap_or_default(),
            file_size_bytes: l.bytes.len() as u64,
        })
        .into_iter()
        .collect();
    let negotiation = client.negotiate(device_id, &saves, &[game.rom_id]).await?;
    let session = negotiation.session_id;
    let op = negotiation
        .operations
        .into_iter()
        .find(|o| o.rom_id == game.rom_id && o.slot.as_deref() == Some(SRAM_SLOT));
    let result = match (op.as_ref().map(|o| o.action.as_str()), local) {
        (Some("upload"), Some(local)) => {
            match upload_sram(client, device_id, game, local.bytes, Some(session), false).await {
                Ok(()) => Ok(SramOutcome::Uploaded),
                Err(Error::Conflict) => {
                    let latest = client
                        .list_saves(game.rom_id, SRAM_SLOT, device_id)
                        .await
                        .ok()
                        .and_then(newest);
                    Ok(SramOutcome::Conflict(SramConflict {
                        save_id: latest.as_ref().map(|s| s.id),
                        server_updated_at: latest.map(|s| s.updated_at),
                        local_updated_at,
                    }))
                }
                Err(e) => Err(e),
            }
        }
        (Some("download"), _) => match op.as_ref().and_then(|o| o.save_id) {
            Some(save_id) => {
                let bytes = client
                    .download_save(save_id, device_id, Some(session))
                    .await?;
                let tag = op.as_ref().and_then(|o| o.emulator.as_deref());
                install_download(game, &bytes, tag)
            }
            None => Err(Error::Decode("download without a save id".into())),
        },
        (Some("conflict"), _) => {
            let op = op.expect("conflict operation");
            Ok(SramOutcome::Conflict(SramConflict {
                save_id: op.save_id,
                server_updated_at: op.server_updated_at,
                local_updated_at,
            }))
        }
        _ => Ok(SramOutcome::InSync),
    };
    let (completed, failed) = match &result {
        Ok(SramOutcome::Uploaded | SramOutcome::Downloaded { .. }) => (1, 0),
        Ok(SramOutcome::InSync | SramOutcome::Conflict(_)) => (0, 0),
        Err(_) => (0, 1),
    };
    let _ = client.complete_session(session, completed, failed).await;
    result
}

pub async fn resolve_sram(
    client: &Client,
    device_id: &str,
    game: &GameSaves,
    conflict: &SramConflict,
    keep: Keep,
) -> Result<SramOutcome, Error> {
    match keep {
        Keep::Local => {
            let local = game
                .local_save()?
                .ok_or_else(|| Error::Decode("could not read the save: it is gone".into()))?;
            upload_sram(client, device_id, game, local.bytes, None, true).await?;
            Ok(SramOutcome::Uploaded)
        }
        Keep::Server => {
            let (save_id, tag) = match conflict.save_id {
                Some(id) => (id, None),
                None => newest(client.list_saves(game.rom_id, SRAM_SLOT, device_id).await?)
                    .map(|s| (s.id, s.emulator))
                    .ok_or_else(|| Error::Decode("no server save to use".into()))?,
            };
            let bytes = client.download_save(save_id, device_id, None).await?;
            install_download(game, &bytes, tag.as_deref())
        }
    }
}

pub fn games_with_saves(server_saves: &Path) -> Vec<i64> {
    let has_save = |dir: &Path| {
        dir.join(SRAM_FILE).exists()
            || dir.join(ARMSX2_CARD).exists()
            || !memory_cards(dir).is_empty()
            || STATE_SLOTS
                .iter()
                .any(|slot| dir.join(format!("{slot}.state")).exists())
    };
    std::fs::read_dir(server_saves)
        .into_iter()
        .flatten()
        .flatten()
        .filter_map(|entry| {
            let id = entry.file_name().to_str()?.parse::<i64>().ok()?;
            has_save(&entry.path()).then_some(id)
        })
        .collect()
}

pub const STATE_SLOTS: [&str; 6] = ["auto", "slot-1", "slot-2", "slot-3", "slot-4", "slot-5"];

#[derive(Debug, Default, PartialEq, Eq)]
pub struct StateReport {
    pub uploaded: usize,
    pub downloaded: usize,
}

pub async fn sync_states(
    client: &Client,
    store: &std::sync::Mutex<crate::store::Store>,
    game: &GameSaves,
    core_version: &str,
) -> Result<StateReport, Error> {
    let remote = client.states(game.rom_id).await?;
    let io_err = |e: io::Error| Error::Decode(format!("could not write a save state: {e}"));
    let mut report = StateReport::default();
    for slot in STATE_SLOTS {
        let file = format!("{slot}.state");
        let path = game.dir.join(&file);
        let name = state_remote_name(slot, &game.emulator, core_version);
        let server = remote.iter().find(|r| r.file_name == name);
        let local = std::fs::read(&path).ok();
        let record = store.lock().unwrap().state_record(game.rom_id, slot);
        let local_md5 = local.as_deref().map(md5_hex);
        let local_changed =
            local_md5.is_some() && record.as_ref().map(|r| &r.local_md5) != local_md5.as_ref();
        let remote_changed = server.is_some_and(|srv| {
            record.as_ref().and_then(|r| r.remote_updated_at.as_deref())
                != Some(srv.updated_at.as_str())
        });
        let upload = match (local_changed, remote_changed) {
            (true, false) => true,
            (false, true) => false,
            (true, true) => {
                let local_time = file_iso_mtime(&path).and_then(|t| crate::sync::parse_iso(&t));
                let server_time = server.and_then(|s| crate::sync::parse_iso(&s.updated_at));
                local_time.unwrap_or(i64::MIN) > server_time.unwrap_or(i64::MIN)
            }
            (false, false) => continue,
        };
        let recorded = if upload {
            if let Some(srv) = server.filter(|_| remote_changed) {
                let bytes = client.download_state(srv.id).await?;
                backup_bytes(&game.dir, &format!("server-{file}"), &bytes).map_err(io_err)?;
            }
            let bytes = local.expect("local state to upload");
            let md5 = md5_hex(&bytes);
            let saved = client
                .upload_state(game.rom_id, &game.emulator, &name, bytes)
                .await?;
            report.uploaded += 1;
            crate::store::StateRecord {
                local_md5: md5,
                remote_id: Some(saved.id),
                remote_updated_at: Some(saved.updated_at),
            }
        } else {
            let srv = server.expect("remote state to download");
            let bytes = client.download_state(srv.id).await?;
            backup(&game.dir, &file).map_err(io_err)?;
            replace_file(&path, &bytes).map_err(io_err)?;
            report.downloaded += 1;
            crate::store::StateRecord {
                local_md5: md5_hex(&bytes),
                remote_id: Some(srv.id),
                remote_updated_at: Some(srv.updated_at.clone()),
            }
        };
        store
            .lock()
            .unwrap()
            .set_state_record(game.rom_id, slot, &recorded);
    }
    Ok(report)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn md5_matches_known_vector() {
        assert_eq!(md5_hex(b"abc"), "900150983cd24fb0d6963f7d28e17f72");
    }

    #[test]
    fn mtime_is_iso_utc() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("game.srm");
        std::fs::write(&file, b"x").unwrap();
        let iso = file_iso_mtime(&file).unwrap();
        assert_eq!(iso.len(), "2026-09-26T10:00:00+00:00".len());
        assert!(iso.ends_with("+00:00"));
        assert!(file_iso_mtime(&dir.path().join("missing")).is_none());
    }

    #[test]
    fn only_the_newest_backups_are_kept() {
        let dir = tempfile::tempdir().unwrap();
        for i in 0..(BACKUPS_KEPT + 3) {
            std::fs::write(dir.path().join("game.srm"), format!("v{i}")).unwrap();
            backup(dir.path(), "game.srm").unwrap();
        }
        let kept: Vec<_> = std::fs::read_dir(dir.path().join("backup"))
            .unwrap()
            .flatten()
            .collect();
        assert_eq!(kept.len(), BACKUPS_KEPT);
        let newest = std::fs::read_dir(dir.path().join("backup"))
            .unwrap()
            .flatten()
            .map(|e| e.path())
            .max()
            .unwrap();
        assert_eq!(
            std::fs::read_to_string(newest.join("game.srm")).unwrap(),
            format!("v{}", BACKUPS_KEPT + 2)
        );
    }

    #[test]
    fn a_new_backup_is_never_the_one_rotated_away() {
        // Backups named after a later time than now, as after several within one millisecond.
        let dir = tempfile::tempdir().unwrap();
        let ahead = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_millis()
            + 60_000;
        for i in 0..BACKUPS_KEPT as u128 {
            std::fs::create_dir_all(dir.path().join(format!("backup/{:015}", ahead + i))).unwrap();
        }
        std::fs::write(dir.path().join("game.srm"), b"newest").unwrap();
        let copy = backup(dir.path(), "game.srm").unwrap().unwrap();
        assert_eq!(std::fs::read(&copy).unwrap(), b"newest");
        assert!(!dir.path().join(format!("backup/{ahead:015}")).exists());
    }

    #[test]
    fn server_copies_are_backed_up_without_overwriting() {
        let dir = tempfile::tempdir().unwrap();
        let a = backup_bytes(dir.path(), "server-slot-1.state", b"one").unwrap();
        let b = backup_bytes(dir.path(), "server-slot-1.state", b"two").unwrap();
        assert_ne!(a, b);
        assert_eq!(std::fs::read(a).unwrap(), b"one");
        assert_eq!(std::fs::read(b).unwrap(), b"two");
    }

    #[test]
    fn a_failed_automatic_save_is_set_aside_not_deleted() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join(AUTO_STATE), b"broken").unwrap();
        set_aside_auto_state(dir.path()).unwrap();
        assert!(!dir.path().join(AUTO_STATE).exists());
        let kept: Vec<_> = std::fs::read_dir(dir.path().join("backup"))
            .unwrap()
            .flatten()
            .map(|e| std::fs::read(e.path().join("auto.state.failed")).unwrap())
            .collect();
        assert_eq!(kept, [b"broken".to_vec()]);
        assert!(set_aside_auto_state(dir.path()).is_ok());
    }

    #[test]
    fn backup_copies_existing_file_only() {
        let dir = tempfile::tempdir().unwrap();
        assert!(backup(dir.path(), "game.srm").unwrap().is_none());
        std::fs::write(dir.path().join("game.srm"), b"old").unwrap();
        let copy = backup(dir.path(), "game.srm").unwrap().unwrap();
        assert!(copy.starts_with(dir.path().join("backup")));
        assert_eq!(std::fs::read(copy).unwrap(), b"old");
        assert_eq!(std::fs::read(dir.path().join("game.srm")).unwrap(), b"old");
    }

    #[test]
    fn replace_file_writes_new_content() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("sub/game.srm");
        replace_file(&file, b"one").unwrap();
        replace_file(&file, b"two").unwrap();
        assert_eq!(std::fs::read(&file).unwrap(), b"two");
        assert_eq!(
            std::fs::read_dir(dir.path().join("sub")).unwrap().count(),
            1
        );
    }

    #[test]
    fn state_names_round_trip_and_sanitize() {
        let name = state_remote_name("slot-1", "snes9x", "1.63 185488c");
        assert_eq!(name, "slot-1.snes9x.1-63-185488c.state");
        assert_eq!(
            state_remote_name("auto", "../x", "v/1"),
            "auto.---x.v-1.state"
        );
    }

    fn other_psx_core() -> &'static CoreInfo {
        let default = crate::cores::core_for_platform("psx").unwrap();
        let other = crate::cores::cores_for_platform("psx")[1];
        assert_ne!(other, default);
        other
    }

    #[test]
    fn a_launch_syncs_its_saves_under_the_emulator_it_starts() {
        let dir = tempfile::tempdir().unwrap();
        let other = other_psx_core();
        let mut choices = CoreChoices::new();
        choices.insert("psx".into(), other.id.into());
        let launch = plan_launch(
            "psx",
            &choices,
            Some(other.id),
            5,
            dir.path().into(),
            "Game",
            None,
            None,
        )
        .unwrap();
        assert_eq!(launch.core, other);
        assert_eq!(launch.saves.emulator, launch.core.id);
        assert_eq!(launch.saves.save_emulator, launch.core.id);
        assert!(launch.previous.is_none());

        let default = plan_launch(
            "psx",
            &CoreChoices::new(),
            None,
            5,
            dir.path().into(),
            "Game",
            None,
            None,
        )
        .unwrap();
        assert_eq!(Some(default.core), crate::cores::core_for_platform("psx"));
        assert_eq!(default.saves.emulator, default.core.id);
        assert!(plan_launch(
            "xbox",
            &choices,
            None,
            5,
            dir.path().into(),
            "Game",
            None,
            None
        )
        .is_none());
    }

    #[test]
    fn a_launch_notices_the_game_last_played_with_another_emulator() {
        let dir = tempfile::tempdir().unwrap();
        let other = other_psx_core();
        let default = crate::cores::core_for_platform("psx").unwrap();
        let mut choices = CoreChoices::new();
        choices.insert("psx".into(), other.id.into());
        let plan = |choices: &CoreChoices, last: Option<&str>| {
            plan_launch(
                "psx",
                choices,
                last,
                5,
                dir.path().into(),
                "Game",
                None,
                None,
            )
            .unwrap()
        };
        assert_eq!(plan(&choices, Some(default.id)).previous, Some(default));
        // An older game counts as played with the system's earlier default.
        let earlier = crate::cores::played_core("psx", None).unwrap();
        let unless_same = |core: &CoreInfo| Some(earlier).filter(|e| e.id != core.id);
        assert_eq!(plan(&choices, None).previous, unless_same(other));
        assert_eq!(
            plan(&CoreChoices::new(), Some(other.id)).previous,
            Some(other)
        );
        assert_eq!(
            plan(&CoreChoices::new(), None).previous,
            unless_same(default)
        );
        // Saves left by the earlier emulator keep its name until they are set aside.
        let old = game_saves(default, 5, dir.path().into(), "Game", None, None);
        assert_eq!(old.emulator, default.id);
    }

    #[test]
    fn only_listed_emulators_share_their_save_ram() {
        assert!(same_save_ram("mednafen_psx_hw", "swanstation"));
        assert!(same_save_ram("swanstation", "mednafen_psx_hw"));
        assert!(!same_save_ram("mgba", "gambatte"));
        assert!(!same_save_ram("nestopia", "mesen"));
        assert!(!same_save_ram("swanstation", "swanstation_x"));
        // The pair only holds while SwanStation keeps memory card 1 in save RAM.
        assert!(crate::cores::default_options("swanstation").contains(&(
            "swanstation_MemoryCards_Card1Type".into(),
            "Libretro".into()
        )));
    }

    #[test]
    fn a_shared_save_stays_while_the_states_are_set_aside() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join(SRAM_FILE), b"card").unwrap();
        std::fs::write(dir.path().join(AUTO_STATE), b"auto").unwrap();
        let kept = set_aside_for_core_switch(dir.path(), "mednafen_psx_hw", SRAM_FILE, true)
            .unwrap()
            .unwrap();
        assert_eq!(std::fs::read(dir.path().join(SRAM_FILE)).unwrap(), b"card");
        assert!(!kept.join(SRAM_FILE).exists());
        assert_eq!(std::fs::read(kept.join(AUTO_STATE)).unwrap(), b"auto");
        std::fs::remove_file(dir.path().join(SRAM_FILE)).unwrap();
        assert_eq!(
            set_aside_for_core_switch(dir.path(), "mednafen_psx_hw", SRAM_FILE, true).unwrap(),
            None
        );
    }

    #[test]
    fn switching_emulator_sets_the_old_saves_aside_out_of_rotation() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(
            set_aside_for_core_switch(dir.path(), "mednafen_psx_hw", SRAM_FILE, false).unwrap(),
            None
        );
        std::fs::write(dir.path().join(SRAM_FILE), b"card").unwrap();
        std::fs::write(dir.path().join(AUTO_STATE), b"auto").unwrap();
        std::fs::write(dir.path().join("slot-2.state"), b"two").unwrap();
        std::fs::write(dir.path().join("keep.txt"), b"other").unwrap();
        let kept = set_aside_for_core_switch(dir.path(), "mednafen_psx_hw", SRAM_FILE, false)
            .unwrap()
            .unwrap();
        assert!(kept.starts_with(dir.path().join("backup")));
        assert!(kept.to_string_lossy().ends_with("-mednafen_psx_hw"));
        for (file, bytes) in [
            (SRAM_FILE, &b"card"[..]),
            (AUTO_STATE, b"auto"),
            ("slot-2.state", b"two"),
        ] {
            assert!(!dir.path().join(file).exists(), "{file} left behind");
            assert_eq!(std::fs::read(kept.join(file)).unwrap(), bytes);
        }
        assert!(dir.path().join("keep.txt").exists());
        for i in 0..(BACKUPS_KEPT + 2) {
            std::fs::write(dir.path().join(SRAM_FILE), format!("v{i}")).unwrap();
            backup(dir.path(), SRAM_FILE).unwrap();
        }
        assert!(
            kept.join(SRAM_FILE).exists(),
            "rotation removed the set-aside saves"
        );
        std::fs::write(dir.path().join(AUTO_STATE), b"again").unwrap();
        let again = set_aside_for_core_switch(dir.path(), "mednafen_psx_hw", SRAM_FILE, false)
            .unwrap()
            .unwrap();
        assert_ne!(again, kept);
        assert_eq!(std::fs::read(kept.join(AUTO_STATE)).unwrap(), b"auto");
    }

    #[test]
    fn each_emulator_syncs_its_own_in_game_save() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(
            in_game_save("snes9x", dir.path(), None),
            (SRAM_FILE.into(), "snes9x".into())
        );
        assert_eq!(
            in_game_save("armsx2", dir.path(), None),
            (ARMSX2_CARD.into(), "pcsx2".into())
        );
        // The names RetroArch-based clients derive from the core file, e.g. swanstation_libretro.
        for core in ["swanstation", "gambatte", "mesen"] {
            assert_eq!(
                in_game_save(core, dir.path(), None),
                (SRAM_FILE.into(), core.into())
            );
        }
        let rom = Path::new("/roms/ps2/Ico (USA).m3u");
        assert_eq!(
            in_game_save("pcsx2", dir.path(), Some(rom)),
            ("Ico (USA).ps2".into(), "pcsx2".into())
        );
        std::fs::write(dir.path().join("Ico (USA) (Disc 1).ps2"), b"card").unwrap();
        assert_eq!(
            in_game_save("pcsx2", dir.path(), Some(rom)).0,
            "Ico (USA) (Disc 1).ps2"
        );
    }

    #[test]
    fn ps2_saves_sync_as_a_zip_named_after_the_rom() {
        let rom = Path::new("/roms/ps2/Futurama (USA).chd");
        let armsx2 = Ps2Save::new("armsx2", "Futurama", Some(rom), Some(" BASLUS-20439 ")).unwrap();
        assert_eq!(armsx2.layout, ps2::Layout::Folder);
        assert_eq!(armsx2.zip_name, "Futurama (USA).zip");
        assert_eq!(armsx2.save_target.as_deref(), Some("BASLUS-20439"));
        let pcsx2 = Ps2Save::new("pcsx2", "Ico: Special", None, Some("")).unwrap();
        assert_eq!(pcsx2.layout, ps2::Layout::Image);
        assert_eq!(pcsx2.zip_name, "Ico- Special.zip");
        assert_eq!(pcsx2.save_target, None);
        assert!(Ps2Save::new("play", "x", None, Some("SLUS-20439")).is_none());
        assert!(Ps2Save::new("snes9x", "x", None, None).is_none());
    }

    #[test]
    fn games_with_only_a_memory_card_count_as_having_saves() {
        let root = tempfile::tempdir().unwrap();
        let armsx2 = root.path().join("7").join(ARMSX2_CARD);
        std::fs::create_dir_all(armsx2.parent().unwrap()).unwrap();
        std::fs::write(&armsx2, b"card").unwrap();
        std::fs::create_dir_all(root.path().join("8")).unwrap();
        std::fs::write(root.path().join("8/Ico.ps2"), b"card").unwrap();
        std::fs::create_dir_all(root.path().join("9")).unwrap();
        let mut ids = games_with_saves(root.path());
        ids.sort();
        assert_eq!(ids, [7, 8]);
    }

    mod sram {
        use super::super::*;
        use crate::romm::client::tests::base_of;
        use serde_json::json;
        use wiremock::matchers::{body_json, body_partial_json, method, path, query_param};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        fn game(dir: &Path) -> GameSaves {
            GameSaves {
                rom_id: 5,
                dir: dir.to_path_buf(),
                title: "Zelda: Link".into(),
                emulator: "snes9x".into(),
                save_file: SRAM_FILE.into(),
                save_emulator: "snes9x".into(),
                ps2: None,
            }
        }

        async fn negotiation(server: &MockServer, ops: serde_json::Value) {
            Mock::given(method("POST"))
                .and(path("/api/sync/negotiate"))
                .respond_with(
                    ResponseTemplate::new(200)
                        .set_body_json(json!({"session_id": 9, "operations": ops})),
                )
                .mount(server)
                .await;
        }

        async fn completion_counts(server: &MockServer, completed: u32, failed: u32) {
            Mock::given(method("POST"))
                .and(path("/api/sync/sessions/9/complete"))
                .and(body_json(json!({
                    "operations_completed": completed,
                    "operations_failed": failed
                })))
                .respond_with(ResponseTemplate::new(200).set_body_json(json!({})))
                .expect(1)
                .mount(server)
                .await;
        }

        async fn completion(server: &MockServer, times: u64) {
            Mock::given(method("POST"))
                .and(path("/api/sync/sessions/9/complete"))
                .respond_with(ResponseTemplate::new(200).set_body_json(json!({})))
                .expect(times)
                .mount(server)
                .await;
        }

        fn save_json(id: i64, updated: &str) -> serde_json::Value {
            json!({"id": id, "rom_id": 5, "file_name": "Zelda [t].srm", "slot": "autosave",
                   "updated_at": updated, "content_hash": "h"})
        }

        fn client(server: &MockServer) -> Client {
            Client::new(base_of(server, "/")).with_token("t".into())
        }

        #[tokio::test]
        async fn nothing_to_sync() {
            let server = MockServer::start().await;
            negotiation(&server, json!([])).await;
            completion_counts(&server, 0, 0).await;
            Mock::given(method("POST"))
                .and(path("/api/saves"))
                .respond_with(ResponseTemplate::new(500))
                .expect(0)
                .mount(&server)
                .await;
            let dir = tempfile::tempdir().unwrap();
            let outcome = sync_sram(&client(&server), "dev", &game(dir.path())).await;
            assert_eq!(outcome.unwrap(), SramOutcome::InSync);
        }

        #[tokio::test]
        async fn local_only_uploads() {
            let server = MockServer::start().await;
            negotiation(&server, json!([{"action": "upload", "rom_id": 5, "slot": "autosave", "save_id": null, "file_name": "Zelda- Link.srm"}])).await;
            completion(&server, 1).await;
            Mock::given(method("POST"))
                .and(path("/api/saves"))
                .and(query_param("slot", "autosave"))
                .and(query_param("session_id", "9"))
                .and(query_param("overwrite", "false"))
                .respond_with(ResponseTemplate::new(200).set_body_json(save_json(1, "t")))
                .expect(1)
                .mount(&server)
                .await;
            let dir = tempfile::tempdir().unwrap();
            std::fs::write(dir.path().join(SRAM_FILE), b"mine").unwrap();
            let outcome = sync_sram(&client(&server), "dev", &game(dir.path())).await;
            assert_eq!(outcome.unwrap(), SramOutcome::Uploaded);
        }

        fn ps2_game(dir: &Path, core: &str) -> GameSaves {
            let (save_file, save_emulator) = in_game_save(core, dir, None);
            GameSaves {
                emulator: core.into(),
                save_file,
                save_emulator,
                ps2: Ps2Save::new(
                    core,
                    "Zelda: Link",
                    Some(Path::new("/roms/Zelda (USA).chd")),
                    Some("SLUS-20152"),
                ),
                ..game(dir)
            }
        }

        fn card(game: &GameSaves) -> PathBuf {
            game.sram_path()
        }

        fn plant(dir: &Path, entries: &[(&str, &[u8])]) {
            for (name, data) in entries {
                let path = dir.join(name);
                std::fs::create_dir_all(path.parent().unwrap()).unwrap();
                std::fs::write(path, data).unwrap();
            }
        }

        /// A zip as the RomMix fork uploads it, with another game's folder beside this one's.
        fn server_zip() -> Vec<u8> {
            crate::ps2::archive::tests::foreign_zip(&[
                (
                    "BASLUS-20152AC04/BASLUS-20152AC04",
                    b"progress of the first game",
                ),
                ("BASLUS-20152AC04/icon.sys", b"icon of the first game"),
                ("BASLUS-20152SYS/icon.sys", b"system icon of the first game"),
                ("BASLUS-20152SYS/settings", b"settings of the first game"),
                ("BASLUS-21693XX/icon.sys", b"another game's folder"),
            ])
        }

        /// RomM's content hash of the RomMix fixture's two folders (RomMix zip.test.ts).
        const FIXTURE_HASH: &str = "9451d2d60d9692b2abaa142eb0764ea5";

        async fn serve_download(server: &MockServer, bytes: Vec<u8>, tag: &str) {
            negotiation(
                server,
                json!([{"action": "download", "rom_id": 5, "slot": "autosave", "save_id": 4,
                        "file_name": "Zelda (USA).zip", "emulator": tag}]),
            )
            .await;
            Mock::given(method("GET"))
                .and(path("/api/saves/4/content"))
                .respond_with(ResponseTemplate::new(200).set_body_bytes(bytes))
                .mount(server)
                .await;
        }

        fn find(haystack: &[u8], needle: &[u8]) -> Option<usize> {
            haystack.windows(needle.len()).position(|w| w == needle)
        }

        /// An upload named after the ROM whose file is a zip RomM hashes as `hash`.
        fn uploads_zip_hashing(hash: &'static str) -> impl wiremock::Match {
            move |request: &wiremock::Request| {
                let body = &request.body;
                let named = find(body, b"filename=\"Zelda (USA).zip\"").is_some();
                let zip = find(body, b"PK\x03\x04").and_then(|start| {
                    let end = start + find(&body[start..], b"\r\n--")?;
                    Some(&body[start..end])
                });
                named
                    && zip
                        .and_then(|z| ps2::archive::zip_content_hash(z).ok())
                        .as_deref()
                        == Some(hash)
            }
        }

        async fn no_upload(server: &MockServer) {
            Mock::given(method("POST"))
                .and(path("/api/saves"))
                .respond_with(ResponseTemplate::new(500))
                .expect(0)
                .mount(server)
                .await;
        }

        #[tokio::test]
        async fn a_ps2_game_uploads_a_zip_of_its_folders_not_its_card() {
            for core in ["armsx2", "pcsx2"] {
                let server = MockServer::start().await;
                Mock::given(method("POST"))
                    .and(path("/api/sync/negotiate"))
                    .and(body_partial_json(json!({"saves": [{
                        "file_name": "Zelda (USA).zip", "emulator": "pcsx2",
                        "content_hash": FIXTURE_HASH, "slot": "autosave"}]})))
                    .respond_with(
                        ResponseTemplate::new(200).set_body_json(json!({"session_id": 9,
                        "operations": [{"action": "upload", "rom_id": 5, "slot": "autosave",
                        "save_id": null, "file_name": "Zelda (USA).zip"}]})),
                    )
                    .expect(1)
                    .mount(&server)
                    .await;
                completion(&server, 1).await;
                Mock::given(method("POST"))
                    .and(path("/api/saves"))
                    .and(query_param("emulator", "pcsx2"))
                    .and(query_param("slot", "autosave"))
                    .and(uploads_zip_hashing(FIXTURE_HASH))
                    .respond_with(ResponseTemplate::new(200).set_body_json(save_json(1, "t")))
                    .expect(1)
                    .mount(&server)
                    .await;
                let dir = tempfile::tempdir().unwrap();
                let game = ps2_game(dir.path(), core);
                let fixture = crate::ps2::archive::tests::futurama_like();
                let mut with_others = fixture.clone();
                with_others.insert(
                    "BADATA-SYSTEM".into(),
                    [("history".into(), b"h".to_vec())].into(),
                );
                with_others.insert(
                    "BASLUS-21693XX".into(),
                    [("s".into(), b"x".to_vec())].into(),
                );
                match game.ps2.as_ref().unwrap().layout {
                    ps2::Layout::Folder => {
                        prepare_in_game_save(&game).unwrap();
                        for (folder, files) in &with_others {
                            for (file, data) in files {
                                plant(&card(&game), &[(&format!("{folder}/{file}"), data)]);
                            }
                        }
                        plant(&card(&game), &[("BASLUS-20152SYS/_pcsx2_index", b"index")]);
                    }
                    ps2::Layout::Image => {
                        let image = ps2::card::image_of(&with_others, ps2::card::tod(0));
                        std::fs::write(card(&game), image).unwrap();
                    }
                }
                let outcome = sync_sram(&client(&server), "dev", &game).await;
                assert_eq!(outcome.unwrap(), SramOutcome::Uploaded, "{core}");
            }
        }

        #[tokio::test]
        async fn a_pulled_zip_lands_in_the_games_folders_and_round_trips() {
            for core in ["armsx2", "pcsx2"] {
                let server = MockServer::start().await;
                serve_download(&server, server_zip(), "armsx2").await;
                completion(&server, 1).await;
                let dir = tempfile::tempdir().unwrap();
                let game = ps2_game(dir.path(), core);
                // The card already holds another game's save and the console's folder.
                let others = crate::ps2::archive::tests::unit(&[
                    ("BASLUS-99999ZZ", "keep", b"another game on this card"),
                    ("BADATA-SYSTEM", "history", b"the console's"),
                ]);
                let old = crate::ps2::archive::tests::unit(&[("BASLUS-20152AC04", "save", b"old")]);
                let mut before = others.clone();
                before.extend(old.clone());
                let folder = game.ps2.as_ref().unwrap().layout == ps2::Layout::Folder;
                if folder {
                    prepare_in_game_save(&game).unwrap();
                    for (f, files) in &before {
                        for (n, d) in files {
                            plant(&card(&game), &[(&format!("{f}/{n}"), d)]);
                        }
                    }
                } else {
                    let image = ps2::card::image_of(&before, ps2::card::tod(0));
                    std::fs::write(card(&game), image).unwrap();
                }

                let outcome = sync_sram(&client(&server), "dev", &game).await.unwrap();
                let SramOutcome::Downloaded {
                    backup: Some(backup),
                } = outcome
                else {
                    panic!("{core}: expected a download with a backup, got {outcome:?}");
                };
                let all = |g: &GameSaves| -> ps2::Unit {
                    if folder {
                        ps2::folder::read_folders(&card(g), &|_| true, &|_| false)
                            .unwrap()
                            .0
                    } else {
                        let bytes = std::fs::read(card(g)).unwrap();
                        ps2::card::Card::parse(bytes)
                            .unwrap()
                            .all_folders()
                            .unwrap()
                    }
                };
                let after = all(&game);
                let fixture = crate::ps2::archive::tests::futurama_like();
                let mut expected = others.clone();
                expected.extend(fixture.clone());
                assert_eq!(after, expected, "{core}: only the game's folders changed");
                // Pushed again, the save hashes as the zip it came from.
                let local = game.local_save().unwrap().unwrap();
                assert_eq!(local.hash, FIXTURE_HASH, "{core}");
                assert_eq!(
                    ps2::archive::zip_content_hash(&local.bytes).unwrap(),
                    FIXTURE_HASH
                );
                // What was replaced is in the backup.
                if folder {
                    assert_eq!(
                        std::fs::read(backup.join("BASLUS-20152AC04/save")).unwrap(),
                        b"old"
                    );
                } else {
                    let kept = std::fs::read(&backup).unwrap();
                    assert_eq!(
                        ps2::card::read_unit(&kept, &|n| n.starts_with("BASLUS-20152")).unwrap(),
                        old
                    );
                }
            }
        }

        #[tokio::test]
        async fn a_save_that_is_not_a_ps2_save_of_this_game_is_refused() {
            let not_a_zip = b"a raw save from some other emulator".to_vec();
            let other_game = crate::ps2::archive::tests::foreign_zip(&[("BASLUS-21693XX/s", b"x")]);
            for (bytes, tag, reason) in [
                (server_zip(), "play", "made with play"),
                (
                    not_a_zip,
                    "pcsx2",
                    "neither a PS2 save zip nor a memory card",
                ),
                (other_game, "pcsx2", "no save folders for SLUS-20152"),
            ] {
                for core in ["armsx2", "pcsx2"] {
                    let server = MockServer::start().await;
                    serve_download(&server, bytes.clone(), tag).await;
                    completion_counts(&server, 0, 1).await;
                    let dir = tempfile::tempdir().unwrap();
                    let game = ps2_game(dir.path(), core);
                    prepare_in_game_save(&game).unwrap();
                    if core == "pcsx2" {
                        std::fs::write(
                            card(&game),
                            ps2::card::format_card(true, ps2::card::tod(0)),
                        )
                        .unwrap();
                    } else {
                        plant(&card(&game), &[("BASLUS-20152AC04/save", b"mine")]);
                    }
                    let snapshot = |g: &GameSaves| {
                        let mut files = Vec::new();
                        let mut stack = vec![g.dir.clone()];
                        while let Some(d) = stack.pop() {
                            for e in std::fs::read_dir(d).unwrap().flatten() {
                                if e.file_type().unwrap().is_dir() {
                                    stack.push(e.path());
                                } else {
                                    files.push((e.path(), std::fs::read(e.path()).unwrap()));
                                }
                            }
                        }
                        files.sort();
                        files
                    };
                    let before = snapshot(&game);
                    let err = sync_sram(&client(&server), "dev", &game).await.unwrap_err();
                    assert!(matches!(err, Error::Refused(_)), "{core}: {err:?}");
                    assert!(err.to_string().contains(reason), "{core}: {err}");
                    assert_eq!(snapshot(&game), before, "{core}: nothing changed");
                }
            }
        }

        #[tokio::test]
        async fn a_zip_an_earlier_romp_wrote_as_the_card_is_backed_up_and_replaced() {
            let server = MockServer::start().await;
            serve_download(&server, server_zip(), "pcsx2").await;
            completion(&server, 1).await;
            let dir = tempfile::tempdir().unwrap();
            let game = ps2_game(dir.path(), "pcsx2");
            std::fs::write(card(&game), server_zip()).unwrap();
            let outcome = sync_sram(&client(&server), "dev", &game).await.unwrap();
            let SramOutcome::Downloaded {
                backup: Some(backup),
            } = outcome
            else {
                panic!("expected a download with a backup, got {outcome:?}");
            };
            assert_eq!(std::fs::read(backup).unwrap(), server_zip());
            let image = std::fs::read(card(&game)).unwrap();
            assert_eq!(image.len(), ps2::card::IMAGE_SIZE);
            let owns = |n: &str| n.starts_with("BASLUS-20152");
            assert_eq!(
                ps2::card::read_unit(&image, &owns).unwrap(),
                crate::ps2::archive::tests::futurama_like()
            );
        }

        #[tokio::test]
        async fn a_ps2_game_without_a_serial_syncs_nothing() {
            let server = MockServer::start().await;
            Mock::given(method("POST"))
                .and(path("/api/sync/negotiate"))
                .respond_with(ResponseTemplate::new(500))
                .expect(0)
                .mount(&server)
                .await;
            no_upload(&server).await;
            let dir = tempfile::tempdir().unwrap();
            let mut game = ps2_game(dir.path(), "pcsx2");
            game.ps2.as_mut().unwrap().save_target = None;
            std::fs::write(card(&game), ps2::card::format_card(true, ps2::card::tod(0))).unwrap();
            let err = sync_sram(&client(&server), "dev", &game).await.unwrap_err();
            assert!(err.to_string().contains("serial"), "{err}");
        }

        #[tokio::test]
        async fn a_memory_card_is_never_uploaded_as_a_ps2_save() {
            let server = MockServer::start().await;
            no_upload(&server).await;
            let dir = tempfile::tempdir().unwrap();
            let game = ps2_game(dir.path(), "pcsx2");
            let image = ps2::card::format_card(true, ps2::card::tod(0));
            let err = upload_sram(&client(&server), "dev", &game, image, None, true)
                .await
                .unwrap_err();
            assert!(matches!(err, Error::Refused(_)), "{err:?}");
        }

        #[tokio::test]
        async fn an_old_memory_card_image_is_converted_before_it_syncs() {
            let server = MockServer::start().await;
            Mock::given(method("POST"))
                .and(path("/api/sync/negotiate"))
                .and(body_partial_json(
                    json!({"saves": [{"content_hash": FIXTURE_HASH}]}),
                ))
                .respond_with(
                    ResponseTemplate::new(200)
                        .set_body_json(json!({"session_id": 9, "operations": []})),
                )
                .expect(1)
                .mount(&server)
                .await;
            completion(&server, 1).await;
            no_upload(&server).await;
            let dir = tempfile::tempdir().unwrap();
            let game = ps2_game(dir.path(), "armsx2");
            let fixture = crate::ps2::archive::tests::futurama_like();
            let image = ps2::card::image_of(&fixture, ps2::card::tod(0));
            std::fs::create_dir_all(card(&game).parent().unwrap()).unwrap();
            std::fs::write(card(&game), &image).unwrap();
            let outcome = sync_sram(&client(&server), "dev", &game).await.unwrap();
            assert_eq!(outcome, SramOutcome::InSync);
            assert!(card(&game).is_dir());
            let kept: Vec<PathBuf> = std::fs::read_dir(dir.path().join("backup"))
                .unwrap()
                .flatten()
                .map(|e| e.path().join("Mcd001.ps2"))
                .collect();
            assert_eq!(kept.len(), 1);
            assert_eq!(std::fs::read(&kept[0]).unwrap(), image);
            // Kept out of the rotation, which would drop it after ten more backups.
            assert!(stamped_backups(dir.path()).is_empty());
        }

        #[tokio::test]
        async fn server_newer_downloads_and_backs_up() {
            let server = MockServer::start().await;
            negotiation(
                &server,
                json!([{"action": "download", "rom_id": 5, "slot": "autosave", "save_id": 4, "file_name": "x.srm"}]),
            )
            .await;
            completion(&server, 1).await;
            Mock::given(method("GET"))
                .and(path("/api/saves/4/content"))
                .and(query_param("session_id", "9"))
                .respond_with(ResponseTemplate::new(200).set_body_bytes(b"new".to_vec()))
                .mount(&server)
                .await;
            let dir = tempfile::tempdir().unwrap();
            std::fs::write(dir.path().join(SRAM_FILE), b"old").unwrap();
            let outcome = sync_sram(&client(&server), "dev", &game(dir.path()))
                .await
                .unwrap();
            let SramOutcome::Downloaded {
                backup: Some(backup),
            } = outcome
            else {
                panic!("{outcome:?}")
            };
            assert_eq!(std::fs::read(backup).unwrap(), b"old");
            assert_eq!(std::fs::read(dir.path().join(SRAM_FILE)).unwrap(), b"new");
        }

        #[tokio::test]
        async fn operations_for_other_slots_are_ignored() {
            let server = MockServer::start().await;
            negotiation(&server, json!([
                {"action": "download", "rom_id": 5, "save_id": 11, "file_name": "web.srm", "slot": "web"},
                {"action": "no_op", "rom_id": 5, "save_id": 12, "file_name": "x.srm", "slot": "autosave"}
            ])).await;
            completion(&server, 1).await;
            Mock::given(method("GET"))
                .and(path("/api/saves/11/content"))
                .respond_with(ResponseTemplate::new(200).set_body_bytes(b"wrong".to_vec()))
                .expect(0)
                .mount(&server)
                .await;
            let dir = tempfile::tempdir().unwrap();
            std::fs::write(dir.path().join(SRAM_FILE), b"mine").unwrap();
            let outcome = sync_sram(&client(&server), "dev", &game(dir.path())).await;
            assert_eq!(outcome.unwrap(), SramOutcome::InSync);
            assert_eq!(std::fs::read(dir.path().join(SRAM_FILE)).unwrap(), b"mine");
        }

        #[tokio::test]
        async fn conflict_is_reported_not_resolved() {
            let server = MockServer::start().await;
            negotiation(
                &server,
                json!([{"action": "conflict", "rom_id": 5, "slot": "autosave", "save_id": 4, "file_name": "x.srm",
                "server_updated_at": "2026-09-26T10:00:00+00:00"}]),
            )
            .await;
            completion_counts(&server, 0, 0).await;
            let dir = tempfile::tempdir().unwrap();
            std::fs::write(dir.path().join(SRAM_FILE), b"mine").unwrap();
            let outcome = sync_sram(&client(&server), "dev", &game(dir.path()))
                .await
                .unwrap();
            let SramOutcome::Conflict(c) = outcome else {
                panic!()
            };
            assert_eq!(c.save_id, Some(4));
            assert_eq!(
                c.server_updated_at.as_deref(),
                Some("2026-09-26T10:00:00+00:00")
            );
            assert!(c.local_updated_at.is_some());
            assert_eq!(std::fs::read(dir.path().join(SRAM_FILE)).unwrap(), b"mine");
        }

        #[tokio::test]
        async fn upload_409_becomes_conflict() {
            let server = MockServer::start().await;
            negotiation(
                &server,
                json!([{"action": "upload", "rom_id": 5, "slot": "autosave", "save_id": null, "file_name": "x.srm"}]),
            )
            .await;
            Mock::given(method("POST"))
                .and(path("/api/saves"))
                .respond_with(ResponseTemplate::new(409).set_body_json(json!({"detail": "newer"})))
                .mount(&server)
                .await;
            Mock::given(method("GET"))
                .and(path("/api/saves"))
                .and(query_param("slot", "autosave"))
                .respond_with(ResponseTemplate::new(200).set_body_json(json!([
                    save_json(7, "2026-09-26T10:00:00.5Z"),
                    save_json(6, "2026-09-26T10:00:00+00:00")
                ])))
                .mount(&server)
                .await;
            completion_counts(&server, 0, 0).await;
            let dir = tempfile::tempdir().unwrap();
            std::fs::write(dir.path().join(SRAM_FILE), b"mine").unwrap();
            let outcome = sync_sram(&client(&server), "dev", &game(dir.path()))
                .await
                .unwrap();
            let SramOutcome::Conflict(conflict) = outcome else {
                panic!("expected a conflict");
            };
            assert_eq!(conflict.save_id, Some(7));
            assert_eq!(
                conflict.server_updated_at.as_deref(),
                Some("2026-09-26T10:00:00.5Z")
            );
        }

        #[tokio::test]
        async fn resolving_backs_up_the_loser() {
            let server = MockServer::start().await;
            Mock::given(method("GET"))
                .and(path("/api/saves"))
                .and(query_param("slot", "autosave"))
                .respond_with(ResponseTemplate::new(200).set_body_json(json!([
                    save_json(6, "2026-09-25T10:00:00+00:00"),
                    save_json(7, "2026-09-26T10:00:00+00:00")
                ])))
                .mount(&server)
                .await;
            Mock::given(method("GET"))
                .and(path("/api/saves/7/content"))
                .respond_with(ResponseTemplate::new(200).set_body_bytes(b"server".to_vec()))
                .mount(&server)
                .await;
            Mock::given(method("POST"))
                .and(path("/api/saves"))
                .and(query_param("overwrite", "true"))
                .respond_with(ResponseTemplate::new(200).set_body_json(save_json(8, "t")))
                .expect(1)
                .mount(&server)
                .await;
            let dir = tempfile::tempdir().unwrap();
            std::fs::write(dir.path().join(SRAM_FILE), b"local").unwrap();
            let conflict = SramConflict {
                save_id: None,
                server_updated_at: None,
                local_updated_at: None,
            };
            let c = client(&server);
            let g = game(dir.path());
            assert_eq!(
                resolve_sram(&c, "dev", &g, &conflict, Keep::Local)
                    .await
                    .unwrap(),
                SramOutcome::Uploaded
            );
            let outcome = resolve_sram(&c, "dev", &g, &conflict, Keep::Server)
                .await
                .unwrap();
            let SramOutcome::Downloaded {
                backup: Some(backup),
            } = outcome
            else {
                panic!()
            };
            assert_eq!(std::fs::read(backup).unwrap(), b"local");
            assert_eq!(
                std::fs::read(dir.path().join(SRAM_FILE)).unwrap(),
                b"server"
            );
        }
    }

    mod states {
        use super::super::*;
        use crate::romm::client::tests::base_of;
        use crate::store::{StateRecord, Store};
        use serde_json::json;
        use std::sync::Mutex;
        use wiremock::matchers::{method, path, query_param};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        fn game(dir: &Path) -> GameSaves {
            GameSaves {
                rom_id: 5,
                dir: dir.to_path_buf(),
                title: "Zelda".into(),
                emulator: "snes9x".into(),
                save_file: SRAM_FILE.into(),
                save_emulator: "snes9x".into(),
                ps2: None,
            }
        }

        fn state_json(id: i64, name: &str, updated: &str) -> serde_json::Value {
            json!({"id": id, "rom_id": 5, "file_name": name, "updated_at": updated, "emulator": "snes9x"})
        }

        async fn remote(server: &MockServer, states: serde_json::Value) {
            Mock::given(method("GET"))
                .and(path("/api/states"))
                .and(query_param("rom_id", "5"))
                .respond_with(ResponseTemplate::new(200).set_body_json(states))
                .mount(server)
                .await;
        }

        fn client(server: &MockServer) -> Client {
            Client::new(base_of(server, "/")).with_token("t".into())
        }

        fn store() -> Mutex<Store> {
            Mutex::new(Store::open_in_memory().unwrap())
        }

        #[tokio::test]
        async fn uploads_new_local_state_once() {
            let server = MockServer::start().await;
            remote(&server, json!([])).await;
            Mock::given(method("POST"))
                .and(path("/api/states"))
                .and(query_param("emulator", "snes9x"))
                .respond_with(ResponseTemplate::new(200).set_body_json(state_json(
                    3,
                    "slot-1.snes9x.1-63.state",
                    "2026-09-26T10:00:00+00:00",
                )))
                .expect(1)
                .mount(&server)
                .await;
            let dir = tempfile::tempdir().unwrap();
            std::fs::write(dir.path().join("slot-1.state"), b"S1").unwrap();
            let store = store();
            let report = sync_states(&client(&server), &store, &game(dir.path()), "1.63")
                .await
                .unwrap();
            assert_eq!(
                report,
                StateReport {
                    uploaded: 1,
                    downloaded: 0
                }
            );
            let record = store.lock().unwrap().state_record(5, "slot-1").unwrap();
            assert_eq!(record.remote_id, Some(3));
            assert_eq!(record.local_md5, md5_hex(b"S1"));
        }

        #[tokio::test]
        async fn unchanged_states_do_nothing() {
            let server = MockServer::start().await;
            remote(
                &server,
                json!([state_json(3, "slot-1.snes9x.1-63.state", "t1")]),
            )
            .await;
            Mock::given(method("POST"))
                .and(path("/api/states"))
                .respond_with(ResponseTemplate::new(500))
                .expect(0)
                .mount(&server)
                .await;
            let dir = tempfile::tempdir().unwrap();
            std::fs::write(dir.path().join("slot-1.state"), b"S1").unwrap();
            let store = store();
            store.lock().unwrap().set_state_record(
                5,
                "slot-1",
                &StateRecord {
                    local_md5: md5_hex(b"S1"),
                    remote_id: Some(3),
                    remote_updated_at: Some("t1".into()),
                },
            );
            let report = sync_states(&client(&server), &store, &game(dir.path()), "1.63")
                .await
                .unwrap();
            assert_eq!(report, StateReport::default());
        }

        #[tokio::test]
        async fn downloads_newer_remote_state() {
            let server = MockServer::start().await;
            remote(
                &server,
                json!([state_json(4, "slot-2.snes9x.1-63.state", "t2")]),
            )
            .await;
            Mock::given(method("GET"))
                .and(path("/api/states/4/content"))
                .respond_with(ResponseTemplate::new(200).set_body_bytes(b"REMOTE".to_vec()))
                .mount(&server)
                .await;
            let dir = tempfile::tempdir().unwrap();
            let store = store();
            let report = sync_states(&client(&server), &store, &game(dir.path()), "1.63")
                .await
                .unwrap();
            assert_eq!(report.downloaded, 1);
            assert_eq!(
                std::fs::read(dir.path().join("slot-2.state")).unwrap(),
                b"REMOTE"
            );
        }

        #[tokio::test]
        async fn other_core_versions_are_ignored() {
            let server = MockServer::start().await;
            remote(
                &server,
                json!([
                    state_json(4, "slot-2.snes9x.1-62.state", "t2"),
                    state_json(5, "slot-3.bsnes.1-63.state", "t2")
                ]),
            )
            .await;
            let dir = tempfile::tempdir().unwrap();
            let report = sync_states(&client(&server), &store(), &game(dir.path()), "1.63")
                .await
                .unwrap();
            assert_eq!(report, StateReport::default());
            assert!(!dir.path().join("slot-2.state").exists());
        }
    }

    #[test]
    fn finds_game_folders_with_save_files() {
        let dir = tempfile::tempdir().unwrap();
        for (name, file) in [
            ("7", Some("game.srm")),
            ("9", Some("slot-1.state")),
            ("11", None),
            ("junk", Some("game.srm")),
        ] {
            std::fs::create_dir_all(dir.path().join(name)).unwrap();
            if let Some(file) = file {
                std::fs::write(dir.path().join(name).join(file), b"x").unwrap();
            }
        }
        let mut ids = games_with_saves(dir.path());
        ids.sort();
        assert_eq!(ids, [7, 9]);
        assert!(games_with_saves(&dir.path().join("missing")).is_empty());
    }
}
