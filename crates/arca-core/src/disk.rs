//! Backend de disco: um arquivo por objeto em `<raiz>/<tipo>/<hex>`, gravado em arquivo
//! temporário e movido com `rename` atômico (`SPEC-TECNICA.md` §4). Em unix, pastas `0o700`
//! e arquivos `0o600`.

use crate::object::{ObjectId, ObjectKind};
use crate::store::{BlobStore, Capabilities, StoreError};
use std::fs::{self, DirBuilder, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};

/// Cópia local dos objetos, sempre presente (ADR 0007).
#[derive(Debug, Clone)]
pub struct DiskStore {
    root: PathBuf,
}

impl DiskStore {
    /// Abre (e cria, com `blob/` e `manifest/`) o armazenamento em `root`. Síncrono: só cria
    /// pastas. Uma `root` que já existe não tem a permissão alterada.
    pub fn open(root: impl Into<PathBuf>) -> Result<DiskStore, StoreError> {
        let root = root.into();
        for dir in [
            root.clone(),
            root.join(ObjectKind::Blob.as_str()),
            root.join(ObjectKind::Manifest.as_str()),
        ] {
            create_private_dir(&dir).map_err(unavailable)?;
        }
        Ok(DiskStore { root })
    }

    /// Pasta raiz do armazenamento.
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// `<raiz>/<tipo>/<hex>`: o `Display` do `ObjectId` já é `<tipo>/<hex>`.
    fn path_of(&self, id: &ObjectId) -> PathBuf {
        self.root.join(id.to_string())
    }
}

impl BlobStore for DiskStore {
    async fn put(&self, id: &ObjectId, bytes: &[u8]) -> Result<(), StoreError> {
        if ObjectId::of(id.kind, bytes) != *id {
            return Err(StoreError::IdMismatch);
        }
        let path = self.path_of(id);
        let bytes = bytes.to_vec();
        blocking(move || write_atomic(&path, &bytes)).await
    }

    async fn get(&self, id: &ObjectId) -> Result<Option<Vec<u8>>, StoreError> {
        let path = self.path_of(id);
        blocking(move || match fs::metadata(&path) {
            Ok(meta) if meta.is_file() => match fs::read(&path) {
                Ok(bytes) => Ok(Some(bytes)),
                Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(None),
                Err(e) => Err(e),
            },
            Ok(_) => Ok(None),
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(e),
        })
        .await
    }

    async fn has(&self, id: &ObjectId) -> Result<bool, StoreError> {
        let path = self.path_of(id);
        blocking(move || match fs::metadata(&path) {
            Ok(meta) => Ok(meta.is_file()),
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(false),
            Err(e) => Err(e),
        })
        .await
    }

    async fn list(&self, kind: ObjectKind) -> Result<Vec<ObjectId>, StoreError> {
        let dir = self.root.join(kind.as_str());
        blocking(move || {
            let mut ids = Vec::new();
            for entry in fs::read_dir(&dir)? {
                let entry = entry?;
                // `file_type` não segue symlink: só arquivo regular conta.
                if !entry.file_type()?.is_file() {
                    continue;
                }
                // O nome vem do disco: só vale se for exatamente `<64 hex minúsculos>`.
                let Some(name) = entry.file_name().to_str().map(str::to_owned) else {
                    continue;
                };
                if let Ok(id) = format!("{}/{name}", kind.as_str()).parse::<ObjectId>() {
                    ids.push(id);
                }
            }
            Ok(ids)
        })
        .await
    }

    async fn delete(&self, id: &ObjectId) -> Result<(), StoreError> {
        let path = self.path_of(id);
        blocking(move || match fs::remove_file(&path) {
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(()),
            other => other,
        })
        .await
    }

    fn capabilities(&self) -> Capabilities {
        Capabilities {
            consistent_list: true,
        }
    }
}

/// Só o tipo do erro de E/S: a mensagem do sistema traz o caminho.
fn unavailable(e: io::Error) -> StoreError {
    StoreError::Unavailable(e.kind())
}

/// Roda E/S bloqueante fora do executor.
async fn blocking<T, F>(f: F) -> Result<T, StoreError>
where
    T: Send + 'static,
    F: FnOnce() -> io::Result<T> + Send + 'static,
{
    tokio::task::spawn_blocking(f)
        .await
        .map_err(|_| StoreError::Unavailable(io::ErrorKind::Other))?
        .map_err(unavailable)
}

/// Cria a pasta (e as que faltarem) com `0o700` em unix. Pasta que já existe fica como está.
pub(crate) fn create_private_dir(path: &Path) -> io::Result<()> {
    let mut builder = DirBuilder::new();
    builder.recursive(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        builder.mode(0o700);
    }
    builder.create(path)
}

/// Grava `bytes` em `path` sem nunca deixar um arquivo pela metade: temporário no mesmo
/// diretório (`create_new`, `0o600`), `fsync`, `rename` atômico e `fsync` do diretório.
/// Se algo falhar antes do `rename`, o temporário é apagado.
pub(crate) fn write_atomic(path: &Path, bytes: &[u8]) -> io::Result<()> {
    let dir = path
        .parent()
        .ok_or_else(|| io::Error::from(io::ErrorKind::InvalidInput))?;
    // caminho relativo sem pasta ("estado.json"): o pai é "", que não abre; a pasta é a atual
    let dir = if dir.as_os_str().is_empty() {
        Path::new(".")
    } else {
        dir
    };
    let mut random = [0u8; 8];
    getrandom::fill(&mut random).map_err(|_| io::Error::from(io::ErrorKind::Other))?;
    let suffix: String = random.iter().map(|b| format!("{b:02x}")).collect();
    let tmp = dir.join(format!(".tmp-{suffix}"));

    let result = (|| {
        let mut options = OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let mut file = options.open(&tmp)?;
        file.write_all(bytes)?;
        file.sync_all()?;
        fs::rename(&tmp, path)
    })();
    if result.is_err() {
        let _ = fs::remove_file(&tmp);
        return result;
    }
    #[cfg(unix)]
    fs::File::open(dir)?.sync_all()?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::TempDir;
    use std::io::ErrorKind;

    // ---------- ajudantes ----------

    fn obj(kind: ObjectKind, data: &[u8]) -> (ObjectId, Vec<u8>) {
        (ObjectId::of(kind, data), data.to_vec())
    }

    fn hex_of(kind: ObjectKind, data: &[u8]) -> String {
        ObjectId::of(kind, data)
            .to_string()
            .split_once('/')
            .unwrap()
            .1
            .to_string()
    }

    fn store(tmp: &TempDir) -> DiskStore {
        DiskStore::open(tmp.path().join("store")).unwrap()
    }

    fn names(dir: &Path) -> Vec<String> {
        let mut v: Vec<String> = std::fs::read_dir(dir)
            .unwrap()
            .map(|e| e.unwrap().file_name().into_string().unwrap())
            .collect();
        v.sort();
        v
    }

    async fn sorted_list(s: &DiskStore, kind: ObjectKind) -> Vec<String> {
        let mut v: Vec<String> = s
            .list(kind)
            .await
            .unwrap()
            .iter()
            .map(|i| i.to_string())
            .collect();
        v.sort();
        v
    }

    // ---------- open ----------

    #[test]
    fn open_creates_the_layout() {
        let tmp = TempDir::new();
        let s = store(&tmp);
        assert_eq!(s.root(), tmp.path().join("store"));
        assert_eq!(names(s.root()), ["blob", "manifest"]);
        assert!(DiskStore::open(s.root()).is_ok(), "reabrir é permitido");
    }

    #[test]
    fn open_fails_when_the_root_is_a_file_without_leaking_the_path() {
        let tmp = TempDir::new();
        let file = tmp.path().join("arquivo");
        std::fs::write(&file, b"x").unwrap();
        let err = DiskStore::open(&file).unwrap_err();
        assert!(matches!(err, StoreError::Unavailable(_)));
        let text = format!("{err} {err:?}");
        assert!(
            !text.contains("arca-test") && !text.contains("arquivo"),
            "{text}"
        );
    }

    #[cfg(unix)]
    #[test]
    fn new_folders_are_0700_and_an_existing_root_is_left_alone() {
        use std::os::unix::fs::PermissionsExt;
        let mode = |p: &Path| std::fs::metadata(p).unwrap().permissions().mode() & 0o777;

        let tmp = TempDir::new();
        let s = store(&tmp);
        assert_eq!(mode(s.root()), 0o700);
        assert_eq!(mode(&s.root().join("blob")), 0o700);
        assert_eq!(mode(&s.root().join("manifest")), 0o700);

        let existing = tmp.path().join("existente");
        std::fs::create_dir(&existing).unwrap();
        std::fs::set_permissions(&existing, std::fs::Permissions::from_mode(0o755)).unwrap();
        DiskStore::open(&existing).unwrap();
        assert_eq!(mode(&existing), 0o755);
        assert_eq!(mode(&existing.join("blob")), 0o700);
    }

    #[test]
    fn capabilities_report_a_consistent_list() {
        let tmp = TempDir::new();
        assert!(store(&tmp).capabilities().consistent_list);
    }

    // ---------- put / get / has ----------

    #[tokio::test]
    async fn round_trip_for_both_kinds_and_edge_sizes() {
        let tmp = TempDir::new();
        let s = store(&tmp);
        for kind in [ObjectKind::Blob, ObjectKind::Manifest] {
            for size in [0usize, 1, 36, 4096, 3 * 1024 * 1024] {
                let data: Vec<u8> = (0..size).map(|i| (i % 251) as u8).collect();
                let (id, bytes) = obj(kind, &data);
                assert_eq!(s.has(&id).await, Ok(false));
                s.put(&id, &bytes).await.unwrap();
                assert_eq!(s.has(&id).await, Ok(true));
                assert_eq!(s.get(&id).await, Ok(Some(bytes)), "{kind:?} {size}");
            }
        }
    }

    #[tokio::test]
    async fn on_disk_layout_is_kind_slash_hex() {
        let tmp = TempDir::new();
        let s = store(&tmp);
        let (id, bytes) = obj(ObjectKind::Manifest, b"m");
        s.put(&id, &bytes).await.unwrap();
        let hex = hex_of(ObjectKind::Manifest, b"m");
        assert_eq!(
            names(&s.root().join("manifest")),
            std::slice::from_ref(&hex)
        );
        assert_eq!(
            std::fs::read(s.root().join("manifest").join(hex)).unwrap(),
            bytes
        );
        assert!(names(&s.root().join("blob")).is_empty());
    }

    #[tokio::test]
    async fn put_is_idempotent() {
        let tmp = TempDir::new();
        let s = store(&tmp);
        let (id, bytes) = obj(ObjectKind::Blob, b"mesmo");
        s.put(&id, &bytes).await.unwrap();
        s.put(&id, &bytes).await.unwrap();
        assert_eq!(s.get(&id).await, Ok(Some(bytes)));
        assert_eq!(s.list(ObjectKind::Blob).await, Ok(vec![id]));
    }

    #[tokio::test]
    async fn put_refuses_bytes_that_do_not_match_the_id_and_writes_nothing() {
        let tmp = TempDir::new();
        let s = store(&tmp);
        let id = ObjectId::of(ObjectKind::Blob, b"a");
        assert_eq!(s.put(&id, b"b").await, Err(StoreError::IdMismatch));
        assert_eq!(s.put(&id, b"").await, Err(StoreError::IdMismatch));
        // tipo trocado: mesmo hash, outro tipo no id
        let wrong_kind = ObjectId {
            kind: ObjectKind::Manifest,
            hash: ObjectId::of(ObjectKind::Blob, b"a").hash,
        };
        // O hash só cobre os bytes (SPEC-TECNICA §3), então o tipo não muda a conta: aceito.
        assert_eq!(s.put(&wrong_kind, b"a").await, Ok(()));
        assert!(names(&s.root().join("blob")).is_empty());
        assert_eq!(names(&s.root().join("manifest")).len(), 1);
        // e nenhum temporário ficou para trás
        assert!(names(&s.root().join("manifest"))
            .iter()
            .all(|n| !n.starts_with(".tmp")));
    }

    #[tokio::test]
    async fn put_replaces_a_corrupted_file() {
        let tmp = TempDir::new();
        let s = store(&tmp);
        let (id, bytes) = obj(ObjectKind::Blob, b"correto");
        std::fs::write(s.root().join(id.to_string()), b"lixo").unwrap();
        assert_eq!(s.get(&id).await, Ok(Some(b"lixo".to_vec())));
        s.put(&id, &bytes).await.unwrap();
        assert_eq!(s.get(&id).await, Ok(Some(bytes)));
    }

    #[tokio::test]
    async fn missing_objects_are_none_and_false() {
        let tmp = TempDir::new();
        let s = store(&tmp);
        let id = ObjectId::of(ObjectKind::Blob, b"ausente");
        assert_eq!(s.get(&id).await, Ok(None));
        assert_eq!(s.has(&id).await, Ok(false));
    }

    #[tokio::test]
    async fn a_directory_with_the_name_of_an_object_is_not_an_object() {
        let tmp = TempDir::new();
        let s = store(&tmp);
        let id = ObjectId::of(ObjectKind::Blob, b"x");
        std::fs::create_dir(s.root().join(id.to_string())).unwrap();
        assert_eq!(s.has(&id).await, Ok(false));
        assert_eq!(s.get(&id).await, Ok(None));
    }

    // ---------- delete ----------

    #[tokio::test]
    async fn delete_removes_and_is_idempotent() {
        let tmp = TempDir::new();
        let s = store(&tmp);
        let (id, bytes) = obj(ObjectKind::Blob, b"x");
        let (other, other_bytes) = obj(ObjectKind::Blob, b"y");
        s.put(&id, &bytes).await.unwrap();
        s.put(&other, &other_bytes).await.unwrap();
        assert_eq!(s.delete(&id).await, Ok(()));
        assert_eq!(s.has(&id).await, Ok(false));
        assert_eq!(s.delete(&id).await, Ok(()));
        assert_eq!(s.has(&other).await, Ok(true));
        assert_eq!(
            s.delete(&ObjectId::of(ObjectKind::Manifest, b"nunca"))
                .await,
            Ok(())
        );
    }

    // ---------- list ----------

    #[tokio::test]
    async fn list_separates_kinds() {
        let tmp = TempDir::new();
        let s = store(&tmp);
        assert_eq!(s.list(ObjectKind::Blob).await, Ok(vec![]));
        for (kind, data) in [
            (ObjectKind::Blob, &b"b1"[..]),
            (ObjectKind::Blob, b"b2"),
            (ObjectKind::Manifest, b"m1"),
        ] {
            let (id, bytes) = obj(kind, data);
            s.put(&id, &bytes).await.unwrap();
        }
        let mut blobs = vec![
            ObjectId::of(ObjectKind::Blob, b"b1").to_string(),
            ObjectId::of(ObjectKind::Blob, b"b2").to_string(),
        ];
        blobs.sort();
        assert_eq!(sorted_list(&s, ObjectKind::Blob).await, blobs);
        assert_eq!(
            sorted_list(&s, ObjectKind::Manifest).await,
            [ObjectId::of(ObjectKind::Manifest, b"m1").to_string()]
        );
    }

    #[tokio::test]
    async fn list_ignores_everything_that_is_not_a_well_named_regular_file() {
        let tmp = TempDir::new();
        let s = store(&tmp);
        let (id, bytes) = obj(ObjectKind::Blob, b"real");
        s.put(&id, &bytes).await.unwrap();

        let dir = s.root().join("blob");
        let hex = hex_of(ObjectKind::Blob, b"real");
        std::fs::write(dir.join(".tmp-0123456789abcdef"), b"x").unwrap();
        std::fs::write(dir.join(hex.to_uppercase()), b"x").unwrap();
        std::fs::write(dir.join(&hex[..63]), b"x").unwrap();
        std::fs::write(dir.join(format!("{hex}.bak")), b"x").unwrap();
        std::fs::write(dir.join("notas.txt"), b"x").unwrap();
        // subdiretório com nome de objeto válido
        std::fs::create_dir(dir.join(hex_of(ObjectKind::Blob, b"dir"))).unwrap();
        // symlink com nome de objeto válido, apontando para um arquivo de verdade
        #[cfg(unix)]
        {
            let target = dir.join("alvo.txt");
            std::fs::write(&target, b"x").unwrap();
            std::os::unix::fs::symlink(&target, dir.join(hex_of(ObjectKind::Blob, b"link")))
                .unwrap();
        }

        assert_eq!(s.list(ObjectKind::Blob).await, Ok(vec![id]));
        assert_eq!(s.list(ObjectKind::Manifest).await, Ok(vec![]));
    }

    // ---------- permissões e temporários ----------

    #[cfg(unix)]
    #[tokio::test]
    async fn objects_are_written_0600() {
        use std::os::unix::fs::PermissionsExt;
        let tmp = TempDir::new();
        let s = store(&tmp);
        for kind in [ObjectKind::Blob, ObjectKind::Manifest] {
            let (id, bytes) = obj(kind, b"segredo cifrado");
            s.put(&id, &bytes).await.unwrap();
            let mode = std::fs::metadata(s.root().join(id.to_string()))
                .unwrap()
                .permissions()
                .mode();
            assert_eq!(mode & 0o777, 0o600, "{kind:?}");
        }
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn a_failed_put_leaves_no_temporary_and_reports_unavailable() {
        use std::os::unix::fs::PermissionsExt;
        let tmp = TempDir::new();
        let s = store(&tmp);
        let dir = s.root().join("blob");
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o500)).unwrap();
        if std::fs::write(dir.join("sonda"), b"").is_ok() {
            // rodando como root: permissão não barra a escrita, o teste não prova nada
            std::fs::remove_file(dir.join("sonda")).unwrap();
            return;
        }
        let (id, bytes) = obj(ObjectKind::Blob, b"x");
        assert_eq!(
            s.put(&id, &bytes).await,
            Err(StoreError::Unavailable(ErrorKind::PermissionDenied))
        );
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700)).unwrap();
        assert!(names(&dir).is_empty());
    }

    #[tokio::test]
    async fn no_temporary_is_left_after_successful_puts() {
        let tmp = TempDir::new();
        let s = store(&tmp);
        for i in 0..20u8 {
            let (id, bytes) = obj(ObjectKind::Blob, &[i; 64]);
            s.put(&id, &bytes).await.unwrap();
        }
        let n = names(&s.root().join("blob"));
        assert_eq!(n.len(), 20);
        assert!(n
            .iter()
            .all(|name| name.len() == 64 && !name.starts_with('.')));
    }

    #[tokio::test]
    async fn io_errors_never_leak_the_path() {
        let tmp = TempDir::new();
        let s = store(&tmp);
        std::fs::remove_dir_all(s.root().join("blob")).unwrap();
        let (id, bytes) = obj(ObjectKind::Blob, b"x");
        let errors = [
            s.put(&id, &bytes).await.unwrap_err(),
            s.list(ObjectKind::Blob).await.unwrap_err(),
        ];
        for err in errors {
            let text = format!("{err} {err:?}");
            assert!(
                !text.contains("arca-test") && !text.contains("store"),
                "{text}"
            );
        }
    }

    // ---------- concorrência ----------

    #[tokio::test]
    async fn concurrent_puts_of_the_same_and_of_different_objects_stay_intact() {
        let tmp = TempDir::new();
        let s = store(&tmp);
        let same = obj(ObjectKind::Blob, &[7u8; 100_000]);
        let mut tasks = Vec::new();
        for i in 0..32u8 {
            for (id, bytes) in [same.clone(), obj(ObjectKind::Blob, &[i; 5000])] {
                let s = s.clone();
                tasks.push(tokio::spawn(async move { s.put(&id, &bytes).await }));
            }
        }
        for t in tasks {
            assert_eq!(t.await.unwrap(), Ok(()));
        }
        assert_eq!(s.get(&same.0).await, Ok(Some(same.1)));
        for i in 0..32u8 {
            let (id, bytes) = obj(ObjectKind::Blob, &[i; 5000]);
            assert_eq!(s.get(&id).await, Ok(Some(bytes)));
        }
        let n = names(&s.root().join("blob"));
        assert_eq!(n.len(), 33);
        assert!(
            n.iter().all(|name| !name.starts_with('.')),
            "temporário sobrou: {n:?}"
        );
    }
}
