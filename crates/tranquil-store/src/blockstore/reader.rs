use std::collections::HashMap;
use std::io;
use std::sync::Arc;

use bytes::Bytes;

use crate::io::{FileId, StorageIO};

use super::data_file::{CID_SIZE, ReadBlockRecord, decode_block_record};
use super::hash_index::BlockIndex;
use super::manager::DataFileManager;
use super::types::{BlockLocation, BlockOffset, DataFileId};

pub const BLOCK_CORRUPTION_MARKER: &str = "corrupted block at";

#[derive(Debug, Clone)]
pub enum ReadError {
    Io(Arc<io::Error>),
    Corrupted {
        file_id: DataFileId,
        offset: BlockOffset,
    },
}

impl std::fmt::Display for ReadError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Io(e) => write!(f, "io: {e}"),
            Self::Corrupted { file_id, offset } => {
                write!(f, "{BLOCK_CORRUPTION_MARKER} {file_id}:{}", offset.raw())
            }
        }
    }
}

impl std::error::Error for ReadError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Io(e) => Some(e.as_ref()),
            Self::Corrupted { .. } => None,
        }
    }
}

impl From<io::Error> for ReadError {
    fn from(e: io::Error) -> Self {
        Self::Io(Arc::new(e))
    }
}

pub struct BlockStoreReader<S: StorageIO> {
    index: Arc<BlockIndex>,
    manager: Arc<DataFileManager<S>>,
}

impl<S: StorageIO> Clone for BlockStoreReader<S> {
    fn clone(&self) -> Self {
        Self {
            index: Arc::clone(&self.index),
            manager: Arc::clone(&self.manager),
        }
    }
}

impl<S: StorageIO> BlockStoreReader<S> {
    pub fn new(index: Arc<BlockIndex>, manager: Arc<DataFileManager<S>>) -> Self {
        Self { index, manager }
    }

    pub fn manager(&self) -> &DataFileManager<S> {
        &self.manager
    }

    pub fn get(&self, cid: &[u8; CID_SIZE]) -> Result<Option<Bytes>, ReadError> {
        match self.index.get(cid) {
            Some(e) => self.read_block_at(e.location, cid).map(Some),
            None => Ok(None),
        }
    }

    pub fn has(&self, cid: &[u8; CID_SIZE]) -> Result<bool, ReadError> {
        Ok(self.index.has(cid))
    }

    pub fn get_many(&self, cids: &[[u8; CID_SIZE]]) -> Result<Vec<Option<Bytes>>, ReadError> {
        let mut results: Vec<Option<Bytes>> = vec![None; cids.len()];

        let index_lookups: Vec<(usize, [u8; CID_SIZE], BlockLocation)> = cids
            .iter()
            .enumerate()
            .filter_map(|(i, cid)| self.index.get(cid).map(|entry| (i, *cid, entry.location)))
            .collect();
        self.read_locations_into(&index_lookups, &mut results)?;

        Ok(results)
    }

    fn read_locations_into(
        &self,
        lookups: &[(usize, [u8; CID_SIZE], BlockLocation)],
        results: &mut [Option<Bytes>],
    ) -> Result<(), ReadError> {
        let mut by_file: HashMap<DataFileId, Vec<(usize, [u8; CID_SIZE], BlockLocation)>> =
            HashMap::new();
        lookups.iter().for_each(|&(idx, cid, loc)| {
            by_file
                .entry(loc.file_id)
                .or_default()
                .push((idx, cid, loc));
        });

        by_file.into_iter().try_for_each(|(file_id, mut entries)| {
            let handle = self.manager.open_for_read(file_id)?;
            let file_size = self.manager.io().file_size(handle.fd())?;
            entries.sort_by_key(|(_, _, loc)| loc.offset);
            entries.into_iter().try_for_each(|(orig_idx, cid, loc)| {
                let data = self.decode_and_validate(handle.fd(), file_size, loc, &cid)?;
                results[orig_idx] = Some(data);
                Ok::<_, ReadError>(())
            })
        })
    }

    fn read_block_at(
        &self,
        location: BlockLocation,
        expected_cid: &[u8; CID_SIZE],
    ) -> Result<Bytes, ReadError> {
        let handle = self.manager.open_for_read(location.file_id)?;
        let file_size = self.manager.io().file_size(handle.fd())?;
        self.decode_and_validate(handle.fd(), file_size, location, expected_cid)
    }

    fn decode_and_validate(
        &self,
        fd: FileId,
        file_size: u64,
        location: BlockLocation,
        expected_cid: &[u8; CID_SIZE],
    ) -> Result<Bytes, ReadError> {
        let at_location = ReadError::Corrupted {
            file_id: location.file_id,
            offset: location.offset,
        };
        let attempt_once = || -> Result<Bytes, (ReadError, bool)> {
            match decode_block_record(self.manager.io(), fd, location.offset, file_size) {
                Err(e) => Err((e.into(), false)),
                Ok(Some(ReadBlockRecord::Valid {
                    data, cid_bytes, ..
                })) if cid_bytes == *expected_cid
                    && data.len() == location.length.raw() as usize =>
                {
                    Ok(Bytes::from(data))
                }
                Ok(Some(ReadBlockRecord::Valid { .. })) => Err((at_location.clone(), false)),
                Ok(Some(
                    ReadBlockRecord::Corrupted { offset } | ReadBlockRecord::Truncated { offset },
                )) => Err((
                    ReadError::Corrupted {
                        file_id: location.file_id,
                        offset,
                    },
                    true,
                )),
                Ok(None) => Err((at_location.clone(), true)),
            }
        };
        (0..READ_RETRY_ATTEMPTS.saturating_sub(1))
            .find_map(|_| match attempt_once() {
                Ok(bytes) => Some(Ok(bytes)),
                Err((_, true)) => None,
                Err((e, false)) => Some(Err(e)),
            })
            .unwrap_or_else(|| attempt_once().map_err(|(e, _)| e))
    }
}

const READ_RETRY_ATTEMPTS: u32 = 4;

#[cfg(test)]
mod tests {
    use super::{BlockStoreReader, ReadError};
    use crate::blockstore::data_file::{CID_SIZE, DataFileWriter};
    use crate::blockstore::hash_index::{BlockIndex, HashTable};
    use crate::blockstore::manager::DataFileManager;
    use crate::blockstore::test_cid;
    use crate::blockstore::types::{BlockLocation, DataFileId};
    use crate::io::StorageIO;
    use crate::sim::SimulatedIO;
    use std::path::{Path, PathBuf};
    use std::sync::Arc;

    const BLOCK_A: &[u8] = b"block-a-contents";
    const BLOCK_B: &[u8] = b"block-b-contents";

    fn setup() -> DataFileManager<SimulatedIO> {
        let sim = SimulatedIO::pristine(7);
        let dir = Path::new("/data");
        sim.mkdir(dir).unwrap();
        sim.sync_dir(dir).unwrap();
        DataFileManager::new(sim, dir.to_path_buf(), 1 << 20)
    }

    fn write_two_blocks(
        mgr: &DataFileManager<SimulatedIO>,
    ) -> ([u8; CID_SIZE], BlockLocation, [u8; CID_SIZE], BlockLocation) {
        let handle = mgr.open_for_append(DataFileId::new(0)).unwrap();
        let mut writer = DataFileWriter::new(mgr.io(), handle.fd(), DataFileId::new(0)).unwrap();
        let cid_a = test_cid(1);
        let cid_b = test_cid(2);
        let loc_a = writer.append_block(&cid_a, BLOCK_A).unwrap();
        let loc_b = writer.append_block(&cid_b, BLOCK_B).unwrap();
        writer.sync().unwrap();
        assert_eq!(loc_a.length, loc_b.length, "blocks must be equal length");
        (cid_a, loc_a, cid_b, loc_b)
    }

    fn index_mapping(pairs: &[([u8; CID_SIZE], BlockLocation)]) -> Arc<BlockIndex> {
        let mut table = HashTable::with_capacity(64);
        pairs.iter().for_each(|(cid, loc)| {
            table.insert_or_increment(cid, *loc).unwrap();
        });
        Arc::new(BlockIndex::new(table, PathBuf::from("/index")))
    }

    #[test]
    fn get_rejects_index_pointing_at_different_block() {
        let mgr = Arc::new(setup());
        let (cid_a, _loc_a, _cid_b, loc_b) = write_two_blocks(&mgr);
        let index = index_mapping(&[(cid_a, loc_b)]);
        let reader = BlockStoreReader::new(index, mgr);
        match reader.get(&cid_a) {
            Err(ReadError::Corrupted { .. }) => {}
            other => {
                panic!("expected Corrupted when CID resolves to a foreign block, got {other:?}")
            }
        }
    }

    #[test]
    fn get_many_rejects_index_pointing_at_different_block() {
        let mgr = Arc::new(setup());
        let (cid_a, _loc_a, _cid_b, loc_b) = write_two_blocks(&mgr);
        let index = index_mapping(&[(cid_a, loc_b)]);
        let reader = BlockStoreReader::new(index, mgr);
        match reader.get_many(&[cid_a]) {
            Err(ReadError::Corrupted { .. }) => {}
            other => panic!("expected Corrupted from get_many on foreign block, got {other:?}"),
        }
    }

    #[test]
    fn correct_mapping_still_resolves() {
        let mgr = Arc::new(setup());
        let (cid_a, loc_a, cid_b, loc_b) = write_two_blocks(&mgr);
        let index = index_mapping(&[(cid_a, loc_a), (cid_b, loc_b)]);
        let reader = BlockStoreReader::new(index, mgr);
        assert_eq!(reader.get(&cid_a).unwrap().unwrap().as_ref(), BLOCK_A);
        assert_eq!(reader.get(&cid_b).unwrap().unwrap().as_ref(), BLOCK_B);
        let many = reader.get_many(&[cid_a, cid_b]).unwrap();
        assert_eq!(many[0].as_ref().unwrap().as_ref(), BLOCK_A);
        assert_eq!(many[1].as_ref().unwrap().as_ref(), BLOCK_B);
    }
}
