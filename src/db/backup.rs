//! The database file itself: size, backup, vacuum and restore.

use super::*;

/// Points SQLite's temporary files -- the copy `VACUUM` rebuilds the database
/// into, and any sort too large for memory -- at the directory holding the
/// database. Every deployment keeps that directory writable and sized for the
/// database (the unit's `ReadWritePaths`, the image's /data), and the backup and
/// restore scratch files already go there.
///
/// SQLite otherwise tries $SQLITE_TMPDIR, $TMPDIR, /var/tmp, /usr/tmp, /tmp and
/// the working directory. The Docker image is built from scratch and has none of
/// them writable. The rebuilt copy stays in a page cache sized like the main
/// database's 8 MiB and needs a file only beyond it, so `VACUUM` would succeed
/// on a database compacting to 6.3 MB and fail on one compacting to 9 MB, with
/// "unable to determine a suitable directory for temporary files". SQLite
/// unlinks each file as it opens it, so none remain there.
///
/// Process-wide and not thread-safe, so it is called once, before any
/// connection is opened. A bare file name keeps SQLite's own search, which ends
/// at the working directory holding it.
pub fn temp_files_beside(database: &str) -> Result<()> {
    let Some(dir) = std::path::Path::new(database).parent().filter(|d| !d.as_os_str().is_empty()) else {
        return Ok(());
    };
    let dir = dir.to_string_lossy().replace('\'', "''");
    Connection::open_in_memory()?.execute_batch(&format!("PRAGMA temp_store_directory = '{dir}'"))?;
    Ok(())
}

/// Restricts the database to its owner.
///
/// It is the credential store: node tokens in the clear, the GitHub client
/// secret, the password hash. SQLite creates it under the umask, which at a
/// default 022 is world-readable, and the WAL and shm files hold the same rows.
///
/// Best effort: a filesystem without Unix modes still works.
pub(super) fn restrict(path: &str) {
    for file in [path.to_owned(), format!("{path}-wal"), format!("{path}-shm")] {
        own_only(&file);
    }
}

/// One file, owner-only.
fn own_only(file: &str) {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(file, std::fs::Permissions::from_mode(0o600));
    }
}

/// Opens a read-only connection to the database: [`Db`]'s reader, and the one
/// [`Db::backup_into`] exports through. Read-only from the open, so no statement
/// reaching it can write. It inherits none of the PRAGMAs in `SCHEMA`, so the
/// busy timeout is set again; without it, a checkpoint racing a read would
/// return SQLITE_BUSY immediately.
pub(super) fn read_only(file: &str) -> Result<Connection> {
    let conn = Connection::open_with_flags(
        file,
        OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )?;
    conn.busy_timeout(std::time::Duration::from_secs(5))?;
    Ok(conn)
}

/// The `main` database's path as SQLite reports it, empty for `:memory:`.
/// Queried rather than cached so there is a single answer to which file is
/// open.
pub(super) fn main_file(conn: &Connection) -> String {
    conn.query_row("PRAGMA database_list", [], |r| r.get(2)).unwrap_or_default()
}

pub(super) fn bytes_of(file: &str) -> i64 {
    std::fs::metadata(file).map(|m| m.len() as i64).unwrap_or(0)
}

/// Bytes the database occupies. The WAL is included: committed rows remain there
/// until a checkpoint folds them into the main file, so the two together are what
/// an operator sees on disk.
pub(super) fn on_disk(file: &str) -> i64 {
    bytes_of(file) + bytes_of(&format!("{file}-wal"))
}

impl Db {
    // ---- the database file itself ----

    /// The file this connection is open on, empty for `:memory:`.
    pub fn file(&self) -> String {
        main_file(&self.conn())
    }

    /// The retention window used by both `prune` and the data page. Stored as
    /// text by the settings form, so a missing or unparsable value falls back to
    /// the default rather than erroring.
    pub fn retention_days(&self) -> i64 {
        self.get("retention_days")
            .and_then(|v| v.parse::<i64>().ok())
            .unwrap_or(DEFAULT_RETENTION_DAYS)
            .clamp(1, MAX_RETENTION_DAYS)
    }

    /// What the panel's data page reads: how much space the file occupies, how
    /// much of that is free pages awaiting a `VACUUM`, and how far back the
    /// history actually reaches.
    ///
    /// `oldest` against `retention` is the one pair here that can indicate a
    /// fault: history older than the window means `prune` has not been running.
    pub fn stats(&self) -> Result<serde_json::Value> {
        // Before acquiring the connection, which for `:memory:` is the writer
        // that `retention_days` acquires as well.
        let retention = self.retention_days();
        // A read-only connection of its own: the counts scan every row of both
        // tiers of history, which on a cold cache means reading most of the
        // file -- 13 s for 52 MB at 4 MB/s. Through the writer, agent reports
        // would wait out that read; through the charts' reader, so would the
        // public page's history charts.
        let (own, writer);
        let conn: &Connection = if self.reader.is_some() {
            own = read_only(&self.file())?;
            &own
        } else {
            writer = self.conn();
            &writer
        };
        let file = main_file(conn);
        let page_size: i64 = conn.query_row("PRAGMA page_size", [], |r| r.get(0))?;
        let free_pages: i64 = conn.query_row("PRAGMA freelist_count", [], |r| r.get(0))?;
        // Across both tiers: history begins at whichever row is earliest, and
        // a minute row can predate the hourly tier while a rollup catches up.
        let oldest = oldest(conn, &["metric_hour", "ping_hour", "metric", "ping_record"])?;
        let mut rows = serde_json::Map::new();
        for table in TABLES {
            let n: i64 = conn.query_row(&format!("SELECT COUNT(*) FROM {table}"), [], |r| r.get(0))?;
            rows.insert(table.to_owned(), serde_json::json!(n));
        }
        rows.insert(
            traffic_period::TABLE.to_owned(),
            serde_json::json!(
                conn.query_row("SELECT COUNT(*) FROM fork_traffic_period", [], |r| r.get::<_, i64>(0))?
            ),
        );
        Ok(serde_json::json!({
            "path": file,
            "size": bytes_of(&file),
            "wal": bytes_of(&format!("{file}-wal")),
            "free": free_pages * page_size,
            "oldest": oldest,
            // What the panel shows, counted on the hub's clock: from the browser's,
            // a clock eight hours off would shift the span by up to a day.
            "oldest_ago": oldest.map(|t| (Utc::now().timestamp() - t).max(0)),
            "retention": retention,
            "rows": rows,
        }))
    }

    /// Writes a consistent copy of the live database to `dest`, which must not
    /// already exist.
    ///
    /// `VACUUM INTO` is SQLite's own mechanism for this: one statement, a single
    /// read transaction, and a compacted copy with free pages already dropped. It
    /// reads the whole file, so the caller runs it off the runtime -- every other
    /// statement here is sub-millisecond, this one is not.
    pub fn backup_into(&self, dest: &str) -> Result<()> {
        // The copy is the credential store in one portable file: node tokens in
        // the clear, the GitHub secret, the password hash. SQLite would create
        // it under the umask, world-readable at the usual 022 for as long as the
        // copy takes; it writes into an empty file that already exists instead.
        let mut empty = std::fs::OpenOptions::new();
        empty.write(true).create_new(true);
        #[cfg(unix)]
        std::os::unix::fs::OpenOptionsExt::mode(&mut empty, 0o600);
        empty.open(dest)?;
        // A connection of its own. `VACUUM INTO` only reads, and WAL allows it to
        // read a consistent snapshot while the agents continue writing through
        // the first -- exporting is the one heavy operation here that need not
        // block them. Not the charts' reader, which it would hold throughout.
        read_only(&self.file())?.execute("VACUUM INTO ?1", [dest])?;
        Ok(())
    }

    /// Rebuilds the file, reclaiming the pages deleted history left behind.
    /// Returns the bytes recovered.
    ///
    /// SQLite's constraints on `VACUUM`, and why they hold here: it cannot run
    /// inside a transaction or with a live statement on the connection (there is
    /// one connection, and this call owns it); it requires free disk of about
    /// twice the compacted database -- the temporary copy, then the same pages
    /// again in the WAL -- and a failure rolls back leaving the original
    /// untouched; and it can renumber rowids, which nothing here keys on, since
    /// `metric` and `ping_record` are WITHOUT ROWID and every other table
    /// declares its own primary key.
    ///
    /// In WAL mode the rewrite lands in the WAL first, so without the checkpoint
    /// the file on disk grows rather than shrinking.
    pub fn vacuum(&self) -> Result<i64> {
        let conn = self.conn();
        let file = main_file(&conn);
        let before = on_disk(&file);
        conn.execute_batch("VACUUM")?;
        // Best effort: the space is already reclaimed within the database, and a
        // checkpoint that cannot run now does not constitute a failed vacuum.
        let _ = conn.query_row("PRAGMA wal_checkpoint(TRUNCATE)", [], |_| Ok(()));
        Ok((before - on_disk(&file)).max(0))
    }

    /// What a file must satisfy before a single page of it is copied over the
    /// live database. Restore is the one operation here that destroys data, and
    /// the file behind it originates from a disk this hub knows nothing about.
    ///
    /// **Writes to `src`.** The migrations an older backup requires run here, on
    /// the upload, rather than after copying: everything that can fail does so
    /// while the live database is still untouched. The caller owns that file and
    /// deletes it in either case.
    pub fn check_backup(&self, src: &str) -> Result<()> {
        let page_size = self.conn().query_row("PRAGMA page_size", [], |r| r.get(0))?;
        Self::check_backup_candidate(src, Some(page_size))?;
        Ok(())
    }

    /// Validates and sanitizes a private, offline staging copy before installation.
    pub fn prepare_offline_restore(src: &str) -> Result<()> {
        let candidate = Self::check_backup_candidate(src, None)?;
        put_setting(&candidate, "origin_cdn_only", "off")
    }

    fn check_backup_candidate(src: &str, page_size: Option<i64>) -> Result<Connection> {
        const NOT_A_BACKUP: &str = "这不是 hub 导出的备份文件";
        // Read-write rather than read-only: a plain copy of a running hub's
        // database is in WAL mode, and SQLite cannot open such a file read-only
        // without its -shm companion.
        let candidate = Connection::open(src)?;
        let health: String = candidate
            .query_row("PRAGMA integrity_check", [], |r| r.get(0))
            .context(crate::Shown(NOT_A_BACKUP.into()))?;
        if health != "ok" {
            info!("uploaded backup fails integrity_check: {health}");
            refuse!("备份文件已损坏，数据库完整性检查没有通过");
        }
        // Pages are copied verbatim, so whatever schema the file carries becomes
        // the schema this hub runs its statements against. A view or trigger
        // where a table belongs would route every subsequent write through
        // externally supplied code.
        let plotted: i64 = candidate.query_row(
            "SELECT COUNT(*) FROM sqlite_master WHERE type IN ('view', 'trigger')",
            [],
            |r| r.get(0),
        )?;
        if plotted > 0 {
            refuse!("文件里有视图或触发器，不是 hub 导出的备份");
        }
        // The hourly tables are created by the migration below; their columns are
        // compared with the rest once it has run.
        for table in TABLES.iter().filter(|t| !HOURLY_TABLES.contains(t)) {
            let found: i64 = candidate.query_row(
                "SELECT COUNT(*) FROM sqlite_master WHERE type='table' AND name=?1",
                [table],
                |r| r.get(0),
            )?;
            if found == 0 {
                refuse!("{NOT_A_BACKUP}：缺少 {table} 表");
            }
        }
        let version: i64 = candidate.query_row("PRAGMA user_version", [], |r| r.get(0))?;
        if version > SCHEMA_VERSION {
            refuse!("备份来自更新版本的 hub（数据库版本 {version}，这台只认到 {SCHEMA_VERSION}），先升级 hub 再恢复");
        }
        // The online backup API refuses a page size change while the destination
        // is in WAL mode; an explicit message is clearer than SQLITE_READONLY.
        if let Some(ours) = page_size {
            let theirs: i64 = candidate.query_row("PRAGMA page_size", [], |r| r.get(0))?;
            if theirs != ours {
                refuse!("备份的页大小是 {theirs} 字节，这台 hub 是 {ours} 字节，无法恢复");
            }
        }
        // Brought up to this build's schema here, on the upload. Run after the
        // copy instead, a failed migration would leave the hub on a database it
        // could not use while reporting a failure to the panel -- the one
        // arrangement in which the restore has failed and the original data is
        // also gone.
        migrate(&candidate, version)?;
        // Table names are not a schema. Pages are copied verbatim, so the columns
        // the file carries become the ones this hub's statements run against, and
        // eight correctly named tables holding the wrong columns pass every gate
        // above while leaving the database unusable.
        //
        // Compared against a database this build creates for itself, so there is
        // no second column list to keep in step with `SCHEMA`. Names are compared
        // as sets rather than as stored DDL: a migrated old backup reaches the
        // same columns through `ALTER TABLE`, whose text never matches a fresh
        // `CREATE TABLE`. Extra columns are ignored.
        let reference = Connection::open_in_memory()?;
        reference.execute_batch(SCHEMA)?;
        migrate(&reference, SCHEMA_VERSION)?;
        for table in TABLES {
            let want = columns_of(&reference, table)?;
            let got = columns_of(&candidate, table)?;
            let mut missing: Vec<&str> = want.difference(&got).map(String::as_str).collect();
            if !missing.is_empty() {
                missing.sort_unstable();
                refuse!("{NOT_A_BACKUP}：{table} 表缺少字段 {}", missing.join("、"));
            }
        }
        traffic_period::ensure(&candidate)?;
        crate::management::reset_after_restore(&candidate)?;
        // A restored session must never become live even if the request is cancelled
        // between copying the candidate and issuing the caller a fresh session.
        candidate.execute("DELETE FROM session", [])?;
        candidate.execute("DELETE FROM setting WHERE key='hub_lifecycle_maintenance'", [])?;
        // The extension may have just been created on an old WAL backup.
        let _ = candidate.query_row("PRAGMA wal_checkpoint(TRUNCATE)", [], |_| Ok(()));
        Ok(candidate)
    }

    /// Copies a checked backup over the live database page by page through
    /// SQLite's online backup API: the destination retains its file, permissions
    /// and journal mode, and a partial failure rolls back rather than leaving
    /// half a database behind.
    ///
    /// Call [`Db::check_backup`] first, as it is what brings `src` to this
    /// build's schema; the copy is then the last step that can fail. Like the
    /// other two, this reads and writes the whole file and belongs off the
    /// runtime.
    ///
    /// The live file's id high marks outlast the copy: an id issued after the
    /// backup was taken may still key the hub's memory -- the offline sweep, the
    /// country lookup -- and an older mark would issue it again.
    pub fn restore_authenticated(&self, src: &str, caller: &str, replacement: (&str, i64)) -> Result<bool> {
        let mut conn = self.conn();
        if !live_session(&conn, caller)? {
            return Ok(false);
        }
        // Write the replacement to the private candidate before copying it.
        // A failure here leaves the live database alone, and the copy never
        // exposes a restored database between session invalidation and issuance.
        {
            let mut candidate = Connection::open(src)?;
            let tx = candidate.transaction()?;
            // The operator must confirm how this hub is reachable again.
            put_setting(&tx, "origin_cdn_only", "off")?;
            replace_sessions(&tx, replacement)?;
            tx.commit()?;
        }
        Self::restore_copy(&mut conn, src)?;
        Ok(true)
    }

    #[cfg(test)]
    pub fn restore_from(&self, src: &str) -> Result<()> {
        Self::restore_copy(&mut self.conn(), src)
    }

    fn restore_copy(conn: &mut Connection, src: &str) -> Result<()> {
        // This check shares the live writer lock with poll, submit and maintenance.
        let active: bool = conn.query_row(
            "SELECT EXISTS(SELECT 1 FROM managed_operation WHERE status='delivered') OR EXISTS(SELECT 1 FROM managed_maintenance WHERE expires>?1) OR EXISTS(SELECT 1 FROM managed_lifecycle WHERE status IN ('running','paused','cleaning','finalizing'))",
            [Utc::now().timestamp()], |r|r.get(0))?;
        if active {
            refuse!("节点仍有已发送任务或正在维护，请完成恢复后再恢复后台备份");
        }

        let live = ID_TABLES.map(|table| issued(conn, table));
        conn.restore(rusqlite::MAIN_DB, src, None::<fn(rusqlite::backup::Progress)>)?;
        for (table, live) in ID_TABLES.into_iter().zip(live) {
            let kept = live.and_then(|live| {
                let id = issued(conn, table)?.max(live);
                put_setting(conn, &format!("{table}_id_issued"), &id.to_string())
            });
            if let Err(e) = kept {
                warn!("keeping the {table} id high mark across the restore failed: {e:#}");
            }
        }
        Ok(())
    }
}
