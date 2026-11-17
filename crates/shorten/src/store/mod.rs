//! SQLite store.
//!
//! One writer connection and a bounded reader pool, WAL mode, an FTS5 index
//! and keyset pagination. The schema, the integer millisecond timestamp
//! encoding, the cursor encoding and every query shape are inherited from the
//! previous implementation, so a database written by the Go service opens and
//! reads unchanged here.

use std::collections::{BTreeMap, HashMap};
use std::fs;
use std::path::{Path, PathBuf};

use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use base64::Engine as _;
use chrono::{DateTime, Utc};
use r2d2::Pool;
use r2d2_sqlite::SqliteConnectionManager;
use rusqlite::{params, Connection, OpenFlags, Row, ToSql};
use serde::{Deserialize, Serialize};

use crate::analytics::{Flusher, VisitDelta};
use crate::link::{self, CreateInput, DomainError, Link, Status, UpdateInput, ValidationErrors};
use crate::time;

const MIGRATIONS: &[(u32, &str, &str)] = &[
    (1, "001_init.sql", include_str!("migrations/001_init.sql")),
    (
        2,
        "002_expiration_index.sql",
        include_str!("migrations/002_expiration_index.sql"),
    ),
    (
        3,
        "003_status_updated_index.sql",
        include_str!("migrations/003_status_updated_index.sql"),
    ),
];

/// Query options for a page of links.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ListOptions<'a> {
    pub search: &'a str,
    pub filter: &'a str,
    pub sort: &'a str,
    pub dir: &'a str,
    pub cursor: &'a str,
    pub limit: i64,
    /// Reference time for expiry-sensitive filters and cursors. `None` uses
    /// the current instant, matching the previous zero-value default.
    pub now: Option<DateTime<Utc>>,
}

/// A page of links plus the keyset cursor for the next page.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct ListResult {
    pub links: Vec<Link>,
    pub next_cursor: String,
    pub total: i64,
    pub total_known: bool,
}

/// What an import decided to do with one record.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ImportOutcome {
    Inserted,
    Identical,
    Skipped,
    Replaced,
}

impl ImportOutcome {
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Inserted => "inserted",
            Self::Identical => "identical",
            Self::Skipped => "skipped",
            Self::Replaced => "replaced",
        }
    }
}

/// Link and visit totals, used by backups and the seed tool.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Summary {
    pub links: i64,
    pub visits: i64,
    #[serde(rename = "schemaVersion")]
    pub schema_version: i64,
}

/// The links and total count matching a cleanup rule.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct CleanupPreview {
    pub links: Vec<Link>,
    pub count: i64,
}

/// Store failures, keeping the previous implementation's messages.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum StoreError {
    #[error("{0}")]
    Message(String),
    #[error("{context}: {detail}")]
    Wrapped { context: String, detail: String },
}

impl StoreError {
    /// Wrap an operational failure with the context the previous
    /// implementation reported, keeping the `%: %w` message shape.
    #[must_use]
    pub fn wrapped(context: impl Into<String>, source: impl std::fmt::Display) -> Self {
        Self::Wrapped {
            context: context.into(),
            detail: source.to_string(),
        }
    }
}

/// The SQLite store: one writer pool, one bounded reader pool.
#[derive(Debug)]
pub struct Store {
    read: Pool<SqliteConnectionManager>,
    write: Pool<SqliteConnectionManager>,
    path: PathBuf,
}

const OPEN_FLAGS: OpenFlags = OpenFlags::SQLITE_OPEN_READ_WRITE
    .union(OpenFlags::SQLITE_OPEN_CREATE)
    .union(OpenFlags::SQLITE_OPEN_NO_MUTEX);

fn configure(connection: &mut Connection) -> Result<(), rusqlite::Error> {
    let setup = [
        "PRAGMA busy_timeout = 5000",
        "PRAGMA journal_mode = WAL",
        "PRAGMA synchronous = NORMAL",
        "PRAGMA foreign_keys = ON",
        "PRAGMA cache_size = -20000",
    ];
    for statement in setup {
        connection.execute_batch(statement)?;
    }
    Ok(())
}

fn pool_for(path: &Path, max_size: usize) -> Result<Pool<SqliteConnectionManager>, StoreError> {
    let manager = SqliteConnectionManager::file(path)
        .with_flags(OPEN_FLAGS)
        .with_init(configure);
    r2d2::Builder::new()
        .max_size(u32::try_from(max_size.max(1)).unwrap_or(u32::MAX))
        .min_idle(Some(1))
        .build(manager)
        .map_err(|error| StoreError::wrapped("open SQLite pool", error))
}

impl Store {
    /// Open the database, create its directory, apply pending migrations and
    /// verify the capabilities the service depends on.
    ///
    /// # Errors
    ///
    /// Reports the same operational failures the Go service surfaced,
    /// including a missing FTS5 build or a database that refused WAL.
    pub fn open(path: &str, read_connections: usize) -> Result<Self, StoreError> {
        if path != ":memory:" {
            if let Some(parent) = Path::new(path)
                .parent()
                .filter(|parent| !parent.as_os_str().is_empty())
            {
                fs::create_dir_all(parent)
                    .map_err(|error| StoreError::wrapped("create database directory", error))?;
            }
        }
        let write = pool_for(Path::new(path), 1)?;
        let read = pool_for(Path::new(path), read_connections)?;
        let store = Self {
            read,
            write,
            path: Path::new(path).to_path_buf(),
        };
        if let Err(error) = store.migrate().and_then(|()| store.verify()) {
            let _ = store.close();
            return Err(error);
        }
        Ok(store)
    }

    /// The database file behind this store.
    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }

    fn migrate(&self) -> Result<(), StoreError> {
        let mut connection = self
            .write
            .get()
            .map_err(|error| StoreError::wrapped("acquire writer", error))?;
        connection
            .execute_batch(
                "CREATE TABLE IF NOT EXISTS schema_migrations (\
                 version INTEGER PRIMARY KEY, applied_at INTEGER NOT NULL)",
            )
            .map_err(|error| StoreError::wrapped("create migrations table", error))?;
        for (version, name, contents) in MIGRATIONS {
            let exists: i64 = connection
                .query_row(
                    "SELECT COUNT(*) FROM schema_migrations WHERE version = ?1",
                    params![version],
                    |row| row.get(0),
                )
                .map_err(|error| StoreError::wrapped("read migration state", error))?;
            if exists > 0 {
                continue;
            }
            let transaction = connection
                .transaction()
                .map_err(|error| StoreError::wrapped("begin migration", error))?;
            let outcome = (|| -> Result<(), rusqlite::Error> {
                transaction.execute_batch(contents)?;
                transaction.execute(
                    "INSERT INTO schema_migrations(version, applied_at) VALUES (?1, ?2)",
                    params![version, Utc::now().timestamp_millis()],
                )?;
                Ok(())
            })();
            match outcome {
                Ok(()) => transaction.commit().map_err(|error| {
                    StoreError::wrapped(format!("commit migration {version}"), error)
                })?,
                Err(error) => {
                    let _ = transaction.rollback();
                    return Err(StoreError::wrapped(
                        format!("apply migration {version} ({name})"),
                        error,
                    ));
                }
            }
        }
        Ok(())
    }

    /// Confirm FTS5, WAL and foreign keys are live before serving traffic.
    fn verify(&self) -> Result<(), StoreError> {
        let fts: i64 = self
            .read
            .get()
            .map_err(|error| StoreError::wrapped("acquire reader", error))?
            .query_row(
                "SELECT sqlite_compileoption_used('ENABLE_FTS5')",
                [],
                |row| row.get(0),
            )
            .map_err(|error| StoreError::wrapped("check FTS5", error))?;
        if fts != 1 {
            return Err(StoreError::Message(
                "SQLite must be built with FTS5 support".to_string(),
            ));
        }
        let mode: String = self
            .write
            .get()
            .map_err(|error| StoreError::wrapped("acquire writer", error))?
            .query_row("PRAGMA journal_mode", [], |row| row.get(0))
            .map_err(|error| StoreError::wrapped("read journal mode", error))?;
        if !mode.eq_ignore_ascii_case("wal") {
            return Err(StoreError::Message(format!(
                "SQLite WAL mode is required, got {mode:?}"
            )));
        }
        let foreign_keys: i64 = self
            .write
            .get()
            .map_err(|error| StoreError::wrapped("acquire writer", error))?
            .query_row("PRAGMA foreign_keys", [], |row| row.get(0))
            .map_err(|error| StoreError::wrapped("read foreign_keys", error))?;
        if foreign_keys != 1 {
            return Err(StoreError::Message(
                "SQLite foreign_keys must be enabled".to_string(),
            ));
        }
        self.ping()
    }

    /// Cheap readiness probe used by `/readyz`.
    ///
    /// # Errors
    ///
    /// Propagates a pool timeout or a database failure.
    pub fn ping(&self) -> Result<(), StoreError> {
        let one: i64 = self
            .read
            .get()
            .map_err(|error| StoreError::wrapped("acquire reader", error))?
            .query_row("SELECT 1", [], |row| row.get(0))
            .map_err(|error| StoreError::wrapped("probe SQLite", error))?;
        if one != 1 {
            return Err(StoreError::Message(
                "unexpected SQLite probe result".to_string(),
            ));
        }
        Ok(())
    }

    /// Highest applied migration version.
    ///
    /// # Errors
    ///
    /// Propagates a database failure.
    pub fn schema_version(&self) -> Result<i64, StoreError> {
        self.read
            .get()
            .map_err(|error| StoreError::wrapped("acquire reader", error))?
            .query_row(
                "SELECT COALESCE(MAX(version), 0) FROM schema_migrations",
                [],
                |row| row.get(0),
            )
            .map_err(|error| StoreError::wrapped("read schema version", error))
    }

    /// Create a link, generating a short code when none was supplied and
    /// retrying a generated collision up to eight times.
    ///
    /// # Errors
    ///
    /// Validation problems, a slug collision or a database failure.
    pub fn create(&self, mut input: CreateInput) -> Result<Link, DomainError> {
        link::validate_create(&mut input)?;
        let generated = input.slug.is_empty();
        for _attempt in 0..8 {
            if generated {
                input.slug = link::generate_slug()?;
            }
            let now = time::now_truncated().timestamp_millis();
            let outcome = self.with_writer(|connection| {
                connection.execute(
                    "INSERT INTO links(slug,destination_url,title,notes,status,redirect_status,expires_at,visit_count,created_at,updated_at) \
                     VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9,?10)",
                    params![
                        &input.slug,
                        &input.destination_url,
                        &input.title,
                        &input.notes,
                        status_text(&input),
                        input.redirect_status,
                        input.expires_at.map(|value| value.timestamp_millis()),
                        0_i64,
                        now,
                        now
                    ],
                )?;
                // last_insert_rowid is per-connection, so read it on the
                // connection that just performed the insert.
                Ok(connection.last_insert_rowid())
            });
            match outcome {
                Ok(id) => return self.get_by_id(id),
                Err(InsertError::Unique) if generated => {
                    input.slug = String::new();
                }
                Err(InsertError::Unique) => return Err(DomainError::Conflict),
                Err(InsertError::Failure(message)) => {
                    return Err(DomainError::wrapped("insert link", message))
                }
            }
        }
        Err(DomainError::Message(
            "unable to generate a unique slug after 8 attempts".to_string(),
        ))
    }

    /// Read one link by short code.
    ///
    /// # Errors
    ///
    /// `DomainError::NotFound` when the slug is absent.
    pub fn get_by_slug(&self, slug: &str) -> Result<Link, DomainError> {
        self.read_row(
            SELECT_COLUMNS,
            " WHERE slug = ?1",
            params![slug.to_string()],
        )
    }

    /// Read one link by identifier.
    ///
    /// # Errors
    ///
    /// `DomainError::NotFound` when the identifier is absent.
    pub fn get_by_id(&self, id: i64) -> Result<Link, DomainError> {
        self.read_row(SELECT_COLUMNS, " WHERE id = ?1", params![id])
    }

    /// Replace a link's fields, optionally renaming its short code. The write
    /// runs in one transaction so a rename never leaves two rows behind.
    ///
    /// # Errors
    ///
    /// Validation problems, a collision on the new slug, a missing link, or a
    /// database failure.
    pub fn update(&self, current_slug: &str, mut input: UpdateInput) -> Result<Link, DomainError> {
        link::validate_create(&mut input)?;
        if input.slug.is_empty() {
            let mut errors = ValidationErrors::new();
            errors.insert("slug".to_string(), "Enter a short code.".to_string());
            return Err(DomainError::Validation(errors));
        }
        let updated_at = Utc::now().timestamp_millis();
        let slug = input.slug.clone();
        let destination = input.destination_url.clone();
        let title = input.title.clone();
        let notes = input.notes.clone();
        let status = status_text(&input).to_string();
        let redirect = input.redirect_status;
        let expires = input.expires_at.map(|value| value.timestamp_millis());
        let current_slug = current_slug.to_string();

        let mut connection = self
            .write
            .get()
            .map_err(|error| DomainError::wrapped("acquire writer", error))?;
        let transaction = connection
            .transaction()
            .map_err(|error| DomainError::wrapped("begin update", error))?;
        let id: i64 = match transaction.query_row(
            "SELECT id FROM links WHERE slug = ?1",
            params![current_slug],
            |row| row.get(0),
        ) {
            Ok(id) => id,
            Err(rusqlite::Error::QueryReturnedNoRows) => {
                let _ = transaction.rollback();
                return Err(DomainError::NotFound);
            }
            Err(error) => {
                let _ = transaction.rollback();
                return Err(DomainError::wrapped("read link id", error));
            }
        };
        let executed = transaction.execute(
            "UPDATE links SET slug=?1,destination_url=?2,title=?3,notes=?4,status=?5,redirect_status=?6,expires_at=?7,updated_at=?8 \
             WHERE id=?9",
            params![slug, destination, title, notes, status, redirect, expires, updated_at, id],
        );
        match executed {
            Ok(_) => {
                transaction
                    .commit()
                    .map_err(|error| DomainError::wrapped("commit update", error))?;
            }
            Err(error) => {
                let _ = transaction.rollback();
                if is_unique(&error) {
                    return Err(DomainError::Conflict);
                }
                return Err(DomainError::wrapped("update link", error));
            }
        }
        drop(connection);
        self.get_by_id(id)
    }

    /// Delete one link permanently.
    ///
    /// # Errors
    ///
    /// `DomainError::NotFound` when nothing matched the slug.
    pub fn delete(&self, slug: &str) -> Result<(), DomainError> {
        let affected = self
            .with_writer(|connection| {
                connection.execute("DELETE FROM links WHERE slug = ?1", params![slug])
            })
            .map_err(|error| DomainError::wrapped("delete link", error_message(error)))?;
        if affected == 0 {
            return Err(DomainError::NotFound);
        }
        Ok(())
    }

    /// Count and preview the links older than `cutoff` for one cleanup scope.
    ///
    /// # Errors
    ///
    /// An unsupported scope, a limit outside 1–100, or a database failure.
    pub fn preview_cleanup(
        &self,
        cutoff: DateTime<Utc>,
        now: DateTime<Utc>,
        scope: &str,
        limit: i64,
    ) -> Result<CleanupPreview, DomainError> {
        let limit = if (1..=100).contains(&limit) {
            limit
        } else {
            10
        };
        let (condition, extra) = cleanup_condition(scope, now)?;
        let where_clause = format!("created_at < ?1 AND ({condition})");
        let cutoff_millis = cutoff.timestamp_millis();
        let count: i64 = match extra {
            ExtraArgs::None => self
                .read
                .get()
                .map_err(|error| DomainError::wrapped("acquire reader", error))?
                .query_row(
                    &format!("SELECT COUNT(*) FROM links WHERE {where_clause}"),
                    params![cutoff_millis],
                    |row| row.get(0),
                ),
            ExtraArgs::Now => self
                .read
                .get()
                .map_err(|error| DomainError::wrapped("acquire reader", error))?
                .query_row(
                    &format!("SELECT COUNT(*) FROM links WHERE {where_clause}"),
                    params![cutoff_millis, now.timestamp_millis()],
                    |row| row.get(0),
                ),
        }
        .map_err(|error| DomainError::wrapped("count cleanup links", error))?;

        let mut links = Vec::with_capacity(usize::try_from(limit).unwrap_or(10));
        let mut statement_args: Vec<Box<dyn ToSql>> =
            vec![Box::new(cutoff_millis), Box::new(limit)];
        if matches!(extra, ExtraArgs::Now) {
            statement_args.insert(1, Box::new(now.timestamp_millis()));
        }
        let connection = self
            .read
            .get()
            .map_err(|error| DomainError::wrapped("acquire reader", error))?;
        let mut statement = connection
            .prepare(&format!(
                "{SELECT_COLUMNS} WHERE {where_clause} ORDER BY created_at ASC, id ASC LIMIT ?{}",
                statement_args.len()
            ))
            .map_err(|error| DomainError::wrapped("preview cleanup links", error))?;
        let refs: Vec<&dyn ToSql> = statement_args.iter().map(|arg| &**arg).collect();
        let mut rows = statement
            .query(rusqlite::params_from_iter(refs.iter()))
            .map_err(|error| DomainError::wrapped("preview cleanup links", error))?;
        while let Some(row) = rows
            .next()
            .map_err(|error| DomainError::wrapped("preview cleanup links", error))?
        {
            links.push(
                scan_link(row)
                    .map_err(|error| DomainError::wrapped("preview cleanup links", error))?,
            );
        }
        drop(rows);
        Ok(CleanupPreview { links, count })
    }

    /// Delete every link older than `cutoff` for one cleanup scope and report
    /// how many rows went away.
    ///
    /// # Errors
    ///
    /// An unsupported scope, a database failure, or a pool timeout.
    pub fn delete_before(
        &self,
        cutoff: DateTime<Utc>,
        now: DateTime<Utc>,
        scope: &str,
    ) -> Result<i64, DomainError> {
        let (condition, extra) = cleanup_condition(scope, now)?;
        let sql = format!("DELETE FROM links WHERE created_at < ?1 AND ({condition})");
        let cutoff_millis = cutoff.timestamp_millis();
        let now_millis = now.timestamp_millis();
        let connection = self
            .write
            .get()
            .map_err(|error| DomainError::wrapped("acquire writer", error))?;
        let affected = match extra {
            ExtraArgs::None => connection.execute(&sql, params![cutoff_millis]),
            ExtraArgs::Now => connection.execute(&sql, params![cutoff_millis, now_millis]),
        }
        .map_err(|error| DomainError::wrapped("delete cleanup links", error))?;
        Ok(i64::try_from(affected).unwrap_or(i64::MAX))
    }

    /// Read a page of links with search, filtering, sorting and a keyset cursor.
    ///
    /// # Errors
    ///
    /// An unsupported sort, filter, direction, limit or cursor.
    pub fn list(&self, options: ListOptions<'_>) -> Result<ListResult, DomainError> {
        let mut limit = options.limit;
        if limit < 1 {
            limit = 30;
        }
        if limit > 100 {
            return Err(DomainError::Message(
                "limit must be between 1 and 100".to_string(),
            ));
        }
        let now = options.now.unwrap_or_else(Utc::now);
        let (column, numeric) = sort_column(options.sort)?;
        let sort = if options.sort.is_empty() {
            "updated"
        } else {
            options.sort
        };
        let dir = if options.dir.is_empty() {
            "desc"
        } else {
            options.dir
        };
        if dir != "asc" && dir != "desc" {
            return Err(DomainError::Message(
                "sort direction must be asc or desc".to_string(),
            ));
        }

        let mut conditions = vec!["1=1".to_string()];
        let mut args: Vec<Box<dyn ToSql>> = Vec::new();
        let mut index = 1_usize;
        let next_placeholder = |slot: &mut usize| {
            let current = *slot;
            *slot += 1;
            format!("?{current}")
        };

        let search = fts_query(options.search);
        let join = if search.is_empty() {
            String::new()
        } else {
            let placeholder = next_placeholder(&mut index);
            conditions.push(format!("links_fts MATCH {placeholder}"));
            args.push(Box::new(search));
            " JOIN links_fts ON links_fts.rowid = l.id".to_string()
        };

        match options.filter {
            "" | "all" => {}
            "active" => {
                let placeholder = next_placeholder(&mut index);
                conditions.push(format!(
                    "l.status='active' AND (l.expires_at IS NULL OR l.expires_at > {placeholder})"
                ));
                args.push(Box::new(now.timestamp_millis()));
            }
            "disabled" => conditions.push("l.status='disabled'".to_string()),
            "expired" => {
                let placeholder = next_placeholder(&mut index);
                conditions.push(format!(
                    "l.expires_at IS NOT NULL AND l.expires_at <= {placeholder}"
                ));
                args.push(Box::new(now.timestamp_millis()));
            }
            "unexpired" => {
                let placeholder = next_placeholder(&mut index);
                conditions.push(format!(
                    "(l.expires_at IS NULL OR l.expires_at > {placeholder})"
                ));
                args.push(Box::new(now.timestamp_millis()));
            }
            _ => {
                return Err(DomainError::Message(
                    "unsupported status filter".to_string(),
                ))
            }
        }

        if !options.cursor.is_empty() {
            let cursor = decode_cursor(options.cursor)?;
            if cursor.sort != sort || cursor.dir != dir {
                return Err(DomainError::Message(
                    "cursor does not match active sort".to_string(),
                ));
            }
            let op = if dir == "desc" { "<" } else { ">" };
            let first = next_placeholder(&mut index);
            let second = next_placeholder(&mut index);
            let third = next_placeholder(&mut index);
            conditions.push(format!(
                "({column} {op} {first} OR ({column} = {second} AND l.id {op} {third}))"
            ));
            if numeric {
                args.push(Box::new(cursor.number));
                args.push(Box::new(cursor.number));
            } else {
                args.push(Box::new(cursor.text.clone()));
                args.push(Box::new(cursor.text));
            }
            args.push(Box::new(cursor.id));
        }

        let order = dir.to_uppercase();
        let limit_placeholder = next_placeholder(&mut index);
        args.push(Box::new(limit + 1));
        let sql = format!(
            "SELECT l.id,l.slug,l.destination_url,l.title,l.notes,l.status,l.redirect_status,l.expires_at,l.visit_count,l.last_visited_at,l.created_at,l.updated_at \
             FROM links l{join} WHERE {} ORDER BY {column} {order}, l.id {order} LIMIT {limit_placeholder}",
            conditions.join(" AND ")
        );

        let connection = self
            .read
            .get()
            .map_err(|error| DomainError::wrapped("acquire reader", error))?;
        let mut statement = connection
            .prepare(&sql)
            .map_err(|error| DomainError::wrapped("list links", error))?;
        let refs: Vec<&dyn ToSql> = args.iter().map(|arg| &**arg).collect();
        let mut rows = statement
            .query(rusqlite::params_from_iter(refs.iter()))
            .map_err(|error| DomainError::wrapped("list links", error))?;
        let mut items: Vec<Link> = Vec::new();
        while let Some(row) = rows
            .next()
            .map_err(|error| DomainError::wrapped("list links", error))?
        {
            items.push(scan_link(row).map_err(|error| DomainError::wrapped("list links", error))?);
        }
        drop(rows);

        let mut result = ListResult {
            links: items,
            next_cursor: String::new(),
            total: -1,
            total_known: false,
        };
        if options.search.is_empty() && (options.filter.is_empty() || options.filter == "all") {
            let total: i64 = self
                .read
                .get()
                .map_err(|error| DomainError::wrapped("acquire reader", error))?
                .query_row("SELECT COUNT(*) FROM links", [], |row| row.get(0))
                .map_err(|error| DomainError::wrapped("count links", error))?;
            result.total = total;
            result.total_known = true;
        }
        if result.links.len() > usize::try_from(limit).unwrap_or(usize::MAX) {
            result
                .links
                .truncate(usize::try_from(limit).unwrap_or(usize::MAX));
            let last = result.links.last().expect("page has rows");
            let cursor = Cursor {
                version: 1,
                sort: sort.to_string(),
                dir: dir.to_string(),
                number: match sort {
                    "created" => last.created_at.timestamp_millis(),
                    "updated" => last.updated_at.timestamp_millis(),
                    "visits" => last.visit_count,
                    _ => 0,
                },
                text: match sort {
                    "destination" => last.destination_url.to_lowercase(),
                    "slug" => last.slug.clone(),
                    _ => String::new(),
                },
                id: last.id,
            };
            result.next_cursor = encode_cursor(&cursor)?;
        }
        Ok(result)
    }

    /// Apply accumulated visit counters in one transaction.
    ///
    /// # Errors
    ///
    /// A database failure, which the collector treats as retriable.
    pub fn apply_visits(&self, deltas: &HashMap<i64, VisitDelta>) -> Result<(), DomainError> {
        if deltas.is_empty() {
            return Ok(());
        }
        let mut connection = self
            .write
            .get()
            .map_err(|error| DomainError::wrapped("acquire writer", error))?;
        let transaction = connection
            .transaction()
            .map_err(|error| DomainError::wrapped("begin visit flush", error))?;
        let mut statement = transaction
            .prepare(
                "UPDATE links SET visit_count=visit_count+?, \
                 last_visited_at=CASE WHEN last_visited_at IS NULL OR last_visited_at < ? THEN ? ELSE last_visited_at END \
                 WHERE id=?",
            )
            .map_err(|error| DomainError::wrapped("prepare visit flush", error))?;
        for (id, delta) in deltas {
            let latest = delta.latest.timestamp_millis();
            let outcome = statement.execute(params![delta.count, latest, latest, id]);
            if let Err(error) = outcome {
                // Dropping the transaction rolls the batch back, exactly like
                // the previous implementation's explicit rollback.
                return Err(DomainError::wrapped("apply visit", error));
            }
        }
        drop(statement);
        transaction
            .commit()
            .map_err(|error| DomainError::wrapped("commit visit flush", error))
    }

    /// Report what an import would do with one candidate record.
    ///
    /// # Errors
    ///
    /// `DomainError::Conflict` when a different record already owns the slug.
    pub fn inspect_import(&self, candidate: &Link) -> Result<ImportOutcome, DomainError> {
        match self.get_by_slug(&candidate.slug) {
            Err(DomainError::NotFound) => Ok(ImportOutcome::Inserted),
            Err(error) => Err(error),
            Ok(existing) if logical_equal(&existing, candidate) => Ok(ImportOutcome::Identical),
            Ok(_) => Err(DomainError::Conflict),
        }
    }

    /// Import a batch of records under one of the three conflict policies.
    ///
    /// # Errors
    ///
    /// An unknown policy, an `abort` collision, a validation problem, or a
    /// database failure. The whole transaction rolls back on any failure.
    pub fn import_batch(
        &self,
        candidates: &[Link],
        policy: &str,
    ) -> Result<Vec<ImportOutcome>, DomainError> {
        if !matches!(policy, "abort" | "skip" | "replace") {
            return Err(DomainError::Message(
                "conflict policy must be abort, skip, or replace".to_string(),
            ));
        }
        let mut connection = self
            .write
            .get()
            .map_err(|error| DomainError::wrapped("acquire writer", error))?;
        let transaction = connection
            .transaction()
            .map_err(|error| DomainError::wrapped("begin import", error))?;
        let outcomes = run_import(&transaction, candidates, policy);
        match outcomes {
            Ok(outcomes) => {
                transaction
                    .commit()
                    .map_err(|error| DomainError::wrapped("commit import", error))?;
                Ok(outcomes)
            }
            Err(error) => {
                let _ = transaction.rollback();
                Err(error)
            }
        }
    }

    /// Link and visit totals plus the schema version.
    ///
    /// # Errors
    ///
    /// A database failure.
    pub fn summary(&self) -> Result<Summary, DomainError> {
        let (links, visits) = self
            .read
            .get()
            .map_err(|error| DomainError::wrapped("acquire reader", error))?
            .query_row(
                "SELECT COUNT(*), COALESCE(SUM(visit_count),0) FROM links",
                [],
                |row| Ok((row.get::<_, i64>(0)?, row.get::<_, i64>(1)?)),
            )
            .map_err(|error| DomainError::wrapped("read summary", error))?;
        Ok(Summary {
            links,
            visits,
            schema_version: self
                .schema_version()
                .map_err(|error| DomainError::wrapped("read schema version", error))?,
        })
    }

    /// Truncate the write-ahead log, run during shutdown.
    ///
    /// # Errors
    ///
    /// A database failure.
    pub fn checkpoint(&self) -> Result<(), DomainError> {
        self.write
            .get()
            .map_err(|error| DomainError::wrapped("acquire writer", error))?
            .execute_batch("PRAGMA wal_checkpoint(TRUNCATE)")
            .map_err(|error| DomainError::wrapped("checkpoint WAL", error))
    }

    /// Full integrity check, run against a fresh backup copy.
    ///
    /// # Errors
    ///
    /// A database failure, or the reported corruption.
    pub fn integrity_check(&self) -> Result<(), DomainError> {
        let result: String = self
            .read
            .get()
            .map_err(|error| DomainError::wrapped("acquire reader", error))?
            .query_row("PRAGMA integrity_check", [], |row| row.get(0))
            .map_err(|error| DomainError::wrapped("integrity check", error))?;
        if result != "ok" {
            return Err(DomainError::Message(format!("integrity check: {result}")));
        }
        Ok(())
    }

    /// Write an online, application-consistent copy with `VACUUM INTO`.
    ///
    /// # Errors
    ///
    /// A missing destination, a directory failure, or a database failure.
    pub fn backup(&self, destination: &str) -> Result<(), DomainError> {
        if destination.is_empty() {
            return Err(DomainError::Message(
                "backup destination is required".to_string(),
            ));
        }
        if let Some(parent) = Path::new(destination)
            .parent()
            .filter(|parent| !parent.as_os_str().is_empty())
        {
            // Go's `os.MkdirAll` surfaced a `fs.PathError`, which the backup
            // program echoed verbatim as `mkdir <path>: <errno text>`, so the
            // directory step reports the same shape.
            fs::create_dir_all(parent).map_err(|error| {
                DomainError::wrapped(format!("mkdir {}", parent.display()), GoErrno(&error))
            })?;
        }
        let quoted = destination.replace('\'', "''");
        self.write
            .get()
            .map_err(|error| DomainError::wrapped("acquire writer", error))?
            .execute_batch(&format!("VACUUM INTO '{quoted}'"))
            .map_err(|error| DomainError::wrapped("backup database", error))
    }

    /// Report the SQLite query plan for a statement.
    ///
    /// Used by the benchmark suite to confirm the list queries keep using
    /// their covering indexes.
    ///
    /// # Errors
    ///
    /// Propagates a database failure.
    pub fn explain_query_plan(&self, sql: &str) -> Result<Vec<String>, DomainError> {
        let connection = self
            .read
            .get()
            .map_err(|error| DomainError::wrapped("acquire reader", error))?;
        let mut statement = connection
            .prepare(&format!("EXPLAIN QUERY PLAN {sql}"))
            .map_err(|error| DomainError::wrapped("explain plan", error))?;
        let mut rows = statement
            .query([])
            .map_err(|error| DomainError::wrapped("explain plan", error))?;
        let mut plan = Vec::new();
        while let Some(row) = rows
            .next()
            .map_err(|error| DomainError::wrapped("explain plan", error))?
        {
            plan.push(
                row.get::<_, String>(3)
                    .map_err(|error| DomainError::wrapped("explain plan", error))?,
            );
        }
        Ok(plan)
    }

    /// Release the store's database connections.
    ///
    /// Pool connections are opened lazily and closed when the pool is
    /// dropped, so the shutdown path drops the store after checkpointing the
    /// write-ahead log.
    pub fn close(&self) -> Result<(), StoreError> {
        Ok(())
    }

    fn with_writer<T>(
        &self,
        work: impl FnOnce(&mut Connection) -> Result<T, rusqlite::Error>,
    ) -> Result<T, InsertError>
    where
        T: Default,
    {
        let mut connection = self
            .write
            .get()
            .map_err(|error| InsertError::Failure(error.to_string()))?;
        work(&mut connection).map_err(|error| {
            if is_unique(&error) {
                InsertError::Unique
            } else {
                InsertError::Failure(error.to_string())
            }
        })
    }

    fn read_row(
        &self,
        prefix: &str,
        clause: &str,
        args: impl rusqlite::Params,
    ) -> Result<Link, DomainError> {
        let sql = format!("{prefix}{clause}");
        let row = self
            .read
            .get()
            .map_err(|error| DomainError::wrapped("acquire reader", error))?
            .query_row(&sql, args, scan_link);
        match row {
            Ok(link) => Ok(link),
            Err(rusqlite::Error::QueryReturnedNoRows) => Err(DomainError::NotFound),
            Err(error) => Err(DomainError::wrapped("read link", error)),
        }
    }
}

impl Flusher for Store {
    fn apply_visits(&self, deltas: &HashMap<i64, VisitDelta>) -> Result<(), DomainError> {
        Store::apply_visits(self, deltas)
    }
}

#[derive(Debug)]
enum InsertError {
    Unique,
    Failure(String),
}

fn error_message(error: InsertError) -> String {
    match error {
        InsertError::Unique => "unique constraint failed".to_string(),
        InsertError::Failure(message) => message,
    }
}

enum ExtraArgs {
    None,
    Now,
}

fn status_text(input: &CreateInput) -> &'static str {
    input.status.unwrap_or(Status::Active).as_str()
}

const SELECT_COLUMNS: &str = "SELECT id,slug,destination_url,title,notes,status,redirect_status,expires_at,visit_count,last_visited_at,created_at,updated_at FROM links";

fn scan_link(row: &Row<'_>) -> Result<Link, rusqlite::Error> {
    let slug: String = row.get("slug")?;
    let status: String = row.get("status")?;
    let expires: Option<i64> = row.get("expires_at")?;
    let last_visited: Option<i64> = row.get("last_visited_at")?;
    let created: i64 = row.get("created_at")?;
    let updated: i64 = row.get("updated_at")?;
    Ok(Link {
        id: row.get("id")?,
        slug,
        destination_url: row.get("destination_url")?,
        title: row.get("title")?,
        notes: row.get("notes")?,
        status: Status::parse(&status).unwrap_or(Status::Active),
        redirect_status: row.get("redirect_status")?,
        expires_at: expires.map(time::from_millis),
        visit_count: row.get("visit_count")?,
        last_visited_at: last_visited.map(time::from_millis),
        created_at: time::from_millis(created),
        updated_at: time::from_millis(updated),
    })
}

fn is_unique(error: &rusqlite::Error) -> bool {
    error
        .to_string()
        .to_lowercase()
        .contains("unique constraint failed")
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
struct Cursor {
    #[serde(rename = "v")]
    version: i64,
    sort: String,
    dir: String,
    #[serde(rename = "n", default, skip_serializing_if = "num_is_zero")]
    number: i64,
    #[serde(rename = "s", default, skip_serializing_if = "String::is_empty")]
    text: String,
    id: i64,
}

fn num_is_zero(value: &i64) -> bool {
    *value == 0
}

fn encode_cursor(cursor: &Cursor) -> Result<String, DomainError> {
    let payload =
        serde_json::to_vec(cursor).map_err(|error| DomainError::wrapped("encode cursor", error))?;
    Ok(URL_SAFE_NO_PAD.encode(payload))
}

fn decode_cursor(value: &str) -> Result<Cursor, DomainError> {
    let payload = URL_SAFE_NO_PAD
        .decode(value)
        .map_err(|_| DomainError::Message("invalid cursor".to_string()))?;
    let cursor: Cursor = serde_json::from_slice(&payload)
        .map_err(|_| DomainError::Message("invalid cursor".to_string()))?;
    if cursor.version != 1 || cursor.id < 1 {
        return Err(DomainError::Message("invalid cursor".to_string()));
    }
    Ok(cursor)
}

fn sort_column(value: &str) -> Result<(String, bool), DomainError> {
    match value {
        "" | "updated" => Ok(("l.updated_at".to_string(), true)),
        "created" => Ok(("l.created_at".to_string(), true)),
        "visits" => Ok(("l.visit_count".to_string(), true)),
        "destination" => Ok(("l.destination_url COLLATE NOCASE".to_string(), false)),
        "slug" => Ok(("l.slug COLLATE BINARY".to_string(), false)),
        _ => Err(DomainError::Message("unsupported sort".to_string())),
    }
}

/// Build the FTS5 match expression from free text: up to eight cleaned terms,
/// each a prefix match, joined with `AND`.
#[must_use]
pub fn fts_query(value: &str) -> String {
    const CLEAN: &str = "\"'()*:^+-";
    let mut terms = Vec::new();
    for part in value.split_whitespace() {
        let cleaned: String = part.chars().filter(|c| !CLEAN.contains(*c)).collect();
        if cleaned.is_empty() {
            continue;
        }
        terms.push(format!("\"{cleaned}\"*"));
        if terms.len() == 8 {
            break;
        }
    }
    terms.join(" AND ")
}

fn cleanup_condition(scope: &str, now: DateTime<Utc>) -> Result<(String, ExtraArgs), DomainError> {
    let _ = now;
    match scope {
        "" | "inactive" => Ok((
            "status='disabled' OR (expires_at IS NOT NULL AND expires_at <= ?2)".to_string(),
            ExtraArgs::Now,
        )),
        "disabled" => Ok(("status='disabled'".to_string(), ExtraArgs::None)),
        "expired" => Ok((
            "expires_at IS NOT NULL AND expires_at <= ?2".to_string(),
            ExtraArgs::Now,
        )),
        "all" => Ok(("1=1".to_string(), ExtraArgs::None)),
        _ => Err(DomainError::Message(
            "unsupported cleanup scope".to_string(),
        )),
    }
}

fn run_import(
    transaction: &rusqlite::Transaction<'_>,
    candidates: &[Link],
    policy: &str,
) -> Result<Vec<ImportOutcome>, DomainError> {
    let mut outcomes = Vec::with_capacity(candidates.len());
    for candidate in candidates {
        let mut input = CreateInput {
            slug: candidate.slug.clone(),
            destination_url: candidate.destination_url.clone(),
            title: candidate.title.clone(),
            notes: candidate.notes.clone(),
            status: Some(candidate.status),
            redirect_status: candidate.redirect_status,
            expires_at: candidate.expires_at,
        };
        link::validate_create(&mut input).map_err(|error| {
            DomainError::wrapped(format!("validate import {}", candidate.slug), error)
        })?;
        let slug = input.slug.clone();
        let lookup = transaction.query_row(
            &format!("{SELECT_COLUMNS} WHERE slug = ?1"),
            params![slug.clone()],
            scan_link,
        );
        match lookup {
            Err(rusqlite::Error::QueryReturnedNoRows) => {
                transaction
                    .execute(
                        "INSERT INTO links(slug,destination_url,title,notes,status,redirect_status,expires_at,visit_count,last_visited_at,created_at,updated_at) \
                         VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11)",
                        params![
                            slug,
                            input.destination_url,
                            input.title,
                            input.notes,
                            input.status.unwrap_or(Status::Active).as_str(),
                            input.redirect_status,
                            input.expires_at.map(|value| value.timestamp_millis()),
                            candidate.visit_count,
                            candidate.last_visited_at.map(|value| value.timestamp_millis()),
                            candidate.created_at.timestamp_millis(),
                            candidate.updated_at.timestamp_millis()
                        ],
                    )
                    .map_err(|error| {
                        DomainError::wrapped(format!("import insert {}", candidate.slug), error)
                    })?;
                outcomes.push(ImportOutcome::Inserted);
            }
            Err(error) => {
                return Err(DomainError::wrapped(
                    format!("import lookup {}", candidate.slug),
                    error,
                ))
            }
            Ok(existing) if logical_equal(&existing, candidate) => {
                outcomes.push(ImportOutcome::Identical);
            }
            Ok(_) if policy == "skip" => outcomes.push(ImportOutcome::Skipped),
            Ok(_) if policy == "replace" => {
                transaction
                    .execute(
                        "UPDATE links SET destination_url=?1,title=?2,notes=?3,status=?4,redirect_status=?5,expires_at=?6,visit_count=?7,last_visited_at=?8,created_at=?9,updated_at=?10 \
                         WHERE slug=?11",
                        params![
                            input.destination_url,
                            input.title,
                            input.notes,
                            input.status.unwrap_or(Status::Active).as_str(),
                            input.redirect_status,
                            input.expires_at.map(|value| value.timestamp_millis()),
                            candidate.visit_count,
                            candidate.last_visited_at.map(|value| value.timestamp_millis()),
                            candidate.created_at.timestamp_millis(),
                            candidate.updated_at.timestamp_millis(),
                            slug
                        ],
                    )
                    .map_err(|error| {
                        DomainError::wrapped(format!("import replace {}", candidate.slug), error)
                    })?;
                outcomes.push(ImportOutcome::Replaced);
            }
            Ok(_) => {
                return Err(DomainError::ConflictNamed {
                    slug: candidate.slug.clone(),
                })
            }
        }
    }
    Ok(outcomes)
}

/// The field-by-field comparison the import uses to classify a record.
#[must_use]
pub fn logical_equal(a: &Link, b: &Link) -> bool {
    a.slug == b.slug
        && a.destination_url == b.destination_url
        && a.title == b.title
        && a.notes == b.notes
        && a.status == b.status
        && a.redirect_status == b.redirect_status
        && a.expires_at == b.expires_at
        && a.visit_count == b.visit_count
        && a.last_visited_at == b.last_visited_at
        && a.created_at == b.created_at
        && a.updated_at == b.updated_at
}

/// The canonical field set hashed by the KV importer, ordered for stability.
#[must_use]
pub fn checksum_fields(link: &Link) -> BTreeMap<String, String> {
    let mut fields = BTreeMap::new();
    fields.insert("slug".to_string(), link.slug.clone());
    fields.insert("url".to_string(), link.destination_url.clone());
    fields.insert("title".to_string(), link.title.clone());
    fields.insert("notes".to_string(), link.notes.clone());
    fields.insert("status".to_string(), link.status.as_str().to_string());
    fields.insert("redirect".to_string(), link.redirect_status.to_string());
    fields.insert(
        "expires".to_string(),
        link.expires_at
            .map(|value| value.timestamp_millis().to_string())
            .unwrap_or_default(),
    );
    fields.insert(
        "lastVisited".to_string(),
        link.last_visited_at
            .map(|value| value.timestamp_millis().to_string())
            .unwrap_or_default(),
    );
    fields.insert("visits".to_string(), link.visit_count.to_string());
    fields.insert(
        "created".to_string(),
        link.created_at.timestamp_millis().to_string(),
    );
    fields.insert(
        "updated".to_string(),
        link.updated_at.timestamp_millis().to_string(),
    );
    fields
}

/// `syscall.Errno.Error()` as Go's `os` package printed it inside an
/// `fs.PathError`: the message Go showed the operator, which the backup program
/// echoed verbatim to standard error.
struct GoErrno<'a>(&'a std::io::Error);

impl std::fmt::Display for GoErrno<'_> {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let message = match self.0.kind() {
            std::io::ErrorKind::PermissionDenied => "permission denied",
            std::io::ErrorKind::NotFound => "no such file or directory",
            std::io::ErrorKind::NotADirectory => "not a directory",
            std::io::ErrorKind::AlreadyExists => "file exists",
            std::io::ErrorKind::InvalidInput => "invalid argument",
            std::io::ErrorKind::IsADirectory => "is a directory",
            std::io::ErrorKind::StorageFull => "no space left on device",
            _ => {
                return match self.0.raw_os_error() {
                    Some(code) => write!(formatter, "os error {code}"),
                    None => write!(formatter, "{}", self.0),
                };
            }
        };
        formatter.write_str(message)
    }
}
