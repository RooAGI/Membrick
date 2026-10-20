//! Bound resident receipt caching while retaining the existing durable history.
use super::{MemoryService, WriteAdjudicationReceipt};
use serde::de::{SeqAccess, Visitor};
use serde::Deserializer;
use std::io::{BufRead, BufReader};
use std::path::Path;

const MAX_CACHED_RECEIPTS: usize = 512;
type Entry = ((String, String), WriteAdjudicationReceipt);

// Deserialize one entry at a time, including old multiline snapshot arrays.
// Never materialize the complete snapshot or journal in memory.
fn visit_receipts(path: &Path, mut visit: impl FnMut(Entry)) {
    struct Entries<'a, F>(&'a mut F);
    impl<'de, F: FnMut(Entry)> Visitor<'de> for Entries<'_, F> {
        type Value = ();
        fn expecting(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
            f.write_str("an array of write receipts")
        }
        fn visit_seq<A: SeqAccess<'de>>(self, mut seq: A) -> Result<(), A::Error> {
            while let Some(entry) = seq.next_element::<Entry>()? {
                (self.0)(entry);
            }
            Ok(())
        }
    }
    let Ok(file) = std::fs::File::open(path) else {
        return;
    };
    let mut reader = BufReader::new(file);
    while reader.fill_buf().is_ok_and(|bytes| !bytes.is_empty()) {
        let result =
            serde_json::Deserializer::from_reader(&mut reader).deserialize_seq(Entries(&mut visit));
        if let Err(error) = result {
            if error.is_eof() {
                break;
            }
            // Preserve the old journal's fail-open handling of malformed lines.
            if reader.skip_until(b'\n').is_err() {
                break;
            }
        }
    }
}

impl MemoryService {
    pub(super) fn trim_receipt_cache(&mut self) {
        // Without durable backing, receipts are part of the in-memory service's
        // idempotency contract. Evict only when the disk history is available.
        if self.receipts_path.is_none() {
            return;
        }
        while self.request_receipts.len() > MAX_CACHED_RECEIPTS {
            let key = self
                .request_receipts
                .keys()
                .find(|key| {
                    !self.pending_adds.contains_key(*key)
                        && !self.pending_receipt_persistence.contains(*key)
                })
                .cloned();
            let Some(key) = key else { break };
            self.request_receipts.remove(&key);
        }
    }

    pub(super) fn load_receipts(&mut self) {
        let Some(path) = self.receipts_path.clone() else {
            return;
        };
        for path in [&path, &path.with_extension("jsonl")] {
            visit_receipts(path, |(key, receipt)| {
                self.request_receipts.insert(key, receipt);
                self.trim_receipt_cache();
            });
        }
    }

    pub(super) fn persisted_receipt(
        &self,
        key: &(String, String),
    ) -> Option<WriteAdjudicationReceipt> {
        let path = self.receipts_path.as_ref()?;
        let mut receipt = None;
        for path in [path, &path.with_extension("jsonl")] {
            visit_receipts(path, |(candidate, value)| {
                if &candidate == key {
                    receipt = Some(value);
                }
            });
        }
        receipt
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn failed_receipt_writes_are_pinned_until_a_successful_retry() {
        let dir = std::env::temp_dir().join(format!(
            "membrick-receipt-failure-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let dir = dir.canonicalize().unwrap();
        let mut owner = MemoryService::at_path(&dir, crate::PipelineOptions::default()).unwrap();
        let receipt = owner.build_adjudication_receipt("receipt".into(), &[]);
        let mut keys = Vec::new();
        for i in 0..MAX_CACHED_RECEIPTS + 8 {
            let key = ("alice".to_string(), format!("request-{i}"));
            owner.request_receipts.insert(key.clone(), receipt.clone());
            keys.push(key);
        }
        let journal = dir.join("receipts.jsonl");
        std::fs::create_dir(&journal).unwrap();
        owner.persist_receipts(&keys);
        assert_eq!(owner.pending_receipt_persistence.len(), keys.len());
        owner.trim_receipt_cache();
        assert_eq!(owner.request_receipts.len(), keys.len());
        std::fs::remove_dir(&journal).unwrap();
        owner.persist_receipts(&keys[..1]);
        assert!(owner.pending_receipt_persistence.is_empty());
        assert!(owner.request_receipts.len() <= MAX_CACHED_RECEIPTS);
        let evicted = keys
            .iter()
            .find(|key| !owner.request_receipts.contains_key(*key))
            .unwrap();
        for key in [&keys[0], keys.last().unwrap(), evicted] {
            assert!(owner.persisted_receipt(key).is_some());
        }
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn malformed_journal_lines_do_not_hide_later_receipts() {
        let dir = std::env::temp_dir().join(format!(
            "membrick-receipt-corrupt-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let dir = dir.canonicalize().unwrap();
        let owner = MemoryService::at_path(&dir, crate::PipelineOptions::default()).unwrap();
        let receipt = owner.build_adjudication_receipt("receipt".into(), &[]);
        let key = ("alice".to_string(), "request".to_string());
        let entries = serde_json::to_string(&vec![(key.clone(), receipt)]).unwrap();
        std::fs::write(
            dir.join("receipts.jsonl"),
            format!("broken JSON\n{entries}\n"),
        )
        .unwrap();
        assert!(owner.persisted_receipt(&key).is_some());
        std::fs::remove_dir_all(dir).unwrap();
    }
}
