use super::{on_ui, Controller};
use crate::cores::CoreInfo;
use crate::paths;
use crate::play::CoreIdentity;
use crate::romm::client::{Client, Error};
use crate::saves::{self, GameSaves, Keep, SramConflict, SramOutcome};
use crate::store::{GameDetail, Store};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

pub(super) struct PendingLaunch {
    pub detail: GameDetail,
    pub rom: PathBuf,
    pub info: &'static CoreInfo,
    pub saves: GameSaves,
    pub core: PathBuf,
    pub conflict: SramConflict,
}

pub(super) fn conflict_text(conflict: &SramConflict) -> String {
    let when = |t: &Option<String>| {
        t.as_deref()
            .and_then(crate::sync::parse_iso)
            .map(|ms| {
                let time =
                    std::time::UNIX_EPOCH + std::time::Duration::from_millis(ms.max(0) as u64);
                crate::sync::iso_utc(time)
                    .replace('T', " ")
                    .replace("+00:00", " UTC")
            })
            .unwrap_or_else(|| "unknown".into())
    };
    format!(
        "This computer: {}. Server: {}. The one you don't choose is kept as a backup.",
        when(&conflict.local_updated_at),
        when(&conflict.server_updated_at)
    )
}

fn version_key(rom_id: i64) -> String {
    format!("core_version:{rom_id}")
}

/// The id of the emulator a game was last played with, whose files are in its save folder.
fn core_key(rom_id: i64) -> String {
    format!("core:{rom_id}")
}

/// Gets a game's save folder ready for `core`. When the game was last played with another
/// emulator, that emulator's unsynced saves are first synced under its own name, then its
/// save states, and its in-game save unless both emulators keep the same file, are set aside,
/// so they are never loaded by, or synced under the name of, the new one. When unsynced saves
/// can't be synced first, nothing is moved and the error is the status to show: the game
/// doesn't start, and keeps its emulator until the saves sync or the choice is undone.
pub(super) async fn switch_core(
    online: Option<(&Client, &str)>,
    store: &Mutex<Store>,
    game: &GameSaves,
    core: &CoreInfo,
    previous: Option<(&'static CoreInfo, &GameSaves)>,
) -> Result<(), String> {
    let id = game.rom_id;
    if let Some((previous, old)) = previous {
        let pending = store.lock().unwrap().pending().contains(&id);
        if pending {
            let refused = || {
                format!(
                    "Connect to your server once so {}'s saves can sync before this game \
                     switches to {}. To play now, choose {} again in Settings.",
                    previous.name, core.name, previous.name
                )
            };
            let version = store.lock().unwrap().get(&version_key(id));
            let Some((client, device)) = online else {
                tracing::warn!(
                    "not switching {id} to {}: {}'s saves are unsynced",
                    core.id,
                    previous.id
                );
                return Err(refused());
            };
            if let Err(e) = sync_game(client, store, device, old, version.as_deref()).await {
                tracing::warn!(
                    "not switching {id} to {}: syncing {}'s saves: {e}",
                    core.id,
                    previous.id
                );
                return Err(refused());
            }
            store.lock().unwrap().remove_pending(id);
        }
        let keep_save =
            saves::same_save_ram(previous.id, core.id) && old.save_file == game.save_file;
        match saves::set_aside_for_core_switch(&old.dir, previous.id, &old.save_file, keep_save) {
            Ok(Some(kept)) => tracing::info!(
                "game {id} now plays with {}; {}'s saves set aside in {}",
                core.id,
                previous.id,
                kept.display()
            ),
            Ok(None) => {}
            Err(e) => {
                tracing::warn!("setting aside {id}'s {} saves: {e}", previous.id);
                return Err(format!(
                    "Could not set aside the saves from {}: {e}",
                    previous.name
                ));
            }
        }
        if keep_save {
            tracing::info!(
                "game {id} keeps its in-game save: {} and {} share it",
                previous.id,
                core.id
            );
        }
        let mut store = store.lock().unwrap();
        store.remove(&version_key(id));
        store.remove_pending(id);
    }
    store.lock().unwrap().set(&core_key(id), core.id);
    Ok(())
}

async fn sync_game(
    client: &Client,
    store: &Mutex<Store>,
    device_id: &str,
    game: &GameSaves,
    core_version: Option<&str>,
) -> Result<SramOutcome, Error> {
    let sram = saves::sync_sram(client, device_id, game).await?;
    if let Some(version) = core_version {
        saves::sync_states(client, store, game, version).await?;
    }
    Ok(sram)
}

impl Controller {
    pub(super) fn sync_device(&self) -> Option<String> {
        let store = self.shared.store.lock().unwrap();
        let scopes = store.get("scopes")?;
        scopes
            .split(' ')
            .any(|s| s == "devices.write")
            .then(|| store.get("device_uuid"))
            .flatten()
    }

    pub(super) fn update_pairing_prompt(&self) {
        let signed_in = self.client.borrow().is_some();
        let prompt = if self.sync_device().is_none() {
            Some("Pair again to turn on save sync")
        } else if !self.has_scope("collections.write") {
            Some("Pair again to use favorites and collections")
        } else if !self.has_scope("roms.user.write") {
            Some("Pair again to share when you last played")
        } else {
            None
        };
        if let Some(ui) = self.ui() {
            ui.set_needs_pairing(signed_in && prompt.is_some());
            ui.set_pair_prompt(prompt.unwrap_or_default().into());
        }
    }

    pub(super) fn pair_again(&self) {
        let Some(server) = self.client.borrow().as_ref().map(|c| c.base().to_string()) else {
            return;
        };
        if let Some(ui) = self.ui() {
            ui.set_server_url(server.trim_end_matches('/').into());
        }
        self.connect();
    }

    fn last_core(&self, rom_id: i64) -> Option<String> {
        self.shared.store.lock().unwrap().get(&core_key(rom_id))
    }

    /// How a game starts with the emulator chosen for its system, and the saves it syncs.
    pub(super) fn plan_launch(&self, detail: &GameDetail) -> Option<saves::Launch> {
        saves::plan_launch(
            &detail.platform_slug,
            &self.core_choices(),
            self.last_core(detail.id).as_deref(),
            detail.id,
            paths::game_save_dir(&paths::data_dir(), &self.server(), detail.id),
            &detail.title,
            detail.local_path.as_deref().map(std::path::Path::new),
        )
    }

    pub(super) fn saves_with(&self, core: &CoreInfo, detail: &GameDetail) -> GameSaves {
        saves::game_saves(
            core,
            detail.id,
            paths::game_save_dir(&paths::data_dir(), &self.server(), detail.id),
            &detail.title,
            detail.local_path.as_deref().map(std::path::Path::new),
        )
    }

    /// The saves in a game's folder, as the emulator it was last played with syncs them, so
    /// they are never uploaded under another emulator's name.
    pub(super) fn game_saves(&self, detail: &GameDetail) -> Option<GameSaves> {
        let core =
            crate::cores::played_core(&detail.platform_slug, self.last_core(detail.id).as_deref())?;
        Some(self.saves_with(core, detail))
    }

    pub(super) fn clear_conflict(&self) {
        if self.pending_launch.borrow_mut().take().is_some() {
            self.preparing.set(false);
        }
        if let Some(ui) = self.ui() {
            ui.set_save_conflict(false);
        }
    }

    pub(super) fn show_conflict(&self, pending: PendingLaunch) {
        if let Some(ui) = self.ui() {
            ui.set_conflict_text(conflict_text(&pending.conflict).into());
            ui.set_save_conflict(true);
            ui.set_game_status("".into());
        }
        *self.pending_launch.borrow_mut() = Some(pending);
    }

    pub(super) fn keep_save(&self, keep: Keep) {
        let Some(pending) = self.pending_launch.borrow_mut().take() else {
            return;
        };
        let game = pending.saves.clone();
        let (Some(client), Some(device)) = (self.client.borrow().clone(), self.sync_device())
        else {
            self.preparing.set(false);
            if let Some(ui) = self.ui() {
                ui.set_save_conflict(false);
            }
            return;
        };
        if let Some(ui) = self.ui() {
            ui.set_save_conflict(false);
            ui.set_game_status("Syncing your save…".into());
        }
        let conflict = pending.conflict.clone();
        self.shared.rt.spawn(async move {
            let result = saves::resolve_sram(&client, &device, &game, &conflict, keep).await;
            on_ui(move |c| {
                if let Err(e) = result {
                    tracing::warn!("resolving save conflict: {e}");
                }
                c.start_game(pending.detail, pending.rom, pending.info, pending.core);
            });
        });
    }

    pub(super) fn after_play(&self, detail: GameDetail, identity: Option<CoreIdentity>) {
        if let Some((_, version)) = &identity {
            self.shared
                .store
                .lock()
                .unwrap()
                .set(&version_key(detail.id), version);
        }
        let Some(game) = self.game_saves(&detail) else {
            return;
        };
        let Some(device) = self.sync_device() else {
            self.shared.store.lock().unwrap().add_pending(detail.id);
            return self.game_status(
                detail.id,
                "Save sync is off. Choose \"Pair again to turn on save sync\" in the library."
                    .into(),
            );
        };
        let client = self.client.borrow().clone();
        let Some(client) = client.filter(|_| !self.offline.get()) else {
            self.shared.store.lock().unwrap().add_pending(detail.id);
            return self.game_status(
                detail.id,
                "Saves will sync when the server is reachable.".into(),
            );
        };
        let store = self.shared.store.clone();
        let version = identity.map(|(_, v)| v);
        let id = detail.id;
        self.syncing_game.set(Some(id));
        self.shared.rt.spawn(async move {
            let result = sync_game(&client, &store, &device, &game, version.as_deref()).await;
            on_ui(move |c| c.after_play_synced(id, result));
        });
    }

    fn after_play_synced(&self, id: i64, result: Result<SramOutcome, Error>) {
        self.syncing_game.set(None);
        let status = match result {
            Ok(SramOutcome::Conflict(_)) => {
                "A newer save is on the server. You'll be asked which to keep next time you play."
                    .to_string()
            }
            Ok(_) => "Saves synced.".to_string(),
            Err(Error::Unreachable) => {
                self.shared.store.lock().unwrap().add_pending(id);
                "Saves will sync when the server is reachable.".to_string()
            }
            Err(e) => {
                tracing::warn!("syncing saves for {id}: {e}");
                format!("Couldn't sync saves: {e}.")
            }
        };
        self.game_status(id, status);
    }

    pub(super) fn sync_pending(&self) {
        let busy = self.running.borrow().is_some()
            || self.preparing.get()
            || self.syncing_game.get().is_some();
        let (Some(client), Some(device)) = (self.client.borrow().clone(), self.sync_device())
        else {
            return;
        };
        if busy {
            return;
        }
        let jobs: Vec<(GameSaves, Option<String>)> = {
            let store = self.shared.store.lock().unwrap();
            store
                .pending()
                .into_iter()
                .filter_map(|id| {
                    let detail = store.game(id)?;
                    Some((detail, store.get(&version_key(id))))
                })
                .collect::<Vec<_>>()
        }
        .into_iter()
        .filter_map(|(detail, version)| Some((self.game_saves(&detail)?, version)))
        .collect();
        if jobs.is_empty() {
            return;
        }
        let store: Arc<Mutex<Store>> = self.shared.store.clone();
        self.shared.rt.spawn(async move {
            for (game, version) in jobs {
                match sync_game(&client, &store, &device, &game, version.as_deref()).await {
                    Ok(_) => store.lock().unwrap().remove_pending(game.rom_id),
                    Err(Error::Unreachable) => {}
                    Err(e) => {
                        tracing::warn!("pending save sync for {}: {e}", game.rom_id);
                        store.lock().unwrap().remove_pending(game.rom_id);
                    }
                }
            }
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn core(id: &str) -> &'static CoreInfo {
        crate::cores::all_cores().find(|c| c.id == id).unwrap()
    }

    struct Switch {
        dir: tempfile::TempDir,
        store: Mutex<Store>,
        old: GameSaves,
        new: GameSaves,
    }

    fn switch(from: &str, to: &str, pending: bool) -> Switch {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join(saves::SRAM_FILE), b"card").unwrap();
        std::fs::write(dir.path().join(saves::AUTO_STATE), b"state").unwrap();
        let mut store = Store::open_in_memory().unwrap();
        store.set(&core_key(5), from);
        if pending {
            store.add_pending(5);
        }
        Switch {
            old: saves::game_saves(core(from), 5, dir.path().into(), "Game", None),
            new: saves::game_saves(core(to), 5, dir.path().into(), "Game", None),
            store: Mutex::new(store),
            dir,
        }
    }

    fn untouched(s: &Switch, from: &str) {
        assert!(s.dir.path().join(saves::SRAM_FILE).exists());
        assert!(s.dir.path().join(saves::AUTO_STATE).exists());
        assert!(!s.dir.path().join("backup").exists());
        let store = s.store.lock().unwrap();
        assert_eq!(store.pending(), [5]);
        assert_eq!(store.get(&core_key(5)).as_deref(), Some(from));
    }

    #[tokio::test]
    async fn unsynced_saves_offline_keep_the_game_on_its_emulator() {
        let s = switch("nestopia", "mesen", true);
        let previous = Some((core("nestopia"), &s.old));
        let err = switch_core(None, &s.store, &s.new, core("mesen"), previous)
            .await
            .unwrap_err();
        assert!(err.starts_with("Connect to your server once"), "{err}");
        assert!(
            err.contains("Nestopia UE") && err.contains("Mesen"),
            "{err}"
        );
        untouched(&s, "nestopia");
    }

    #[tokio::test]
    async fn unsynced_saves_that_fail_to_sync_keep_the_game_on_its_emulator() {
        let s = switch("nestopia", "mesen", true);
        let client = Client::new("http://127.0.0.1:9/".parse().unwrap()).with_token("t".into());
        let previous = Some((core("nestopia"), &s.old));
        let online = Some((&client, "device"));
        let err = switch_core(online, &s.store, &s.new, core("mesen"), previous)
            .await
            .unwrap_err();
        assert!(err.starts_with("Connect to your server once"), "{err}");
        untouched(&s, "nestopia");
    }

    #[tokio::test]
    async fn synced_saves_are_set_aside_when_the_emulator_changes() {
        let s = switch("nestopia", "mesen", false);
        let previous = Some((core("nestopia"), &s.old));
        switch_core(None, &s.store, &s.new, core("mesen"), previous)
            .await
            .unwrap();
        assert!(!s.dir.path().join(saves::SRAM_FILE).exists());
        assert!(!s.dir.path().join(saves::AUTO_STATE).exists());
        assert_eq!(
            s.store.lock().unwrap().get(&core_key(5)).as_deref(),
            Some("mesen")
        );
    }

    #[tokio::test]
    async fn emulators_with_the_same_save_ram_keep_the_in_game_save() {
        for (from, to) in [
            ("mednafen_psx_hw", "swanstation"),
            ("swanstation", "mednafen_psx_hw"),
        ] {
            let s = switch(from, to, false);
            let previous = Some((core(from), &s.old));
            switch_core(None, &s.store, &s.new, core(to), previous)
                .await
                .unwrap();
            assert_eq!(
                std::fs::read(s.dir.path().join(saves::SRAM_FILE)).unwrap(),
                b"card",
                "{from} to {to}"
            );
            assert!(
                !s.dir.path().join(saves::AUTO_STATE).exists(),
                "states are core-bound"
            );
            assert_eq!(s.new.save_emulator, to, "it syncs under the new name");
        }
    }

    #[test]
    fn conflict_text_names_both_times() {
        let text = conflict_text(&SramConflict {
            save_id: Some(1),
            server_updated_at: Some("2026-09-26T10:00:00+00:00".into()),
            local_updated_at: None,
        });
        assert!(text.contains("Server: 2026-09-26 10:00:00 UTC"));
        assert!(text.contains("This computer: unknown"));
        let text = conflict_text(&SramConflict {
            save_id: Some(1),
            server_updated_at: Some("2026-09-26T12:00:00.5+02:00".into()),
            local_updated_at: None,
        });
        assert!(text.contains("Server: 2026-09-26 10:00:00 UTC"));
    }
}
