//! Optional clipboard history. A single worker owns the SQLite connection.

use std::{
    env,
    fs::File,
    path::{Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    thread,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use lyteboard_core::{
    bulk::{Payload, thumbnail},
    peer::storage::{check_private_file, lock_named, open_private_dir},
};
use lyteboard_protocol::{
    ipc::network::{
        HISTORY_PER_PAGE, HistoryConfiguration, HistoryDirection, HistoryEntry, HistoryLimits,
        MAX_HISTORY_QUERY_BYTES, Reply,
    },
    peer::{MAX_BULK_TEXT_BYTES, MAX_IMAGE_BYTES, Media, NoiseKey},
};
use rusqlite::{Connection, Error, OpenFlags, OptionalExtension, Transaction, params};
use rustix::{
    fs::{Mode, OFlags, openat},
    io::Errno,
};
use tokio::{
    runtime,
    sync::{mpsc, oneshot},
    time::{interval, timeout},
};
use zeroize::Zeroizing;

use crate::preferences::{HistorySettings, Preferences};

const DB: &str = "history.sqlite3";
const APPLICATION_ID: i64 = 0x4c594843;
const MIB: i64 = 1024 * 1024;
const DISABLED: &str =
    "History is disabled. Enable it in Settings or run lyteboard history enable.";
const UNAVAILABLE: &str = "History storage is unavailable. Check the private profile, permissions, free space, and database integrity, then restart the daemon.";

type Bytes = Zeroizing<Vec<u8>>;
type Result<T> = std::result::Result<T, String>;

type Job = (Operation, oneshot::Sender<Result<Output>>);

#[derive(Clone)]
pub(super) struct History {
    worker: mpsc::Sender<Job>,
    recording: Arc<AtomicBool>,
}

pub(super) enum Operation {
    Status,
    Configuration,
    Configure(HistorySettings),
    List(Option<i64>, Option<String>),
    Load(i64),
    Preview(i64),
    Delete(i64),
    Clear,
    Record(HistoryDirection, NoiseKey, i64, Arc<Payload>),
}

pub(super) enum Output {
    Control(Reply),
    Payload(Media, Bytes),
}

impl History {
    pub(super) fn start(
        profile: Option<PathBuf>,
        preferences: Preferences,
    ) -> std::io::Result<Self> {
        let (worker, rx) = mpsc::channel(4);
        let settings = preferences
            .values()
            .and_then(|values| values.history.map_or_else(environment, Ok));
        let recording = Arc::new(AtomicBool::new(
            settings.as_ref().is_ok_and(|settings| settings.enabled),
        ));
        let active = Arc::clone(&recording);
        let runtime = runtime::Builder::new_current_thread()
            .enable_time()
            .build()?;
        thread::Builder::new()
            .name("lyteboard-history".into())
            .spawn(move || {
                runtime.block_on(run(profile, preferences, settings, active, rx));
            })?;
        Ok(Self { worker, recording })
    }

    async fn dispatch(&self, operation: Operation) -> Result<Output> {
        let (tx, rx) = oneshot::channel();
        self.worker
            .send((operation, tx))
            .await
            .map_err(|_| UNAVAILABLE)?;
        rx.await.map_err(|_| UNAVAILABLE)?
    }

    pub(super) async fn control(&self, operation: Operation) -> Reply {
        match timeout(
            lyteboard_core::ipc::linux::REQUEST_TIMEOUT - Duration::from_millis(250),
            self.dispatch(operation),
        )
        .await
        {
            Ok(Ok(Output::Control(reply))) => reply,
            Ok(Err(error)) => Reply::Error(error),
            Err(_) => Reply::Error("History storage is busy. Retry shortly.".into()),
            Ok(Ok(Output::Payload(..))) => unreachable!(),
        }
    }

    pub(super) async fn load(&self, id: i64) -> Result<Payload> {
        let Output::Payload(media, bytes) = self.dispatch(Operation::Load(id)).await? else {
            unreachable!()
        };
        Payload::validate(media, bytes, None)
            .await
            .map_err(|_| "History entry contains invalid clipboard data.".into())
    }

    pub(super) async fn preview(&self, id: i64) -> Result<(Media, Bytes)> {
        let Output::Payload(media, bytes) = self.dispatch(Operation::Preview(id)).await? else {
            unreachable!()
        };
        if media == Media::Text {
            Ok((media, bytes))
        } else {
            thumbnail(media, bytes)
                .await
                .map(|png| (Media::Png, png))
                .map_err(|_| "History image cannot be previewed.".into())
        }
    }

    pub(super) async fn record(
        &self,
        direction: HistoryDirection,
        peer: NoiseKey,
        payload: &Arc<Payload>,
    ) {
        if self.recording.load(Ordering::Relaxed) {
            // Bound how long a peer session waits on a slow filesystem.
            let _ = timeout(
                Duration::from_secs(3),
                self.dispatch(Operation::Record(
                    direction,
                    peer,
                    now(),
                    Arc::clone(payload),
                )),
            )
            .await;
        }
    }
}

fn environment() -> Result<HistorySettings> {
    let enabled = match env::var("LYTEBOARD_HISTORY") {
        Ok(value) if value == "on" => true,
        Ok(value) if value == "off" => false,
        Err(env::VarError::NotPresent) => false,
        _ => return Err("Set LYTEBOARD_HISTORY to on or off.".into()),
    };
    fn limit(name: &str, default: u32, max: u32) -> Result<u32> {
        match env::var(name) {
            Err(env::VarError::NotPresent) => Ok(default),
            value => value
                .ok()
                .and_then(|value| value.parse().ok())
                .filter(|value| (1..=max).contains(value))
                .ok_or_else(|| format!("Set {name} to an integer between 1 and {max}.")),
        }
    }
    Ok(HistorySettings {
        enabled,
        limits: if !enabled {
            HistoryLimits::default()
        } else {
            HistoryLimits {
                entries: limit("LYTEBOARD_HISTORY_ENTRIES", 100, 10_000)?,
                mib: limit("LYTEBOARD_HISTORY_MIB", 100, 1024)?,
                days: limit("LYTEBOARD_HISTORY_DAYS", 7, 3650)?,
            }
        },
    })
}

fn now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs() as i64
}

struct ActiveHistory {
    settings: Result<HistorySettings>,
    store: Option<Store>,
    error: Option<String>,
    profile: Option<PathBuf>,
}

impl ActiveHistory {
    fn apply(&mut self, settings: HistorySettings) {
        self.settings = Ok(settings);
        self.error = None;
        if !settings.enabled {
            self.store = None;
        } else {
            let result = match self.store.as_mut() {
                Some(store) => store
                    .set_limits(settings.limits)
                    .map_err(|_| UNAVAILABLE.into()),
                None => self
                    .profile
                    .as_deref()
                    .ok_or_else(|| UNAVAILABLE.into())
                    .and_then(|path| Store::open(path, settings.limits))
                    .map(|store| self.store = Some(store)),
            };
            if let Err(error) = result {
                self.store = None;
                self.error = Some(error);
            }
        }
    }

    fn request(&mut self, operation: Operation, preferences: &Preferences) -> Result<Output> {
        match operation {
            Operation::Configure(settings) => {
                if !settings.limits.is_valid() {
                    return Err("Invalid history retention limits.".into());
                }
                if let Err(error) = preferences.update(|values| values.history = Some(settings)) {
                    if !settings.enabled {
                        self.apply(settings);
                    }
                    return Err(error);
                }
                self.apply(settings);
                self.configuration()
            }
            Operation::Configuration => self.configuration(),
            Operation::Status => {
                let settings = self.settings.as_ref().map_err(Clone::clone)?;
                if let Some(error) = &self.error {
                    return Err(error.clone());
                }
                Ok(Output::Control(Reply::HistoryStatus(
                    settings.enabled.then_some(settings.limits),
                )))
            }
            operation => {
                let settings = self.settings.as_ref().map_err(Clone::clone)?;
                if !settings.enabled {
                    return Err(DISABLED.into());
                }
                let store = self.store.as_mut().ok_or(UNAVAILABLE)?;
                let mut failed = false;
                let recover = matches!(
                    operation,
                    Operation::Record(..) | Operation::Delete(..) | Operation::Clear
                );
                let result = store.request(operation, &mut failed);
                if failed {
                    self.error = Some(UNAVAILABLE.into());
                } else if recover && result.is_ok() {
                    self.error = None;
                }
                result
            }
        }
    }

    fn configuration(&self) -> Result<Output> {
        let settings = self.settings.as_ref().map_err(Clone::clone)?;
        Ok(Output::Control(Reply::HistoryConfiguration(
            HistoryConfiguration {
                enabled: settings.enabled,
                limits: settings.limits,
                error: self.error.clone(),
            },
        )))
    }
}

async fn run(
    profile: Option<PathBuf>,
    preferences: Preferences,
    settings: Result<HistorySettings>,
    recording: Arc<AtomicBool>,
    mut rx: mpsc::Receiver<Job>,
) {
    let mut active = ActiveHistory {
        settings,
        store: None,
        error: None,
        profile,
    };
    if let Ok(settings) = active.settings {
        active.apply(settings);
    }
    recording.store(active.store.is_some(), Ordering::Relaxed);
    let mut cleanup = interval(Duration::from_secs(60));
    loop {
        tokio::select! {
            request = rx.recv() => {
                let Some((operation, reply)) = request else { break };
                if reply.is_closed() && !matches!(operation, Operation::Record(..)) {
                    continue;
                }
                let result = active.request(operation, &preferences);
                recording.store(active.store.is_some(), Ordering::Relaxed);
                let _ = reply.send(result);
            }
            _ = cleanup.tick(), if active.store.is_some() => {
                if let Some(store) = &mut active.store && store.expire(now()).is_err() {
                    active.error = Some(UNAVAILABLE.into());
                }
            }
        }
    }
}

struct Store {
    db: Connection,
    limits: HistoryLimits,
    _lock: File,
}

impl Store {
    fn open(profile: &Path, limits: HistoryLimits) -> Result<Self> {
        let path = profile.join("history");
        let dir = open_private_dir(profile)
            .and_then(|_| open_private_dir(&path))
            .map_err(|_| UNAVAILABLE)?;
        let lock = lock_named(&dir, "history.lock").map_err(|_| UNAVAILABLE)?;
        let flags = OFlags::RDWR | OFlags::NOFOLLOW | OFlags::CLOEXEC | OFlags::NONBLOCK;
        let (fd, new) = match openat(&dir, DB, flags, Mode::empty()) {
            Ok(fd) => (fd, false),
            Err(Errno::NOENT) => (
                openat(
                    &dir,
                    DB,
                    flags | OFlags::CREATE | OFlags::EXCL,
                    Mode::RUSR | Mode::WUSR,
                )
                .map_err(|_| UNAVAILABLE)?,
                true,
            ),
            Err(_) => return Err(UNAVAILABLE.into()),
        };
        let file = File::from(fd);
        check_private_file(&file).map_err(|_| UNAVAILABLE)?;
        for name in [
            "history.sqlite3-journal",
            "history.sqlite3-wal",
            "history.sqlite3-shm",
        ] {
            match openat(
                &dir,
                name,
                OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::NONBLOCK,
                Mode::empty(),
            ) {
                Ok(fd) => check_private_file(&File::from(fd)).map_err(|_| UNAVAILABLE)?,
                Err(Errno::NOENT) => (),
                Err(_) => return Err(UNAVAILABLE.into()),
            }
        }

        let db = Self::connect(&path.join(DB), new).map_err(|_| UNAVAILABLE)?;
        let mut store = Self {
            db,
            limits,
            _lock: lock,
        };
        let tx = store.db.transaction().map_err(|_| UNAVAILABLE)?;
        Self::prune(&tx, limits, now()).map_err(|_| UNAVAILABLE)?;
        tx.commit().map_err(|_| UNAVAILABLE)?;
        dir.sync_all().map_err(|_| UNAVAILABLE)?;
        Ok(store)
    }

    fn connect(path: &Path, new: bool) -> rusqlite::Result<Connection> {
        let flags = OpenFlags::SQLITE_OPEN_READ_WRITE
            | OpenFlags::SQLITE_OPEN_NO_MUTEX
            | OpenFlags::SQLITE_OPEN_NOFOLLOW;
        let mut db = Connection::open_with_flags(path, flags)?;
        db.busy_timeout(Duration::from_millis(500))?;
        db.execute_batch("PRAGMA trusted_schema=OFF; PRAGMA temp_store=MEMORY;")?;
        if new {
            db.pragma_update(None, "auto_vacuum", "FULL")?;
            let tx = db.transaction()?;
            tx.execute_batch(
                "CREATE TABLE entries(
                    id INTEGER PRIMARY KEY AUTOINCREMENT,
                    timestamp INTEGER NOT NULL CHECK(timestamp >= 0),
                    media INTEGER NOT NULL CHECK(media BETWEEN 0 AND 3),
                    sent INTEGER NOT NULL CHECK(sent IN (0, 1)),
                    peer BLOB NOT NULL CHECK(length(peer) = 32),
                    size INTEGER NOT NULL CHECK(size BETWEEN 1 AND CASE media WHEN 0 THEN 1048576 ELSE 10485760 END),
                    payload BLOB NOT NULL CHECK(length(payload) = size)
                ) STRICT;
                CREATE INDEX entries_timestamp ON entries(timestamp);",
            )?;
            tx.pragma_update(None, "application_id", APPLICATION_ID)?;
            tx.pragma_update(None, "user_version", 1)?;
            tx.commit()?;
        }
        let app: i64 = db.pragma_query_value(None, "application_id", |row| row.get(0))?;
        let version: i64 = db.pragma_query_value(None, "user_version", |row| row.get(0))?;
        let vacuum: i64 = db.pragma_query_value(None, "auto_vacuum", |row| row.get(0))?;
        if app != APPLICATION_ID || version != 1 || vacuum != 1 {
            return Err(Error::InvalidQuery);
        }
        let check: String = db.query_row("PRAGMA quick_check(1)", [], |row| row.get(0))?;
        let journal: String = db.query_row("PRAGMA journal_mode=DELETE", [], |row| row.get(0))?;
        if check != "ok" || journal != "delete" || db.is_readonly("main")? {
            return Err(Error::InvalidQuery);
        }
        db.execute_batch(
            "PRAGMA synchronous=FULL; PRAGMA secure_delete=ON; PRAGMA cache_size=-2048;",
        )?;
        Ok(db)
    }

    fn expire(&mut self, time: i64) -> rusqlite::Result<()> {
        let cutoff = time - i64::from(self.limits.days) * 86_400;
        self.db
            .execute("DELETE FROM entries WHERE timestamp <= ?1", [cutoff])?;
        Ok(())
    }

    fn prune(tx: &Transaction<'_>, limits: HistoryLimits, time: i64) -> rusqlite::Result<()> {
        let budget = i64::from(limits.mib) * MIB;
        let cutoff = time - i64::from(limits.days) * 86_400;
        tx.execute("DELETE FROM entries WHERE timestamp <= ?1", [cutoff])?;
        tx.execute(
            "WITH retained AS MATERIALIZED (
                SELECT id, sum(size) OVER (ORDER BY id DESC) AS bytes,
                       row_number() OVER (ORDER BY id DESC) AS position FROM entries
            )
            DELETE FROM entries WHERE id IN (
                SELECT id FROM retained WHERE position > ?1 OR bytes > ?2
            )",
            params![limits.entries, budget],
        )?;

        // FULL auto-vacuum drops free pages at commit. Count live pages to budget for metadata too.
        let page_bytes: i64 = tx.pragma_query_value(None, "page_size", |row| row.get(0))?;
        loop {
            let pages: i64 = tx.pragma_query_value(None, "page_count", |row| row.get(0))?;
            let free: i64 = tx.pragma_query_value(None, "freelist_count", |row| row.get(0))?;
            if pages.saturating_sub(free) <= budget / page_bytes {
                return Ok(());
            }
            if tx.execute(
                "DELETE FROM entries WHERE id=(SELECT min(id) FROM entries)",
                [],
            )? == 0
            {
                return Err(Error::InvalidQuery);
            }
        }
    }

    fn record(
        &mut self,
        direction: HistoryDirection,
        peer: NoiseKey,
        time: i64,
        payload: &Payload,
    ) -> rusqlite::Result<()> {
        let tx = self.db.transaction()?;
        // A limit smaller than one item leaves the existing history intact.
        if payload.bytes().len() as i64 >= i64::from(self.limits.mib) * MIB {
            return Ok(());
        }
        tx.execute(
            "INSERT INTO entries(timestamp, media, sent, peer, size, payload) VALUES(?1, ?2, ?3, ?4, ?5, ?6)",
            params![time, payload.media() as u8, matches!(direction, HistoryDirection::Sent),
                peer.as_slice(), payload.bytes().len() as u32, payload.bytes()],
        )?;
        let id = tx.last_insert_rowid();
        Self::prune(&tx, self.limits, now())?;
        let retained: bool = tx.query_row(
            "SELECT EXISTS(SELECT 1 FROM entries WHERE id=?1)",
            [id],
            |row| row.get(0),
        )?;
        if retained { tx.commit() } else { tx.rollback() }
    }

    fn set_limits(&mut self, limits: HistoryLimits) -> rusqlite::Result<()> {
        let tx = self.db.transaction()?;
        Self::prune(&tx, limits, now())?;
        tx.commit()?;
        self.limits = limits;
        Ok(())
    }

    fn list(
        &self,
        before: Option<i64>,
        query: Option<&str>,
    ) -> rusqlite::Result<Vec<HistoryEntry>> {
        let mut statement = self.db.prepare(
            "SELECT id, timestamp, media, sent, peer, size FROM entries
             WHERE id <= ?1 AND CASE
               WHEN ?3 IS NULL THEN 1
               WHEN media = 0 THEN instr(lower(CAST(payload AS TEXT)), lower(?3)) > 0
               ELSE 0 END
             ORDER BY id DESC LIMIT ?2",
        )?;
        statement
            .query_map(
                params![
                    before.map_or(i64::MAX, |id| id.saturating_sub(1)),
                    (HISTORY_PER_PAGE + 1) as u32,
                    query
                ],
                |row| {
                    let peer: NoiseKey = row.get(4)?;
                    Ok(HistoryEntry {
                        id: row.get(0)?,
                        timestamp: row.get(1)?,
                        media: media(row.get(2)?)?,
                        direction: if row.get(3)? {
                            HistoryDirection::Sent
                        } else {
                            HistoryDirection::Received
                        },
                        peer: hex::encode(peer),
                        size: row.get(5)?,
                    })
                },
            )?
            .collect()
    }

    fn load(&self, id: i64, preview: bool) -> rusqlite::Result<Option<(Media, Bytes)>> {
        self.db
            .query_row(
                "SELECT media, CASE WHEN ?4 AND media=0 THEN CAST(substr(CAST(payload AS TEXT), 1, 160) AS BLOB)
                   ELSE payload END FROM entries WHERE id=?1 AND size=length(payload)
             AND size BETWEEN 1 AND CASE media WHEN 0 THEN ?2 ELSE ?3 END",
                params![id, MAX_BULK_TEXT_BYTES as u32, MAX_IMAGE_BYTES as u32, preview],
                |row| {
                    let kind = media(row.get(0)?)?;
                    let bytes = row.get_ref(1)?.as_blob()?;
                    Ok((kind, Zeroizing::new(bytes.to_vec())))
                },
            )
            .optional()
    }

    fn request(&mut self, operation: Operation, failed: &mut bool) -> Result<Output> {
        self.execute(operation, failed).map_err(|_| {
            *failed = true;
            UNAVAILABLE.to_owned()
        })?
    }

    fn execute(
        &mut self,
        operation: Operation,
        failed: &mut bool,
    ) -> rusqlite::Result<Result<Output>> {
        use Operation::*;
        let reply = match operation {
            Status | Configuration | Configure(_) => unreachable!(),
            Record(direction, peer, time, payload) => {
                self.record(direction, peer, time, &payload)?;
                *failed = false;
                Reply::Ok
            }
            List(before, query) => {
                if query.as_ref().is_some_and(|query| {
                    query.len() > MAX_HISTORY_QUERY_BYTES || query.contains('\0')
                }) {
                    return Ok(Err("Search must be at most 256 UTF-8 bytes.".into()));
                }
                self.expire(now())?;
                let mut entries = self.list(before, query.as_deref())?;
                let more = entries.len() > HISTORY_PER_PAGE;
                entries.truncate(HISTORY_PER_PAGE);
                Reply::HistoryPage { entries, more }
            }
            Load(id) | Preview(id) => {
                self.expire(now())?;
                return Ok(self
                    .load(id, matches!(operation, Preview(_)))?
                    .map(|(media, bytes)| Output::Payload(media, bytes))
                    .ok_or_else(|| "History entry was not found or has expired.".into()));
            }
            Delete(id) => {
                self.expire(now())?;
                if self.db.execute("DELETE FROM entries WHERE id=?1", [id])? == 0 {
                    return Ok(Err("History entry was not found or has expired.".into()));
                }
                *failed = false;
                Reply::Ok
            }
            Clear => {
                self.db.execute("DELETE FROM entries", [])?;
                *failed = false;
                Reply::Ok
            }
        };
        Ok(Ok(Output::Control(reply)))
    }
}

fn media(tag: u8) -> rusqlite::Result<Media> {
    match tag {
        0 => Ok(Media::Text),
        1 => Ok(Media::Png),
        2 => Ok(Media::Jpeg),
        3 => Ok(Media::Webp),
        _ => Err(Error::InvalidQuery),
    }
}

#[cfg(test)]
mod tests {
    use std::{fs, os::unix::fs::PermissionsExt};

    use super::*;

    fn limits(entries: u32, mib: u32) -> HistoryLimits {
        HistoryLimits {
            entries,
            mib,
            days: 7,
        }
    }

    fn insert(store: &mut Store, payload: &Payload) {
        store
            .record(HistoryDirection::Sent, [255; 32], now(), payload)
            .unwrap();
    }

    #[test]
    fn persists_private_content_and_enforces_count_age_and_deletion() {
        let root = tempfile::tempdir().unwrap();
        let profile = root.path().join("profile");
        let path = profile.join("history").join(DB);
        let text = Payload::text("Clipboard ✓".into()).unwrap();
        {
            let mut store = Store::open(&profile, limits(2, 100)).unwrap();
            for _ in 0..3 {
                insert(&mut store, &text);
            }
            assert!(Store::open(&profile, limits(2, 100)).is_err());
            assert_eq!(
                fs::metadata(path.parent().unwrap())
                    .unwrap()
                    .permissions()
                    .mode()
                    & 0o777,
                0o700
            );
            assert_eq!(
                fs::metadata(&path).unwrap().permissions().mode() & 0o777,
                0o600
            );
            assert_eq!(
                store
                    .list(None, None)
                    .unwrap()
                    .iter()
                    .map(|entry| entry.id)
                    .collect::<Vec<_>>(),
                [3, 2]
            );
            assert_eq!(store.list(Some(3), None).unwrap()[0].id, 2);
            // Backdate one saved item. Restart must prune it before exposing a list.
            store
                .db
                .execute(
                    "UPDATE entries SET timestamp=?1 WHERE id=2",
                    [now() - 7 * 86_400],
                )
                .unwrap();
        }
        let mut store = Store::open(&profile, limits(2, 100)).unwrap();
        let entries = store.list(None, None).unwrap();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].id, 3);
        assert!(matches!(entries[0].direction, HistoryDirection::Sent));
        assert_eq!(entries[0].peer, "ff".repeat(32));
        let (media, bytes) = store.load(3, false).unwrap().unwrap();
        assert_eq!(media, Media::Text);
        assert_eq!(&*bytes, text.bytes());
        let mut failed = false;
        assert!(store.request(Operation::Delete(3), &mut failed).is_ok());
        assert!(store.load(3, false).unwrap().is_none());
        insert(&mut store, &text);
        assert!(store.request(Operation::Clear, &mut failed).is_ok());
        drop(store);
        assert!(
            Store::open(&profile, limits(2, 100))
                .unwrap()
                .list(None, None)
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn caps_database_size_including_pages_and_preserves_rows_if_item_cannot_fit() {
        let root = tempfile::tempdir().unwrap();
        let profile = root.path().join("profile");
        let path = profile.join("history").join(DB);
        let mut store = Store::open(&profile, limits(100, 1)).unwrap();
        let medium = Payload::text("a".repeat(514 * 1024)).unwrap();
        insert(&mut store, &medium);
        insert(&mut store, &medium);
        assert_eq!(store.list(None, None).unwrap()[0].id, 2);
        assert_eq!(store.list(None, None).unwrap().len(), 1);
        let tiny = Payload::text("b".into()).unwrap();
        insert(&mut store, &tiny);
        let near_limit = Payload::text("a".repeat(MIB as usize - 1)).unwrap();
        insert(&mut store, &near_limit);
        assert_eq!(store.list(None, None).unwrap().len(), 2);
        // These payloads fit the byte quota together, but the SQLite database exceeds it
        // once the table and index pages are included.
        let half = Payload::text("c".repeat(512 * 1024)).unwrap();
        insert(&mut store, &half);
        insert(&mut store, &half);
        assert!(fs::metadata(path).unwrap().len() <= MIB as u64);
        assert_eq!(store.list(None, None).unwrap().len(), 1);
    }

    #[test]
    fn refuses_corrupt_unsafe_and_inaccessible_storage() {
        let root = tempfile::tempdir().unwrap();
        let profile = root.path().join("profile");
        drop(Store::open(&profile, limits(100, 100)).unwrap());
        let path = profile.join("history").join(DB);
        fs::set_permissions(&path, fs::Permissions::from_mode(0o644)).unwrap();
        assert!(Store::open(&profile, limits(100, 100)).is_err());
        fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).unwrap();
        fs::write(&path, b"not a database").unwrap();
        assert!(Store::open(&profile, limits(100, 100)).is_err());
        assert_eq!(fs::read(&path).unwrap(), b"not a database");
        fs::write(&path, b"").unwrap();
        assert!(Store::open(&profile, limits(100, 100)).is_err());
        fs::remove_file(&path).unwrap();
        std::os::unix::fs::symlink(root.path(), &path).unwrap();
        assert!(Store::open(&profile, limits(100, 100)).is_err());
        assert!(Store::open(Path::new("relative"), limits(100, 100)).is_err());
    }
}
