//! Validate an alternate filesystem view of a loaded ELF without loader locks.
//!
//! Linux overlay mounts may give stat() a different device/inode than maps.
//! Compare mapped ELF headers, load layout and a GNU build ID before trusting
//! the pathname. /proc/self/mem pread fails safely if a mapping disappears;
//! never dereference sampled addresses or use dl_iterate_phdr/dladdr here.
use super::Mapping;
use object::{
    elf,
    read::elf::{FileHeader, NoteIterator, ProgramHeader},
    LittleEndian as LE,
};
use std::{fs::File, os::unix::fs::FileExt};

type Header = elf::FileHeader64<LE>;
type Segment = elf::ProgramHeader64<LE>;
const HEADER_SIZE: usize = std::mem::size_of::<Header>();
const MAX_HEADERS: u16 = 256;
const MAX_METADATA: usize = 64 * 1024;

pub(super) fn matches(file: &File, target: &Mapping, mappings: &[Mapping]) -> bool {
    validate(file, target, mappings).is_some()
}

fn validate(file: &File, target: &Mapping, mappings: &[Mapping]) -> Option<()> {
    let mut initial = [0; HEADER_SIZE];
    file.read_exact_at(&mut initial, 0).ok()?;
    let header = Header::parse(&initial[..]).ok()?;
    let endian = header.endian().ok()?;
    if !matches!(header.e_type(endian), elf::ET_DYN | elf::ET_EXEC)
        || header.e_ehsize(endian) as usize != HEADER_SIZE
        || header.e_version(endian) != u32::from(elf::EV_CURRENT.0)
        || !(1..=MAX_HEADERS).contains(&header.e_phnum(endian))
        || header.e_phentsize(endian) as usize != std::mem::size_of::<Segment>()
    {
        return None;
    }
    let phoff = usize::try_from(header.e_phoff(endian)).ok()?;
    let end = phoff.checked_add(
        usize::from(header.e_phnum(endian)).checked_mul(std::mem::size_of::<Segment>())?,
    )?;
    if phoff < HEADER_SIZE || end > MAX_METADATA {
        return None;
    }
    let mut headers = vec![0; end];
    file.read_exact_at(&mut headers, 0).ok()?;
    if headers[..HEADER_SIZE] != initial {
        return None;
    }
    let segments = header.program_headers(endian, &headers[..]).ok()?;
    let file_size = file.metadata().ok()?.len();
    for segment in segments {
        let align = segment.p_align(endian);
        if segment
            .p_offset(endian)
            .checked_add(segment.p_filesz(endian))?
            > file_size
            || segment.p_filesz(endian) > segment.p_memsz(endian)
            || segment
                .p_vaddr(endian)
                .checked_add(segment.p_memsz(endian))
                .is_none()
            || (align > 1
                && (!align.is_power_of_two()
                    || segment.p_offset(endian) % align != segment.p_vaddr(endian) % align))
        {
            return None;
        }
    }
    let header_segment = segments.iter().find(|s| {
        s.p_type(endian) == elf::PT_LOAD
            && s.p_flags(endian).contains(elf::PF_R)
            && s.p_offset(endian) == 0
            && s.p_filesz(endian) >= end as u64
    })?;
    // Select this loaded instance, including when dlmopen loads a library twice.
    // Checking just a matching pathname/header elsewhere would accept the wrong
    // instance after a replacement or a race with dlclose.
    let (base, bias) = mappings.iter().find_map(|m| {
        if !m.readable || m.offset != 0 || !same_file(m, target) {
            return None;
        }
        let bias = (m.start as u64).checked_sub(header_segment.p_vaddr(endian))?;
        segments
            .iter()
            .any(|s| {
                let start =
                    i128::from(bias) + i128::from(s.p_vaddr(endian)) + i128::from(target.offset)
                        - i128::from(s.p_offset(endian));
                s.p_type(endian) == elf::PT_LOAD
                    && s.p_flags(endian).contains(elf::PF_X)
                    && start == target.start as i128
                    && target.offset < s.p_offset(endian) + s.p_filesz(endian)
                    && u128::from(target.offset) + target.end.saturating_sub(target.start) as u128
                        > u128::from(s.p_offset(endian))
            })
            .then_some((m.start as u64, bias))
    })?;
    let memory = File::open("/proc/self/mem").ok()?;
    let loaded = read_mapped(&memory, target, mappings, base, 0, end)?;
    if loaded[..HEADER_SIZE] != headers[..HEADER_SIZE] || loaded[phoff..] != headers[phoff..] {
        return None;
    }
    let mut remaining = MAX_METADATA;
    let mut matched = false;
    for note in segments.iter().filter(|s| s.p_type(endian) == elf::PT_NOTE) {
        let len = usize::try_from(note.p_filesz(endian)).ok()?;
        remaining = remaining.checked_sub(len)?;
        let mut bytes = vec![0; len];
        file.read_exact_at(&mut bytes, note.p_offset(endian)).ok()?;
        let mut notes = NoteIterator::<Header>::new(endian, note.p_align(endian), &bytes).ok()?;
        while let Some(entry) = notes.next().ok()? {
            if entry.name() != elf::ELF_NOTE_GNU || entry.n_type(endian) != elf::NT_GNU_BUILD_ID {
                continue;
            }
            if !(2..=128).contains(&entry.desc().len()) {
                return None;
            }
            let load = segments.iter().find(|s| {
                s.p_type(endian) == elf::PT_LOAD
                    && s.p_flags(endian).contains(elf::PF_R)
                    && s.p_offset(endian) <= note.p_offset(endian)
                    && s.p_offset(endian) + s.p_filesz(endian)
                        >= note.p_offset(endian) + note.p_filesz(endian)
            })?;
            let address = bias
                .checked_add(load.p_vaddr(endian))?
                .checked_add(note.p_offset(endian) - load.p_offset(endian))?;
            if bias.checked_add(note.p_vaddr(endian))? != address {
                return None;
            }
            let loaded = read_mapped(
                &memory,
                target,
                mappings,
                address,
                note.p_offset(endian),
                len,
            )?;
            // Comparing the containing note segment also rejects contradictory
            // duplicate build IDs and changes to note layout or descriptors.
            if loaded != bytes {
                return None;
            }
            matched = true;
        }
    }
    matched.then_some(())
}

fn same_file(a: &Mapping, b: &Mapping) -> bool {
    a.device == b.device && a.inode == b.inode && a.path == b.path
}

fn read_mapped(
    memory: &File,
    target: &Mapping,
    mappings: &[Mapping],
    mut address: u64,
    mut offset: u64,
    len: usize,
) -> Option<Vec<u8>> {
    if len > MAX_METADATA {
        return None;
    }
    let mut bytes = vec![0; len];
    let mut remaining = bytes.as_mut_slice();
    while !remaining.is_empty() {
        let index = mappings
            .partition_point(|m| m.start as u64 <= address)
            .checked_sub(1)?;
        let map = mappings.get(index)?;
        if !map.readable
            || address >= map.end as u64
            || !same_file(map, target)
            || map.offset.checked_add(address - map.start as u64)? != offset
        {
            return None;
        }
        let count = remaining
            .len()
            .min(usize::try_from(map.end as u64 - address).ok()?);
        memory
            .read_exact_at(&mut remaining[..count], address)
            .ok()?;
        remaining = &mut remaining[count..];
        address = address.checked_add(count as u64)?;
        offset = offset.checked_add(count as u64)?;
    }
    Some(bytes)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{
        io::Write,
        os::unix::fs::MetadataExt,
        path::PathBuf,
        sync::atomic::{AtomicUsize, Ordering},
    };

    const PAGE: usize = 4096;
    const NOTE: usize = 512;
    static NEXT: AtomicUsize = AtomicUsize::new(0);

    // Two load segments and one mapped build-ID note, with no section table.
    // The fallback must use metadata actually loaded in memory, not sections.
    fn elf(id: u8) -> Vec<u8> {
        let mut bytes = vec![0; 2 * PAGE];
        bytes[..7].copy_from_slice(b"\x7fELF\x02\x01\x01");
        bytes[16..18].copy_from_slice(&3_u16.to_le_bytes());
        bytes[18..20].copy_from_slice(&62_u16.to_le_bytes());
        bytes[20..24].copy_from_slice(&1_u32.to_le_bytes());
        bytes[32..40].copy_from_slice(&64_u64.to_le_bytes());
        bytes[52..54].copy_from_slice(&64_u16.to_le_bytes());
        bytes[54..56].copy_from_slice(&56_u16.to_le_bytes());
        bytes[56..58].copy_from_slice(&3_u16.to_le_bytes());
        for (index, (kind, flags, offset, size, align)) in [
            (1_u32, 4_u32, 0_u64, PAGE as u64, PAGE as u64),
            (1, 5, PAGE as u64, PAGE as u64, PAGE as u64),
            (4, 4, NOTE as u64, 36, 4),
        ]
        .into_iter()
        .enumerate()
        {
            let h = &mut bytes[64 + index * 56..64 + (index + 1) * 56];
            h[..4].copy_from_slice(&kind.to_le_bytes());
            h[4..8].copy_from_slice(&flags.to_le_bytes());
            h[8..16].copy_from_slice(&offset.to_le_bytes());
            h[16..24].copy_from_slice(&offset.to_le_bytes());
            h[32..40].copy_from_slice(&size.to_le_bytes());
            h[40..48].copy_from_slice(&size.to_le_bytes());
            h[48..56].copy_from_slice(&align.to_le_bytes());
        }
        bytes[NOTE..NOTE + 4].copy_from_slice(&4_u32.to_le_bytes());
        bytes[NOTE + 4..NOTE + 8].copy_from_slice(&20_u32.to_le_bytes());
        bytes[NOTE + 8..NOTE + 12].copy_from_slice(&3_u32.to_le_bytes());
        bytes[NOTE + 12..NOTE + 16].copy_from_slice(b"GNU\0");
        bytes[NOTE + 16..NOTE + 36].fill(id);
        bytes
    }

    struct Loaded {
        path: PathBuf,
        address: *mut libc::c_void,
        mappings: Vec<Mapping>,
    }
    impl Loaded {
        fn new(bytes: &[u8]) -> Self {
            let path = std::env::temp_dir().join(format!(
                "mars-elf-{}-{}-{}",
                std::process::id(),
                NEXT.fetch_add(1, Ordering::Relaxed),
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap()
                    .as_nanos()
            ));
            let mut file = File::options()
                .read(true)
                .write(true)
                .create_new(true)
                .open(&path)
                .unwrap();
            file.write_all(bytes).unwrap();
            use std::os::fd::AsRawFd;
            // SAFETY: the file has two pages; the read-only view stays alive
            // through this guard and is used only via /proc/self/mem pread.
            let address = unsafe {
                libc::mmap(
                    std::ptr::null_mut(),
                    2 * PAGE,
                    libc::PROT_READ,
                    libc::MAP_PRIVATE,
                    file.as_raw_fd(),
                    0,
                )
            };
            assert_ne!(address, libc::MAP_FAILED);
            let metadata = file.metadata().unwrap();
            let mappings = (0..2)
                .map(|i| Mapping {
                    start: address as usize + i * PAGE,
                    end: address as usize + (i + 1) * PAGE,
                    offset: (i * PAGE) as u64,
                    device: metadata.dev() ^ 1, // The alternate overlay identity.
                    inode: metadata.ino(),
                    path: path.clone(),
                    readable: true,
                    executable: i == 1,
                })
                .collect();
            Self {
                path,
                address,
                mappings,
            }
        }
        fn accepted(&self) -> bool {
            self.mappings[1].open(&self.mappings).is_some()
        }
        fn replace(&self, bytes: &[u8]) {
            let replacement = self.path.with_extension("replacement");
            std::fs::write(&replacement, bytes).unwrap();
            std::fs::rename(replacement, &self.path).unwrap();
        }
    }
    impl Drop for Loaded {
        fn drop(&mut self) {
            // SAFETY: this guard owns the unchanged two-page mapping.
            unsafe {
                libc::munmap(self.address, 2 * PAGE);
            }
            let _ = std::fs::remove_file(&self.path);
        }
    }

    #[test]
    fn matching_build_id_accepts_alternate_device_and_inode() {
        let mut loaded = Loaded::new(&elf(1));
        assert!(loaded.accepted());
        for map in &mut loaded.mappings {
            map.inode ^= 1;
        }
        assert!(loaded.accepted());
    }

    #[test]
    fn replacement_at_same_path_with_different_build_id_is_rejected() {
        let loaded = Loaded::new(&elf(1));
        loaded.replace(&elf(2));
        assert!(!loaded.accepted());
    }

    #[test]
    fn matching_build_id_does_not_override_changed_load_layout() {
        let loaded = Loaded::new(&elf(1));
        let mut replacement = elf(1);
        replacement[64 + 56 + 16..64 + 56 + 24].copy_from_slice(&(3 * PAGE as u64).to_le_bytes());
        loaded.replace(&replacement);
        assert!(!loaded.accepted());
    }

    #[test]
    fn wrong_instance_offsets_and_missing_readable_headers_are_rejected() {
        let mut loaded = Loaded::new(&elf(1));
        loaded.mappings[1].offset += 1;
        assert!(!loaded.accepted());
        loaded.mappings[1].offset -= 1;
        loaded.mappings[0].readable = false;
        assert!(!loaded.accepted());
    }

    #[test]
    fn missing_build_id_and_malformed_metadata_fail_closed() {
        let mut no_id = elf(1);
        no_id[NOTE + 8..NOTE + 12].copy_from_slice(&1_u32.to_le_bytes());
        assert!(!Loaded::new(&no_id).accepted());
        let loaded = Loaded::new(&elf(1));
        for (offset, size, value) in [
            (32, 8, u64::MAX),
            (56, 2, 65535),
            (NOTE + 4, 4, u32::MAX as u64),
        ] {
            let mut malformed = elf(1);
            malformed[offset..offset + size].copy_from_slice(&value.to_le_bytes()[..size]);
            loaded.replace(&malformed);
            assert!(!loaded.accepted());
        }
        loaded.replace(b"\x7fELF");
        assert!(!loaded.accepted());
    }

    #[test]
    fn unmapped_memory_returns_failure_instead_of_dereferencing_it() {
        let mut loaded = Loaded::new(&elf(1));
        for (i, map) in loaded.mappings.iter_mut().enumerate() {
            map.start = (i + 1) * PAGE;
            map.end = (i + 2) * PAGE;
        }
        assert!(!loaded.accepted());
    }
}
