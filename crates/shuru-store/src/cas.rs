use std::collections::HashMap;
use std::fs;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::RwLock;

use anyhow::{Context, Result};

pub const CHUNK_SIZE: usize = 64 * 1024; // 64KB

/// Trait for content-addressable chunk storage backends.
/// Implement this for S3, SSH, or any other remote storage.
pub trait ChunkStore: Send + Sync {
    /// Store a chunk, returning its blake3 hash hex string.
    fn put(&self, data: &[u8]) -> Result<String>;
    /// Read a chunk by hash. Returns None if not found.
    fn get(&self, hash: &str) -> Result<Option<Vec<u8>>>;
}

/// Local filesystem chunk store — chunks stored as files named by hash.
pub struct LocalChunkStore {
    chunks_dir: PathBuf,
}

impl LocalChunkStore {
    pub fn open(cas_dir: &str) -> Result<Self> {
        let chunks_dir = Path::new(cas_dir).join("chunks");
        fs::create_dir_all(&chunks_dir)
            .with_context(|| format!("failed to create chunks dir: {}", chunks_dir.display()))?;
        Ok(LocalChunkStore { chunks_dir })
    }

    fn chunk_path(&self, hash: &str) -> PathBuf {
        self.chunks_dir.join(hash)
    }
}

impl ChunkStore for LocalChunkStore {
    fn put(&self, data: &[u8]) -> Result<String> {
        let hash = blake3::hash(data);
        let hex = hash.to_hex().to_string();
        let path = self.chunk_path(&hex);
        if path.exists() {
            return Ok(hex);
        }
        static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let seq = NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let partial = self
            .chunks_dir
            .join(format!(".partial-{}-{seq}", std::process::id()));
        let mut file = fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&partial)?;
        let result = (|| -> Result<()> {
            file.write_all(data)?;
            file.sync_all()?;
            fs::rename(&partial, &path)?;
            fs::File::open(&self.chunks_dir)?.sync_all()?;
            Ok(())
        })();
        if result.is_err() {
            let _ = fs::remove_file(&partial);
        }
        result?;
        Ok(hex)
    }

    fn get(&self, hash: &str) -> Result<Option<Vec<u8>>> {
        let path = self.chunk_path(hash);
        match fs::read(&path) {
            Ok(data) => Ok(Some(data)),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(e).with_context(|| format!("failed to read chunk {}", hash)),
        }
    }
}

/// Index mapping chunk positions to hashes. One index per disk image / checkpoint.
/// ZERO entries mean "ask parent" — enables delta-only checkpoints.
pub struct ChunkIndex {
    hashes: Vec<String>,
    disk_size: u64,
    /// Path to parent index. On read, ZERO entries resolve through the parent chain.
    pub parent_path: Option<String>,
    /// Path to the flat rootfs file at the bottom of the chain (for lazy ingestion).
    pub fallback_path: Option<String>,
}

const ZERO_CHUNK_HASH: &str = "ZERO";

impl ChunkIndex {
    pub fn new(disk_size: u64) -> Self {
        let num_chunks = ((disk_size + CHUNK_SIZE as u64 - 1) / CHUNK_SIZE as u64) as usize;
        ChunkIndex {
            hashes: vec![ZERO_CHUNK_HASH.to_string(); num_chunks],
            disk_size,
            parent_path: None,
            fallback_path: None,
        }
    }

    pub fn disk_size(&self) -> u64 {
        self.disk_size
    }

    pub fn num_chunks(&self) -> usize {
        self.hashes.len()
    }

    pub fn get_hash(&self, chunk_idx: usize) -> Option<&str> {
        self.hashes.get(chunk_idx).map(|s| s.as_str())
    }

    pub fn set_hash(&mut self, chunk_idx: usize, hash: String) {
        if chunk_idx < self.hashes.len() {
            self.hashes[chunk_idx] = hash;
        }
    }

    /// Save index to a file.
    pub fn save(&self, path: &str) -> Result<()> {
        if let Some(parent) = Path::new(path).parent() {
            fs::create_dir_all(parent)?;
        }
        let mut f =
            fs::File::create(path).with_context(|| format!("failed to create index: {}", path))?;
        // Header: disk_size, num_chunks, parent_path, fallback_path
        f.write_all(&self.disk_size.to_le_bytes())?;
        f.write_all(&(self.hashes.len() as u64).to_le_bytes())?;
        let parent_bytes = self.parent_path.as_deref().unwrap_or("").as_bytes();
        f.write_all(&(parent_bytes.len() as u32).to_le_bytes())?;
        f.write_all(parent_bytes)?;
        let fallback_bytes = self.fallback_path.as_deref().unwrap_or("").as_bytes();
        f.write_all(&(fallback_bytes.len() as u32).to_le_bytes())?;
        f.write_all(fallback_bytes)?;
        // Chunk hashes
        for hash in &self.hashes {
            let bytes = hash.as_bytes();
            f.write_all(&(bytes.len() as u32).to_le_bytes())?;
            f.write_all(bytes)?;
        }
        Ok(())
    }

    /// Load index from a file.
    pub fn load(path: &str) -> Result<Self> {
        let mut f =
            fs::File::open(path).with_context(|| format!("failed to open index: {}", path))?;
        let mut buf8 = [0u8; 8];
        f.read_exact(&mut buf8)?;
        let disk_size = u64::from_le_bytes(buf8);
        f.read_exact(&mut buf8)?;
        let num_chunks = u64::from_le_bytes(buf8) as usize;

        let expected_chunks = ((disk_size + CHUNK_SIZE as u64 - 1) / CHUNK_SIZE as u64) as usize;
        anyhow::ensure!(
            num_chunks == expected_chunks,
            "index {}: chunk count {} does not match disk_size {} (expected {})",
            path,
            num_chunks,
            disk_size,
            expected_chunks,
        );

        // Parent path
        let mut buf4 = [0u8; 4];
        f.read_exact(&mut buf4)?;
        let parent_len = u32::from_le_bytes(buf4) as usize;
        let parent_path = if parent_len > 0 {
            let mut parent_bytes = vec![0u8; parent_len];
            f.read_exact(&mut parent_bytes)?;
            Some(String::from_utf8(parent_bytes)?)
        } else {
            None
        };

        f.read_exact(&mut buf4)?;
        let fallback_len = u32::from_le_bytes(buf4) as usize;
        let fallback_path = if fallback_len > 0 {
            let mut fallback_bytes = vec![0u8; fallback_len];
            f.read_exact(&mut fallback_bytes)?;
            Some(String::from_utf8(fallback_bytes)?)
        } else {
            None
        };

        let mut hashes = Vec::with_capacity(num_chunks);
        for _ in 0..num_chunks {
            f.read_exact(&mut buf4)?;
            let len = u32::from_le_bytes(buf4) as usize;
            let mut hash_bytes = vec![0u8; len];
            f.read_exact(&mut hash_bytes)?;
            hashes.push(String::from_utf8(hash_bytes)?);
        }

        Ok(ChunkIndex {
            hashes,
            disk_size,
            parent_path,
            fallback_path,
        })
    }

    /// Validate that the given backing store size does not exceed disk_size.
    /// The virtual disk may be larger than the flat file (sparse expansion is valid).
    pub fn check_size_against_backend(&self, backend_size: u64, label: &str) -> Result<()> {
        anyhow::ensure!(
            backend_size <= self.disk_size,
            "{} size ({}) exceeds index disk_size ({})",
            label,
            backend_size,
            self.disk_size,
        );
        Ok(())
    }
}

/// CAS-backed storage backend for the NBD server.
pub struct CasBackend {
    store: Box<dyn ChunkStore>,
    index: RwLock<ChunkIndex>,
    dirty: RwLock<HashMap<usize, Vec<u8>>>,
    /// Parent indexes for chain resolution (loaded lazily from parent_path).
    parents: RwLock<Vec<ChunkIndex>>,
    /// Optional flat file for lazy ingestion at the bottom of the chain.
    fallback: Option<crate::backend::FlatFileBackend>,
}

impl CasBackend {
    pub fn new(store: Box<dyn ChunkStore>, index: ChunkIndex) -> Self {
        // Load the parent chain upfront (typically 0-3 levels)
        let parents = Self::load_parent_chain(&index);
        CasBackend {
            store,
            index: RwLock::new(index),
            dirty: RwLock::new(HashMap::new()),
            parents: RwLock::new(parents),
            fallback: None,
        }
    }

    pub fn with_fallback(
        store: Box<dyn ChunkStore>,
        index: ChunkIndex,
        fallback: crate::backend::FlatFileBackend,
    ) -> Self {
        let parents = Self::load_parent_chain(&index);
        CasBackend {
            store,
            index: RwLock::new(index),
            dirty: RwLock::new(HashMap::new()),
            parents: RwLock::new(parents),
            fallback: Some(fallback),
        }
    }

    fn load_parent_chain(index: &ChunkIndex) -> Vec<ChunkIndex> {
        let mut chain = Vec::new();
        let mut current_parent = index.parent_path.clone();
        while let Some(ref path) = current_parent {
            match ChunkIndex::load(path) {
                Ok(parent) => {
                    current_parent = parent.parent_path.clone();
                    chain.push(parent);
                }
                Err(e) => {
                    tracing::warn!("failed to load parent index {}: {}", path, e);
                    break;
                }
            }
        }
        chain
    }

    pub fn size(&self) -> u64 {
        self.index.read().unwrap().disk_size()
    }

    /// Extend the virtual disk size. The index grows to cover the new size;
    /// new chunks default to ZERO (sparse).
    pub fn set_disk_size(&mut self, new_size: u64) {
        let mut index = self.index.write().unwrap();
        if new_size > index.disk_size() {
            let new_num_chunks = ((new_size + CHUNK_SIZE as u64 - 1) / CHUNK_SIZE as u64) as usize;
            while index.num_chunks() < new_num_chunks {
                index.hashes.push(ZERO_CHUNK_HASH.to_string());
            }
            index.disk_size = new_size;
        }
    }

    pub fn read(&self, offset: u64, buf: &mut [u8]) -> std::io::Result<usize> {
        let mut pos = 0usize;
        let mut file_offset = offset;

        while pos < buf.len() {
            let chunk_idx = (file_offset / CHUNK_SIZE as u64) as usize;
            let offset_in_chunk = (file_offset % CHUNK_SIZE as u64) as usize;
            let remaining_in_chunk = CHUNK_SIZE - offset_in_chunk;
            let to_read = remaining_in_chunk.min(buf.len() - pos);

            let chunk_data = self.read_chunk(chunk_idx)?;
            let available = chunk_data.len().saturating_sub(offset_in_chunk);
            let copy_len = to_read.min(available);

            if copy_len > 0 {
                buf[pos..pos + copy_len]
                    .copy_from_slice(&chunk_data[offset_in_chunk..offset_in_chunk + copy_len]);
            }
            // Zero-fill if chunk is shorter than expected
            if copy_len < to_read {
                buf[pos + copy_len..pos + to_read].fill(0);
            }

            pos += to_read;
            file_offset += to_read as u64;
        }

        Ok(buf.len())
    }

    pub fn write(&self, offset: u64, data: &[u8]) -> std::io::Result<usize> {
        let mut pos = 0usize;
        let mut file_offset = offset;

        while pos < data.len() {
            let chunk_idx = (file_offset / CHUNK_SIZE as u64) as usize;
            let offset_in_chunk = (file_offset % CHUNK_SIZE as u64) as usize;
            let remaining_in_chunk = CHUNK_SIZE - offset_in_chunk;
            let to_write = remaining_in_chunk.min(data.len() - pos);

            // Read-modify-write: get current chunk, overlay the write
            let mut chunk_data = self.read_chunk(chunk_idx)?;
            if chunk_data.len() < offset_in_chunk + to_write {
                chunk_data.resize(offset_in_chunk + to_write, 0);
            }
            chunk_data[offset_in_chunk..offset_in_chunk + to_write]
                .copy_from_slice(&data[pos..pos + to_write]);

            self.dirty.write().unwrap().insert(chunk_idx, chunk_data);

            pos += to_write;
            file_offset += to_write as u64;
        }

        Ok(data.len())
    }

    pub fn flush(&self) -> std::io::Result<()> {
        let mut dirty = self.dirty.write().unwrap();
        let mut index = self.index.write().unwrap();

        for (&chunk_idx, data) in dirty.iter() {
            let hash = self
                .store
                .put(data)
                .map_err(|e| std::io::Error::new(std::io::ErrorKind::Other, e.to_string()))?;
            index.set_hash(chunk_idx, hash);
        }
        dirty.clear();

        Ok(())
    }

    /// Save a standalone index so replacing a checkpoint cannot create a parent cycle.
    /// The caller holds the guest seal throughout this operation.
    pub fn save_index(&self, path: &str) -> Result<()> {
        self.flush().map_err(|e| anyhow::anyhow!(e))?;
        let index = self.index.read().unwrap();
        let parents = self.parents.read().unwrap();
        let mut saved = ChunkIndex::new(index.disk_size);
        for (i, hash) in index.hashes.iter().enumerate() {
            saved.hashes[i] = if hash != ZERO_CHUNK_HASH {
                hash.clone()
            } else {
                parents
                    .iter()
                    .filter_map(|p| p.get_hash(i))
                    .find(|h| *h != ZERO_CHUNK_HASH)
                    .unwrap_or(ZERO_CHUNK_HASH)
                    .to_string()
            };
        }
        drop(parents);
        drop(index);
        if let Some(fallback) = &self.fallback {
            for (i, hash) in saved.hashes.iter_mut().enumerate() {
                if *hash == ZERO_CHUNK_HASH && (i as u64) * (CHUNK_SIZE as u64) < fallback.size() {
                    let data = self.read_chunk(i)?;
                    if data.iter().any(|b| *b != 0) {
                        *hash = self.store.put(&data)?;
                    }
                }
            }
        }
        saved.save(path)
    }

    fn read_chunk(&self, chunk_idx: usize) -> std::io::Result<Vec<u8>> {
        // 1. Check dirty map
        if let Some(data) = self.dirty.read().unwrap().get(&chunk_idx) {
            return Ok(data.clone());
        }

        // 2. Check current index
        let hash = {
            let index = self.index.read().unwrap();
            index
                .get_hash(chunk_idx)
                .unwrap_or(ZERO_CHUNK_HASH)
                .to_string()
        };
        if hash != ZERO_CHUNK_HASH {
            return self.fetch_chunk(&hash);
        }

        // 3. Walk parent chain
        for parent in self.parents.read().unwrap().iter() {
            let parent_hash = parent.get_hash(chunk_idx).unwrap_or(ZERO_CHUNK_HASH);
            if parent_hash != ZERO_CHUNK_HASH {
                return self.fetch_chunk(parent_hash);
            }
        }

        // 4. Fallback to flat file (lazy ingestion at the bottom of the chain)
        if let Some(ref fb) = self.fallback {
            let offset = chunk_idx as u64 * CHUNK_SIZE as u64;
            if offset < fb.size() {
                let read_len = CHUNK_SIZE.min((fb.size() - offset) as usize);
                let mut buf = vec![0u8; read_len];
                if fb.read(offset, &mut buf)? != read_len {
                    return Err(std::io::Error::new(
                        std::io::ErrorKind::UnexpectedEof,
                        "short read from CAS base image",
                    ));
                }

                if buf.iter().all(|&b| b == 0) {
                    return Ok(vec![0u8; CHUNK_SIZE]);
                }

                // Cache in chunk store for future reads
                let new_hash = self
                    .store
                    .put(&buf)
                    .map_err(|e| std::io::Error::new(std::io::ErrorKind::Other, e.to_string()))?;
                self.index.write().unwrap().set_hash(chunk_idx, new_hash);
                return Ok(buf);
            }
        }

        // 5. Truly zero
        Ok(vec![0u8; CHUNK_SIZE])
    }

    fn fetch_chunk(&self, hash: &str) -> std::io::Result<Vec<u8>> {
        match self.store.get(hash) {
            Ok(Some(data)) => Ok(data),
            Ok(None) => {
                tracing::warn!("chunk {} not found in store, returning zeros", hash);
                Ok(vec![0u8; CHUNK_SIZE])
            }
            Err(e) => Err(std::io::Error::new(
                std::io::ErrorKind::Other,
                e.to_string(),
            )),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_chunk_store_put_get() {
        let tmp = tempfile::tempdir().unwrap();
        let store = LocalChunkStore::open(tmp.path().to_str().unwrap()).unwrap();

        let data = b"hello world";
        let hash = store.put(data).unwrap();
        let retrieved = store.get(&hash).unwrap().unwrap();
        assert_eq!(retrieved, data);
    }

    #[test]
    fn test_chunk_store_dedup() {
        let tmp = tempfile::tempdir().unwrap();
        let store = LocalChunkStore::open(tmp.path().to_str().unwrap()).unwrap();

        let data = b"same content";
        let h1 = store.put(data).unwrap();
        let h2 = store.put(data).unwrap();
        assert_eq!(h1, h2);
    }

    #[test]
    fn test_index_save_load() {
        let tmp = tempfile::tempdir().unwrap();
        let idx_path = tmp.path().join("test.idx");

        let mut index = ChunkIndex::new(1024 * 1024);
        index.set_hash(0, "abc123".to_string());
        index.set_hash(5, "def456".to_string());
        index.save(idx_path.to_str().unwrap()).unwrap();

        let loaded = ChunkIndex::load(idx_path.to_str().unwrap()).unwrap();
        assert_eq!(loaded.disk_size(), 1024 * 1024);
        assert_eq!(loaded.get_hash(0).unwrap(), "abc123");
        assert_eq!(loaded.get_hash(5).unwrap(), "def456");
        assert_eq!(loaded.get_hash(1).unwrap(), ZERO_CHUNK_HASH);
    }

    #[test]
    fn test_cas_backend_read_write() {
        let tmp = tempfile::tempdir().unwrap();
        let store = LocalChunkStore::open(tmp.path().to_str().unwrap()).unwrap();
        let index = ChunkIndex::new(256 * 1024); // 256KB = 4 chunks
        let backend = CasBackend::new(Box::new(store), index);

        // Write some data
        let data = b"hello from CAS";
        backend.write(100, data).unwrap();

        // Read it back
        let mut buf = vec![0u8; data.len()];
        backend.read(100, &mut buf).unwrap();
        assert_eq!(&buf, data);

        // Flush and read again (now from chunk store, not dirty map)
        backend.flush().unwrap();
        let mut buf2 = vec![0u8; data.len()];
        backend.read(100, &mut buf2).unwrap();
        assert_eq!(&buf2, data);
    }

    #[test]
    fn test_cas_backend_cross_chunk_write() {
        let tmp = tempfile::tempdir().unwrap();
        let store = LocalChunkStore::open(tmp.path().to_str().unwrap()).unwrap();
        let index = ChunkIndex::new(256 * 1024);
        let backend = CasBackend::new(Box::new(store), index);

        // Write across chunk boundary (chunk 0 ends at 65536)
        let offset = CHUNK_SIZE as u64 - 4;
        let data = b"crosschunk";
        backend.write(offset, data).unwrap();

        let mut buf = vec![0u8; data.len()];
        backend.read(offset, &mut buf).unwrap();
        assert_eq!(&buf, data);
    }

    #[test]
    fn test_load_rejects_mismatched_chunk_count() {
        let tmp = tempfile::tempdir().unwrap();
        let idx_path = tmp.path().join("bad.idx");

        // Write an index with disk_size=64KB (1 chunk) but num_chunks=100
        let mut f = fs::File::create(&idx_path).unwrap();
        f.write_all(&(CHUNK_SIZE as u64).to_le_bytes()).unwrap(); // disk_size
        f.write_all(&100u64.to_le_bytes()).unwrap(); // num_chunks (should be 1)
        f.write_all(&0u32.to_le_bytes()).unwrap(); // parent_path len
        f.write_all(&0u32.to_le_bytes()).unwrap(); // fallback_path len
        drop(f);

        match ChunkIndex::load(idx_path.to_str().unwrap()) {
            Err(e) => assert!(e.to_string().contains("chunk count")),
            Ok(_) => panic!("expected load to fail for mismatched chunk count"),
        }
    }

    #[test]
    fn test_check_size_against_backend() {
        let index = ChunkIndex::new(4 * 1024 * 1024); // 4MB virtual disk

        // flat file == virtual disk: ok
        assert!(index
            .check_size_against_backend(4 * 1024 * 1024, "test")
            .is_ok());
        // flat file < virtual disk: ok (sparse expansion — the normal case)
        assert!(index
            .check_size_against_backend(1024 * 1024, "test")
            .is_ok());
        // flat file > virtual disk: corrupt — must fail
        match index.check_size_against_backend(8 * 1024 * 1024, "test") {
            Err(e) => assert!(e.to_string().contains("exceeds")),
            Ok(_) => panic!("expected size check to fail when flat file exceeds virtual disk"),
        }
    }
}

#[cfg(test)]
mod checkpoint_tests {
    use super::*;

    #[test]
    fn saved_checkpoint_does_not_depend_on_mutable_base_file() {
        let dir = tempfile::tempdir().unwrap();
        let flat = dir.path().join("base.ext4");
        fs::write(&flat, b"unread base data").unwrap();
        let store = || Box::new(LocalChunkStore::open(dir.path().to_str().unwrap()).unwrap());
        let backend = CasBackend::with_fallback(
            store(),
            ChunkIndex::new(CHUNK_SIZE as u64),
            crate::backend::FlatFileBackend::open(flat.to_str().unwrap()).unwrap(),
        );
        let path = dir.path().join("saved.idx");
        backend.save_index(path.to_str().unwrap()).unwrap();
        fs::write(&flat, b"replaced image!").unwrap();
        let saved = ChunkIndex::load(path.to_str().unwrap()).unwrap();
        assert!(saved.fallback_path.is_none());
        let restored = CasBackend::new(store(), saved);
        let mut data = [0; 16];
        restored.read(0, &mut data).unwrap();
        assert_eq!(&data, b"unread base data");
    }

    #[test]
    fn replacing_a_parent_keeps_inherited_chunks() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("disk.idx");
        let path = path.to_str().unwrap();
        let store = || Box::new(LocalChunkStore::open(dir.path().to_str().unwrap()).unwrap());
        let base = CasBackend::new(store(), ChunkIndex::new(2 * CHUNK_SIZE as u64));
        base.write(0, b"base").unwrap();
        base.save_index(path).unwrap();
        let mut delta = ChunkIndex::new(2 * CHUNK_SIZE as u64);
        delta.parent_path = Some(path.to_string());
        let backend = CasBackend::new(store(), delta);
        backend.write(CHUNK_SIZE as u64, b"child").unwrap();
        backend.save_index(path).unwrap();
        let saved = ChunkIndex::load(path).unwrap();
        assert!(saved.parent_path.is_none());
        let restored = CasBackend::new(store(), saved);
        let mut head = [0; 4];
        restored.read(0, &mut head).unwrap();
        assert_eq!(&head, b"base");
        let mut tail = [0; 5];
        restored.read(CHUNK_SIZE as u64, &mut tail).unwrap();
        assert_eq!(&tail, b"child");
    }
}
