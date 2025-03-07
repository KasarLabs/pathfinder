//! Local storage.
//!
//! Currently this consists of a Sqlite backend implementation.

// This is intended for internal use only -- do not make public.
mod prelude;

mod connection;
pub mod fake;
mod params;
mod schema;
pub mod test_utils;

use std::num::NonZeroU32;
use std::path::{Path, PathBuf};
use std::sync::Arc;

pub use connection::*;

use pathfinder_common::{BlockHash, BlockNumber};
use rusqlite::functions::FunctionFlags;

use anyhow::Context;
use r2d2::Pool;
use r2d2_sqlite::SqliteConnectionManager;

/// Sqlite key used for the PRAGMA user version.
const VERSION_KEY: &str = "user_version";

/// Specifies the [journal mode](https://sqlite.org/pragma.html#pragma_journal_mode)
/// of the [Storage].
#[derive(Clone, Copy)]
pub enum JournalMode {
    Rollback,
    WAL,
}

/// Identifies a specific starknet block stored in the database.
///
/// Note that this excludes the `Pending` variant since we never store pending data
/// in the database.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BlockId {
    Latest,
    Number(BlockNumber),
    Hash(BlockHash),
}

impl From<BlockHash> for BlockId {
    fn from(value: BlockHash) -> Self {
        Self::Hash(value)
    }
}

impl From<BlockNumber> for BlockId {
    fn from(value: BlockNumber) -> Self {
        Self::Number(value)
    }
}

impl TryFrom<pathfinder_common::BlockId> for BlockId {
    type Error = &'static str;

    fn try_from(value: pathfinder_common::BlockId) -> Result<Self, Self::Error> {
        match value {
            pathfinder_common::BlockId::Number(x) => Ok(BlockId::Number(x)),
            pathfinder_common::BlockId::Hash(x) => Ok(BlockId::Hash(x)),
            pathfinder_common::BlockId::Latest => Ok(BlockId::Latest),
            pathfinder_common::BlockId::Pending => {
                Err("Pending is invalid within the storage context")
            }
        }
    }
}

/// Used to create [Connection's](Connection) to the pathfinder database.
///
/// Intended usage:
/// - Use [Storage::migrate] to create the app's database.
/// - Pass the [Storage] (or clones thereof) to components which require database access.
/// - Use [Storage::connection] to create connection's to the database, which can in turn
///   be used to interact with the various [tables](self).
#[derive(Clone)]
pub struct Storage(Inner);

#[derive(Clone)]
struct Inner {
    /// Uses [`Arc`] to allow _shallow_ [Storage] cloning
    database_path: Arc<PathBuf>,
    pool: Pool<SqliteConnectionManager>,
}

pub struct StorageManager(PathBuf);

impl StorageManager {
    pub fn create_pool(&self, capacity: NonZeroU32) -> anyhow::Result<Storage> {
        let pool_manager = SqliteConnectionManager::file(&self.0).with_init(setup_connection);
        let pool = Pool::builder()
            .max_size(capacity.get())
            .build(pool_manager)?;

        Ok(Storage(Inner {
            database_path: Arc::new(self.0.clone()),
            pool,
        }))
    }
}

impl Storage {
    /// Performs the database schema migration and returns a [storage manager](StorageManager).
    ///
    /// This should be called __once__ at the start of the application,
    /// and passed to the various components which require access to the database.
    ///
    /// Panics if u32
    pub fn migrate(
        database_path: PathBuf,
        journal_mode: JournalMode,
    ) -> anyhow::Result<StorageManager> {
        let mut connection = rusqlite::Connection::open(&database_path)
            .context("Opening DB for setting journal mode")?;
        setup_connection(&mut connection).context("Setting up database connection")?;
        setup_journal_mode(&mut connection, journal_mode).context("Setting journal mode")?;
        migrate_database(&mut connection).context("Migrate database")?;
        connection
            .close()
            .map_err(|(_connection, error)| error)
            .context("Closing DB after setting journal mode")?;

        Ok(StorageManager(database_path))
    }

    /// Returns a new Sqlite [Connection] to the database.
    pub fn connection(&self) -> anyhow::Result<Connection> {
        let conn = self.0.pool.get()?;
        Ok(Connection::from_inner(conn))
    }

    /// Convenience function for tests to create an in-memory database.
    /// Equivalent to [Storage::migrate] with an in-memory backed database.
    // No longer cfg(test) because needed in benchmarks
    pub fn in_memory() -> anyhow::Result<Self> {
        // Create a unique database name so that they are not shared between
        // concurrent tests. i.e. Make every in-mem Storage unique.
        lazy_static::lazy_static!(
            static ref COUNT: std::sync::Mutex<u64> = Default::default();
        );
        let unique_mem_db = {
            let mut count = COUNT.lock().unwrap();
            // &cache=shared allows other threads to see and access the inmemory database
            let unique_mem_db = format!("file:memdb{count}?mode=memory&cache=shared");
            *count += 1;
            unique_mem_db
        };

        let database_path = PathBuf::from(unique_mem_db);
        // This connection must be held until a pool has been created, since an
        // in-memory database is dropped once all its connections are. This connection
        // therefore holds the database in-place until the pool is established.
        let _conn = rusqlite::Connection::open(&database_path)?;

        let storage = Self::migrate(database_path, JournalMode::Rollback)?;

        storage.create_pool(NonZeroU32::new(5).unwrap())
    }

    pub fn path(&self) -> &Path {
        &self.0.database_path
    }
}

fn setup_journal_mode(
    connection: &mut rusqlite::Connection,
    journal_mode: JournalMode,
) -> Result<(), rusqlite::Error> {
    // set journal mode related pragmas
    match journal_mode {
        JournalMode::Rollback => connection.pragma_update(None, "journal_mode", "DELETE"),
        JournalMode::WAL => {
            connection.pragma_update(None, "journal_mode", "WAL")?;
            // set journal size limit to 1 GB
            connection.pragma_update(
                None,
                "journal_size_limit",
                (1024usize * 1024 * 1024).to_string(),
            )?;
            // According to the documentation NORMAL is a good choice for WAL mode.
            connection.pragma_update(None, "synchronous", "normal")
        }
    }
}

fn setup_connection(connection: &mut rusqlite::Connection) -> Result<(), rusqlite::Error> {
    // enable foreign keys
    connection.set_db_config(
        rusqlite::config::DbConfig::SQLITE_DBCONFIG_ENABLE_FKEY,
        true,
    )?;

    connection.create_scalar_function(
        "base64_felts_to_index_prefixed_base32_felts",
        1,
        FunctionFlags::SQLITE_UTF8 | FunctionFlags::SQLITE_DETERMINISTIC,
        move |ctx| {
            assert_eq!(ctx.len(), 1, "called with unexpected number of arguments");
            let base64_felts = ctx
                .get_raw(0)
                .as_str()
                .map_err(|e| rusqlite::Error::UserFunctionError(e.into()))?;

            Ok(base64_felts_to_index_prefixed_base32_felts(base64_felts))
        },
    )?;

    Ok(())
}

fn base64_felts_to_index_prefixed_base32_felts(base64_felts: &str) -> String {
    let strings = base64_felts
        .split(' ')
        // Convert only the first 256 elements so that the index fits into one u8
        // we will use as a prefix byte.
        .take(connection::EVENT_KEY_FILTER_LIMIT)
        .enumerate()
        .map(|(index, key)| {
            let mut buf: [u8; 33] = [0u8; 33];
            buf[0] = index as u8;
            base64::decode_config_slice(key, base64::STANDARD, &mut buf[1..]).unwrap();
            data_encoding::BASE32_NOPAD.encode(&buf)
        })
        .collect::<Vec<_>>();

    strings.join(" ")
}

/// Migrates the database to the latest version. This __MUST__ be called
/// at the beginning of the application.
fn migrate_database(connection: &mut rusqlite::Connection) -> anyhow::Result<()> {
    let mut current_revision = schema_version(connection)?;
    let migrations = schema::migrations();

    // The target version is the number of null migrations which have been replaced
    // by the base schema + the new migrations built on top of that.
    let latest_revision = schema::BASE_SCHEMA_REVISION + migrations.len();

    // Apply the base schema if the database is new.
    if current_revision == 0 {
        let tx = connection
            .transaction()
            .context("Create database transaction")?;
        schema::base_schema(&tx).context("Applying base schema")?;
        tx.pragma_update(None, VERSION_KEY, schema::BASE_SCHEMA_REVISION)
            .context("Failed to update the schema version number")?;
        tx.commit().context("Commit migration transaction")?;

        current_revision = schema::BASE_SCHEMA_REVISION;
    }

    // Skip migration if we already at latest.
    if current_revision == latest_revision {
        tracing::info!(%current_revision, "No database migrations required");
        return Ok(());
    }

    // Check for database version compatibility.
    if current_revision < schema::BASE_SCHEMA_REVISION {
        tracing::error!(
            version=%current_revision,
            limit=%schema::BASE_SCHEMA_REVISION,
            "Database version is too old to migrate"
        );
        anyhow::bail!("Database version {current_revision} too old to migrate");
    }

    if current_revision > latest_revision {
        tracing::error!(
            version=%current_revision,
            limit=%latest_revision,
            "Database version is from a newer than this application expected"
        );
        anyhow::bail!(
            "Database version {current_revision} is newer than this application expected {latest_revision}",
        );
    }

    let amount = latest_revision - current_revision;
    tracing::info!(%current_revision, %latest_revision, migrations=%amount, "Performing database migrations");

    // Sequentially apply each missing migration.
    migrations
        .iter()
        .rev()
        .take(amount)
        .rev()
        .try_for_each(|migration| {
            let mut do_migration = || -> anyhow::Result<()> {
                current_revision += 1;
                let span = tracing::info_span!("db_migration", revision = current_revision);
                let _enter = span.enter();

                let transaction = connection
                    .transaction()
                    .context("Create database transaction")?;
                migration(&transaction)?;
                transaction
                    .pragma_update(None, VERSION_KEY, current_revision)
                    .context("Failed to update the schema version number")?;
                transaction
                    .commit()
                    .context("Commit migration transaction")?;

                Ok(())
            };

            do_migration().with_context(|| format!("Migrating to {current_revision}"))
        })?;

    Ok(())
}

/// Returns the current schema version of the existing database,
/// or `0` if database does not yet exist.
fn schema_version(connection: &rusqlite::Connection) -> anyhow::Result<usize> {
    // We store the schema version in the Sqlite provided PRAGMA "user_version",
    // which stores an INTEGER and defaults to 0.
    let version = connection.query_row(
        &format!("SELECT {VERSION_KEY} FROM pragma_user_version;"),
        [],
        |row| row.get::<_, usize>(0),
    )?;
    Ok(version)
}

#[cfg(test)]
mod tests {
    use pathfinder_common::felt;
    use stark_hash::Felt;

    use super::*;

    #[test]
    fn schema_version_defaults_to_zero() {
        let mut conn = rusqlite::Connection::open_in_memory().unwrap();
        let transaction = conn.transaction().unwrap();

        let version = schema_version(&transaction).unwrap();
        assert_eq!(version, 0);
    }

    #[test]
    fn full_migration() {
        let mut conn = rusqlite::Connection::open_in_memory().unwrap();
        setup_connection(&mut conn).unwrap();
        migrate_database(&mut conn).unwrap();
        let version = schema_version(&conn).unwrap();
        let expected = schema::migrations().len() + schema::BASE_SCHEMA_REVISION;
        assert_eq!(version, expected);
    }

    #[test]
    fn migration_fails_if_db_is_newer() {
        let mut conn = rusqlite::Connection::open_in_memory().unwrap();
        setup_connection(&mut conn).unwrap();

        // Force the schema to a newer version
        let current_version = schema::migrations().len();
        conn.pragma_update(None, VERSION_KEY, current_version + 1)
            .unwrap();

        // Migration should fail.
        migrate_database(&mut conn).unwrap_err();
    }

    #[test]
    fn foreign_keys_are_enforced() {
        let conn = rusqlite::Connection::open_in_memory().unwrap();

        // We first disable foreign key support. Sqlite currently enables this by default,
        // but this may change in the future. So we disable to check that our enable function
        // works regardless of what Sqlite's default is.
        use rusqlite::config::DbConfig::SQLITE_DBCONFIG_ENABLE_FKEY;
        conn.set_db_config(SQLITE_DBCONFIG_ENABLE_FKEY, false)
            .unwrap();

        // Enable foreign key support.
        conn.set_db_config(SQLITE_DBCONFIG_ENABLE_FKEY, true)
            .unwrap();

        // Create tables with a parent-child foreign key requirement.
        conn.execute_batch(
            r"
                    CREATE TABLE parent(
                        id INTEGER PRIMARY KEY
                    );

                    CREATE TABLE child(
                        id INTEGER PRIMARY KEY,
                        parent_id INTEGER NOT NULL REFERENCES parent(id)
                    );
                ",
        )
        .unwrap();

        // Check that foreign keys are enforced.
        conn.execute("INSERT INTO parent (id) VALUES (2)", [])
            .unwrap();
        conn.execute("INSERT INTO child (id, parent_id) VALUES (0, 2)", [])
            .unwrap();
        conn.execute("INSERT INTO child (id, parent_id) VALUES (1, 1)", [])
            .unwrap_err();
    }

    #[test]
    fn felts_to_index_prefixed_base32_strings() {
        let input: String = [felt!("0x901823"), felt!("0x901823"), felt!("0x901825")]
            .iter()
            .map(|f| base64::encode(f.as_be_bytes()))
            .collect::<Vec<_>>()
            .join(" ");
        assert_eq!(
            super::base64_felts_to_index_prefixed_base32_felts(&input),
            "AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAASAMCG AEAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAASAMCG AIAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAASAMCK".to_owned()
        );
    }

    #[test]
    fn felts_to_index_prefixed_base32_strings_encodes_the_first_256_felts() {
        let input = [Felt::ZERO; 257]
            .iter()
            .map(|f| base64::encode(f.as_be_bytes()))
            .collect::<Vec<_>>()
            .join(" ");
        let output = super::base64_felts_to_index_prefixed_base32_felts(&input);

        assert_eq!(output.split(' ').count(), 256);
    }

    #[test]
    fn rpc_test_db_is_migrated() {
        let mut source_path = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
        source_path.push("../rpc/fixtures/mainnet.sqlite");

        let db_dir = tempfile::TempDir::new().unwrap();
        let mut db_path = PathBuf::from(db_dir.path());
        db_path.push("mainnet.sqlite");

        std::fs::copy(&source_path, &db_path).unwrap();

        let database = rusqlite::Connection::open(db_path).unwrap();
        let version = schema_version(&database).unwrap();
        let expected = schema::migrations().len() + schema::BASE_SCHEMA_REVISION;

        assert_eq!(version, expected, "RPC database fixture needs migrating");
    }
}
