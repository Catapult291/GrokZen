//! Persistent index storage on redb.
//!
//! redb over LMDB: pure Rust, so cross-compiling to six release targets needs
//! no C toolchain, and the licence is MIT/Apache-2.0 with no attribution
//! obligation. BurntSushi picked it for ripgrep's experimental index too.
//!
//! **Write in bounded batches.** Measured on the prior art: switching from
//! per-entry synchronous writes to an unbounded async queue made indexing 5.7x
//! faster but grew steady-state RSS from 807 MB to 4049 MB, because every
//! pending value stayed buffered in memory. Batching into transactions of a
//! few thousand entries kept the full speedup at 883 MB. Never let the write
//! queue grow without a ceiling.
//!
//! Cold start must not deserialize the whole graph. Follow gopls: keep the
//! index on disk and load `O(files being worked on)`, not `O(repo)` — that one
//! architectural change bought gopls 53-83% of its memory back.

use crate::types::{
    CodeEdge, CodeFile, CodeSymbol, Confidence, EdgeKind, FileId, Language, RelPath, SymbolId,
    SymbolKind,
};
use redb::{
    Database, ReadOnlyDatabase, ReadableDatabase, ReadableTable, ReadableTableMetadata,
    TableDefinition,
};
use serde::{Deserialize, Serialize};
use std::path::Path;

#[derive(Debug, thiserror::Error)]
pub enum StoreError {
    #[error("storage: {0}")]
    Backend(String),
    #[error("schema version mismatch: found {found}, expected {expected}")]
    Version { found: u32, expected: u32 },
    #[error("database locked: {0}")]
    Locked(String),
    /// The file is not a readable database (truncated, overwritten, torn
    /// write). The store is a rebuildable cache, so this is safe to repair
    /// by moving the file aside and recreating it.
    #[error("storage corrupt: {0}")]
    Corrupt(String),
}

pub const SCHEMA_VERSION: u32 = 1;

/// Entries buffered before a transaction is committed.
pub const WRITE_BATCH: usize = 2048;

const META: TableDefinition<&str, u32> = TableDefinition::new("meta");
const FILES: TableDefinition<u32, &[u8]> = TableDefinition::new("files");
const SYMBOLS: TableDefinition<u32, &[u8]> = TableDefinition::new("symbols");
const EDGES: TableDefinition<&str, &[u8]> = TableDefinition::new("edges");
const PARSE_CACHE: TableDefinition<&str, &[u8]> = TableDefinition::new("parse-cache");
/// `path -> FileStamp` (JSON): lets a warm start skip reading a file whose
/// `(mtime, len)` is unchanged. Created lazily; absent in older databases.
const FILE_STAMPS: TableDefinition<&str, &[u8]> = TableDefinition::new("file-stamps");
const VERSION_KEY: &str = "schema-version";

pub struct Store {
    db: Db,
    read_only: bool,
}

/// A writable handle, or a shared read-only one that never touches the file
/// (redb's writable `open` rewrites the header, bumping mtime and taking the
/// exclusive lock).
enum Db {
    Rw(Database),
    Ro(ReadOnlyDatabase),
}

impl Db {
    fn begin_read(&self) -> Result<redb::ReadTransaction, redb::TransactionError> {
        match self {
            Db::Rw(db) => db.begin_read(),
            Db::Ro(db) => db.begin_read(),
        }
    }

    fn begin_write(&self) -> Result<redb::WriteTransaction, StoreError> {
        match self {
            Db::Rw(db) => db.begin_write().map_err(classify_txn_error),
            Db::Ro(_) => Err(StoreError::Backend("store opened read-only".to_string())),
        }
    }
}

/// What the previous index run learned about one file, enough to reuse its
/// cached parse without reading the file again.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FileStamp {
    pub mtime_ns: u128,
    pub len: u64,
    /// Full [`parse_cache_key`] (including any path salt).
    pub key: String,
    pub sha: String,
    pub loc: u32,
    pub language: u8,
}

impl FileStamp {
    pub fn language(&self) -> Option<Language> {
        language_from_u8(self.language)
    }
}

/// Prefix shared by every [`parse_cache_key`] written by this binary.
///
/// Deliberately free of the crate version: a release that changes neither
/// the extractor nor the payload encoding keeps every cached parse (it used
/// to re-parse the whole repository on each upgrade and leave the previous
/// generation's keys behind). Invalidation is explicit instead:
/// [`EXTRACTOR_VERSION`] for what is extracted, [`CACHE_FORMAT_VERSION`] for
/// how it is encoded.
pub fn parse_cache_key_prefix() -> String {
    format!("x{EXTRACTOR_VERSION}-f{CACHE_FORMAT_VERSION}:")
}

impl Store {
    /// Attempts before a lock-contention open gives up and the caller falls
    /// back to an unpersisted index. Six tries at 50ms spans the window in
    /// which a sibling cold start holds the file lock.
    pub const DEFAULT_RETRY_ATTEMPTS: usize = 6;
    /// Fixed wait between lock-contention retries.
    pub const DEFAULT_RETRY_DELAY: std::time::Duration = std::time::Duration::from_millis(50);

    pub fn open(path: &Path) -> Result<Self, StoreError> {
        let db = match Database::create(path) {
            Ok(db) => db,
            Err(redb::DatabaseError::DatabaseAlreadyOpen) => {
                return Err(StoreError::Locked(
                    "Database already open. Cannot acquire lock.".to_string(),
                ));
            }
            Err(error) => return Err(classify_open_error(error)),
        };
        // Fast path: an up-to-date database with every table needs no write
        // transaction. A writable open used to commit (and fsync) on every
        // incremental apply even though nothing changed.
        if Self::is_current(&db)? {
            return Ok(Self {
                db: Db::Rw(db),
                read_only: false,
            });
        }
        let write = db.begin_write().map_err(classify_txn_error)?;
        {
            let mut meta = write.open_table(META).map_err(backend)?;
            let found = meta
                .get(VERSION_KEY)
                .map_err(backend)?
                .map(|value| value.value());
            match found {
                Some(found) if found != SCHEMA_VERSION => {
                    return Err(StoreError::Version {
                        found,
                        expected: SCHEMA_VERSION,
                    });
                }
                None => {
                    meta.insert(VERSION_KEY, SCHEMA_VERSION).map_err(backend)?;
                }
                _ => {}
            }
        }
        // Opening a table in a write transaction creates it when absent.
        drop(write.open_table(FILES).map_err(backend)?);
        drop(write.open_table(SYMBOLS).map_err(backend)?);
        drop(write.open_table(EDGES).map_err(backend)?);
        drop(write.open_table(PARSE_CACHE).map_err(backend)?);
        drop(write.open_table(FILE_STAMPS).map_err(backend)?);
        write.commit().map_err(backend)?;
        Ok(Self {
            db: Db::Rw(db),
            read_only: false,
        })
    }

    /// Whether `db` already carries the current schema version and every
    /// table, so [`Self::open`] can skip its setup write. A version mismatch
    /// is `false`; the write path then reports it.
    fn is_current(db: &Database) -> Result<bool, StoreError> {
        let read = db.begin_read().map_err(classify_txn_error)?;
        let version = match read.open_table(META) {
            Ok(meta) => meta
                .get(VERSION_KEY)
                .map_err(backend)?
                .map(|value| value.value()),
            Err(redb::TableError::TableDoesNotExist(_)) => return Ok(false),
            Err(error) => return Err(backend(error)),
        };
        if version != Some(SCHEMA_VERSION) {
            return Ok(false);
        }
        fn exists<K: redb::Key + 'static>(
            read: &redb::ReadTransaction,
            table: TableDefinition<K, &[u8]>,
        ) -> Result<bool, StoreError> {
            match read.open_table(table) {
                Ok(_) => Ok(true),
                Err(redb::TableError::TableDoesNotExist(_)) => Ok(false),
                Err(error) => Err(backend(error)),
            }
        }
        Ok(exists(&read, FILES)?
            && exists(&read, SYMBOLS)?
            && exists(&read, EDGES)?
            && exists(&read, PARSE_CACHE)?
            && exists(&read, FILE_STAMPS)?)
    }

    /// Open an existing store without creating or modifying anything.
    ///
    /// Returns `Ok(None)` when there is no database file, or when its schema
    /// is not the current one (a read-only user must not delete it). Writes
    /// on the returned store fail. redb still takes its single-process file
    /// lock, so this reports [`StoreError::Locked`] while a writer holds it.
    pub fn open_read_only(path: &Path) -> Result<Option<Self>, StoreError> {
        if !path.is_file() {
            return Ok(None);
        }
        let db = match ReadOnlyDatabase::open(path) {
            Ok(db) => db,
            Err(redb::DatabaseError::DatabaseAlreadyOpen) => {
                return Err(StoreError::Locked(
                    "Database already open. Cannot acquire lock.".to_string(),
                ));
            }
            Err(error) => return Err(classify_open_error(error)),
        };
        let read = db.begin_read().map_err(classify_txn_error)?;
        let version = match read.open_table(META) {
            Ok(meta) => meta
                .get(VERSION_KEY)
                .map_err(backend)?
                .map(|value| value.value()),
            Err(_) => None,
        };
        if version != Some(SCHEMA_VERSION) {
            return Ok(None);
        }
        drop(read);
        Ok(Some(Self {
            db: Db::Ro(db),
            read_only: true,
        }))
    }

    /// [`Self::open_read_only`] with the same retry policy as
    /// [`Self::open_or_heal_with_retry`].
    pub fn open_read_only_with_retry(
        path: &Path,
        attempts: usize,
        delay: std::time::Duration,
    ) -> Result<Option<Self>, StoreError> {
        let attempts = attempts.max(1);
        for attempt in 0..attempts {
            match Self::open_read_only(path) {
                Ok(store) => return Ok(store),
                Err(error) => {
                    if attempt + 1 == attempts || !is_lock_contention(&error) {
                        return Err(error);
                    }
                    std::thread::sleep(delay);
                }
            }
        }
        unreachable!("retry loop returns on its final attempt")
    }

    pub fn is_read_only(&self) -> bool {
        self.read_only
    }

    /// Open an existing store, or delete and recreate it when the on-disk
    /// schema does not match [`SCHEMA_VERSION`].
    ///
    /// `open` still rejects a mismatch so callers that want to inspect the
    /// error can; indexing uses this path so a bump never takes the process
    /// down. Other open failures (permissions, a corrupt file) are returned
    /// unchanged — those are not safe to repair by deleting the database.
    pub fn open_or_heal(path: &Path) -> Result<Self, StoreError> {
        match Self::open(path) {
            Ok(store) => Ok(store),
            Err(StoreError::Version { found, expected }) => {
                tracing::warn!(
                    found,
                    expected,
                    path = %path.display(),
                    "schema version mismatch; recreating store"
                );
                match std::fs::remove_file(path) {
                    Ok(()) => {}
                    Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                    Err(error) => return Err(backend(error)),
                }
                Self::open(path)
            }
            Err(StoreError::Corrupt(reason)) => {
                // The file is a rebuildable cache; keep the broken copy for
                // inspection but never let it disable persistence forever.
                let stamp = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map(|d| d.as_secs())
                    .unwrap_or(0);
                let mut aside = path.as_os_str().to_owned();
                aside.push(format!(".bad-{stamp}"));
                tracing::warn!(
                    reason = %reason,
                    path = %path.display(),
                    "store corrupt; moving it aside and recreating"
                );
                if std::fs::rename(path, &aside).is_err() {
                    match std::fs::remove_file(path) {
                        Ok(()) => {}
                        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                        Err(error) => return Err(backend(error)),
                    }
                }
                Self::open(path)
            }
            Err(error) => Err(error),
        }
    }

    /// Open a store, retrying while another process holds the database lock.
    ///
    /// redb allows one writer process per file: when several processes cold
    /// start the same repository, the losers see `DatabaseAlreadyOpen` and
    /// used to fall straight back to an unpersisted full reparse. That error
    /// is a race the loser can win by waiting, not a broken database, so
    /// retry up to `attempts` times (0 counts as one attempt) with
    /// `initial_delay` between tries. Any error that is not plausibly lock
    /// contention — permissions, corruption, an unhealable schema — returns
    /// immediately: retrying cannot fix those.
    pub fn open_or_heal_with_retry(
        path: &Path,
        attempts: usize,
        initial_delay: std::time::Duration,
    ) -> Result<Self, StoreError> {
        let attempts = attempts.max(1);
        for attempt in 0..attempts {
            match Self::open_or_heal(path) {
                Ok(store) => {
                    if attempt > 0 {
                        tracing::info!(
                            attempt,
                            path = %path.display(),
                            "acquired store lock after retry"
                        );
                    }
                    return Ok(store);
                }
                Err(error) => {
                    if attempt + 1 == attempts || !is_lock_contention(&error) {
                        return Err(error);
                    }
                    std::thread::sleep(initial_delay);
                }
            }
        }
        unreachable!("retry loop returns on its final attempt")
    }

    pub fn put_files(&self, files: &[CodeFile]) -> Result<(), StoreError> {
        // `chunks` is the hard bound: no transaction can retain more than
        // WRITE_BATCH newly serialized values.
        for chunk in files.chunks(WRITE_BATCH) {
            let write = self.db.begin_write()?;
            {
                let mut table = write.open_table(FILES).map_err(backend)?;
                for file in chunk {
                    let value = serde_json::to_vec(&FileWire::from(file)).map_err(backend)?;
                    table.insert(file.id.0, value.as_slice()).map_err(backend)?;
                }
            }
            write.commit().map_err(backend)?;
        }
        Ok(())
    }

    pub fn put_symbols(&self, symbols: &[CodeSymbol]) -> Result<(), StoreError> {
        for chunk in symbols.chunks(WRITE_BATCH) {
            let write = self.db.begin_write()?;
            {
                let mut table = write.open_table(SYMBOLS).map_err(backend)?;
                for symbol in chunk {
                    let value = serde_json::to_vec(&SymbolWire::from(symbol)).map_err(backend)?;
                    table
                        .insert(symbol.id.0, value.as_slice())
                        .map_err(backend)?;
                }
            }
            write.commit().map_err(backend)?;
        }
        Ok(())
    }

    pub fn put_edges(&self, edges: &[CodeEdge]) -> Result<(), StoreError> {
        for chunk in edges.chunks(WRITE_BATCH) {
            let write = self.db.begin_write()?;
            {
                let mut table = write.open_table(EDGES).map_err(backend)?;
                for edge in chunk {
                    let key = edge_key(edge.from, edge.to, edge.kind);
                    let value = serde_json::to_vec(&EdgeWire::from(edge)).map_err(backend)?;
                    table
                        .insert(key.as_str(), value.as_slice())
                        .map_err(backend)?;
                }
            }
            write.commit().map_err(backend)?;
        }
        Ok(())
    }

    pub fn get_file(&self, id: FileId) -> Result<Option<CodeFile>, StoreError> {
        let read = self.db.begin_read().map_err(backend)?;
        let table = read.open_table(FILES).map_err(backend)?;
        let bytes = table.get(id.0).map_err(backend)?;
        bytes
            .map(|value| {
                serde_json::from_slice::<FileWire>(value.value())
                    .map(CodeFile::from)
                    .map_err(backend)
            })
            .transpose()
    }

    pub fn get_symbol(&self, id: SymbolId) -> Result<Option<CodeSymbol>, StoreError> {
        let read = self.db.begin_read().map_err(backend)?;
        let table = read.open_table(SYMBOLS).map_err(backend)?;
        let bytes = table.get(id.0).map_err(backend)?;
        bytes
            .map(|value| {
                serde_json::from_slice::<SymbolWire>(value.value())
                    .map(CodeSymbol::from)
                    .map_err(backend)
            })
            .transpose()
    }

    pub fn get_edge(
        &self,
        from: u32,
        to: u32,
        kind: EdgeKind,
    ) -> Result<Option<CodeEdge>, StoreError> {
        let read = self.db.begin_read().map_err(backend)?;
        let table = read.open_table(EDGES).map_err(backend)?;
        let key = edge_key(from, to, kind);
        let bytes = table.get(key.as_str()).map_err(backend)?;
        bytes
            .map(|value| {
                serde_json::from_slice::<EdgeWire>(value.value())
                    .map(CodeEdge::from)
                    .map_err(backend)
            })
            .transpose()
    }

    pub fn put_cached_parse(&self, sha: &str, parsed: &[u8]) -> Result<(), StoreError> {
        self.put_cached_parses([(sha, parsed)])
    }

    /// Write cached parse payloads in transactions of at most [`WRITE_BATCH`].
    ///
    /// An empty iterator is a no-op: a warm reindex must not pay for a write
    /// transaction when nothing changed.
    pub fn put_cached_parses<'a, I>(&self, entries: I) -> Result<(), StoreError>
    where
        I: IntoIterator<Item = (&'a str, &'a [u8])>,
    {
        self.ensure_writable()?;
        let entries: Vec<(&str, &[u8])> = entries.into_iter().collect();
        for chunk in entries.chunks(WRITE_BATCH) {
            let write = self.db.begin_write()?;
            {
                let mut table = write.open_table(PARSE_CACHE).map_err(backend)?;
                for (sha, parsed) in chunk {
                    table.insert(*sha, *parsed).map_err(backend)?;
                }
            }
            write.commit().map_err(backend)?;
        }
        Ok(())
    }

    fn ensure_writable(&self) -> Result<(), StoreError> {
        if self.read_only {
            Err(StoreError::Backend("store opened read-only".to_string()))
        } else {
            Ok(())
        }
    }

    /// Rewrite the file without its free pages (see
    /// [`Self::prune_parse_cache`]). Needs exclusive use of the handle: no
    /// transaction may be open.
    pub fn compact(&mut self) -> Result<bool, StoreError> {
        match &mut self.db {
            Db::Rw(db) => db.compact().map_err(backend),
            Db::Ro(_) => Err(StoreError::Backend("store opened read-only".to_string())),
        }
    }

    /// Number of parse-cache entries (diagnostics and tests).
    pub fn parse_cache_len(&self) -> Result<u64, StoreError> {
        let read = self.db.begin_read().map_err(backend)?;
        let table = read.open_table(PARSE_CACHE).map_err(backend)?;
        table.len().map_err(backend)
    }

    /// Delete every parse-cache entry whose key is not in `live` and every
    /// file stamp whose path is not in `live_paths`, then return how many
    /// parse entries were removed.
    ///
    /// Call after a *full* scan only: the live sets must cover every file of
    /// the repository, otherwise still-valid entries are dropped (that only
    /// costs a re-parse, never wrong data). Keys from other crate or
    /// extractor versions are never in `live`, so an upgrade's dead entries
    /// go the first time the new binary indexes. redb reuses freed pages, so
    /// the file stops growing; it does not shrink.
    pub fn prune_parse_cache(
        &self,
        live_keys: &std::collections::HashSet<String>,
        live_paths: &std::collections::HashSet<String>,
    ) -> Result<usize, StoreError> {
        self.ensure_writable()?;
        let stale_keys: Vec<String> = {
            let read = self.db.begin_read().map_err(backend)?;
            let table = read.open_table(PARSE_CACHE).map_err(backend)?;
            let mut stale = Vec::new();
            for entry in table.iter().map_err(backend)? {
                let (key, _) = entry.map_err(backend)?;
                if !live_keys.contains(key.value()) {
                    stale.push(key.value().to_owned());
                }
            }
            stale
        };
        let stale_paths: Vec<String> = {
            let read = self.db.begin_read().map_err(backend)?;
            let table = read.open_table(FILE_STAMPS).map_err(backend)?;
            let mut stale = Vec::new();
            for entry in table.iter().map_err(backend)? {
                let (key, _) = entry.map_err(backend)?;
                if !live_paths.contains(key.value()) {
                    stale.push(key.value().to_owned());
                }
            }
            stale
        };
        for chunk in stale_keys.chunks(WRITE_BATCH) {
            let write = self.db.begin_write()?;
            {
                let mut table = write.open_table(PARSE_CACHE).map_err(backend)?;
                for key in chunk {
                    table.remove(key.as_str()).map_err(backend)?;
                }
            }
            write.commit().map_err(backend)?;
        }
        for chunk in stale_paths.chunks(WRITE_BATCH) {
            let write = self.db.begin_write()?;
            {
                let mut table = write.open_table(FILE_STAMPS).map_err(backend)?;
                for key in chunk {
                    table.remove(key.as_str()).map_err(backend)?;
                }
            }
            write.commit().map_err(backend)?;
        }
        Ok(stale_keys.len())
    }

    /// Record `(mtime, len, key)` per path so a later run can skip reading
    /// an unchanged file. Batched like [`Self::put_cached_parses`].
    pub fn put_file_stamps(&self, stamps: &[(String, FileStamp)]) -> Result<(), StoreError> {
        self.ensure_writable()?;
        for chunk in stamps.chunks(WRITE_BATCH) {
            let write = self.db.begin_write()?;
            {
                let mut table = write.open_table(FILE_STAMPS).map_err(backend)?;
                for (path, stamp) in chunk {
                    let value = encode_stamp(stamp);
                    table
                        .insert(path.as_str(), value.as_slice())
                        .map_err(backend)?;
                }
            }
            write.commit().map_err(backend)?;
        }
        Ok(())
    }

    /// Stamp recorded for `path`, if any. A missing table (database from an
    /// older binary opened read-only) is just "no stamp".
    pub fn file_stamp(&self, path: &str) -> Result<Option<FileStamp>, StoreError> {
        let read = self.db.begin_read().map_err(backend)?;
        let table = match read.open_table(FILE_STAMPS) {
            Ok(table) => table,
            Err(redb::TableError::TableDoesNotExist(_)) => return Ok(None),
            Err(error) => return Err(backend(error)),
        };
        Ok(table
            .get(path)
            .map_err(backend)?
            .and_then(|value| decode_stamp(value.value())))
    }

    /// Cached parse result keyed by content hash, so an unchanged file is
    /// never reparsed.
    pub fn cached_parse(&self, sha: &str) -> Result<Option<Vec<u8>>, StoreError> {
        let read = self.db.begin_read().map_err(backend)?;
        let table = read.open_table(PARSE_CACHE).map_err(backend)?;
        Ok(table
            .get(sha)
            .map_err(backend)?
            .map(|value| value.value().to_vec()))
    }
}

/// Version of the symbol extractor (tree-sitter queries + `parse` logic).
///
/// Bump this whenever a change alters what `parse_file` produces for the
/// same bytes (new captures, different anchors/kinds). The parse cache is
/// keyed by content hash, so without this an upgraded binary keeps serving
/// symbols extracted by the old version until each file is edited.
pub const EXTRACTOR_VERSION: u32 = 6;

/// Encoding of parse-cache payloads and file stamps.
///
/// Bump when [`encode_parsed`] / the stamp encoding changes layout. It is part
/// of every key prefix, so old payloads are never decoded by a new reader
/// (they become cache misses and the next full scan prunes them).
///
/// History: 1 = serde_json (keys also carried the crate version), 2 =
/// compact varint binary.
pub const CACHE_FORMAT_VERSION: u32 = 2;

/// First byte of a binary parse payload; the second is the format version.
const PARSED_MAGIC: u8 = 0xA5;
/// First byte of a binary file stamp; the second is the format version.
const STAMP_MAGIC: u8 = 0xA6;

/// Cache key for a parsed file: extractor version, payload format, language
/// and content hash.
///
/// The store table is a flat string map, so the language prefix keeps two
/// files that happen to share bytes (a renamed `.js` → `.ts`, a copied
/// snippet) from handing each other the wrong query results. The version
/// prefix makes an extractor or encoding change re-parse instead of reusing
/// stale symbols.
pub fn parse_cache_key(language: Language, sha: &str) -> String {
    format!(
        "x{EXTRACTOR_VERSION}-f{CACHE_FORMAT_VERSION}:{}:{sha}",
        language.name()
    )
}

/// Serialize a [`crate::parse::ParsedFile`] for [`PARSE_CACHE`].
///
/// Compact binary (LEB128 varints, length-prefixed UTF-8, no field names, no
/// placeholder ids): several times smaller and faster to decode than the
/// JSON it replaced, which dominated warm starts on large repositories.
pub fn encode_parsed(parsed: &crate::parse::ParsedFile) -> Result<Vec<u8>, StoreError> {
    let mut size = 8 + parsed.imports.iter().map(|i| i.len() + 2).sum::<usize>();
    size += parsed
        .symbols
        .iter()
        .map(|s| s.name.len() + s.signature.len() + 10)
        .sum::<usize>();
    size += parsed.calls.iter().map(|(c, _)| c.len() + 5).sum::<usize>();
    let mut out = Vec::with_capacity(size);
    out.push(PARSED_MAGIC);
    out.push(CACHE_FORMAT_VERSION as u8);
    out.push(u8::from(parsed.partial) | (u8::from(parsed.c_family_rewrite) << 1));
    put_varint(&mut out, parsed.symbols.len() as u64);
    for symbol in &parsed.symbols {
        put_str(&mut out, &symbol.name);
        out.push(symbol_kind_to_u8(symbol.kind) | (u8::from(symbol.exported) << 7));
        put_str(&mut out, &symbol.signature);
        put_varint(&mut out, u64::from(symbol.start_line));
        // Spans are short; the delta keeps most end lines in one byte.
        put_varint(
            &mut out,
            u64::from(symbol.end_line.wrapping_sub(symbol.start_line)),
        );
    }
    put_varint(&mut out, parsed.imports.len() as u64);
    for import in &parsed.imports {
        put_str(&mut out, import);
    }
    put_varint(&mut out, parsed.calls.len() as u64);
    for (callee, line) in &parsed.calls {
        put_str(&mut out, callee);
        put_varint(&mut out, u64::from(*line));
    }
    Ok(out)
}

/// Inverse of [`encode_parsed`]. A corrupt, truncated or foreign-format
/// payload is a cache miss at the call site, not an index failure.
pub fn decode_parsed(bytes: &[u8]) -> Result<crate::parse::ParsedFile, StoreError> {
    let mut r = WireReader::new(bytes);
    if r.byte()? != PARSED_MAGIC || r.byte()? != CACHE_FORMAT_VERSION as u8 {
        return Err(StoreError::Backend(
            "parse payload: unknown format".to_string(),
        ));
    }
    let flags = r.byte()?;
    let n = r.len()?;
    let mut symbols = Vec::with_capacity(n);
    for _ in 0..n {
        let name = r.string()?;
        let tag = r.byte()?;
        let signature = r.string()?;
        let start_line = r.u32()?;
        let end_line = start_line.wrapping_add(r.u32()?);
        symbols.push(CodeSymbol {
            id: SymbolId(0),
            file: FileId(0),
            name,
            kind: symbol_kind_from_u8(tag & 0x7f),
            signature,
            start_line,
            end_line,
            exported: tag & 0x80 != 0,
        });
    }
    let n = r.len()?;
    let mut imports = Vec::with_capacity(n);
    for _ in 0..n {
        imports.push(r.string()?);
    }
    let n = r.len()?;
    let mut calls = Vec::with_capacity(n);
    for _ in 0..n {
        let callee = r.string()?;
        calls.push((callee, r.u32()?));
    }
    r.finish()?;
    Ok(crate::parse::ParsedFile {
        symbols,
        imports,
        calls,
        partial: flags & 1 != 0,
        c_family_rewrite: flags & 2 != 0,
    })
}

fn encode_stamp(stamp: &FileStamp) -> Vec<u8> {
    let mut out = Vec::with_capacity(stamp.key.len() + stamp.sha.len() + 32);
    out.push(STAMP_MAGIC);
    out.push(CACHE_FORMAT_VERSION as u8);
    put_varint(&mut out, (stamp.mtime_ns >> 64) as u64);
    put_varint(&mut out, stamp.mtime_ns as u64);
    put_varint(&mut out, stamp.len);
    put_str(&mut out, &stamp.key);
    put_str(&mut out, &stamp.sha);
    put_varint(&mut out, u64::from(stamp.loc));
    out.push(stamp.language);
    out
}

/// `None` for anything but a current-format stamp (an older binary's JSON
/// stamp is simply "no stamp": the file is re-read once and re-stamped).
fn decode_stamp(bytes: &[u8]) -> Option<FileStamp> {
    let mut r = WireReader::new(bytes);
    if r.byte().ok()? != STAMP_MAGIC || r.byte().ok()? != CACHE_FORMAT_VERSION as u8 {
        return None;
    }
    let hi = r.varint().ok()?;
    let lo = r.varint().ok()?;
    let stamp = FileStamp {
        mtime_ns: (u128::from(hi) << 64) | u128::from(lo),
        len: r.varint().ok()?,
        key: r.string().ok()?,
        sha: r.string().ok()?,
        loc: r.u32().ok()?,
        language: r.byte().ok()?,
    };
    r.finish().ok()?;
    Some(stamp)
}

fn put_varint(out: &mut Vec<u8>, mut value: u64) {
    while value >= 0x80 {
        out.push((value as u8) | 0x80);
        value >>= 7;
    }
    out.push(value as u8);
}

fn put_str(out: &mut Vec<u8>, s: &str) {
    put_varint(out, s.len() as u64);
    out.extend_from_slice(s.as_bytes());
}

/// Bounds-checked cursor over a binary payload. Every failure is a
/// [`StoreError::Backend`] the caller treats as a cache miss.
struct WireReader<'a> {
    buf: &'a [u8],
    pos: usize,
}

impl<'a> WireReader<'a> {
    fn new(buf: &'a [u8]) -> Self {
        Self { buf, pos: 0 }
    }

    fn truncated() -> StoreError {
        StoreError::Backend("parse payload: truncated".to_string())
    }

    fn byte(&mut self) -> Result<u8, StoreError> {
        let b = *self.buf.get(self.pos).ok_or_else(Self::truncated)?;
        self.pos += 1;
        Ok(b)
    }

    fn varint(&mut self) -> Result<u64, StoreError> {
        let mut value = 0u64;
        for shift in (0..64).step_by(7) {
            let b = self.byte()?;
            value |= u64::from(b & 0x7f) << shift;
            if b & 0x80 == 0 {
                return Ok(value);
            }
        }
        Err(StoreError::Backend("parse payload: bad varint".to_string()))
    }

    fn u32(&mut self) -> Result<u32, StoreError> {
        u32::try_from(self.varint()?)
            .map_err(|_| StoreError::Backend("parse payload: value out of range".to_string()))
    }

    /// A length that must fit in what is left of the buffer (each element is
    /// at least one byte), so a corrupt count cannot trigger a huge
    /// allocation.
    fn len(&mut self) -> Result<usize, StoreError> {
        let n = self.varint()?;
        if n > (self.buf.len() - self.pos) as u64 {
            return Err(Self::truncated());
        }
        Ok(n as usize)
    }

    fn string(&mut self) -> Result<String, StoreError> {
        let n = self.len()?;
        let bytes = &self.buf[self.pos..self.pos + n];
        self.pos += n;
        std::str::from_utf8(bytes)
            .map(str::to_owned)
            .map_err(|_| StoreError::Backend("parse payload: invalid utf-8".to_string()))
    }

    fn finish(&self) -> Result<(), StoreError> {
        if self.pos == self.buf.len() {
            Ok(())
        } else {
            Err(StoreError::Backend(
                "parse payload: trailing bytes".to_string(),
            ))
        }
    }
}

fn backend(error: impl std::fmt::Display) -> StoreError {
    StoreError::Backend(error.to_string())
}

/// Corruption (not locks, permissions or full disks) is the only open failure
/// that deleting the cache can fix.
fn classify_storage_error(error: redb::StorageError) -> StoreError {
    match error {
        redb::StorageError::Corrupted(reason) => StoreError::Corrupt(reason),
        // redb reports a wrong magic number / short header as an I/O error.
        redb::StorageError::Io(io)
            if matches!(
                io.kind(),
                std::io::ErrorKind::InvalidData | std::io::ErrorKind::UnexpectedEof
            ) =>
        {
            StoreError::Corrupt(io.to_string())
        }
        other => backend(other),
    }
}

fn classify_txn_error(error: redb::TransactionError) -> StoreError {
    match error {
        redb::TransactionError::Storage(storage) => classify_storage_error(storage),
        other => backend(other),
    }
}

fn classify_open_error(error: redb::DatabaseError) -> StoreError {
    match error {
        redb::DatabaseError::Storage(storage) => classify_storage_error(storage),
        redb::DatabaseError::UpgradeRequired(version) => {
            StoreError::Corrupt(format!("file format version {version} needs an upgrade"))
        }
        other => backend(other),
    }
}

/// Whether an open failure looks like another process holding the file lock.
///
/// redb maps contention to [`redb::DatabaseError::DatabaseAlreadyOpen`], which
/// [`Store::open`] turns into [`StoreError::Locked`]. For defense-in-depth,
/// we also match strings containing "already open".
fn is_lock_contention(error: &StoreError) -> bool {
    matches!(error, StoreError::Locked(_))
        || error.to_string().to_lowercase().contains("already open")
}

fn edge_key(from: u32, to: u32, kind: EdgeKind) -> String {
    format!("{from:08x}:{to:08x}:{}", edge_kind_to_u8(kind))
}

fn edge_kind_to_u8(kind: EdgeKind) -> u8 {
    match kind {
        EdgeKind::Import => 0,
        EdgeKind::Call => 1,
        EdgeKind::Inherit => 2,
        EdgeKind::Namespace => 3,
        EdgeKind::Pair => 4,
    }
}

#[derive(Serialize, Deserialize)]
struct FileWire {
    id: u32,
    path: String,
    language: Option<u8>,
    loc: u32,
    sha: String,
}

impl From<&CodeFile> for FileWire {
    fn from(file: &CodeFile) -> Self {
        Self {
            id: file.id.0,
            path: file.path.as_str().to_owned(),
            language: file.language.map(language_to_u8),
            loc: file.loc,
            sha: file.sha.clone(),
        }
    }
}

impl From<FileWire> for CodeFile {
    fn from(file: FileWire) -> Self {
        Self {
            id: FileId(file.id),
            path: RelPath::new(file.path),
            language: file.language.and_then(language_from_u8),
            loc: file.loc,
            sha: file.sha,
        }
    }
}

pub fn language_to_u8(language: Language) -> u8 {
    match language {
        Language::Python => 0,
        Language::Go => 1,
        Language::Java => 2,
        Language::Rust => 3,
        Language::TypeScript => 4,
        Language::Tsx => 5,
        Language::JavaScript => 6,
        Language::C => 7,
        Language::Cpp => 8,
        Language::ObjC => 9,
        Language::ObjCpp => 10,
        Language::Swift => 11,
        Language::Php => 12,
        Language::Vue => 13,
        Language::Dart => 14,
        Language::CSharp => 15,
        Language::VisualBasic => 16,
        // lang-batch-8: append only, never renumber.
        Language::Kotlin => 17,
        Language::Shell => 18,
        Language::Ruby => 19,
        Language::Lua => 20,
    }
}

pub fn language_from_u8(language: u8) -> Option<Language> {
    Some(match language {
        0 => Language::Python,
        1 => Language::Go,
        2 => Language::Java,
        3 => Language::Rust,
        4 => Language::TypeScript,
        5 => Language::Tsx,
        6 => Language::JavaScript,
        7 => Language::C,
        8 => Language::Cpp,
        9 => Language::ObjC,
        10 => Language::ObjCpp,
        11 => Language::Swift,
        12 => Language::Php,
        13 => Language::Vue,
        14 => Language::Dart,
        15 => Language::CSharp,
        16 => Language::VisualBasic,
        17 => Language::Kotlin,
        18 => Language::Shell,
        19 => Language::Ruby,
        20 => Language::Lua,
        _ => return None,
    })
}

#[derive(Serialize, Deserialize)]
struct SymbolWire {
    id: u32,
    file: u32,
    name: String,
    kind: u8,
    signature: String,
    start_line: u32,
    end_line: u32,
    exported: bool,
}

impl From<&CodeSymbol> for SymbolWire {
    fn from(symbol: &CodeSymbol) -> Self {
        Self {
            id: symbol.id.0,
            file: symbol.file.0,
            name: symbol.name.clone(),
            kind: symbol_kind_to_u8(symbol.kind),
            signature: symbol.signature.clone(),
            start_line: symbol.start_line,
            end_line: symbol.end_line,
            exported: symbol.exported,
        }
    }
}

impl From<SymbolWire> for CodeSymbol {
    fn from(symbol: SymbolWire) -> Self {
        Self {
            id: SymbolId(symbol.id),
            file: FileId(symbol.file),
            name: symbol.name,
            kind: symbol_kind_from_u8(symbol.kind),
            signature: symbol.signature,
            start_line: symbol.start_line,
            end_line: symbol.end_line,
            exported: symbol.exported,
        }
    }
}

fn symbol_kind_to_u8(kind: SymbolKind) -> u8 {
    match kind {
        SymbolKind::Function => 0,
        SymbolKind::Method => 1,
        SymbolKind::Class => 2,
        SymbolKind::Interface => 3,
        SymbolKind::Struct => 4,
        SymbolKind::Enum => 5,
        SymbolKind::Trait => 6,
        SymbolKind::Type => 7,
        SymbolKind::Const => 8,
        SymbolKind::Module => 9,
        SymbolKind::Field => 10,
        SymbolKind::Variable => 11,
    }
}

fn symbol_kind_from_u8(kind: u8) -> SymbolKind {
    match kind {
        0 => SymbolKind::Function,
        1 => SymbolKind::Method,
        2 => SymbolKind::Class,
        3 => SymbolKind::Interface,
        4 => SymbolKind::Struct,
        5 => SymbolKind::Enum,
        6 => SymbolKind::Trait,
        7 => SymbolKind::Type,
        8 => SymbolKind::Const,
        9 => SymbolKind::Module,
        10 => SymbolKind::Field,
        11 => SymbolKind::Variable,
        _ => SymbolKind::Function,
    }
}

#[derive(Serialize, Deserialize)]
struct EdgeWire {
    from: u32,
    to: u32,
    kind: u8,
    weight: u32,
    confidence: u8,
}

impl From<&CodeEdge> for EdgeWire {
    fn from(edge: &CodeEdge) -> Self {
        Self {
            from: edge.from,
            to: edge.to,
            kind: edge_kind_to_u8(edge.kind),
            weight: edge.weight,
            confidence: match edge.confidence {
                Confidence::Exact => 0,
                Confidence::Scoped => 1,
                Confidence::Syntactic => 2,
                Confidence::Unknown => 3,
            },
        }
    }
}

impl From<EdgeWire> for CodeEdge {
    fn from(edge: EdgeWire) -> Self {
        Self {
            from: edge.from,
            to: edge.to,
            kind: match edge.kind {
                0 => EdgeKind::Import,
                1 => EdgeKind::Call,
                2 => EdgeKind::Inherit,
                3 => EdgeKind::Namespace,
                4 => EdgeKind::Pair,
                // Unknown future tags stay Call so an older reader does not
                // invent Import edges. Namespace is tag 3, append-only.
                _ => EdgeKind::Call,
            },
            weight: edge.weight,
            confidence: match edge.confidence {
                0 => Confidence::Exact,
                1 => Confidence::Scoped,
                2 => Confidence::Syntactic,
                _ => Confidence::Unknown,
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use std::time::{Duration, SystemTime, UNIX_EPOCH};

    struct TestDir(std::path::PathBuf);

    impl TestDir {
        fn new(name: &str) -> Self {
            let unique = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos();
            let path = std::env::temp_dir().join(format!(
                "astrolabe-store-{name}-{}-{unique}",
                std::process::id()
            ));
            std::fs::create_dir_all(&path).unwrap();
            Self(path)
        }

        fn db(&self) -> std::path::PathBuf {
            self.0.join("index.redb")
        }
    }

    impl Drop for TestDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn file(id: u32) -> CodeFile {
        CodeFile {
            id: FileId(id),
            path: RelPath::new(format!("src/{id}.rs")),
            language: Some(Language::Rust),
            loc: id + 1,
            sha: format!("sha-{id}"),
        }
    }

    #[test]
    fn open_reopen_preserves_data() {
        let dir = TestDir::new("reopen");
        {
            let store = Store::open(&dir.db()).unwrap();
            store.put_files(&[file(7)]).unwrap();
        }
        let store = Store::open(&dir.db()).unwrap();
        let loaded = store.get_file(FileId(7)).unwrap().unwrap();
        assert_eq!(loaded.path.as_str(), "src/7.rs");
        assert_eq!(loaded.sha, "sha-7");
    }

    #[test]
    fn schema_version_mismatch_is_rejected() {
        let dir = TestDir::new("version");
        {
            let store = Store::open(&dir.db()).unwrap();
            drop(store);
            let db = Database::create(dir.db()).unwrap();
            let write = db.begin_write().unwrap();
            {
                let mut meta = write.open_table(META).unwrap();
                meta.insert(VERSION_KEY, SCHEMA_VERSION + 1).unwrap();
            }
            write.commit().unwrap();
        }
        assert!(matches!(
            Store::open(&dir.db()),
            Err(StoreError::Version {
                found,
                expected
            }) if found == SCHEMA_VERSION + 1 && expected == SCHEMA_VERSION
        ));
    }

    #[test]
    fn writes_more_than_one_batch_and_reads_every_entry() {
        let dir = TestDir::new("batch");
        let store = Store::open(&dir.db()).unwrap();
        let files: Vec<_> = (0..5000).map(file).collect();
        store.put_files(&files).unwrap();
        for id in 0..5000 {
            let loaded = store.get_file(FileId(id)).unwrap().unwrap();
            assert_eq!(loaded.id, FileId(id));
            assert_eq!(loaded.sha, format!("sha-{id}"));
        }
    }

    #[test]
    fn parse_cache_hit_and_miss() {
        let dir = TestDir::new("cache");
        let store = Store::open(&dir.db()).unwrap();
        assert_eq!(store.cached_parse("missing").unwrap(), None);
        store.put_cached_parse("abc", b"parsed tree").unwrap();
        assert_eq!(
            store.cached_parse("abc").unwrap(),
            Some(b"parsed tree".to_vec())
        );
    }

    #[test]
    fn concurrent_reads_do_not_panic() {
        let dir = TestDir::new("reads");
        let store = Arc::new(Store::open(&dir.db()).unwrap());
        store
            .put_files(&(0..64).map(file).collect::<Vec<_>>())
            .unwrap();
        let threads: Vec<_> = (0..8)
            .map(|thread| {
                let store = Arc::clone(&store);
                std::thread::spawn(move || {
                    for offset in 0..64 {
                        let id = (thread + offset) % 64;
                        assert!(store.get_file(FileId(id)).unwrap().is_some());
                    }
                })
            })
            .collect();
        for thread in threads {
            thread.join().unwrap();
        }
    }

    #[test]
    fn open_or_heal_rebuilds_on_schema_mismatch() {
        let dir = TestDir::new("heal");
        {
            let store = Store::open(&dir.db()).unwrap();
            store.put_cached_parse("old", b"stale").unwrap();
            drop(store);
            let db = Database::create(dir.db()).unwrap();
            let write = db.begin_write().unwrap();
            {
                let mut meta = write.open_table(META).unwrap();
                meta.insert(VERSION_KEY, SCHEMA_VERSION + 1).unwrap();
            }
            write.commit().unwrap();
        }
        let store = Store::open_or_heal(&dir.db()).unwrap();
        assert_eq!(
            store.cached_parse("old").unwrap(),
            None,
            "recreated store must not keep the previous schema's cache"
        );
        store.put_cached_parse("new", b"fresh").unwrap();
        assert_eq!(store.cached_parse("new").unwrap(), Some(b"fresh".to_vec()));
        // A healed store is a current-schema store; open must accept it.
        drop(store);
        Store::open(&dir.db()).unwrap();
    }

    #[test]
    fn put_cached_parses_writes_more_than_one_batch() {
        let dir = TestDir::new("parse-batch");
        let store = Store::open(&dir.db()).unwrap();
        let payloads: Vec<(String, Vec<u8>)> = (0..WRITE_BATCH + 128)
            .map(|i| (format!("k{i}"), format!("v{i}").into_bytes()))
            .collect();
        let refs: Vec<(&str, &[u8])> = payloads
            .iter()
            .map(|(k, v)| (k.as_str(), v.as_slice()))
            .collect();
        store.put_cached_parses(refs).unwrap();
        assert_eq!(store.cached_parse("k0").unwrap(), Some(b"v0".to_vec()));
        assert_eq!(
            store
                .cached_parse(&format!("k{}", WRITE_BATCH + 127))
                .unwrap(),
            Some(format!("v{}", WRITE_BATCH + 127).into_bytes())
        );
    }

    #[test]
    fn parsed_file_roundtrip_through_cache() {
        let dir = TestDir::new("parsed");
        let store = Store::open(&dir.db()).unwrap();
        let parsed = crate::parse::ParsedFile {
            symbols: vec![CodeSymbol {
                id: SymbolId(0),
                file: FileId(0),
                name: "Hello".into(),
                kind: SymbolKind::Function,
                signature: "func Hello()".into(),
                start_line: 1,
                end_line: 3,
                exported: true,
            }],
            imports: vec!["fmt".into()],
            calls: vec![("Println".into(), 2)],
            partial: false,
            c_family_rewrite: false,
        };
        let bytes = encode_parsed(&parsed).unwrap();
        let key = parse_cache_key(Language::Go, "abc");
        store.put_cached_parse(&key, &bytes).unwrap();
        let loaded = decode_parsed(&store.cached_parse(&key).unwrap().unwrap()).unwrap();
        assert_eq!(loaded.imports, parsed.imports);
        assert_eq!(loaded.calls, parsed.calls);
        assert_eq!(loaded.symbols[0].name, "Hello");
        assert_eq!(loaded.symbols[0].kind, SymbolKind::Function);
        assert!(loaded.symbols[0].exported);
    }

    #[test]
    fn partial_flag_round_trips_and_legacy_json_is_a_miss() {
        let mut parsed = crate::parse::ParsedFile {
            symbols: vec![],
            imports: vec![],
            calls: vec![],
            partial: true,
            c_family_rewrite: false,
        };
        let bytes = encode_parsed(&parsed).unwrap();
        assert!(decode_parsed(&bytes).unwrap().partial);
        parsed.partial = false;
        parsed.c_family_rewrite = true;
        let clean = encode_parsed(&parsed).unwrap();
        let back = decode_parsed(&clean).unwrap();
        assert!(!back.partial && back.c_family_rewrite);
        // A format-1 (JSON) payload is rejected, never misread.
        let legacy = br#"{"symbols":[],"imports":[],"calls":[]}"#;
        assert!(decode_parsed(legacy).is_err());
    }

    #[test]
    fn binary_payload_round_trips_and_rejects_corruption() {
        let parsed = crate::parse::ParsedFile {
            symbols: vec![
                CodeSymbol {
                    id: SymbolId(0),
                    file: FileId(0),
                    name: "Ünïcode_名".into(),
                    kind: SymbolKind::Variable,
                    signature: "let x = \"y\";".into(),
                    start_line: 300_000,
                    end_line: 300_200,
                    exported: true,
                },
                CodeSymbol {
                    id: SymbolId(0),
                    file: FileId(0),
                    name: String::new(),
                    kind: SymbolKind::Field,
                    signature: String::new(),
                    start_line: 1,
                    end_line: 1,
                    exported: false,
                },
            ],
            imports: vec!["a/b".into(), String::new()],
            calls: vec![("x.y".into(), u32::MAX)],
            partial: false,
            c_family_rewrite: false,
        };
        let bytes = encode_parsed(&parsed).unwrap();
        let back = decode_parsed(&bytes).unwrap();
        assert_eq!(encode_parsed(&back).unwrap(), bytes);
        assert_eq!(back.symbols[0].name, "Ünïcode_名");
        assert_eq!(back.symbols[0].end_line, 300_200);
        assert_eq!(back.symbols[1].kind, SymbolKind::Field);
        assert!(back.symbols[0].exported && !back.symbols[1].exported);
        assert_eq!(back.calls, parsed.calls);
        assert_eq!(back.imports, parsed.imports);
        // Every truncation and a trailing byte are errors, not panics.
        for cut in 0..bytes.len() {
            assert!(decode_parsed(&bytes[..cut]).is_err(), "cut {cut}");
        }
        let mut long = bytes.clone();
        long.push(0);
        assert!(decode_parsed(&long).is_err());
        // A huge count cannot allocate past the buffer.
        let mut huge = bytes[..3].to_vec();
        huge.extend_from_slice(&[0xff, 0xff, 0xff, 0xff, 0x0f]);
        assert!(decode_parsed(&huge).is_err());
    }

    #[test]
    fn file_stamp_round_trips_in_the_current_format_only() {
        let stamp = FileStamp {
            mtime_ns: (1u128 << 70) + 12_345,
            len: 9_000_000_000,
            key: parse_cache_key(Language::Rust, "deadbeef"),
            sha: "deadbeef".into(),
            loc: 77,
            language: language_to_u8(Language::Rust),
        };
        assert_eq!(decode_stamp(&encode_stamp(&stamp)), Some(stamp.clone()));
        let json = serde_json::to_vec(&stamp).unwrap();
        assert_eq!(decode_stamp(&json), None);
        let dir = TestDir::new("stamp");
        let store = Store::open(&dir.db()).unwrap();
        store
            .put_file_stamps(&[("src/a.rs".into(), stamp.clone())])
            .unwrap();
        assert_eq!(store.file_stamp("src/a.rs").unwrap(), Some(stamp));
    }

    #[test]
    fn reopening_a_current_store_skips_the_setup_write() {
        let dir = TestDir::new("fast-open");
        drop(Store::open(&dir.db()).unwrap());
        let db = Database::create(dir.db()).unwrap();
        assert!(Store::is_current(&db).unwrap());
        drop(db);
        // Still a working writable store.
        let store = Store::open(&dir.db()).unwrap();
        store.put_cached_parse("k", b"v").unwrap();
        assert_eq!(store.parse_cache_len().unwrap(), 1);
    }

    /// Sources covering the common constructs of every extractor, for
    /// [`extractor_output_matches_its_version`].
    const EXTRACTOR_PROBES: &[(Language, &str, &str)] = &[
        (Language::Python, "m.py", "import os\nfrom a.b import c\n\nclass K(Base):\n    X = 1\n    def m(self):\n        return helper(os.path)\n\ndef helper(p):\n    return K().m()\n"),
        (Language::Go, "m.go", "package m\n\nimport (\n\t\"fmt\"\n\tx \"example.com/x\"\n)\n\ntype S struct{ A int }\ntype I interface{ M() }\n\nconst C = 1\n\nfunc (s *S) M() { fmt.Println(x.F()) }\nfunc New() *S { return &S{} }\n"),
        (Language::Java, "M.java", "package p;\nimport java.util.List;\npublic class M extends B implements I {\n  private int f;\n  public M() {}\n  public void run(List<String> xs) { helper(xs); }\n  interface Inner { void x(); }\n  enum E { A, B }\n}\n"),
        (Language::Rust, "m.rs", "use crate::a::{b, c};\nmod inner;\npub struct S { f: u32 }\npub enum E { A, B }\npub trait T { fn t(&self); }\nimpl T for S { fn t(&self) { helper(self.f); } }\npub const C: u32 = 1;\nfn helper(x: u32) -> u32 { x }\nmacro_rules! m { () => {} }\n"),
        (Language::TypeScript, "m.ts", "import { a } from './a';\nimport * as b from 'b';\nexport interface I { x: number }\nexport type T = I | null;\nexport enum E { A }\nexport class K implements I { x = 1; m(): void { a(b.c()); } }\nexport function f(): T { return null; }\nexport const g = () => f();\n"),
        (Language::Tsx, "m.tsx", "import React from 'react';\nexport function App(props: { n: number }) {\n  const v = useThing();\n  return <div onClick={() => go(v)}>{props.n}</div>;\n}\nexport const Button = () => <b>{go(1)}</b>;\nexport default App;\n"),
        (Language::JavaScript, "m.js", "const fs = require('fs');\nimport x from './x.js';\nclass K { m() { return fs.readFileSync(x); } }\nfunction f() { return new K().m(); }\nmodule.exports = { f };\n"),
        (Language::C, "m.c", "#include <stdio.h>\n#include \"m.h\"\n#define N 3\nstruct S { int a; };\ntypedef struct S S_t;\nenum E { A, B };\nstatic int helper(int x) { return x; }\nint main(void) { printf(\"%d\", helper(N)); return 0; }\n"),
        (Language::Cpp, "m.cpp", "#include <vector>\nnamespace n {\nclass K : public B {\n public:\n  K();\n  void m() const { helper(); }\n};\ntemplate <typename T> T id(T t) { return t; }\n}\nusing V = std::vector<int>;\n"),
        (Language::ObjC, "m.m", "#import \"m.h\"\n@interface K : NSObject\n- (void)run;\n@end\n@implementation K\n- (void)run { [self helper]; }\n@end\n"),
        (Language::Swift, "m.swift", "import Foundation\nprotocol P { func p() }\nstruct S: P { let a: Int\n  func p() { helper(a) } }\nclass K { var v = 1\n  init() {} }\nenum E { case a, b }\nfunc helper(_ x: Int) {}\n"),
        (Language::Php, "m.php", "<?php\nnamespace App;\nuse Foo\\Bar;\ninterface I { public function i(); }\nclass K extends Bar implements I {\n  const C = 1;\n  public function i() { return helper($this); }\n}\nfunction helper($x) { return $x; }\n"),
        (Language::Dart, "m.dart", "import 'package:a/a.dart';\nabstract class I { void i(); }\nclass K extends B implements I {\n  final int v;\n  K(this.v);\n  @override\n  void i() { helper(v); }\n}\nvoid helper(int x) {}\nenum E { a, b }\n"),
        (Language::CSharp, "M.cs", "using System;\nusing Foo.Bar;\nnamespace App {\n  public interface I { void M(); }\n  public class K : B, I {\n    public int P { get; set; }\n    public void M() { Helper(P); }\n    private static void Helper(int x) {}\n  }\n  public enum E { A, B }\n  public record R(int X);\n}\n"),
        (Language::VisualBasic, "M.vb", "Imports System\nNamespace App\n  Public Class K\n    Public Property P As Integer\n    Public Sub M()\n      Helper(P)\n    End Sub\n    Private Function Helper(x As Integer) As Integer\n      Return x\n    End Function\n  End Class\nEnd Namespace\n"),
        (Language::Kotlin, "m.kt", "package p\n\nimport a.b.C\n\ninterface I {\n    fun i()\n}\n\nclass K : I {\n    override fun i() {\n        helper(1)\n    }\n}\n\nfun helper(x: Int): Int {\n    return x\n}\n\nobject O\n"),
        (Language::Shell, "m.sh", "#!/bin/bash\nsource ./lib.sh\nreadonly N=3\nhelper() { echo \"$1\"; }\nfunction main {\n  helper \"$N\"\n}\nmain \"$@\"\n"),
        (Language::Ruby, "m.rb", "require 'json'\nrequire_relative 'a'\nmodule M\n  class K < Base\n    C = 1\n    def m\n      helper(C)\n    end\n    def self.s; end\n  end\nend\ndef helper(x) = x\n"),
        (Language::Lua, "m.lua", "local a = require('a')\nlocal M = {}\nfunction M.f(x) return a.g(x) end\nlocal function helper(y) return y end\nfunction M:m() return helper(self) end\nreturn M\n"),
    ];

    /// What [`EXTRACTOR_PROBES`] extract under the current [`EXTRACTOR_VERSION`].
    /// See [`extractor_output_matches_its_version`].
    const EXTRACTOR_FINGERPRINT: (u32, u64) = (6, 0x1240_3a72_764b_e436);

    /// Guard for the explicit cache versioning. The parse cache is no longer
    /// invalidated by every release, so an extractor change that forgets to
    /// bump [`EXTRACTOR_VERSION`] would keep serving the old symbols from
    /// every user's cache. If this fails: bump `EXTRACTOR_VERSION` (when the
    /// change is intended) and paste the printed fingerprint.
    #[test]
    fn extractor_output_matches_its_version() {
        let pool = crate::parse::ParserPool::new();
        let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
        let mut feed = |bytes: &[u8]| {
            for b in bytes {
                hash ^= u64::from(*b);
                hash = hash.wrapping_mul(0x100_0000_01b3);
            }
        };
        for (language, path, source) in EXTRACTOR_PROBES {
            let parsed = pool
                .parse(*language, &RelPath::new(*path), source)
                .unwrap_or_else(|e| panic!("{path}: {e}"));
            assert!(
                parsed.symbols.len() >= 2 && !parsed.partial,
                "{path}: probe must parse cleanly and extract enough to guard anything"
            );
            feed(language.name().as_bytes());
            feed(&encode_parsed(&parsed).unwrap());
        }
        let expected = EXTRACTOR_FINGERPRINT;
        assert_eq!(
            (EXTRACTOR_VERSION, hash),
            expected,
            "extractor output changed: if intended, bump EXTRACTOR_VERSION \
             and set EXTRACTOR_FINGERPRINT to ({EXTRACTOR_VERSION}, {hash:#018x})"
        );
    }

    #[test]
    fn parse_cache_key_is_versioned() {
        let key = parse_cache_key(Language::Go, "abc");
        assert!(
            key.ends_with(&format!(":{}:abc", Language::Go.name())),
            "{key}"
        );
        assert!(key.starts_with(&parse_cache_key_prefix()), "{key}");
        assert!(key.contains(&format!("x{EXTRACTOR_VERSION}")), "{key}");
        assert!(key.contains(&format!("-f{CACHE_FORMAT_VERSION}:")), "{key}");
        // A release must not invalidate the cache by itself: the crate
        // version is not part of the key.
        assert!(
            !key.contains(&format!("-v{}", env!("CARGO_PKG_VERSION"))),
            "{key}"
        );
        // A key written by a pre-versioned binary must never be hit.
        assert_ne!(key, format!("{}:abc", Language::Go.name()));
        // Nor one written by the JSON-format binaries (`x{N}-v{crate}:`).
        assert!(!key.starts_with(&format!("x{EXTRACTOR_VERSION}-v")));
    }

    #[test]
    fn test_open_or_heal_with_retry_succeeds_when_lock_released() {
        let dir = TestDir::new("retry");
        let holder = Store::open(&dir.db()).unwrap();
        let path = dir.db();
        let handle = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(40));
            drop(holder);
        });
        // The first attempts lose the race for the file lock; the winner must
        // be acquired once the sibling handle is dropped, well inside the
        // 10 x 50ms retry budget.
        let store = Store::open_or_heal_with_retry(&path, 10, Duration::from_millis(50)).unwrap();
        handle.join().unwrap();
        store.put_files(&[file(9)]).unwrap();
        assert_eq!(store.get_file(FileId(9)).unwrap().unwrap().sha, "sha-9");
    }

    #[test]
    fn test_open_or_heal_with_retry_fails_when_lock_never_released() {
        let dir = TestDir::new("retry-fail");
        let _holder = Store::open(&dir.db()).unwrap();
        let err = match Store::open_or_heal_with_retry(&dir.db(), 2, Duration::from_millis(20)) {
            Ok(_) => panic!("expected lock failure"),
            Err(e) => e,
        };
        assert!(matches!(err, StoreError::Locked(_)));
        assert!(is_lock_contention(&err));
    }

    #[test]
    fn test_is_lock_contention_classifier() {
        assert!(is_lock_contention(&StoreError::Locked("busy".into())));
        assert!(is_lock_contention(&StoreError::Backend(
            "Database already open".into()
        )));
        assert!(!is_lock_contention(&StoreError::Backend(
            "Permission denied".into()
        )));
        assert!(!is_lock_contention(&StoreError::Version {
            found: 1,
            expected: 2
        }));
    }

    #[test]
    fn corrupt_store_is_moved_aside_and_recreated() {
        let dir = TestDir::new("corrupt");
        std::fs::write(dir.db(), vec![0xABu8; 4096]).unwrap();
        // Plain `open` still reports it.
        assert!(matches!(
            Store::open(&dir.db()),
            Err(StoreError::Corrupt(_))
        ));
        let store = Store::open_or_heal(&dir.db()).expect("heals a corrupt cache");
        store.put_cached_parse("k", b"v").unwrap();
        assert_eq!(store.cached_parse("k").unwrap(), Some(b"v".to_vec()));
        drop(store);
        Store::open(&dir.db()).expect("recreated database is valid");
        let aside = std::fs::read_dir(&dir.0)
            .unwrap()
            .filter_map(|e| e.ok())
            .any(|e| e.file_name().to_string_lossy().contains(".bad-"));
        assert!(aside, "the broken file is kept for inspection");
    }

    #[test]
    fn truncated_store_is_healed_too() {
        let dir = TestDir::new("truncated");
        {
            let store = Store::open(&dir.db()).unwrap();
            store.put_cached_parse("k", &vec![7u8; 100_000]).unwrap();
        }
        let len = std::fs::metadata(dir.db()).unwrap().len();
        let f = std::fs::OpenOptions::new()
            .write(true)
            .open(dir.db())
            .unwrap();
        f.set_len(len / 3).unwrap();
        drop(f);
        let store = Store::open_or_heal(&dir.db()).expect("heals a truncated cache");
        assert_eq!(store.cached_parse("k").unwrap(), None);
    }

    #[test]
    fn read_only_open_never_creates_or_writes() {
        let dir = TestDir::new("ro");
        assert!(Store::open_read_only(&dir.db()).unwrap().is_none());
        assert!(!dir.db().exists());
        {
            let store = Store::open(&dir.db()).unwrap();
            store.put_cached_parse("k", b"v").unwrap();
        }
        let ro = Store::open_read_only(&dir.db()).unwrap().unwrap();
        assert!(ro.is_read_only());
        assert_eq!(ro.cached_parse("k").unwrap(), Some(b"v".to_vec()));
        assert!(ro.put_cached_parse("x", b"y").is_err());
    }

    #[test]
    fn prune_keeps_only_live_entries_and_stamps() {
        let dir = TestDir::new("prune");
        let store = Store::open(&dir.db()).unwrap();
        store
            .put_cached_parses([("a", b"1".as_slice()), ("b", b"2".as_slice())])
            .unwrap();
        let stamp = |k: &str| FileStamp {
            mtime_ns: 1,
            len: 1,
            key: k.into(),
            sha: "s".into(),
            loc: 1,
            language: 0,
        };
        store
            .put_file_stamps(&[("p1".into(), stamp("a")), ("p2".into(), stamp("b"))])
            .unwrap();
        let live_keys = ["a".to_string()].into_iter().collect();
        let live_paths = ["p1".to_string()].into_iter().collect();
        assert_eq!(store.prune_parse_cache(&live_keys, &live_paths).unwrap(), 1);
        assert_eq!(store.cached_parse("a").unwrap(), Some(b"1".to_vec()));
        assert_eq!(store.cached_parse("b").unwrap(), None);
        assert!(store.file_stamp("p1").unwrap().is_some());
        assert!(store.file_stamp("p2").unwrap().is_none());
    }

    #[test]
    fn language_u8_roundtrip() {
        for lang in [
            Language::Python,
            Language::Go,
            Language::Java,
            Language::Rust,
            Language::TypeScript,
            Language::Tsx,
            Language::JavaScript,
            Language::C,
            Language::Cpp,
            Language::ObjC,
            Language::ObjCpp,
            Language::Swift,
            Language::Php,
            Language::Vue,
            Language::Dart,
            Language::CSharp,
            Language::VisualBasic,
            Language::Kotlin,
            Language::Shell,
            Language::Ruby,
            Language::Lua,
        ] {
            assert_eq!(language_from_u8(language_to_u8(lang)), Some(lang));
        }
    }

    #[test]
    fn lang_batch_8_tags_are_appended_not_renumbered() {
        assert_eq!(language_to_u8(Language::VisualBasic), 16);
        assert_eq!(language_to_u8(Language::Kotlin), 17);
        assert_eq!(language_to_u8(Language::Shell), 18);
        assert_eq!(language_to_u8(Language::Ruby), 19);
        assert_eq!(language_to_u8(Language::Lua), 20);
    }
}
