use super::{DATA_FORMAT, PROTOCOL, RUNTIME_ABI, version::Version};
use crate::error::{Fault, Result};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use sqlx::{Connection, SqliteConnection, sqlite::SqliteConnectOptions};
use std::{
    io::{Read, Write},
    path::{Path, PathBuf},
    time::Duration,
};
use tokio::io::AsyncReadExt;

#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Installed {
    pub version: String,
    pub sha256: String,
    pub size: u64,
    pub tag: Option<String>,
}
#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Registry {
    pub schema_version: u32,
    pub active: Installed,
    pub history: Vec<Installed>,
}
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Transaction {
    pub old: Installed,
    pub target: Installed,
    pub backup: String,
}
pub fn protected_dir(path: &Path) -> Result<()> {
    std::fs::create_dir_all(path).map_err(|_| Fault::internal())?;
    if std::fs::symlink_metadata(path)
        .map_err(|_| Fault::internal())?
        .file_type()
        .is_symlink()
    {
        return Err(Fault::internal());
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700))
            .map_err(|_| Fault::internal())?;
    }
    Ok(())
}
pub fn atomic_json(path: &Path, value: &impl Serialize) -> Result<()> {
    let temp = path.with_extension(format!("{}.tmp", uuid::Uuid::new_v4()));
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options.open(&temp).map_err(|_| Fault::internal())?;
    file.write_all(&serde_json::to_vec(value).map_err(|_| Fault::internal())?)
        .and_then(|_| file.sync_all())
        .map_err(|_| Fault::internal())?;
    std::fs::rename(&temp, path).map_err(|_| Fault::internal())?;
    #[cfg(unix)]
    {
        std::fs::File::open(path.parent().ok_or_else(Fault::internal)?)
            .and_then(|f| f.sync_all())
            .map_err(|_| Fault::internal())?;
    }
    Ok(())
}
pub fn read_json<T: serde::de::DeserializeOwned>(path: &Path, limit: u64) -> Result<T> {
    let meta = std::fs::symlink_metadata(path).map_err(|_| Fault::internal())?;
    if !meta.is_file() || meta.file_type().is_symlink() || meta.len() > limit {
        return Err(Fault::internal());
    }
    serde_json::from_slice(&std::fs::read(path).map_err(|_| Fault::internal())?)
        .map_err(|_| Fault::internal())
}
pub fn digest(path: &Path) -> Result<(String, u64)> {
    let mut file = std::fs::File::open(path).map_err(|_| Fault::internal())?;
    let mut hash = Sha256::new();
    let mut len = 0;
    let mut buffer = [0; 65536];
    loop {
        let n = file.read(&mut buffer).map_err(|_| Fault::internal())?;
        if n == 0 {
            break;
        }
        hash.update(&buffer[..n]);
        len += n as u64;
    }
    Ok((hex::encode(hash.finalize()), len))
}
impl Installed {
    pub fn validate(&self) -> Result<()> {
        if Version::parse(&self.version).is_none()
            || self.sha256.len() != 64
            || !self
                .sha256
                .bytes()
                .all(|v| v.is_ascii_digit() || (b'a'..=b'f').contains(&v))
            || self.size == 0
        {
            return Err(Fault::internal());
        }
        Ok(())
    }
    pub fn path(&self, root: &Path) -> Result<PathBuf> {
        self.validate()?;
        Ok(root.join("versions").join(&self.sha256).join("claud2api"))
    }
    pub fn verify(&self, root: &Path) -> Result<PathBuf> {
        let path = self.path(root)?;
        if std::fs::symlink_metadata(&path)
            .map_err(|_| Fault::internal())?
            .file_type()
            .is_symlink()
            || digest(&path)? != (self.sha256.clone(), self.size)
        {
            return Err(Fault::new(
                422,
                "api_error",
                "installed release checksum failed",
            ));
        }
        Ok(path)
    }
}
impl Registry {
    pub fn load(root: &Path) -> Result<Self> {
        let path = root.join("state.json");
        if path.exists() {
            let result: Self = read_json(&path, 16384)?;
            if result.schema_version != 1 || result.history.len() > 2 {
                return Err(Fault::internal());
            }
            result.active.validate()?;
            for item in &result.history {
                item.validate()?;
            }
            return Ok(result);
        }
        let executable = std::env::current_exe().map_err(|_| Fault::internal())?;
        let (sha256, size) = digest(&executable)?;
        let active = Installed {
            version: crate::VERSION.into(),
            sha256,
            size,
            tag: None,
        };
        let target = active.path(root)?;
        protected_dir(target.parent().ok_or_else(Fault::internal)?)?;
        sync_parent(target.parent().ok_or_else(Fault::internal)?)?;
        if !target.exists() {
            let temp = target.with_extension(format!("{}-copy", uuid::Uuid::new_v4()));
            std::fs::copy(&executable, &temp).map_err(|_| Fault::internal())?;
            executable_permissions(&temp)?;
            std::fs::File::open(&temp)
                .and_then(|f| f.sync_all())
                .map_err(|_| Fault::internal())?;
            std::fs::rename(&temp, &target).map_err(|_| Fault::internal())?;
            sync_parent(&target)?;
        }
        active.verify(root)?;
        let result = Self {
            schema_version: 1,
            active,
            history: vec![],
        };
        result.save(root)?;
        Ok(result)
    }
    pub fn save(&self, root: &Path) -> Result<()> {
        atomic_json(&root.join("state.json"), self)
    }
    pub fn activate(&self, target: Installed) -> Self {
        let mut history = vec![self.active.clone()];
        history.extend(self.history.clone());
        history.retain(|v| v.sha256 != target.sha256);
        let mut seen = std::collections::HashSet::new();
        history.retain(|v| seen.insert(v.sha256.clone()));
        history.truncate(2);
        Self {
            schema_version: 1,
            active: target,
            history,
        }
    }
}
pub fn executable_permissions(path: &Path) -> Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700))
            .map_err(|_| Fault::internal())?;
    }
    Ok(())
}
pub fn sync_parent(path: &Path) -> Result<()> {
    #[cfg(unix)]
    std::fs::File::open(path.parent().ok_or_else(Fault::internal)?)
        .and_then(|f| f.sync_all())
        .map_err(|_| Fault::internal())?;
    Ok(())
}
pub async fn preflight(path: &Path, version: &str) -> Result<()> {
    let mut header = [0; 20];
    std::fs::File::open(path)
        .and_then(|mut f| f.read_exact(&mut header))
        .map_err(|_| Fault::new(422, "api_error", "release binary header is invalid"))?;
    if &header[..4] != b"\x7fELF"
        || header[4] != 2
        || header[5] != 1
        || header[18] != 62
        || header[19] != 0
    {
        return Err(Fault::new(
            422,
            "api_error",
            "release is not a Linux AMD64 executable",
        ));
    }
    let mut child = tokio::process::Command::new(path)
        .arg("--describe-build")
        .env_clear()
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .kill_on_drop(true)
        .spawn()
        .map_err(|_| Fault::new(422, "api_error", "release cannot execute on this runtime"))?;
    let mut output = child.stdout.take().ok_or_else(Fault::internal)?.take(8193);
    tokio::time::timeout(Duration::from_secs(5), async {
        let mut bytes = vec![];
        output
            .read_to_end(&mut bytes)
            .await
            .map_err(|_| Fault::upstream())?;
        if bytes.len() > 8192 || !child.wait().await.map_err(|_| Fault::upstream())?.success() {
            return Err(Fault::new(422, "api_error", "release preflight failed"));
        }
        let value: serde_json::Value = serde_json::from_slice(&bytes).map_err(|_| {
            Fault::new(422, "api_error", "release does not support managed updates")
        })?;
        if value["version"] != version
            || value["protocol"] != PROTOCOL
            || value["data_format"] != DATA_FORMAT
            || value["runtime_abi"] != RUNTIME_ABI
        {
            return Err(Fault::new(
                409,
                "api_error",
                "release executable compatibility check failed",
            ));
        }
        Ok(())
    })
    .await
    .map_err(|_| Fault::timeout())?
}
pub async fn database_lock(data: &Path) -> Result<fslock::LockFile> {
    let mut lock =
        fslock::LockFile::open(&data.join("instance.lock")).map_err(|_| Fault::internal())?;
    for _ in 0..300 {
        if lock.try_lock().map_err(|_| Fault::internal())? {
            return Ok(lock);
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    Err(Fault::new(
        409,
        "api_error",
        "previous worker has not released the data directory",
    ))
}
pub async fn backup(data: &Path, path: &Path) -> Result<()> {
    let _guard = database_lock(data).await?;
    let mut db = SqliteConnection::connect_with(
        &SqliteConnectOptions::new().filename(data.join("control.sqlite3")),
    )
    .await
    .map_err(|_| Fault::internal())?;
    sqlx::query("VACUUM INTO ?")
        .bind(path.to_str().ok_or_else(Fault::internal)?)
        .execute(&mut db)
        .await
        .map_err(|_| Fault::internal())?;
    db.close().await.map_err(|_| Fault::internal())?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))
            .map_err(|_| Fault::internal())?;
    }
    std::fs::File::open(path)
        .and_then(|v| v.sync_all())
        .map_err(|_| Fault::internal())?;
    sync_parent(path)?;
    Ok(())
}
pub async fn restore(data: &Path, path: &Path) -> Result<()> {
    let _guard = database_lock(data).await?;
    let mut db =
        SqliteConnection::connect_with(&SqliteConnectOptions::new().filename(path).read_only(true))
            .await
            .map_err(|_| Fault::internal())?;
    let checked: String = sqlx::query_scalar("PRAGMA quick_check")
        .fetch_one(&mut db)
        .await
        .map_err(|_| Fault::internal())?;
    let version: i64 = sqlx::query_scalar("PRAGMA user_version")
        .fetch_one(&mut db)
        .await
        .map_err(|_| Fault::internal())?;
    db.close().await.map_err(|_| Fault::internal())?;
    if checked != "ok" || version != 1 {
        return Err(Fault::internal());
    }
    let target = data.join(format!("restore-{}.sqlite3", uuid::Uuid::new_v4()));
    std::fs::copy(path, &target).map_err(|_| Fault::internal())?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&target, std::fs::Permissions::from_mode(0o600))
            .map_err(|_| Fault::internal())?;
    }
    std::fs::File::open(&target)
        .and_then(|v| v.sync_all())
        .map_err(|_| Fault::internal())?;
    for name in ["control.sqlite3-wal", "control.sqlite3-shm"] {
        let path = data.join(name);
        if path.exists() {
            std::fs::remove_file(path).map_err(|_| Fault::internal())?;
        }
    }
    let database = data.join("control.sqlite3");
    std::fs::rename(target, &database).map_err(|_| Fault::internal())?;
    sync_parent(&database)?;
    Ok(())
}
pub fn backup_path(root: &Path, name: &str) -> Result<PathBuf> {
    if uuid::Uuid::parse_str(name).is_err() {
        return Err(Fault::internal());
    }
    Ok(root.join("backups").join(format!("{name}.sqlite3")))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[tokio::test]
    async fn consistent_backup_restores_latest_state_and_rejects_incompatible_backup() {
        let dir = tempfile::tempdir().unwrap();
        let data = dir.path().join("state");
        protected_dir(&data).unwrap();
        let db_path = data.join("control.sqlite3");
        let mut db = SqliteConnection::connect_with(
            &SqliteConnectOptions::new()
                .filename(&db_path)
                .create_if_missing(true),
        )
        .await
        .unwrap();
        for q in [
            "PRAGMA user_version=1",
            "CREATE TABLE vault(id INTEGER PRIMARY KEY,payload BLOB)",
            "CREATE TABLE usage(id INTEGER)",
            "INSERT INTO vault VALUES(1,x'00010203')",
            "INSERT INTO usage VALUES(7)",
        ] {
            sqlx::query(q).execute(&mut db).await.unwrap();
        }
        db.close().await.unwrap();
        let copy = dir.path().join("backup.sqlite3");
        backup(&data, &copy).await.unwrap();
        std::fs::write(&db_path, b"corrupt candidate state").unwrap();
        restore(&data, &copy).await.unwrap();
        let mut db =
            SqliteConnection::connect_with(&SqliteConnectOptions::new().filename(&db_path))
                .await
                .unwrap();
        assert_eq!(
            sqlx::query_scalar::<_, i64>("SELECT id FROM usage")
                .fetch_one(&mut db)
                .await
                .unwrap(),
            7
        );
        assert_eq!(
            sqlx::query_scalar::<_, Vec<u8>>("SELECT payload FROM vault")
                .fetch_one(&mut db)
                .await
                .unwrap(),
            [0, 1, 2, 3]
        );
        db.close().await.unwrap();
        let mut db = SqliteConnection::connect_with(&SqliteConnectOptions::new().filename(&copy))
            .await
            .unwrap();
        sqlx::query("PRAGMA user_version=2")
            .execute(&mut db)
            .await
            .unwrap();
        db.close().await.unwrap();
        assert!(restore(&data, &copy).await.is_err());
        let mut db =
            SqliteConnection::connect_with(&SqliteConnectOptions::new().filename(&db_path))
                .await
                .unwrap();
        assert_eq!(
            sqlx::query_scalar::<_, i64>("SELECT id FROM usage")
                .fetch_one(&mut db)
                .await
                .unwrap(),
            7
        );
    }
    #[tokio::test]
    async fn active_store_lock_and_non_executable_assets_are_not_bypassed() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("fake");
        std::fs::write(&path, b"not an ELF executable; never execute this fixture").unwrap();
        assert_eq!(preflight(&path, "v1.0.0").await.unwrap_err().status, 422);
        assert!(backup_path(dir.path(), "../../outside").is_err());
        let mut lock = fslock::LockFile::open(&dir.path().join("instance.lock")).unwrap();
        assert!(lock.try_lock().unwrap());
        let mut other = fslock::LockFile::open(&dir.path().join("instance.lock")).unwrap();
        assert!(!other.try_lock().unwrap());
    }
    #[test]
    fn activation_retains_two_distinct_binaries_and_never_derives_paths_from_tags() {
        let item = |n: u8| Installed {
            version: format!("v1.0.{n}"),
            sha256: format!("{n:064x}"),
            size: 20,
            tag: Some(format!("v1.0.{n}")),
        };
        let original = Registry {
            schema_version: 1,
            active: item(1),
            history: vec![],
        };
        let next = original
            .activate(item(2))
            .activate(item(3))
            .activate(item(4));
        assert_eq!(
            next.history
                .iter()
                .map(|r| r.version.as_str())
                .collect::<Vec<_>>(),
            ["v1.0.3", "v1.0.2"]
        );
        let rolled = next.activate(item(2));
        assert_eq!(rolled.history[0].version, "v1.0.4");
        assert_eq!(rolled.history[1].version, "v1.0.3");
        let mut invalid = item(1);
        invalid.sha256 = "../../outside".into();
        assert!(invalid.path(Path::new("/protected")).is_err());
    }
}
