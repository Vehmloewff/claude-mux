//! The daemon's state database (`<home>/state.db`), backed by Turso.
//!
//! Schema changes are embedded migrations, applied in order when the daemon
//! opens the database. Each applied migration is recorded by name in
//! `_applied_migrations`, so a migration runs at most once; never edit or
//! reorder one that has shipped — add a new one instead.

use std::path::Path;

use anyhow::{Context, Result, anyhow};
use include_dir::{Dir, include_dir};
use turso::Value;

use crate::paths;
use crate::protocol::{SessionInfo, Status};

/// Every `migrations/*.sql` file, applied in file-name order. A migration's
/// name is its file stem.
static MIGRATIONS: Dir = include_dir!("$CARGO_MANIFEST_DIR/migrations");

/// `(name, sql)` for each embedded migration, in the order to apply them.
fn migrations() -> Vec<(&'static str, &'static str)> {
    let mut migrations: Vec<_> = MIGRATIONS
        .files()
        .filter(|f| f.path().extension().is_some_and(|e| e == "sql"))
        .map(|f| {
            let name = f.path().file_stem().and_then(|s| s.to_str()).expect("UTF-8 migration name");
            let sql = f.contents_utf8().expect("UTF-8 migration");
            (name, sql)
        })
        .collect();
    migrations.sort_by_key(|&(name, _)| name);
    migrations
}

const SESSION_COLUMNS: &str = "id, description, cwd, prompt, status, status_detail, title, \
    created_at, updated_at, exit_code, turns, last_message, \
    claude_args, auto_trust, background, asleep, continued_as";

pub struct Db {
    conn: turso::Connection,
    _db: turso::Database,
}

impl Db {
    /// Open (creating if needed) the database at `path` and apply any
    /// pending migrations.
    pub async fn open(path: &Path) -> Result<Db> {
        let path_str = path.to_str().ok_or_else(|| anyhow!("non-UTF-8 database path {}", path.display()))?;
        let db = turso::Builder::new_local(path_str)
            .build()
            .await
            .with_context(|| format!("open {}", path.display()))?;
        let conn = db.connect()?;
        let db = Db { conn, _db: db };
        db.migrate().await.context("apply database migrations")?;
        Ok(db)
    }

    async fn migrate(&self) -> Result<()> {
        self.conn
                .execute(
                    "CREATE TABLE IF NOT EXISTS _applied_migrations (
                        name TEXT PRIMARY KEY NOT NULL,
                        applied_at INTEGER NOT NULL
                    )",
                    (),
                )
                .await?;
            let mut applied = Vec::new();
            let mut rows = self.conn.query("SELECT name FROM _applied_migrations", ()).await?;
            while let Some(row) = rows.next().await? {
                applied.push(row.get::<String>(0)?);
            }
            drop(rows);

            for (name, sql) in migrations() {
                if applied.iter().any(|a| a == name) {
                    continue;
                }
                self.conn.execute("BEGIN", ()).await?;
                let result = async {
                    self.conn.execute_batch(sql).await?;
                    self.conn
                        .execute(
                            "INSERT INTO _applied_migrations (name, applied_at) VALUES (?1, ?2)",
                            (name, paths::now() as i64),
                        )
                        .await?;
                    self.conn.execute("COMMIT", ()).await?;
                    anyhow::Ok(())
                }
                .await;
                if let Err(e) = result {
                    let _ = self.conn.execute("ROLLBACK", ()).await;
                    return Err(e.context(format!("migration {name}")));
                }
                eprintln!("[{}] applied migration {name}", paths::now());
            }
            Ok(())
    }

    pub async fn load_sessions(&self) -> Result<Vec<SessionInfo>> {
        let mut rows = self
                .conn
                .query(format!("SELECT {SESSION_COLUMNS} FROM sessions ORDER BY id"), ())
                .await?;
            let mut infos = Vec::new();
            while let Some(row) = rows.next().await? {
                let id: String = row.get(0)?;
                let info = session_from_row(&row).with_context(|| format!("session {id}"))?;
                infos.push(info);
            }
            Ok(infos)
    }

    /// Replace the stored sessions with `infos`, atomically.
    pub async fn save_sessions<'a>(&self, infos: impl IntoIterator<Item = &'a SessionInfo>) -> Result<()> {
        let insert = format!(
            "INSERT INTO sessions ({SESSION_COLUMNS}) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16, ?17)"
        );
        let rows = infos.into_iter().map(session_params).collect::<Result<Vec<_>>>()?;
        self.conn.execute("BEGIN", ()).await?;
            let result = async {
                self.conn.execute("DELETE FROM sessions", ()).await?;
                for params in rows {
                    self.conn.execute(&insert, params).await?;
                }
                self.conn.execute("COMMIT", ()).await?;
                anyhow::Ok(())
            }
            .await;
            if result.is_err() {
                let _ = self.conn.execute("ROLLBACK", ()).await;
            }
            result
    }
}

fn text(s: &str) -> Value {
    Value::Text(s.to_string())
}

fn opt_text(s: &Option<String>) -> Value {
    s.as_deref().map_or(Value::Null, text)
}

/// A status as stored: its serde (snake_case) name.
fn status_name(status: Status) -> Result<Value> {
    match serde_json::to_value(status)? {
        serde_json::Value::String(name) => Ok(Value::Text(name)),
        other => Err(anyhow!("unexpected status encoding {other}")),
    }
}

fn session_params(info: &SessionInfo) -> Result<Vec<Value>> {
    Ok(vec![
        text(&info.id),
        opt_text(&info.description),
        text(&info.cwd),
        text(&info.prompt),
        status_name(info.status)?,
        opt_text(&info.status_detail),
        opt_text(&info.title),
        Value::Integer(info.created_at as i64),
        Value::Integer(info.updated_at as i64),
        info.exit_code.map_or(Value::Null, |c| Value::Integer(c.into())),
        Value::Integer(info.turns as i64),
        opt_text(&info.last_message),
        Value::Text(serde_json::to_string(&info.claude_args)?),
        Value::Integer(info.auto_trust.into()),
        Value::Text(serde_json::to_string(&info.background)?),
        Value::Integer(info.asleep.into()),
        opt_text(&info.continued_as),
    ])
}

fn session_from_row(row: &turso::Row) -> Result<SessionInfo> {
    let status: String = row.get(4)?;
    Ok(SessionInfo {
        id: row.get(0)?,
        description: row.get(1)?,
        cwd: row.get(2)?,
        prompt: row.get(3)?,
        status: serde_json::from_value(serde_json::Value::String(status.clone()))
            .map_err(|_| anyhow!("unknown status '{status}'"))?,
        status_detail: row.get(5)?,
        title: row.get(6)?,
        created_at: row.get::<i64>(7)? as u64,
        updated_at: row.get::<i64>(8)? as u64,
        exit_code: row.get::<Option<i64>>(9)?.map(|c| c as i32),
        queued: 0,
        turns: row.get::<i64>(10)? as u64,
        last_message: row.get(11)?,
        claude_args: serde_json::from_str(&row.get::<String>(12)?)?,
        auto_trust: row.get::<i64>(13)? != 0,
        background: serde_json::from_str(&row.get::<String>(14)?)?,
        asleep: row.get::<i64>(15)? != 0,
        continued_as: row.get(16)?,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::BackgroundTask;

    fn temp_db_path(name: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("claude-mux-db-test-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir.join("state.db")
    }

    fn sample(id: &str) -> SessionInfo {
        SessionInfo {
            id: id.into(),
            description: Some("demo".into()),
            cwd: "/tmp".into(),
            prompt: "do the thing".into(),
            status: Status::NeedsInput,
            status_detail: None,
            title: Some("title".into()),
            created_at: 10,
            updated_at: 20,
            exit_code: Some(-1),
            queued: 3,
            turns: 4,
            last_message: None,
            claude_args: vec!["--model".into(), "opus".into()],
            auto_trust: true,
            background: vec![BackgroundTask { id: "b1".into(), kind: "shell".into(), description: "npm test".into() }],
            asleep: true,
            continued_as: Some("c3".into()),
        }
    }

    #[tokio::test]
    async fn migrates_once_and_round_trips_sessions() {
        let path = temp_db_path("roundtrip");
        {
            let db = Db::open(&path).await.unwrap();
            db.save_sessions(&[sample("a1"), sample("b2")]).await.unwrap();
            db.save_sessions(&[sample("b2")]).await.unwrap();
        }
        // Reopening must not re-run migrations (CREATE TABLE would fail).
        let db = Db::open(&path).await.unwrap();
        let infos = db.load_sessions().await.unwrap();
        assert_eq!(infos.len(), 1);
        let s = &infos[0];
        assert_eq!(s.id, "b2");
        assert_eq!(s.status, Status::NeedsInput);
        assert_eq!(s.exit_code, Some(-1));
        assert_eq!(s.queued, 0);
        assert_eq!(s.claude_args, ["--model", "opus"]);
        assert!(s.auto_trust);
        assert!(s.asleep);
        assert_eq!(s.continued_as.as_deref(), Some("c3"));
        assert_eq!(s.background[0].description, "npm test");
        let _ = std::fs::remove_dir_all(path.parent().unwrap());
    }
}
