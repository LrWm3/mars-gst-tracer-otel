//! Own borrowed adapter names before the activity callback returns. All live
//! v2 names share this bounded table, including queued and formatted names.
use std::{
    collections::HashSet,
    sync::{Arc, Mutex},
};

pub(super) const MAX_RAW: usize = 16 * 1024;
const MAX_NAMES: usize = 1024;
const MAX_BYTES: usize = 1024 * 1024;

#[derive(Default)]
struct Table {
    names: HashSet<Arc<str>>,
    bytes: usize,
}
impl Table {
    fn reclaim(&mut self) {
        self.names.retain(|name| {
            // Lookup and reclamation share the same lock: an entry cannot gain
            // a new owner while we remove its final reference.
            if Arc::strong_count(name) == 1 {
                self.bytes -= name.len();
                false
            } else {
                true
            }
        });
    }
}

#[derive(Default)]
pub(super) struct Symbols(Mutex<Table>);
impl Symbols {
    pub fn intern(&self, raw: &[u8]) -> Option<Arc<str>> {
        if raw.len() > MAX_RAW {
            return None;
        }
        let raw = std::str::from_utf8(raw).ok()?;
        // CUPTI activity delivery never waits for another callback or the
        // report worker. The caller accounts for contention/capacity as a drop.
        let mut table = self.0.try_lock().ok()?;
        if let Some(name) = table.names.get(raw) {
            return Some(name.clone());
        }
        if table.names.len() >= MAX_NAMES || table.bytes + raw.len() > MAX_BYTES {
            table.reclaim();
        }
        if table.names.len() >= MAX_NAMES || table.bytes + raw.len() > MAX_BYTES {
            return None;
        }
        let name: Arc<str> = raw.into();
        table.bytes += name.len();
        table.names.insert(name.clone());
        Some(name)
    }

    pub fn reclaim(&self) {
        if let Ok(mut table) = self.0.try_lock() {
            table.reclaim();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn full_names_survive_callback_storage_and_remain_distinct_past_byte_255() {
        let pool = Symbols::default();
        let prefix = "x".repeat(300);
        let mut bytes = format!("{prefix}one").into_bytes();
        let one = pool.intern(&bytes).unwrap();
        bytes.fill(b'z'); // adapter storage is only borrowed for the callback
        let two = pool.intern(format!("{prefix}two").as_bytes()).unwrap();
        assert_eq!(&*one, format!("{prefix}one"));
        assert_ne!(one, two);
        assert!(Arc::ptr_eq(&one, &pool.intern(one.as_bytes()).unwrap()));
        pool.reclaim();
        assert_eq!(pool.0.lock().unwrap().names.len(), 2);
        drop((one, two));
        pool.reclaim();
        assert_eq!(pool.0.lock().unwrap().bytes, 0);
    }

    #[test]
    fn callbacks_never_wait_and_reject_invalid_or_oversized_names() {
        let pool = Symbols::default();
        let guard = pool.0.lock().unwrap();
        assert!(pool.intern(b"busy").is_none());
        pool.reclaim();
        drop(guard);
        assert!(pool.intern(&[255]).is_none());
        assert!(pool.intern(&vec![b'x'; MAX_RAW + 1]).is_none());
        assert!(pool.intern(&vec![b'x'; MAX_RAW]).is_some());
    }

    #[test]
    fn byte_and_entry_limits_include_names_still_queued_or_in_reports() {
        for length in [16, MAX_RAW] {
            let pool = Symbols::default();
            let count = MAX_NAMES.min(MAX_BYTES / length);
            let mut held = Vec::new();
            for i in 0..count {
                held.push(pool.intern(format!("{i:0length$}").as_bytes()).unwrap());
            }
            assert!(pool.intern(b"another").is_none());
            assert!(Arc::ptr_eq(
                &held[0],
                &pool.intern(held[0].as_bytes()).unwrap()
            ));
            held.pop();
            assert!(pool.intern(b"another").is_some());
            drop(held);
            pool.reclaim();
            assert_eq!(pool.0.lock().unwrap().bytes, 0);
        }
    }
}
