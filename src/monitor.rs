//! USN journal change monitoring: keeps the SQLite index live.
//!
//! Polls `FSCTL_READ_USN_JOURNAL` (admin) and applies create/delete/rename
//! events to the store. Deletions are applied by FRN so they work even after
//! the MFT record has been recycled.

use std::collections::HashMap;
use std::thread;
use std::time::Duration;

use anyhow::{Result, bail};
use windows_sys::Win32::System::Ioctl::{
    USN_REASON_FILE_CREATE, USN_REASON_FILE_DELETE, USN_REASON_RENAME_NEW_NAME,
    USN_REASON_RENAME_OLD_NAME,
};

use crate::store::Store;
use crate::usn::{UsnVolume, resolve_path};

const MASK: u32 = USN_REASON_FILE_CREATE
    | USN_REASON_FILE_DELETE
    | USN_REASON_RENAME_NEW_NAME
    | USN_REASON_RENAME_OLD_NAME;

/// Watch one volume forever, applying journal events every `interval`.
pub fn run(store: &mut Store, drive: char, interval: Duration) -> Result<()> {
    let vol = UsnVolume::open(drive)?;
    let mut start = store
        .get_meta(&format!("last_usn:{drive}"))
        .and_then(|v| v.parse::<i64>().ok())
        .unwrap_or_else(|| sync_to_now(&vol));
    eprintln!("[monitor] watching {drive}: from USN {start}");
    let mut cache: HashMap<u64, Option<String>> = HashMap::new();
    loop {
        let (next, records) = vol.read_journal(start, MASK)?;
        if !records.is_empty() && next < start {
            bail!(
                "USN journal on {drive}: wrapped (next={next} < start={start}) — \
                 run `fer index` again to rebuild"
            );
        }
        let mut applied = 0usize;
        for r in &records {
            if r.reason & (USN_REASON_FILE_DELETE | USN_REASON_RENAME_OLD_NAME) != 0 {
                store.delete_by_frn(r.frn)?;
                applied += 1;
            }
            if r.reason & (USN_REASON_FILE_CREATE | USN_REASON_RENAME_NEW_NAME) != 0 {
                if let Some(parent) = resolve_path(&vol, drive, r.parent_frn, &mut cache) {
                    let path = if parent.is_empty() {
                        format!("{drive}:\\{}", r.name)
                    } else {
                        format!("{parent}\\{}", r.name)
                    };
                    store.upsert(
                        &path,
                        crate::EntryMeta { is_dir: r.is_dir, frn: Some(r.frn), ..Default::default() },
                    )?;
                    applied += 1;
                }
            }
        }
        if next != start {
            store.set_meta(&format!("last_usn:{drive}"), &next.to_string())?;
            start = next;
        }
        if applied > 0 {
            eprintln!("[monitor] applied {applied} changes (usn={start})");
        }
        // Bound the FRN→path resolution cache so a huge catch-up replay
        // cannot grow without limit.
        if cache.len() > 1_000_000 {
            cache.clear();
        }
        thread::sleep(interval);
    }
}

/// Start from the journal's current position (QUERY_USN_JOURNAL.NextUsn) so a
/// fresh monitor applies only future changes instead of replaying history.
fn sync_to_now(vol: &UsnVolume) -> i64 {
    match vol.query_journal() {
        Ok((_id, next)) => {
            eprintln!("[monitor] no stored USN — syncing to current journal position ({next})");
            next
        }
        Err(_) => 0,
    }
}
