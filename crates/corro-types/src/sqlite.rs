use std::{
    ops::{Deref, DerefMut},
    time::{Duration, Instant},
};

use once_cell::sync::Lazy;
use rusqlite::{params, trace::TraceEventCodes, Connection, Transaction};
use sqlite_pool::{Committable, SqliteConn};
use tempfile::TempDir;
use tracing::{error, info, trace, warn};

pub type SqlitePool = sqlite_pool::Pool<CrConn>;
pub type SqlitePoolError = sqlite_pool::PoolError;

const CRSQL_EXT_GENERIC_NAME: &str = "crsqlite";

#[cfg(target_os = "macos")]
pub const CRSQL_EXT_FILENAME: &str = "crsqlite.dylib";
#[cfg(target_os = "linux")]
pub const CRSQL_EXT_FILENAME: &str = "crsqlite.so";

#[cfg(all(target_arch = "aarch64", target_os = "macos"))]
pub const CRSQL_EXT: &[u8] = include_bytes!("../crsqlite-darwin-aarch64.dylib");
#[cfg(all(target_arch = "x86_64", target_os = "linux"))]
pub const CRSQL_EXT: &[u8] = include_bytes!("../crsqlite-linux-x86_64.so");
#[cfg(all(target_arch = "aarch64", target_os = "linux"))]
pub const CRSQL_EXT: &[u8] = include_bytes!("../crsqlite-linux-aarch64.so");

// TODO: support windows

// need to keep this alive!
static CRSQL_EXT_DIR: Lazy<TempDir> = Lazy::new(|| {
    let dir = TempDir::new().expect("could not create temp dir!");
    std::fs::write(dir.path().join(CRSQL_EXT_GENERIC_NAME), CRSQL_EXT)
        .expect("could not write crsql ext file");
    dir
});

pub fn rusqlite_to_crsqlite_write(conn: rusqlite::Connection) -> rusqlite::Result<CrConn> {
    let conn = rusqlite_to_crsqlite(conn)?;
    conn.execute_batch("PRAGMA cache_size = -32000;")?;

    Ok(conn)
}

pub fn rusqlite_to_crsqlite(mut conn: rusqlite::Connection) -> rusqlite::Result<CrConn> {
    init_cr_conn(&mut conn)?;
    setup_conn(&conn)?;
    sqlite_functions::add_to_connection(&conn)?;

    const SLOW_THRESHOLD: Duration = Duration::from_secs(1);
    conn.trace_v2(
        TraceEventCodes::SQLITE_TRACE_PROFILE,
        Some(|event| {
            if let rusqlite::trace::TraceEvent::Profile(stmt_ref, duration) = event {
                if duration >= SLOW_THRESHOLD {
                    warn!("SLOW query {duration:?} => {}", stmt_ref.sql());
                }
            }
        }),
    );

    Ok(CrConn(conn))
}

#[derive(Debug)]
pub struct CrConn(Connection);

impl CrConn {
    pub fn init(mut conn: Connection) -> Result<Self, rusqlite::Error> {
        init_cr_conn(&mut conn)?;
        Ok(Self(conn))
    }

    pub fn immediate_transaction(&mut self) -> rusqlite::Result<Transaction> {
        self.0
            .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)
    }
}

impl SqliteConn for CrConn {
    fn conn(&self) -> &rusqlite::Connection {
        &self.0
    }
}

impl Deref for CrConn {
    type Target = Connection;

    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

impl DerefMut for CrConn {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.0
    }
}

impl Drop for CrConn {
    fn drop(&mut self) {
        if let Err(e) = self.execute_batch("select crsql_finalize();") {
            error!("could not crsql_finalize: {e}");
        }
    }
}

impl Committable for CrConn {
    fn commit(self) -> Result<(), rusqlite::Error> {
        Ok(())
    }

    fn savepoint(&mut self) -> Result<rusqlite::Savepoint<'_>, rusqlite::Error> {
        Err(rusqlite::Error::ModuleError(String::from(
            "cannot create savepoint from connection",
        )))
    }
}

fn init_cr_conn(conn: &mut Connection) -> Result<(), rusqlite::Error> {
    let ext_dir = &CRSQL_EXT_DIR;
    trace!(
        "loading crsqlite extension from path: {}",
        ext_dir.path().display()
    );
    unsafe {
        trace!("enabled loading extension");
        conn.load_extension_enable()?;
        conn.load_extension(
            ext_dir.path().join(CRSQL_EXT_GENERIC_NAME),
            Some("sqlite3_crsqlite_init"),
        )?;
        conn.load_extension_disable()?;
    }
    trace!("loaded crsqlite extension");

    Ok(())
}

pub fn setup_conn(conn: &Connection) -> Result<(), rusqlite::Error> {
    // WAL journal mode and synchronous NORMAL for best performance / crash resilience compromise
    conn.execute_batch(
        r#"
            PRAGMA journal_mode = WAL;
            PRAGMA journal_size_limit = 1073741824;
            PRAGMA synchronous = NORMAL;
            PRAGMA recursive_triggers = ON;
            PRAGMA mmap_size = 8589934592; -- 8GB
        "#,
    )?;

    rusqlite::vtab::series::load_module(conn)?;

    Ok(())
}

pub trait Migration {
    fn migrate(&self, tx: &Transaction) -> rusqlite::Result<()>;
}

impl<F> Migration for F
where
    F: Fn(&Transaction) -> rusqlite::Result<()>,
{
    fn migrate(&self, tx: &Transaction) -> rusqlite::Result<()> {
        self(tx)
    }
}

const SCHEMA_VERSION_KEY: &str = "schema_version";

// Read migration version field from the SQLite db
pub fn migration_version(tx: &Transaction) -> Option<usize> {
    #[allow(deprecated)] // To keep compatibility with lower rusqlite versions
    tx.query_row(
        "SELECT value FROM __corro_state WHERE key = ?",
        [SCHEMA_VERSION_KEY],
        |row| row.get::<_, i64>(0),
    )
    .map(|v| v as usize)
    .ok()
}

// Set user version field from the SQLite db
pub fn set_migration_version(tx: &Transaction, v: usize) -> rusqlite::Result<usize> {
    tx.execute(
        "INSERT OR REPLACE INTO __corro_state VALUES (?, ?)",
        params![SCHEMA_VERSION_KEY, v],
    )
}

// should be a noop if up to date!
pub fn migrate(conn: &mut Connection, migrations: Vec<Box<dyn Migration>>) -> rusqlite::Result<()> {
    let target_version = migrations.len();

    let tx = conn.transaction()?;

    // determine how many migrations to skip (skip as many as we are at)
    let skip_n = migration_version(&tx).unwrap_or_default();

    if skip_n > migrations.len() {
        warn!("Skipping migrations, database is at migration version {skip_n} which is greater than {}", migrations.len());
        return Ok(());
    }

    for (i, migration) in migrations.into_iter().skip(skip_n).enumerate() {
        let new_version = skip_n + i;
        info!("Applying migration to v{new_version}");
        let start = Instant::now();
        migration.migrate(&tx)?;
        info!("Applied v{new_version} in {:?}", start.elapsed());
    }
    set_migration_version(&tx, target_version)?;

    tx.commit()?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use futures::{stream::FuturesUnordered, TryStreamExt};
    use sqlite_pool::Config;
    use sqlite_pool::InterruptibleTransaction;
    use tokio::task::block_in_place;

    use super::*;

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn concurrent_writes() -> Result<(), Box<dyn std::error::Error>> {
        let tmpdir = tempfile::TempDir::new()?;

        let pool = sqlite_pool::Config::new(tmpdir.path().join("test.db"))
            .max_size(1)
            .create_pool_transform(rusqlite_to_crsqlite)?;

        {
            let conn = pool.get().await?;

            conn.execute_batch(
                "
                CREATE TABLE foo (a INTEGER NOT NULL PRIMARY KEY, b INTEGER);
                SELECT crsql_as_crr('foo');
            ",
            )?;
        }

        let total: i64 = 1000;
        let per_worker: i64 = 5;

        let futs = FuturesUnordered::from_iter((0..total).map(|_| {
            let pool = pool.clone();
            async move {
                tokio::spawn(async move {
                    let _: () = FuturesUnordered::from_iter((0..per_worker).map(|_| {
                        let pool = pool.clone();
                        async move {
                            let conn = pool.get().await?;
                            block_in_place(|| {
                                conn.prepare_cached(
                                    "INSERT INTO foo (a, b) VALUES (random(), random())",
                                )?
                                .execute(())?;
                                Ok::<_, TestError>(())
                            })?;
                            Ok::<_, TestError>(())
                        }
                    }))
                    .try_collect()
                    .await?;
                    Ok::<_, TestError>(())
                })
                .await??;
                Ok::<_, TestError>(())
            }
        }));

        let _: () = futs.try_collect().await?;

        let conn = pool.get().await?;

        let count: i64 = conn.query_row("SELECT COUNT(*) FROM foo;", (), |row| row.get(0))?;

        assert_eq!(count, total * per_worker);

        Ok(())
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn test_interruptible_transaction() -> Result<(), Box<dyn std::error::Error>> {
        let tmpdir = tempfile::tempdir()?;

        let path = tmpdir.path().join("db.sqlite");
        let pool = Config::new(path)
            .max_size(1)
            .create_pool_transform(rusqlite_to_crsqlite)?;

        let mut conn = pool.get().await.unwrap();
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS testsbool (
            id INTEGER NOT NULL PRIMARY KEY,
            b boolean not null default false
        ); SELECT crsql_as_crr('testsbool')",
        )?;

        {
            let tx = conn.transaction()?;
            let timeout = Some(tokio::time::Duration::from_millis(5));
            let itx = InterruptibleTransaction::new(tx, timeout, "test_interruptible_transaction");
            let res = itx.execute("INSERT INTO testsbool (id) WITH RECURSIVE    cte(id) AS (       SELECT random()       UNION ALL       SELECT random()         FROM cte        LIMIT 100000000  ) SELECT id FROM cte;", &[]);

            assert!(res.is_err_and(
                |e| e.sqlite_error_code() == Some(rusqlite::ErrorCode::OperationInterrupted)
            ));
        }

        let count: i64 = conn.query_row("SELECT COUNT(*) FROM testsbool;", (), |row| row.get(0))?;
        assert_eq!(count, 0);
        Ok(())
    }

    #[derive(Debug, thiserror::Error)]
    enum TestError {
        #[error(transparent)]
        Rusqlite(#[from] rusqlite::Error),
        #[error(transparent)]
        Bb8Rusqlite(#[from] SqlitePoolError),
        #[error(transparent)]
        Join(#[from] tokio::task::JoinError),
    }
}
