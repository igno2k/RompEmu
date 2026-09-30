//! PS2 memory card images, read and edited as the save folders games wrote into them.
//!
//! LRPS2, the PCSX2 core Romp plays PS2 games with on x86_64 Windows and Linux, keeps a
//! memory card as one 8 MB image and has no folder-card mode at all. RomM's other clients carry
//! a PS2 save as a zip of the game's own card folders, so on those computers Romp reads the
//! same folders out of the image, and writes them back into it.
//!
//! The image is the console's own file system, as documented by mymc and PCSX2:
//!
//! - A superblock on page 0 gives the geometry: 512-byte pages, two to a cluster, 8192
//!   clusters.
//! - Each page may be followed by 16 bytes of ECC. PCSX2 and LRPS2 both write them
//!   (8,650,752 bytes, 528 per page); a card without them is 8,388,608.
//! - Clusters are chained by a FAT reached through a two-level index: the superblock's
//!   `ifc_list` names indirect clusters, which name FAT clusters, which hold one 32-bit entry
//!   per cluster. Bit 31 marks a cluster in use, and 0x7FFFFFFF in the low bits ends a chain.
//! - A directory is a file of 512-byte entries, "." and ".." first. File and directory
//!   clusters count from `alloc_offset`; the FAT and indirect clusters do not.
//!
//! Ported from Ludo's `engine/romm_sync_engine/ps2_memcard.py` (rommapp/ludo, GPL-3.0), the
//! RomM organisation's own client, so the two edit a card the same way: a save folder is
//! deleted the way the console deletes one, its entry kept with the exists bit cleared, and a
//! new one is written into free clusters, so every other folder's clusters stay byte for byte
//! as they were. The ECC matches bazzite-maint's `romm-save-import`, checked against real cards.

use super::meta::{self, file_meta_key, is_meta};
use super::rule::UnitRule;
use super::{Files, Unit};

pub const PAGE: usize = 512;
const SPARE: usize = 16;
const PAGES: usize = 16384;
const PAGES_PER_CLUSTER: usize = 2;
const CLUSTER: usize = PAGE * PAGES_PER_CLUSTER;
const WORDS: u32 = (CLUSTER / 4) as u32;
const CLUSTERS: u32 = (PAGES / PAGES_PER_CLUSTER) as u32;
/// An image with the ECC spare area after every page, as PCSX2 and LRPS2 write them.
pub const IMAGE_SIZE: usize = PAGES * (PAGE + SPARE);
/// An image without it.
pub const PLAIN_SIZE: usize = PAGES * PAGE;
/// The first erase block, superblock first, as a PCSX2 folder card keeps it in
/// `_pcsx2_superblock`.
pub const SUPERBLOCK_BLOCK: usize = PAGE * 16;
pub const MAGIC: &[u8; 28] = b"Sony PS2 Memory Card Format ";
const ENTRY: usize = 512;
const MODE_EXISTS: u16 = 0x8000;
const MODE_DIR: u16 = 0x0020;
const MODE_FILE: u16 = 0x0010;
const DIR_MODE: u16 = meta::DIR_MODE;
const FILE_MODE: u16 = meta::FILE_MODE;
const IN_USE: u32 = 0x8000_0000;
const CHAIN_END: u32 = 0x7FFF_FFFF;
/// The longest name a card entry holds, its terminating zero aside.
pub const NAME_MAX: usize = 31;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CardError(pub String);

impl std::fmt::Display for CardError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for CardError {}

type Result<T> = std::result::Result<T, CardError>;

fn fail<T>(message: impl Into<String>) -> Result<T> {
    Err(CardError(message.into()))
}

fn u16_at(bytes: &[u8], at: usize) -> u16 {
    u16::from_le_bytes([bytes[at], bytes[at + 1]])
}

fn u32_at(bytes: &[u8], at: usize) -> u32 {
    u32::from_le_bytes([bytes[at], bytes[at + 1], bytes[at + 2], bytes[at + 3]])
}

// ── ECC ─────────────────────────────────────────────────────────────────
// Three bytes per 128-byte chunk, twelve per page, then four zero bytes of the 16-byte spare
// area: the console's Hamming code, as mymc's ps2mc_ecc computes it.

fn parity(b: u8) -> u8 {
    (b.count_ones() & 1) as u8
}

fn column(b: u8) -> u8 {
    [0x55, 0x33, 0x0F, 0x00, 0xAA, 0xCC, 0xF0]
        .iter()
        .enumerate()
        .fold(0, |acc, (i, &mask)| acc | (parity(b & mask) << i))
}

fn ecc_chunk(chunk: &[u8]) -> [u8; 3] {
    let (mut col, mut line0, mut line1) = (0x77u8, 0x7Fu8, 0x7Fu8);
    for (i, &b) in chunk.iter().enumerate() {
        col ^= column(b);
        if parity(b) == 1 {
            line0 ^= !(i as u8);
            line1 ^= i as u8;
        }
    }
    [col, line0 & 0x7F, line1]
}

/// The 16-byte spare area of one 512-byte page.
pub fn page_ecc(page: &[u8]) -> [u8; SPARE] {
    let mut spare = [0u8; SPARE];
    for (i, chunk) in page.chunks(128).enumerate() {
        spare[i * 3..i * 3 + 3].copy_from_slice(&ecc_chunk(chunk));
    }
    spare
}

// ── timestamps ─────────────────────────────────────────────────────────
// A card keeps the console's clock, which runs on Japan time: unused, second, minute, hour,
// day, month, then the year as a u16.

const JST: i64 = 9 * 3600;

/// A card timestamp for a moment in Unix seconds.
pub fn tod(unix: i64) -> [u8; 8] {
    let local = unix + JST;
    let (y, m, d) = crate::sync::civil_from_days(local.div_euclid(86_400));
    let s = local.rem_euclid(86_400);
    let year = (y.clamp(0, i64::from(u16::MAX)) as u16).to_le_bytes();
    [
        0,
        (s % 60) as u8,
        (s / 60 % 60) as u8,
        (s / 3600) as u8,
        d as u8,
        m as u8,
        year[0],
        year[1],
    ]
}

/// The Unix time of a card timestamp, or None where the fields are not a real date, which
/// unformatted and hand-built entries often are.
pub fn unix(tod: &[u8]) -> Option<i64> {
    let (sec, min, hour, day, month) = (tod[1], tod[2], tod[3], tod[4], tod[5]);
    let year = i64::from(u16_at(tod, 6));
    let valid = (1..=12).contains(&month)
        && (1..=31).contains(&day)
        && hour < 24
        && min < 60
        && sec < 60
        && year >= 1990;
    valid.then(|| {
        crate::sync::days_from_civil(year, u32::from(month), u32::from(day)) * 86_400
            + i64::from(hour) * 3600
            + i64::from(min) * 60
            + i64::from(sec)
            - JST
    })
}

// ── names ──────────────────────────────────────────────────────────────

/// Whether a name can be a save folder or file on a card and, unchanged, a file on any
/// computer: ASCII, at most 31 bytes, and nothing a file system reads as a separator or
/// refuses.
pub fn valid_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= NAME_MAX
        && name != "."
        && name != ".."
        && name
            .bytes()
            .all(|b| (0x20..0x7F).contains(&b) && !b"/\\:*?\"<>|".contains(&b))
}

fn check_name(name: &str) -> Result<()> {
    if valid_name(name) {
        Ok(())
    } else {
        fail(format!("{name:?} can't be a memory card entry"))
    }
}

/// The name a metadata file gives an entry, checked against the card's rules and against
/// the host name PCSX2 would give it.
fn check_real_name(real: &[u8], host: &str) -> Result<()> {
    let ok = !real.is_empty()
        && real.len() <= NAME_MAX
        && !real.iter().any(|b| b"/?*".contains(b) || *b < 0x20)
        && meta::clean(real) == host.as_bytes();
    if ok {
        Ok(())
    } else {
        fail(format!(
            "the metadata of {host} names it {:?}, which doesn't fit",
            String::from_utf8_lossy(real)
        ))
    }
}

/// One directory entry: a save folder, or a file inside one.
#[derive(Debug, Clone)]
pub struct Entry {
    mode: u16,
    length: u32,
    cluster: u32,
    modified: [u8; 8],
    /// The name PCSX2 gives its host file or folder, the card's name cleaned.
    name: String,
    raw: Vec<u8>,
}

impl Entry {
    fn parse(raw: &[u8]) -> Self {
        Entry {
            mode: u16_at(raw, 0),
            length: u32_at(raw, 4),
            cluster: u32_at(raw, 16),
            modified: raw[24..32].try_into().expect("8 bytes"),
            name: String::from_utf8_lossy(&meta::clean(meta::real_name(raw))).into_owned(),
            raw: raw.to_vec(),
        }
    }

    fn exists(&self) -> bool {
        self.mode & MODE_EXISTS != 0
    }

    fn is_dir(&self) -> bool {
        self.mode & MODE_DIR != 0
    }

    pub fn name(&self) -> &str {
        &self.name
    }

    /// When the entry was last written, in Unix seconds.
    pub fn modified(&self) -> Option<i64> {
        unix(&self.modified)
    }
}

fn entry(
    name: &str,
    mode: u16,
    length: u32,
    cluster: u32,
    when: [u8; 8],
    dir_entry: u32,
) -> Vec<u8> {
    let mut raw = vec![0u8; ENTRY];
    raw[0..2].copy_from_slice(&mode.to_le_bytes());
    raw[4..8].copy_from_slice(&length.to_le_bytes());
    raw[8..16].copy_from_slice(&when);
    raw[16..20].copy_from_slice(&cluster.to_le_bytes());
    raw[20..24].copy_from_slice(&dir_entry.to_le_bytes());
    raw[24..32].copy_from_slice(&when);
    raw[64..64 + name.len()].copy_from_slice(name.as_bytes());
    raw
}

// ── the card ───────────────────────────────────────────────────────────

/// A formatted PS2 memory card image, held in memory.
pub struct Card {
    data: Vec<u8>,
    stride: usize,
    alloc_offset: u32,
    alloc_end: u32,
    root: u32,
    ifc: [u32; 32],
}

/// Whether an image is one no game has formatted yet: empty, or erased flash throughout, which
/// is how PCSX2 and LRPS2 create a card.
pub fn is_blank(data: &[u8]) -> bool {
    data.iter().all(|&b| b == 0xFF)
}

impl Card {
    pub fn parse(data: Vec<u8>) -> Result<Self> {
        let stride = match data.len() {
            IMAGE_SIZE => PAGE + SPARE,
            PLAIN_SIZE => PAGE,
            n => return fail(format!("a PS2 memory card is 8 MB, this file is {n} bytes")),
        };
        if !data.starts_with(MAGIC) {
            return fail("the memory card is not formatted");
        }
        let (page_len, per_cluster) = (u16_at(&data, 0x28), u16_at(&data, 0x2A));
        if usize::from(page_len) != PAGE || usize::from(per_cluster) != PAGES_PER_CLUSTER {
            return fail(format!(
                "unsupported memory card geometry: {page_len}-byte pages, {per_cluster} to a cluster"
            ));
        }
        let (clusters, alloc_offset, alloc_end) = (
            u32_at(&data, 0x30),
            u32_at(&data, 0x34),
            u32_at(&data, 0x38),
        );
        if clusters != CLUSTERS || alloc_offset.saturating_add(alloc_end) > CLUSTERS {
            return fail("the memory card's superblock describes another size of card");
        }
        let mut ifc = [0u32; 32];
        for (i, slot) in ifc.iter_mut().enumerate() {
            *slot = u32_at(&data, 0x50 + i * 4);
        }
        Ok(Card {
            root: u32_at(&data, 0x3C),
            data,
            stride,
            alloc_offset,
            alloc_end,
            ifc,
        })
    }

    pub fn image(self) -> Vec<u8> {
        self.data
    }

    fn has_ecc(&self) -> bool {
        self.stride != PAGE
    }

    /// The first erase block without its ECC: what a folder card keeps as `_pcsx2_superblock`.
    pub fn superblock_block(&self) -> Vec<u8> {
        (0..SUPERBLOCK_BLOCK / PAGE)
            .flat_map(|p| {
                self.data[p * self.stride..p * self.stride + PAGE]
                    .iter()
                    .copied()
            })
            .collect()
    }

    // ── clusters and the FAT ──

    fn cluster(&self, n: u32) -> Result<Vec<u8>> {
        if n >= CLUSTERS {
            return fail(format!("cluster {n} is outside the memory card"));
        }
        let first = n as usize * PAGES_PER_CLUSTER;
        Ok((first..first + PAGES_PER_CLUSTER)
            .flat_map(|p| {
                self.data[p * self.stride..p * self.stride + PAGE]
                    .iter()
                    .copied()
            })
            .collect())
    }

    /// Where word `index` of cluster `n` is in the image.
    fn word_at(&self, n: u32, index: u32) -> Result<(usize, usize)> {
        if n >= CLUSTERS || index >= WORDS {
            return fail(format!("cluster {n} is outside the memory card"));
        }
        let byte = index as usize * 4;
        let page = n as usize * PAGES_PER_CLUSTER + byte / PAGE;
        Ok((page, page * self.stride + byte % PAGE))
    }

    fn word(&self, n: u32, index: u32) -> Result<u32> {
        let (_, at) = self.word_at(n, index)?;
        Ok(u32_at(&self.data, at))
    }

    /// The FAT cluster and index holding the entry for allocatable cluster `n`.
    fn fat_slot(&self, n: u32) -> Result<(u32, u32)> {
        if n >= self.alloc_end {
            return fail(format!("cluster {n} is beyond the memory card's FAT"));
        }
        let (indirect, offset) = (n / WORDS, n % WORDS);
        let (double, indirect_offset) = (indirect / WORDS, indirect % WORDS);
        let Some(&indirect_cluster) = self.ifc.get(double as usize) else {
            return fail(format!("cluster {n} is beyond the memory card's FAT"));
        };
        Ok((self.word(indirect_cluster, indirect_offset)?, offset))
    }

    fn fat(&self, n: u32) -> Result<u32> {
        let (cluster, index) = self.fat_slot(n)?;
        self.word(cluster, index)
    }

    fn chain(&self, start: u32) -> Result<Vec<u32>> {
        let mut out = Vec::new();
        let mut n = start;
        loop {
            if out.len() > self.alloc_end as usize {
                return fail(format!("a cluster chain on the memory card loops at {n}"));
            }
            let next = self.fat(n)?;
            if next & IN_USE == 0 {
                return fail(format!(
                    "a cluster chain on the memory card runs into free cluster {n}"
                ));
            }
            out.push(n);
            if next & CHAIN_END == CHAIN_END {
                return Ok(out);
            }
            n = next & CHAIN_END;
        }
    }

    fn read(&self, start: u32, length: usize) -> Result<Vec<u8>> {
        if length == 0 {
            return Ok(Vec::new());
        }
        let need = length.div_ceil(CLUSTER);
        let chain = self.chain(start)?;
        if chain.len() < need {
            return fail(format!(
                "a file on the memory card is shorter than its entry says ({length} bytes)"
            ));
        }
        let mut data = Vec::with_capacity(need * CLUSTER);
        for &c in &chain[..need] {
            data.extend(self.cluster(c + self.alloc_offset)?);
        }
        data.truncate(length);
        Ok(data)
    }

    // ── directories ──

    fn entries(&self, cluster: u32, count: u32) -> Result<Vec<Entry>> {
        let raw = self.read(cluster, count as usize * ENTRY)?;
        Ok(raw.chunks(ENTRY).map(Entry::parse).collect())
    }

    fn root_raw(&self) -> Result<Vec<u8>> {
        // The root's own length, its entry count, is in its "." entry.
        let own = Entry::parse(&self.read(self.root, ENTRY)?);
        self.read(self.root, own.length as usize * ENTRY)
    }

    /// The save folders at the root of the card, deleted ones left out.
    pub fn folders(&self) -> Result<Vec<Entry>> {
        Ok(self
            .root_raw()?
            .chunks(ENTRY)
            .skip(2)
            .map(Entry::parse)
            .filter(|e| e.exists() && e.is_dir())
            .collect())
    }

    /// A save folder as a PCSX2 folder card holds it: its files by host name, and the
    /// metadata files PCSX2 writes for entries that need them (see [`meta`]), in canonical
    /// form. A save folder never holds another folder, and one that does is refused rather
    /// than copied in part, as is one whose names no computer can hold.
    pub fn files(&self, folder: &Entry) -> Result<Files> {
        self.folder_files(folder, true)
    }

    /// A save folder's files; `strict` refuses names a computer can't hold, which only
    /// matters for folders that are going to one.
    fn folder_files(&self, folder: &Entry, strict: bool) -> Result<Files> {
        if strict && !valid_name(&folder.name) {
            return fail(format!("{:?} can't be a folder on a computer", folder.name));
        }
        let folder_cleaned =
            meta::clean(meta::real_name(&folder.raw)) != meta::real_name(&folder.raw);
        let mut files = Files::new();
        let mut metas = Files::new();
        for e in self.entries(folder.cluster, folder.length)?.iter().skip(2) {
            if !e.exists() {
                continue;
            }
            if e.is_dir() || e.mode & MODE_FILE == 0 {
                return fail(format!("{}/{} is not a file", folder.name, e.name));
            }
            if strict && (!valid_name(&e.name) || is_meta(&e.name) || e.name == meta::INDEX) {
                return fail(format!(
                    "{}/{:?} can't be a file on a computer",
                    folder.name, e.name
                ));
            }
            let data = self.read(e.cluster, e.length as usize)?;
            if meta::is_nonstandard(&e.raw, FILE_MODE, folder_cleaned) {
                metas.insert(
                    file_meta_key(&e.name),
                    meta::canonical_entry(&e.raw, e.length),
                );
            }
            if files.insert(e.name.clone(), data).is_some() {
                return fail(format!("{} holds two files named {}", folder.name, e.name));
            }
        }
        if meta::is_nonstandard(&folder.raw, DIR_MODE, false) {
            let count = 2 + files.len() as u32;
            files.insert(
                meta::DIR_META.into(),
                meta::canonical_entry(&folder.raw, count),
            );
        }
        files.extend(metas);
        Ok(files)
    }

    /// Every save folder on the card with its files, by host name, for comparing a card
    /// before and after a change; other games' folders need no names a computer can hold.
    pub fn all_folders(&self) -> Result<Unit> {
        let mut unit = Unit::new();
        for folder in self.folders()? {
            let files = self.folder_files(&folder, false)?;
            if unit.insert(folder.name.clone(), files).is_some() {
                return fail(format!("the card holds two folders named {}", folder.name));
            }
        }
        Ok(unit)
    }

    // ── writing ──

    fn set_page(&mut self, n: usize, page: &[u8]) {
        let start = n * self.stride;
        self.data[start..start + PAGE].copy_from_slice(page);
        if self.has_ecc() {
            let spare = page_ecc(page);
            self.data[start + PAGE..start + self.stride].copy_from_slice(&spare);
        }
    }

    fn set_cluster(&mut self, n: u32, data: &[u8]) {
        let mut cluster = data.to_vec();
        cluster.resize(CLUSTER, 0);
        for (i, page) in cluster.chunks(PAGE).enumerate() {
            self.set_page(n as usize * PAGES_PER_CLUSTER + i, page);
        }
    }

    fn set_word(&mut self, n: u32, index: u32, value: u32) -> Result<()> {
        let (page, at) = self.word_at(n, index)?;
        self.data[at..at + 4].copy_from_slice(&value.to_le_bytes());
        let start = page * self.stride;
        let content = self.data[start..start + PAGE].to_vec();
        self.set_page(page, &content);
        Ok(())
    }

    fn set_fat(&mut self, n: u32, value: u32) -> Result<()> {
        let (cluster, index) = self.fat_slot(n)?;
        self.set_word(cluster, index, value)
    }

    /// `count` free clusters, chained together.
    fn allocate(&mut self, count: usize) -> Result<Vec<u32>> {
        let mut free = Vec::with_capacity(count);
        for n in 0..self.alloc_end {
            if free.len() == count {
                break;
            }
            if self.fat(n)? & IN_USE == 0 {
                free.push(n);
            }
        }
        if free.len() < count {
            return fail("the memory card is full");
        }
        for (i, &n) in free.iter().enumerate() {
            let next = free.get(i + 1).copied().unwrap_or(CHAIN_END);
            self.set_fat(n, IN_USE | next)?;
        }
        Ok(free)
    }

    fn free(&mut self, start: u32) -> Result<()> {
        for n in self.chain(start)? {
            self.set_fat(n, CHAIN_END)?;
        }
        Ok(())
    }

    /// Stores `data` in newly allocated clusters, returning the first.
    fn write_new(&mut self, data: &[u8]) -> Result<u32> {
        let chain = self.allocate(data.len().div_ceil(CLUSTER).max(1))?;
        for (i, &n) in chain.iter().enumerate() {
            let end = ((i + 1) * CLUSTER).min(data.len());
            let part = data.get(i * CLUSTER..end).unwrap_or_default();
            self.set_cluster(n + self.alloc_offset, part);
        }
        Ok(chain[0])
    }

    /// Overwrites the chain at `start` with `data`, growing it where needed.
    fn rewrite(&mut self, start: u32, data: &[u8]) -> Result<()> {
        let mut chain = self.chain(start)?;
        let need = data.len().div_ceil(CLUSTER).max(1);
        if need > chain.len() {
            let extra = self.allocate(need - chain.len())?;
            let last = *chain.last().expect("a chain has a cluster");
            self.set_fat(last, IN_USE | extra[0])?;
            chain.extend(extra);
        }
        for (i, &n) in chain[..need].iter().enumerate() {
            let end = ((i + 1) * CLUSTER).min(data.len());
            let part = data.get(i * CLUSTER..end).unwrap_or_default();
            self.set_cluster(n + self.alloc_offset, part);
        }
        Ok(())
    }

    /// Removes every root save folder named `name` and frees its clusters, the way the console
    /// does: the entry stays, with its exists bit cleared, for the next folder to reuse.
    pub fn delete_folder(&mut self, name: &str) -> Result<bool> {
        let mut raw = self.root_raw()?;
        let mut deleted = false;
        for i in 2..raw.len() / ENTRY {
            let e = Entry::parse(&raw[i * ENTRY..(i + 1) * ENTRY]);
            if !(e.exists() && e.is_dir() && e.name == name) {
                // Matched by host name, which is what a unit names folders by.
                continue;
            }
            for child in self.entries(e.cluster, e.length)?.iter().skip(2) {
                if child.exists() && child.length > 0 {
                    self.free(child.cluster)?;
                }
            }
            self.free(e.cluster)?;
            raw[i * ENTRY..i * ENTRY + 2].copy_from_slice(&(e.mode & !MODE_EXISTS).to_le_bytes());
            deleted = true;
        }
        if deleted {
            self.rewrite(self.root, &raw)?;
        }
        Ok(deleted)
    }

    /// Writes a new root save folder holding `files`, dated `when`. Where the files carry
    /// PCSX2 metadata, the entries take their real name, mode, attributes and dates from it,
    /// as PCSX2 does when it loads a folder card.
    pub fn add_folder(&mut self, name: &str, files: &Files, when: [u8; 8]) -> Result<()> {
        check_name(name)?;
        let dir_template = files.get(meta::DIR_META).map(|raw| meta::loaded(raw, name));
        if let Some(t) = &dir_template {
            check_real_name(meta::real_name(t), name)?;
        }
        let mut raw = self.root_raw()?;
        let slot = (2..raw.len() / ENTRY)
            .find(|&i| !Entry::parse(&raw[i * ENTRY..(i + 1) * ENTRY]).exists())
            .unwrap_or(raw.len() / ENTRY);
        let mut body = Vec::new();
        let mut count = 2u32;
        for (file, data) in files.iter().filter(|(key, _)| !is_meta(key)) {
            check_name(file)?;
            let Ok(length) = u32::try_from(data.len()) else {
                return fail(format!("{name}/{file} is too large for a memory card"));
            };
            let template = files
                .get(&file_meta_key(file))
                .map(|raw| meta::loaded(raw, file));
            if let Some(t) = &template {
                check_real_name(meta::real_name(t), file)?;
            }
            // An empty file has no clusters, which the console marks with an all-ones cluster.
            let cluster = if data.is_empty() {
                u32::MAX
            } else {
                self.write_new(data)?
            };
            body.extend(match template {
                Some(t) => placed(t, length, cluster),
                None => entry(file, FILE_MODE, length, cluster, when, 0),
            });
            count += 1;
        }
        // "." points back at this folder's own entry in the root, ".." is empty; both carry
        // the folder mode, which is what the console writes.
        let mut own = entry(".", DIR_MODE, 0, self.root, when, slot as u32);
        own.extend(entry("..", DIR_MODE, 0, 0, when, 0));
        own.extend(body);
        let here = self.write_new(&own)?;
        let new = match dir_template {
            Some(t) => placed(t, count, here),
            None => entry(name, DIR_MODE, count, here, when, 0),
        };
        if slot * ENTRY < raw.len() {
            raw[slot * ENTRY..(slot + 1) * ENTRY].copy_from_slice(&new);
        } else {
            raw.extend(new);
            let entries = (raw.len() / ENTRY) as u32;
            raw[4..8].copy_from_slice(&entries.to_le_bytes());
        }
        self.rewrite(self.root, &raw)
    }
}

/// An entry from PCSX2 metadata, with the length and first cluster it has on this card.
fn placed(mut entry: Vec<u8>, length: u32, cluster: u32) -> Vec<u8> {
    entry[4..8].copy_from_slice(&length.to_le_bytes());
    entry[16..20].copy_from_slice(&cluster.to_le_bytes());
    entry
}

/// The superblock page of a freshly formatted 8 MB card, as the console formats one. The
/// trailing fields are copied from cards the console formatted (Ludo's `format_card`).
pub fn formatted_superblock_page() -> [u8; PAGE] {
    let mut sb = [0u8; PAGE];
    sb[..28].copy_from_slice(MAGIC);
    sb[28..35].copy_from_slice(b"1.2.0.0");
    let geometry: [u32; 6] = [8192, 41, 8135, 0, 1023, 1022];
    for (i, v) in [512u16, 2, 16, 0xFF00].iter().enumerate() {
        sb[0x28 + i * 2..0x2A + i * 2].copy_from_slice(&v.to_le_bytes());
    }
    for (i, v) in geometry.iter().enumerate() {
        sb[0x30 + i * 4..0x34 + i * 4].copy_from_slice(&v.to_le_bytes());
    }
    sb[0x50..0x54].copy_from_slice(&8u32.to_le_bytes());
    sb[0xD0..0x150].fill(0xFF);
    sb[0x150] = 2;
    sb[0x151] = 0x2B;
    let trailer: [u32; 12] = [
        0x400,
        0x100,
        8,
        0xFFFF_FFFF,
        0,
        0,
        0,
        0x1F41,
        0,
        0,
        0xFFFF_FFFF,
        0xFFFF_FFFF,
    ];
    for (i, v) in trailer.iter().enumerate() {
        sb[0x154 + i * 4..0x158 + i * 4].copy_from_slice(&v.to_le_bytes());
    }
    sb[0x184..].fill(0xFF);
    sb
}

/// A blank, formatted 8 MB card, laid out as the console formats one. PCSX2 and LRPS2 create
/// cards unformatted and leave formatting to the first game that saves, so a pull before that
/// has to format the card itself.
pub fn format_card(ecc: bool, when: [u8; 8]) -> Vec<u8> {
    let stride = PAGE + if ecc { SPARE } else { 0 };
    let mut card = Card {
        data: vec![0xFF; PAGES * stride],
        stride,
        alloc_offset: 41,
        alloc_end: 8135,
        root: 0,
        ifc: [0; 32],
    };
    card.ifc[0] = 8;
    card.set_page(0, &formatted_superblock_page());
    let mut indirect: Vec<u8> = (9u32..41).flat_map(u32::to_le_bytes).collect();
    indirect.resize(CLUSTER, 0xFF);
    card.set_cluster(8, &indirect);
    let fat: Vec<u32> = std::iter::once(u32::MAX)
        .chain(std::iter::repeat_n(CHAIN_END, 8134))
        .chain(std::iter::repeat(u32::MAX))
        .take(32 * WORDS as usize)
        .collect();
    for (i, words) in fat.chunks(WORDS as usize).enumerate() {
        let bytes: Vec<u8> = words.iter().flat_map(|w| w.to_le_bytes()).collect();
        card.set_cluster(9 + i as u32, &bytes);
    }
    let mut root = entry(".", DIR_MODE, 2, 0, when, 0);
    root.extend(entry("..", 0xA426, 0, 0, when, 0));
    card.set_cluster(41, &root);
    card.data
}

/// The folders of a card that `owns` claims, with their files. A blank card holds none.
pub fn read_unit(data: &[u8], owns: &dyn Fn(&str) -> bool) -> Result<Unit> {
    if is_blank(data) {
        return Ok(Unit::new());
    }
    let card = Card::parse(data.to_vec())?;
    let mut unit = Unit::new();
    for folder in card.folders()? {
        if owns(folder.name()) {
            let files = card.files(&folder)?;
            if unit.insert(folder.name().to_string(), files).is_some() {
                return fail(format!(
                    "the card holds two folders named {}",
                    folder.name()
                ));
            }
        }
    }
    Ok(unit)
}

/// A card holding exactly `unit`, for tests to start from.
#[cfg(test)]
pub fn image_of(unit: &Unit, when: [u8; 8]) -> Vec<u8> {
    let mut card = Card::parse(format_card(true, when)).expect("a formatted card");
    for (name, files) in unit {
        card.add_folder(name, files, when)
            .expect("room on the card");
    }
    card.image()
}

/// The card `original` with the game's own folders replaced by those in `unit`, the shared
/// ones in it written only where the card has none, and every other folder left as it was.
/// A missing or blank card is formatted first, with the ECC LRPS2 writes. The result is read
/// back through a fresh parse before it is returned, and refused unless it holds exactly
/// that.
pub fn apply_unit(
    original: Option<&[u8]>,
    unit: &Unit,
    rule: &UnitRule,
    when: [u8; 8],
) -> Result<Vec<u8>> {
    if let Some(stray) = unit.keys().find(|name| !rule.owns(name)) {
        return fail(format!("{stray} is not a save folder of {}", rule.key()));
    }
    let mut card = match original {
        Some(bytes) if !is_blank(bytes) => Card::parse(bytes.to_vec())?,
        _ => Card::parse(format_card(true, when))?,
    };
    let before = card.all_folders()?;
    let replaced: Vec<String> = before.keys().filter(|n| rule.is_own(n)).cloned().collect();
    let added: Vec<&String> = unit
        .keys()
        .filter(|n| rule.is_own(n) || !before.contains_key(*n))
        .collect();
    for name in &replaced {
        card.delete_folder(name)?;
    }
    for name in &added {
        card.add_folder(name, &unit[*name], when)?;
    }
    let image = card.image();
    let after = Card::parse(image.clone())?.all_folders()?;
    let mut expected: Unit = before
        .into_iter()
        .filter(|(name, _)| !replaced.contains(name))
        .collect();
    for name in added {
        expected.insert(name.clone(), unit[name].clone());
    }
    if after != expected {
        let differs = after
            .keys()
            .chain(expected.keys())
            .find(|n| after.get(*n) != expected.get(*n))
            .cloned()
            .unwrap_or_default();
        return fail(format!(
            "{differs} did not read back from the memory card as it should"
        ));
    }
    Ok(image)
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    pub(crate) fn files(entries: &[(&str, &[u8])]) -> Files {
        entries
            .iter()
            .map(|(n, d)| ((*n).to_string(), d.to_vec()))
            .collect()
    }

    const WHEN: [u8; 8] = [0, 5, 4, 3, 2, 1, 0xEA, 0x07];

    fn rule(key: &str) -> UnitRule {
        UnitRule::new(key, &crate::ps2::GameDb::empty()).unwrap()
    }

    /// Gran Turismo 4, which reads Gran Turismo 3's saves (its GameDB memcardFilters).
    pub(crate) fn gt4() -> UnitRule {
        let db = crate::ps2::GameDb::parse(
            "SCUS-97328:\n  memcardFilters:\n    - \"SCUS-97328\"\n    - \"SCUS-97102\"\n",
        );
        UnitRule::new("SCUS-97328", &db).unwrap()
    }

    /// A metadata entry as PCSX2 writes one, in canonical form.
    pub(crate) fn meta_entry(name: &[u8], mode: u16, attr: u32, length: u32) -> Vec<u8> {
        let mut raw = entry("x", mode, length, 0, [0, 40, 25, 1, 5, 2, 0xE7, 0x07], 0);
        raw[0x20..0x24].copy_from_slice(&attr.to_le_bytes());
        raw[64..96].fill(0);
        raw[64..64 + name.len()].copy_from_slice(name);
        raw
    }

    fn card_with(folders: &[(&str, Files)]) -> Vec<u8> {
        let mut card = Card::parse(format_card(true, WHEN)).unwrap();
        for (name, f) in folders {
            card.add_folder(name, f, WHEN).unwrap();
        }
        card.image()
    }

    #[test]
    fn ecc_matches_the_console() {
        // Vectors from Ludo's page_ecc (rommapp/ludo) and bazzite-maint's romm-save-import,
        // which agree, and which were checked against real LRPS2 and PCSX2 cards.
        let erased = [0xFFu8; PAGE];
        assert_eq!(
            page_ecc(&erased),
            [0x77, 0x7F, 0x7F, 0x77, 0x7F, 0x7F, 0x77, 0x7F, 0x7F, 0x77, 0x7F, 0x7F, 0, 0, 0, 0]
        );
        let mut x: u32 = 12345;
        let noise: Vec<u8> = (0..PAGE)
            .map(|_| {
                x = x.wrapping_mul(1_103_515_245).wrapping_add(12345) & 0x7FFF_FFFF;
                (x >> 16) as u8
            })
            .collect();
        assert_eq!(
            page_ecc(&noise),
            [0, 12, 12, 22, 56, 71, 82, 56, 71, 0, 93, 93, 0, 0, 0, 0]
        );
    }

    #[test]
    fn timestamps_are_japan_time() {
        // 2026-09-30 12:00:00 UTC is 21:00 in Tokyo.
        let noon = 1_790_769_600;
        assert_eq!(tod(noon), [0, 0, 0, 21, 30, 9, 0xEA, 0x07]);
        assert_eq!(unix(&tod(noon)), Some(noon));
        assert_eq!(unix(&[0xFF; 8]), None);
    }

    #[test]
    fn a_formatted_card_is_empty_and_readable() {
        for ecc in [true, false] {
            let image = format_card(ecc, WHEN);
            assert_eq!(image.len(), if ecc { IMAGE_SIZE } else { PLAIN_SIZE });
            let card = Card::parse(image).unwrap();
            assert!(card.folders().unwrap().is_empty());
            assert_eq!(card.superblock_block().len(), SUPERBLOCK_BLOCK);
            assert_eq!(
                &card.superblock_block()[..PAGE],
                &formatted_superblock_page()
            );
        }
    }

    #[test]
    fn folders_round_trip() {
        let big: Vec<u8> = (0..5000u32).map(|i| (i % 251) as u8).collect();
        let a = files(&[
            ("icon.sys", b"icon"),
            ("BASLUS-20152AC04", &big),
            ("empty", b""),
        ]);
        let image = card_with(&[("BASLUS-20152AC04", a.clone())]);
        let card = Card::parse(image).unwrap();
        let folders = card.folders().unwrap();
        assert_eq!(folders.len(), 1);
        assert_eq!(folders[0].name(), "BASLUS-20152AC04");
        assert_eq!(folders[0].modified(), unix(&WHEN));
        assert_eq!(card.files(&folders[0]).unwrap(), a);
    }

    #[test]
    fn replacing_one_game_leaves_the_others_byte_for_byte() {
        let other = files(&[("icon.sys", b"other icon"), ("data", &[7u8; 3000])]);
        let old = files(&[("icon.sys", b"old icon"), ("save", &[1u8; 2000])]);
        let image = card_with(&[
            ("BASLUS-21693XX", other.clone()),
            ("BASLUS-20152AC04", old),
            ("BADATA-SYSTEM", files(&[("history", b"h")])),
        ]);
        let before = Card::parse(image.clone()).unwrap();
        let other_entry = before
            .folders()
            .unwrap()
            .into_iter()
            .find(|f| f.name() == "BASLUS-21693XX")
            .unwrap();
        let other_clusters: Vec<Vec<u8>> = before
            .entries(other_entry.cluster, other_entry.length)
            .unwrap()
            .iter()
            .skip(2)
            .flat_map(|e| before.chain(e.cluster).unwrap())
            .chain(before.chain(other_entry.cluster).unwrap())
            .map(|c| before.cluster(c + before.alloc_offset).unwrap())
            .collect();

        let new = files(&[("icon.sys", b"new icon"), ("save", &[2u8; 9000])]);
        let unit = Unit::from([("BASLUS-20152AC04".to_string(), new.clone())]);
        let owns = |n: &str| n.starts_with("BASLUS-20152");
        let written = apply_unit(Some(&image), &unit, &rule("SLUS-20152"), WHEN).unwrap();

        let after = Card::parse(written.clone()).unwrap();
        let all = after.all_folders().unwrap();
        assert_eq!(all["BASLUS-20152AC04"], new);
        assert_eq!(all["BASLUS-21693XX"], other);
        assert_eq!(all["BADATA-SYSTEM"], files(&[("history", b"h")]));
        let still: Vec<Vec<u8>> = before
            .entries(other_entry.cluster, other_entry.length)
            .unwrap()
            .iter()
            .skip(2)
            .flat_map(|e| before.chain(e.cluster).unwrap())
            .chain(before.chain(other_entry.cluster).unwrap())
            .map(|c| after.cluster(c + after.alloc_offset).unwrap())
            .collect();
        assert_eq!(
            still, other_clusters,
            "the other game's clusters are untouched"
        );
        assert_eq!(read_unit(&written, &owns).unwrap(), unit);
    }

    #[test]
    fn a_pull_removes_the_games_folders_it_no_longer_has() {
        let image = card_with(&[
            ("BASLUS-20152AC04", files(&[("a", b"1")])),
            ("BASLUS-20152SYS", files(&[("b", b"2")])),
        ]);
        let unit = Unit::from([("BASLUS-20152AC04".to_string(), files(&[("a", b"3")]))]);
        let owns = |n: &str| n.starts_with("BASLUS-20152");
        let written = apply_unit(Some(&image), &unit, &rule("SLUS-20152"), WHEN).unwrap();
        assert_eq!(read_unit(&written, &owns).unwrap(), unit);
    }

    #[test]
    fn shared_folders_are_written_only_where_the_card_has_none() {
        let gt4_save = files(&[("save", b"GT4 progress")]);
        let gt3_local = files(&[("garage", b"the GT3 garage as saved here")]);
        let image = card_with(&[
            ("BASCUS-97328GT4", files(&[("save", b"old GT4")])),
            ("BASCUS-97102GT3", gt3_local.clone()),
        ]);
        let all = |_: &str| true;
        // A pull without GT3 keeps the card's GT3.
        let without = Unit::from([("BASCUS-97328GT4".to_string(), gt4_save.clone())]);
        let written = apply_unit(Some(&image), &without, &gt4(), WHEN).unwrap();
        let after = read_unit(&written, &all).unwrap();
        assert_eq!(after["BASCUS-97102GT3"], gt3_local);
        assert_eq!(after["BASCUS-97328GT4"], gt4_save);
        // A pull with a stale GT3 doesn't overwrite it.
        let mut with = without.clone();
        with.insert("BASCUS-97102GT3".into(), files(&[("garage", b"stale")]));
        let written = apply_unit(Some(&image), &with, &gt4(), WHEN).unwrap();
        assert_eq!(
            read_unit(&written, &all).unwrap()["BASCUS-97102GT3"],
            gt3_local
        );
        // Where the card has no GT3, the pulled one is written.
        let bare = card_with(&[]);
        let written = apply_unit(Some(&bare), &with, &gt4(), WHEN).unwrap();
        assert_eq!(read_unit(&written, &all).unwrap(), with);
    }

    #[test]
    fn metadata_round_trips_through_the_card() {
        // A copy-protected save as the real BASLUS-20314-TS2-OPT: mode 0x842F on the folder.
        let mut save = files(&[
            ("icon.sys", b"icon"),
            ("BASLUS-20314-TS2-OPT", &[5u8; 1500]),
        ]);
        save.insert(
            meta::DIR_META.into(),
            meta_entry(b"BASLUS-20314-TS2-OPT", 0x842F, 0, 4),
        );
        save.insert(
            file_meta_key("icon.sys"),
            meta_entry(b"icon.sys", 0x8417, 0, 4),
        );
        // A folder whose PS2 name a computer can't hold: its host name is cleaned.
        let mut cleaned = files(&[("f", b"x")]);
        cleaned.insert(
            meta::DIR_META.into(),
            meta_entry(b"BASLUS-20314:B", DIR_MODE, 0, 3),
        );
        cleaned.insert(file_meta_key("f"), meta_entry(b"f", FILE_MODE, 0, 1));
        let unit = Unit::from([
            ("BASLUS-20314-TS2-OPT".to_string(), save),
            ("BASLUS-20314_B".to_string(), cleaned),
        ]);
        let written = apply_unit(None, &unit, &rule("SLUS-20314"), WHEN).unwrap();
        assert_eq!(read_unit(&written, &|_| true).unwrap(), unit);
        let card = Card::parse(written).unwrap();
        let folders = card.folders().unwrap();
        let ts2 = folders
            .iter()
            .find(|f| f.name() == "BASLUS-20314-TS2-OPT")
            .unwrap();
        assert_eq!(
            ts2.mode, 0x842F,
            "the card keeps the save's copy protection"
        );
        let b = folders
            .iter()
            .find(|f| f.name() == "BASLUS-20314_B")
            .unwrap();
        assert_eq!(
            meta::real_name(&b.raw),
            b"BASLUS-20314:B",
            "and its real name"
        );
    }

    #[test]
    fn another_games_odd_name_does_not_block_this_games_pull() {
        // A folder name with a character no computer's file system takes, on another game.
        let mut card = Card::parse(format_card(true, WHEN)).unwrap();
        card.add_folder("BASLUS-99999", &files(&[("a", b"x")]), WHEN)
            .unwrap();
        let mut raw = card.root_raw().unwrap();
        raw[2 * ENTRY + 64 + "BASLUS-99999".len()] = b'?';
        card.rewrite(card.root, &raw).unwrap();
        assert!(card.files(&card.folders().unwrap()[0]).is_err());
        let unit = Unit::from([("BASLUS-20152AC04".to_string(), files(&[("s", b"1")]))]);
        let written = apply_unit(Some(&card.image()), &unit, &rule("SLUS-20152"), WHEN).unwrap();
        assert_eq!(
            read_unit(&written, &|n| n.starts_with("BASLUS-20152")).unwrap(),
            unit
        );
    }

    #[test]
    fn metadata_that_names_another_folder_is_refused() {
        let mut odd = files(&[("f", b"x")]);
        odd.insert(
            meta::DIR_META.into(),
            meta_entry(b"BASLUS-20314ELSE", 0x842F, 0, 3),
        );
        let unit = Unit::from([("BASLUS-20314-TS2".to_string(), odd)]);
        assert!(apply_unit(None, &unit, &rule("SLUS-20314"), WHEN).is_err());
    }

    #[test]
    fn a_blank_or_missing_card_is_formatted_first() {
        let unit = Unit::from([("BESLES-50330X".to_string(), files(&[("f", b"x")]))]);
        let owns = |_: &str| true;
        let r = rule("SLES-50330");
        // Always the layout LRPS2 writes, with ECC, even over a blank card without it.
        let plain = apply_unit(Some(&vec![0xFF; PLAIN_SIZE]), &unit, &r, WHEN).unwrap();
        assert_eq!(plain.len(), IMAGE_SIZE);
        let fresh = apply_unit(None, &unit, &r, WHEN).unwrap();
        assert_eq!(fresh.len(), IMAGE_SIZE);
        assert_eq!(read_unit(&fresh, &owns).unwrap(), unit);
        assert!(read_unit(&[0xFF; 64], &owns).unwrap().is_empty());
    }

    #[test]
    fn what_is_not_a_card_is_refused() {
        assert!(Card::parse(vec![0; IMAGE_SIZE]).is_err());
        assert!(Card::parse(b"PK\x03\x04".to_vec()).is_err());
        let unit = Unit::new();
        assert!(apply_unit(Some(b"not a card"), &unit, &rule("SLUS-20152"), WHEN).is_err());
    }

    #[test]
    fn a_broken_chain_is_an_error_not_a_hang() {
        let mut image = card_with(&[("BASLUS-20152AC04", files(&[("a", &[1u8; 3000])]))]);
        // Point the root's FAT entry at itself.
        let mut card = Card::parse(image.clone()).unwrap();
        card.set_fat(0, IN_USE).unwrap();
        image = card.image();
        let card = Card::parse(image).unwrap();
        assert!(card.folders().is_err());
    }

    #[test]
    fn names_a_card_or_a_computer_cannot_hold_are_refused() {
        assert!(valid_name("BASLUS-20152AC04"));
        for bad in [
            "",
            ".",
            "..",
            "a/b",
            "a\\b",
            "x:y",
            &"n".repeat(32),
            "s\u{e9}ve",
        ] {
            assert!(!valid_name(bad), "{bad:?}");
        }
        let unit = Unit::from([("../evil".to_string(), files(&[("f", b"x")]))]);
        assert!(apply_unit(None, &unit, &rule("SLUS-20152"), WHEN).is_err());
    }

    #[test]
    fn the_card_fills_up_rather_than_overwriting() {
        let unit = Unit::from([(
            "BASLUS-20152BIG".to_string(),
            files(&[("f", &vec![0u8; 9_000_000])]),
        )]);
        assert!(apply_unit(None, &unit, &rule("SLUS-20152"), WHEN).is_err());
    }
}
