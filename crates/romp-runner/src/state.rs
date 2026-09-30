use crate::frontend::Frontend;
use romp_libretro as lr;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};
use tracing::{info, warn};

const SRAM_CHECK: Duration = Duration::from_secs(2);

pub fn slot_file_name(slot: u8) -> String {
    if (1..=5).contains(&slot) {
        format!("slot-{slot}.state")
    } else {
        "auto.state".to_string()
    }
}

pub fn write_atomic(path: &Path, data: &[u8]) -> std::io::Result<()> {
    let tmp = path.with_extension("tmp");
    std::fs::write(&tmp, data)?;
    std::fs::rename(&tmp, path).inspect_err(|_| {
        let _ = std::fs::remove_file(&tmp);
    })
}

/// Moves a save RAM file the core can't use into the save folder's backups, so the core's own
/// save RAM never overwrites it. The folder name isn't a plain timestamp, so the app's backup
/// rotation keeps it.
fn set_aside_sram(dir: &Path, path: &Path, len: usize) -> std::io::Result<PathBuf> {
    let stamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis();
    let mut n = 0;
    let target = loop {
        let candidate = dir
            .join("backup")
            .join(format!("{stamp:015}-{n}-sram-size-{len}"));
        if !candidate.exists() {
            break candidate;
        }
        n += 1;
    };
    std::fs::create_dir_all(&target)?;
    let target = target.join("game.srm");
    std::fs::rename(path, &target)?;
    Ok(target)
}

pub const SAVING_OFF: &str = "In-game saving is off for this session: the game's save file \
could not be read or set aside, so it is left untouched.";

#[derive(Debug, PartialEq, Eq)]
enum SramLoad {
    Loaded,
    Absent,
    SetAside(PathBuf),
    /// The file is still there and must not be overwritten.
    Untouchable,
}

/// Loads `game.srm` into the core's save RAM when it is the same size. A file of another size,
/// such as one written by another emulator, is set aside rather than left to be overwritten.
fn load_sram(dir: &Path, region: &mut [u8]) -> SramLoad {
    let path = dir.join("game.srm");
    match std::fs::read(&path) {
        Ok(bytes) if bytes.len() == region.len() => {
            region.copy_from_slice(&bytes);
            info!(len = bytes.len(), "SRAM loaded");
            SramLoad::Loaded
        }
        Ok(bytes) => match set_aside_sram(dir, &path, bytes.len()) {
            Ok(kept) => {
                warn!(
                    file = bytes.len(),
                    core = region.len(),
                    kept = %kept.display(),
                    "SRAM size mismatch; set the file aside"
                );
                SramLoad::SetAside(kept)
            }
            Err(e) => {
                warn!(
                    file = bytes.len(),
                    core = region.len(),
                    "SRAM size mismatch and the file could not be set aside, so it won't be saved over: {e}"
                );
                SramLoad::Untouchable
            }
        },
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => SramLoad::Absent,
        Err(e) => {
            warn!("read SRAM, so it won't be saved over: {e}");
            SramLoad::Untouchable
        }
    }
}

pub struct StateManager {
    dir: PathBuf,
    last_saved_sram: Option<Vec<u8>>,
    last_check: Instant,
    sram_untouchable: bool,
}

impl StateManager {
    pub fn new(dir: &Path) -> Self {
        Self {
            dir: dir.to_path_buf(),
            last_saved_sram: None,
            last_check: Instant::now(),
            sram_untouchable: false,
        }
    }

    fn sram_path(&self) -> PathBuf {
        self.dir.join("game.srm")
    }

    pub fn load_initial_sram(&mut self, core: &mut lr::Core, frontend: &mut Frontend) {
        let Some(region) = (unsafe { core.memory_region(lr::RETRO_MEMORY_SAVE_RAM, frontend) })
        else {
            return;
        };
        self.sram_untouchable = load_sram(&self.dir, region) == SramLoad::Untouchable;
        self.last_saved_sram = Some(region.to_vec());
    }

    /// What the player needs to know about in-game saving this session.
    pub fn notice(&self) -> Option<&'static str> {
        self.sram_untouchable.then_some(SAVING_OFF)
    }

    pub fn tick_sram(&mut self, core: &mut lr::Core, frontend: &mut Frontend) -> bool {
        if self.last_check.elapsed() < SRAM_CHECK {
            return false;
        }
        self.last_check = Instant::now();
        self.flush_sram(core, frontend)
    }

    pub fn flush_sram(&mut self, core: &mut lr::Core, frontend: &mut Frontend) -> bool {
        if self.sram_untouchable {
            return false;
        }
        let Some(snapshot) = (unsafe { core.memory_region(lr::RETRO_MEMORY_SAVE_RAM, frontend) })
            .map(|r| r.to_vec())
        else {
            return false;
        };
        if self.last_saved_sram.as_deref() == Some(&snapshot[..]) {
            return false;
        }
        match write_atomic(&self.sram_path(), &snapshot) {
            Ok(()) => {
                self.last_saved_sram = Some(snapshot);
                true
            }
            Err(e) => {
                warn!("write SRAM: {e}");
                false
            }
        }
    }

    pub fn save_state(&self, slot: u8, core: &mut lr::Core, frontend: &mut Frontend) -> bool {
        let data = match core.serialize(frontend) {
            Ok(d) => d,
            Err(e) => {
                warn!("serialize: {e}");
                return false;
            }
        };
        match write_atomic(&self.dir.join(slot_file_name(slot)), trimmed(&data)) {
            Ok(()) => true,
            Err(e) => {
                warn!("write state: {e}");
                false
            }
        }
    }

    pub fn load_state(&self, slot: u8, core: &mut lr::Core, frontend: &mut Frontend) -> bool {
        match self.try_load_state(slot, core, frontend) {
            Ok(()) => true,
            Err(e) => {
                warn!("{e}");
                false
            }
        }
    }

    pub fn try_load_state(
        &self,
        slot: u8,
        core: &mut lr::Core,
        frontend: &mut Frontend,
    ) -> Result<(), String> {
        let data = std::fs::read(self.dir.join(slot_file_name(slot)))
            .map_err(|e| format!("read state: {e}"))?;
        let data = padded(data, core.serialize_size(frontend));
        core.unserialize(&data, frontend)
            .map_err(|e| format!("unserialize: {e}"))
    }

    pub fn save_on_shutdown(
        &mut self,
        core: &mut lr::Core,
        frontend: &mut Frontend,
        auto_state: bool,
    ) {
        self.flush_sram(core, frontend);
        if auto_state {
            self.save_state(0, core, frontend);
        }
    }
}

// Some cores report a fixed maximum size and leave the rest zeroed; the zeros are
// not stored, and come back as padding when the state is loaded.
fn trimmed(data: &[u8]) -> &[u8] {
    let end = data.iter().rposition(|&b| b != 0).map_or(0, |i| i + 1);
    &data[..end.max(1).min(data.len())]
}

fn padded(mut data: Vec<u8>, size: usize) -> Vec<u8> {
    if data.len() < size {
        data.resize(size, 0);
    }
    data
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn trailing_zeros_are_not_stored_and_come_back_on_load() {
        let state = [7, 0, 3, 0, 0, 0, 0, 0];
        let stored = trimmed(&state);
        assert_eq!(stored, [7, 0, 3]);
        assert_eq!(padded(stored.to_vec(), state.len()), state);
        assert_eq!(padded(state.to_vec(), 4), state);
        assert_eq!(trimmed(&[0, 0]), [0]);
    }

    #[test]
    fn slot_names() {
        assert_eq!(slot_file_name(0), "auto.state");
        assert_eq!(slot_file_name(1), "slot-1.state");
        assert_eq!(slot_file_name(5), "slot-5.state");
        assert_eq!(slot_file_name(9), "auto.state");
    }

    #[test]
    fn save_ram_of_the_right_size_is_loaded() {
        let dir = tempfile::tempdir().unwrap();
        let mut region = [0u8; 4];
        assert_eq!(load_sram(dir.path(), &mut region), SramLoad::Absent);
        std::fs::write(dir.path().join("game.srm"), [1, 2, 3, 4]).unwrap();
        assert_eq!(load_sram(dir.path(), &mut region), SramLoad::Loaded);
        assert_eq!(region, [1, 2, 3, 4]);
    }

    #[test]
    fn save_ram_of_another_size_is_set_aside_not_overwritten() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("game.srm");
        std::fs::write(&path, [9; 8]).unwrap();
        let mut region = [0u8; 4];
        let SramLoad::SetAside(kept) = load_sram(dir.path(), &mut region) else {
            panic!("the file was not set aside");
        };
        assert_eq!(region, [0; 4]);
        assert!(!path.exists());
        assert_eq!(std::fs::read(&kept).unwrap(), [9; 8]);
        assert!(kept.starts_with(dir.path().join("backup")));
        let folder = kept
            .parent()
            .unwrap()
            .file_name()
            .unwrap()
            .to_str()
            .unwrap();
        assert!(folder.ends_with("-sram-size-8"), "{folder}");
        assert!(
            !folder.chars().all(|c| c.is_ascii_digit()),
            "kept out of rotation"
        );

        std::fs::write(&path, [7; 8]).unwrap();
        let SramLoad::SetAside(again) = load_sram(dir.path(), &mut region) else {
            panic!("the second file was not set aside");
        };
        assert_ne!(kept, again);
        assert_eq!(std::fs::read(&kept).unwrap(), [9; 8]);
    }

    #[test]
    fn an_unreadable_save_is_left_alone_and_saving_is_off() {
        let dir = tempfile::tempdir().unwrap();
        // A folder in place of the file can't be read as one.
        std::fs::create_dir(dir.path().join("game.srm")).unwrap();
        let mut region = [5u8; 4];
        assert_eq!(load_sram(dir.path(), &mut region), SramLoad::Untouchable);
        assert!(dir.path().join("game.srm").is_dir());
        let mut saves = StateManager::new(dir.path());
        assert_eq!(saves.notice(), None);
        saves.sram_untouchable = true;
        assert_eq!(saves.notice(), Some(SAVING_OFF));
    }

    #[test]
    fn write_atomic_replaces_and_leaves_no_temp_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("game.srm");
        write_atomic(&path, b"one").unwrap();
        write_atomic(&path, b"two").unwrap();
        assert_eq!(std::fs::read(&path).unwrap(), b"two");
        assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 1);
    }
}
