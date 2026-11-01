//! Núcleo local: junta cripto, manifest e `BlobStore` para **guardar o Vault e restaurá-lo
//! do zero** (`SPEC-TECNICA.md` §6, "Envio" e "Recebimento", sem a parte de rede). Sync entre
//! dispositivos, junção de pontas e regra de conflito são da Fase 2.

use crate::crypto::{open, seal, EpochKeys};
use crate::disk::{create_private_dir, write_atomic};
use crate::error::Error;
use crate::heads::{tips, Head};
use crate::manifest::{validate_device, validate_path, Class, FileEntry, Hash32, Manifest};
use crate::object::{ObjectId, ObjectKind};
use crate::scan::{scan_class, Candidate, Skipped, MAX_FILE_BYTES};
use crate::store::{verified_get, BlobStore, StoreError};
use crate::FORMAT_VERSION;
use sha2::{Digest, Sha256};
use std::collections::HashSet;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use zeroize::Zeroizing;

/// Pasta de origem/destino de cada classe. Classe sem pasta está **desligada**: não é lida no
/// snapshot (as entradas da ponta são mantidas como estão) nem aplicada na restauração.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Roots {
    pub vault: Option<PathBuf>,
    pub config: Option<PathBuf>,
    pub secret: Option<PathBuf>,
}

impl Roots {
    /// As classes ligadas e suas pastas, em ordem fixa.
    fn enabled(&self) -> Vec<(Class, &Path)> {
        [
            (Class::Vault, &self.vault),
            (Class::Config, &self.config),
            (Class::Secret, &self.secret),
        ]
        .into_iter()
        .filter_map(|(class, root)| root.as_deref().map(|r| (class, r)))
        .collect()
    }

    fn is_enabled(&self, class: Class) -> bool {
        self.enabled().iter().any(|(c, _)| *c == class)
    }
}

/// Erros do núcleo local. Nunca carregam caminho, nome de arquivo nem bytes.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum EngineError {
    #[error("storage error: {0}")]
    Store(#[from] StoreError),
    /// Objeto ilegível ou inválido (formato, tag, época, manifest fora das regras).
    #[error("invalid object: {0}")]
    Object(#[from] Error),
    /// Mais de uma ponta, ou algum manifest ilegível: construir em cima seria criar uma
    /// divergência silenciosa. Resolver é da Fase 2.
    #[error(
        "storage has {tips} tips and {rejected} unreadable manifests; refusing to build on it"
    )]
    Diverged { tips: usize, rejected: usize },
    /// A restauração só escreve em pasta inexistente ou vazia.
    #[error("restore target is not empty")]
    TargetNotEmpty,
    /// O texto claro não tem o tamanho ou o SHA-256 que o manifest declara.
    #[error("file content does not match the manifest")]
    Integrity,
    /// O manifest (ou um blob que ele cita) não está no armazenamento.
    #[error("object referenced is missing from the store")]
    MissingObject,
    #[error("i/o error: {0}")]
    Io(io::ErrorKind),
}

/// Um manifest que não pôde ser lido e por quê.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Rejected {
    pub id: ObjectId,
    pub error: EngineError,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Heads {
    pub tips: Vec<Head>,
    pub rejected: Vec<Rejected>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Snapshot {
    /// Gravou um manifest novo.
    Created {
        id: ObjectId,
        manifest: Manifest,
        /// Blobs novos enviados (os inalterados são reaproveitados).
        new_blobs: usize,
        skipped: Vec<Skipped>,
    },
    /// Nada mudou desde a ponta: nenhum objeto foi gravado.
    Unchanged {
        head: ObjectId,
        skipped: Vec<Skipped>,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Restored {
    pub files: usize,
}

pub struct Engine<S> {
    store: S,
    keys: EpochKeys,
    device: String,
}

impl<S: BlobStore> Engine<S> {
    /// `device` segue as regras do manifest (ASCII `[A-Za-z0-9._-]`, sem ponto inicial).
    pub fn new(
        store: S,
        keys: EpochKeys,
        device: impl Into<String>,
    ) -> Result<Engine<S>, EngineError> {
        let device = device.into();
        validate_device(&device)?;
        Ok(Engine {
            store,
            keys,
            device,
        })
    }

    /// Lê todos os manifests do armazenamento e calcula as pontas. Um manifest ilegível vai
    /// para `rejected` e nunca vira ponta; backend fora do ar aborta (não é "ilegível").
    pub async fn heads(&self) -> Result<Heads, EngineError> {
        let mut readable = Vec::new();
        let mut rejected = Vec::new();
        for id in self.store.list(ObjectKind::Manifest).await? {
            match self.read_manifest(&id).await {
                Ok(manifest) => readable.push((id, manifest)),
                Err(e @ EngineError::Store(StoreError::Unavailable(_))) => return Err(e),
                Err(error) => rejected.push(Rejected { id, error }),
            }
        }
        rejected.sort_by_key(|r| r.id.hash);
        Ok(Heads {
            tips: tips(readable),
            rejected,
        })
    }

    async fn read_manifest(&self, id: &ObjectId) -> Result<Manifest, EngineError> {
        let bytes = verified_get(&self.store, id)
            .await?
            .ok_or(EngineError::MissingObject)?;
        Ok(Manifest::open(&self.keys, &bytes)?)
    }

    /// Fotografa as pastas de `roots` e grava o estado como um manifest novo (blobs primeiro,
    /// manifest por último). `now` são segundos Unix (UTC), injetados para testes determinísticos.
    pub async fn snapshot(&self, roots: &Roots, now: u64) -> Result<Snapshot, EngineError> {
        let Heads { tips, rejected } = self.heads().await?;
        if tips.len() > 1 || !rejected.is_empty() {
            return Err(EngineError::Diverged {
                tips: tips.len(),
                rejected: rejected.len(),
            });
        }
        let prev = tips.into_iter().next();
        let prev_files: &[FileEntry] = prev.as_ref().map_or(&[], |h| &h.manifest.files);

        // 1. varrer as classes ligadas (nada é lido nem enviado ainda)
        let mut candidates = Vec::new();
        let mut skipped = Vec::new();
        for (class, root) in roots.enabled() {
            let root = root.to_path_buf();
            let scan = run(move || scan_class(class, &root).map_err(io_error)).await?;
            candidates.extend(scan.candidates);
            skipped.extend(scan.skipped);
        }

        // 2. entradas de classes desligadas vêm da ponta como estão
        let carried: Vec<&FileEntry> = prev_files
            .iter()
            .filter(|f| !roots.is_enabled(f.class))
            .collect();
        let candidates = dedupe(candidates, &mut skipped);

        // 3. ler, comparar com a ponta e enviar só o que mudou
        let mut files: Vec<FileEntry> = carried.into_iter().cloned().collect();
        let mut new_blobs = 0;
        for candidate in candidates {
            let Some(data) = read_file(&candidate, &mut skipped).await? else {
                continue;
            };
            let sha256 = Hash32(Sha256::digest(data.as_slice()).into());
            let size = data.len() as u64;
            let unchanged = prev_files.iter().find(|f| {
                f.class == candidate.class
                    && f.path == candidate.path
                    && f.sha256 == sha256
                    && f.size == size
            });
            let blob = match unchanged {
                Some(entry) => entry.blob,
                None => {
                    let sealed = seal(&self.keys, ObjectKind::Blob, &data)?;
                    self.store.put(&sealed.id, &sealed.bytes).await?;
                    new_blobs += 1;
                    Hash32(sealed.id.hash)
                }
            };
            files.push(FileEntry {
                path: candidate.path,
                class: candidate.class,
                blob,
                size,
                sha256,
            });
        }
        files.sort_by(|a, b| (a.class, &a.path).cmp(&(b.class, &b.path)));

        if let Some(head) = &prev {
            if head.manifest.files == files {
                return Ok(Snapshot::Unchanged {
                    head: head.id,
                    skipped,
                });
            }
        }

        // 4. o manifest vai por último: sem ele, os blobs são lixo inofensivo
        let manifest = Manifest {
            format_version: FORMAT_VERSION as u16,
            key_epoch: self.keys.epoch(),
            base_version: prev.as_ref().map(|h| Hash32(h.id.hash)),
            merged_from: vec![],
            counter: prev
                .as_ref()
                .map_or(1, |h| h.manifest.counter.saturating_add(1)),
            device: self.device.clone(),
            created_at: now,
            files,
        };
        let sealed = manifest.seal(&self.keys)?;
        self.store.put(&sealed.id, &sealed.bytes).await?;
        Ok(Snapshot::Created {
            id: sealed.id,
            manifest,
            new_blobs,
            skipped,
        })
    }

    /// Restaura o manifest `id` nas pastas de `roots`, que têm de não existir ou estar vazias.
    pub async fn restore(&self, id: &ObjectId, roots: &Roots) -> Result<Restored, EngineError> {
        let manifest = self.read_manifest(id).await?;
        let enabled = roots.enabled();

        // todos os destinos são conferidos antes de escrever qualquer coisa
        let mut touched: Vec<(PathBuf, bool)> = Vec::new();
        for (_, root) in &enabled {
            let root = root.to_path_buf();
            let existed = run({
                let root = root.clone();
                move || check_target(&root)
            })
            .await?;
            touched.push((root, existed));
        }

        let result = self.restore_files(&manifest, &enabled, &touched).await;
        if result.is_err() {
            for (root, existed) in touched {
                let _ = run(move || {
                    cleanup(&root, existed);
                    Ok(())
                })
                .await;
            }
        }
        result
    }

    async fn restore_files(
        &self,
        manifest: &Manifest,
        enabled: &[(Class, &Path)],
        touched: &[(PathBuf, bool)],
    ) -> Result<Restored, EngineError> {
        for (root, _) in touched {
            let root = root.clone();
            run(move || create_private_dir(&root).map_err(io_error)).await?;
        }
        let mut files = 0;
        for entry in &manifest.files {
            let Some((_, root)) = enabled.iter().find(|(class, _)| *class == entry.class) else {
                continue;
            };
            let blob = ObjectId {
                kind: ObjectKind::Blob,
                hash: entry.blob.0,
            };
            let bytes = verified_get(&self.store, &blob)
                .await?
                .ok_or(EngineError::MissingObject)?;
            let plain = open(&self.keys, ObjectKind::Blob, &bytes)?;
            if plain.len() as u64 != entry.size
                || Hash32(Sha256::digest(plain.as_slice()).into()) != entry.sha256
            {
                return Err(EngineError::Integrity);
            }
            let dest = safe_join(root, &entry.path)?;
            run(move || {
                if let Some(parent) = dest.parent() {
                    create_private_dir(parent).map_err(io_error)?;
                }
                write_atomic(&dest, &plain).map_err(io_error)
            })
            .await?;
            files += 1;
        }
        Ok(Restored { files })
    }
}

/// Roda E/S bloqueante fora do executor.
async fn run<T, F>(f: F) -> Result<T, EngineError>
where
    T: Send + 'static,
    F: FnOnce() -> Result<T, EngineError> + Send + 'static,
{
    tokio::task::spawn_blocking(f)
        .await
        .map_err(|_| EngineError::Io(io::ErrorKind::Other))?
}

fn io_error(e: io::Error) -> EngineError {
    EngineError::Io(e.kind())
}

/// Lê o arquivo inteiro. Some entre a varredura e a leitura, ou cresceu além do teto: vai para
/// `skipped` em vez de abortar o snapshot.
async fn read_file(
    candidate: &Candidate,
    skipped: &mut Vec<Skipped>,
) -> Result<Option<Zeroizing<Vec<u8>>>, EngineError> {
    let abs = candidate.abs.clone();
    let read = tokio::task::spawn_blocking(move || fs::read(abs))
        .await
        .map_err(|_| EngineError::Io(io::ErrorKind::Other))?;
    let reason = match read {
        Ok(data) if data.len() as u64 <= MAX_FILE_BYTES => return Ok(Some(Zeroizing::new(data))),
        Ok(_) => "file too large",
        Err(e) if e.kind() == io::ErrorKind::NotFound => "file disappeared",
        Err(e) => return Err(io_error(e)),
    };
    skipped.push(Skipped {
        class: candidate.class,
        path: candidate.path.clone(),
        reason,
    });
    Ok(None)
}

/// Ordena por `(classe, path em bytes)` e resolve as colisões por caixa, que o manifest não
/// representa: dentro de uma classe fica o primeiro e o outro vai para `skipped`. Classes
/// diferentes vivem em pastas diferentes e nunca colidem (P-23).
fn dedupe(mut candidates: Vec<Candidate>, skipped: &mut Vec<Skipped>) -> Vec<Candidate> {
    candidates.sort_by(|a, b| (a.class, &a.path).cmp(&(b.class, &b.path)));
    let mut seen: HashSet<(Class, String)> = HashSet::new();
    let mut kept = Vec::with_capacity(candidates.len());
    for candidate in candidates {
        if seen.insert((candidate.class, candidate.path.to_lowercase())) {
            kept.push(candidate);
        } else {
            skipped.push(Skipped {
                class: candidate.class,
                path: candidate.path,
                reason: "differs only by case from another file",
            });
        }
    }
    kept
}

/// A pasta de destino tem de não existir ou estar vazia. Devolve se ela já existia.
fn check_target(root: &Path) -> Result<bool, EngineError> {
    match fs::read_dir(root) {
        Ok(mut entries) => match entries.next() {
            None => Ok(true),
            Some(_) => Err(EngineError::TargetNotEmpty),
        },
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(false),
        Err(e) if e.kind() == io::ErrorKind::NotADirectory => Err(EngineError::TargetNotEmpty),
        Err(e) => Err(io_error(e)),
    }
}

/// Desfaz uma restauração que falhou: apaga o que ela criou. Só roda em destinos que estavam
/// vazios (ou inexistentes) quando começou, então tudo lá dentro é dela.
fn cleanup(root: &Path, existed: bool) {
    if !existed {
        let _ = fs::remove_dir_all(root);
        return;
    }
    let Ok(entries) = fs::read_dir(root) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let is_dir = entry.file_type().is_ok_and(|t| t.is_dir());
        let _ = if is_dir {
            fs::remove_dir_all(&path)
        } else {
            fs::remove_file(&path)
        };
    }
}

/// `root` + `rel` (separador `/`), recusando qualquer componente que possa sair da raiz.
/// Defesa em profundidade: o manifest já foi validado em `Manifest::from_slice`.
fn safe_join(root: &Path, rel: &str) -> Result<PathBuf, EngineError> {
    validate_path(rel)?;
    let mut path = root.to_path_buf();
    path.extend(rel.split('/'));
    Ok(path)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::backend::Backend;
    use crate::crypto::{seal, VaultKey};
    use crate::disk::DiskStore;
    use crate::manifest::{Class, Hash32};
    use crate::object::ObjectKind;
    use crate::store::{verified_get, Capabilities};
    use crate::testutil::TempDir;
    use sha2::{Digest, Sha256};
    use std::collections::BTreeMap;
    use std::fs;

    // ---------- ajudantes ----------

    fn keys(epoch: u32) -> EpochKeys {
        EpochKeys::derive(&VaultKey::from_bytes([3u8; 32]), epoch)
    }

    struct Rig {
        tmp: TempDir,
        store: Backend,
    }

    impl Rig {
        fn new() -> Rig {
            let tmp = TempDir::new();
            let store = Backend::Disk(DiskStore::open(tmp.path().join("store")).unwrap());
            Rig { tmp, store }
        }

        fn engine(&self, device: &str) -> Engine<Backend> {
            Engine::new(self.store.clone(), keys(1), device).unwrap()
        }

        /// Pasta nova (criada, vazia) dentro da pasta temporária do teste.
        fn dir(&self, name: &str) -> PathBuf {
            let p = self.tmp.path().join(name);
            fs::create_dir_all(&p).unwrap();
            p
        }

        fn store_dir(&self) -> PathBuf {
            self.tmp.path().join("store")
        }

        async fn count(&self, kind: ObjectKind) -> usize {
            self.store.list(kind).await.unwrap().len()
        }
    }

    fn write(root: &Path, rel: &str, data: &[u8]) {
        let p = root.join(rel);
        fs::create_dir_all(p.parent().unwrap()).unwrap();
        fs::write(p, data).unwrap();
    }

    fn vault_only(p: &Path) -> Roots {
        Roots {
            vault: Some(p.to_path_buf()),
            ..Roots::default()
        }
    }

    /// Todos os arquivos sob `root`: caminho relativo com `/` -> conteúdo.
    fn tree(root: &Path) -> BTreeMap<String, Vec<u8>> {
        fn walk(dir: &Path, prefix: &str, out: &mut BTreeMap<String, Vec<u8>>) {
            for entry in fs::read_dir(dir).unwrap() {
                let entry = entry.unwrap();
                let name = entry.file_name().into_string().unwrap();
                let rel = if prefix.is_empty() {
                    name
                } else {
                    format!("{prefix}/{name}")
                };
                if entry.file_type().unwrap().is_dir() {
                    walk(&entry.path(), &rel, out);
                } else {
                    out.insert(rel, fs::read(entry.path()).unwrap());
                }
            }
        }
        let mut out = BTreeMap::new();
        walk(root, "", &mut out);
        out
    }

    fn copy_dir(src: &Path, dst: &Path) {
        fs::create_dir_all(dst).unwrap();
        for entry in fs::read_dir(src).unwrap() {
            let entry = entry.unwrap();
            let to = dst.join(entry.file_name());
            if entry.file_type().unwrap().is_dir() {
                copy_dir(&entry.path(), &to);
            } else {
                fs::copy(entry.path(), to).unwrap();
            }
        }
    }

    fn sha(data: &[u8]) -> Hash32 {
        Hash32(Sha256::digest(data).into())
    }

    fn created(s: Snapshot) -> (ObjectId, Manifest, usize, Vec<Skipped>) {
        match s {
            Snapshot::Created {
                id,
                manifest,
                new_blobs,
                skipped,
            } => (id, manifest, new_blobs, skipped),
            other => panic!("esperava Created, veio {other:?}"),
        }
    }

    fn file_paths(m: &Manifest) -> Vec<&str> {
        m.files.iter().map(|f| f.path.as_str()).collect()
    }

    /// Grava no armazenamento um manifest derivado da ponta atual, depois de `edit`.
    async fn put_forged(rig: &Rig, edit: impl FnOnce(&mut Manifest)) -> ObjectId {
        let heads = rig.engine("pc").heads().await.unwrap();
        let tip = heads.tips.last().unwrap();
        let mut m = tip.manifest.clone();
        m.base_version = Some(Hash32(tip.id.hash));
        m.counter += 1;
        edit(&mut m);
        let sealed = m.seal(&keys(1)).unwrap();
        rig.store.put(&sealed.id, &sealed.bytes).await.unwrap();
        sealed.id
    }

    /// Backend que recusa gravar manifests (para provar que o manifest vai por último).
    #[derive(Clone)]
    struct FailManifestPut(Backend);

    impl BlobStore for FailManifestPut {
        async fn put(&self, id: &ObjectId, bytes: &[u8]) -> Result<(), StoreError> {
            if id.kind == ObjectKind::Manifest {
                Err(StoreError::Unavailable(io::ErrorKind::TimedOut))
            } else {
                self.0.put(id, bytes).await
            }
        }
        async fn get(&self, id: &ObjectId) -> Result<Option<Vec<u8>>, StoreError> {
            self.0.get(id).await
        }
        async fn has(&self, id: &ObjectId) -> Result<bool, StoreError> {
            self.0.has(id).await
        }
        async fn list(&self, kind: ObjectKind) -> Result<Vec<ObjectId>, StoreError> {
            self.0.list(kind).await
        }
        async fn delete(&self, id: &ObjectId) -> Result<(), StoreError> {
            self.0.delete(id).await
        }
        fn capabilities(&self) -> Capabilities {
            self.0.capabilities()
        }
    }

    // ---------- new / heads ----------

    #[test]
    fn new_applies_the_manifest_device_rules() {
        let rig = Rig::new();
        for bad in ["", ".oculto", "a b", "ação", "a/b"] {
            assert!(
                matches!(
                    Engine::new(rig.store.clone(), keys(1), bad),
                    Err(EngineError::Object(Error::InvalidManifest(_)))
                ),
                "{bad:?}"
            );
        }
        assert!(Engine::new(rig.store.clone(), keys(1), "portatil-fabio").is_ok());
    }

    #[tokio::test]
    async fn an_empty_store_has_no_tips() {
        let rig = Rig::new();
        let heads = rig.engine("pc").heads().await.unwrap();
        assert!(heads.tips.is_empty() && heads.rejected.is_empty());
    }

    // ---------- snapshot ----------

    #[tokio::test]
    async fn the_first_snapshot_of_an_empty_vault_creates_an_empty_manifest() {
        let rig = Rig::new();
        let vault = rig.dir("vault");
        let (id, m, new_blobs, skipped) = created(
            rig.engine("portatil")
                .snapshot(&vault_only(&vault), 1_000)
                .await
                .unwrap(),
        );
        assert!(m.files.is_empty() && skipped.is_empty());
        assert_eq!(new_blobs, 0);
        assert_eq!((m.base_version, m.counter, m.created_at), (None, 1, 1_000));
        assert_eq!((m.device.as_str(), m.key_epoch), ("portatil", 1));
        let heads = rig.engine("portatil").heads().await.unwrap();
        assert_eq!(heads.tips.len(), 1);
        assert_eq!(heads.tips[0].id, id);
        assert_eq!(heads.tips[0].manifest, m);
    }

    #[tokio::test]
    async fn the_first_snapshot_records_every_file_with_its_hash() {
        let rig = Rig::new();
        let vault = rig.dir("vault");
        write(&vault, "b.md", b"segundo");
        write(&vault, "sub/a.md", b"primeiro");
        write(&vault, ".oculto/x", b"");
        let (_, m, new_blobs, _) = created(
            rig.engine("pc")
                .snapshot(&vault_only(&vault), 5)
                .await
                .unwrap(),
        );
        assert_eq!(new_blobs, 3);
        assert_eq!(file_paths(&m), [".oculto/x", "b.md", "sub/a.md"]);
        for f in &m.files {
            let data = fs::read(vault.join(&f.path)).unwrap();
            assert_eq!(
                (f.class, f.size, f.sha256),
                (Class::Vault, data.len() as u64, sha(&data))
            );
            let blob = ObjectId {
                kind: ObjectKind::Blob,
                hash: f.blob.0,
            };
            assert_eq!(rig.store.has(&blob).await, Ok(true));
        }
        assert_eq!(rig.count(ObjectKind::Blob).await, 3);
        assert_eq!(rig.count(ObjectKind::Manifest).await, 1);
    }

    #[tokio::test]
    async fn a_second_snapshot_without_changes_writes_nothing() {
        let rig = Rig::new();
        let vault = rig.dir("vault");
        write(&vault, "a.md", b"1");
        let eng = rig.engine("pc");
        let (first, ..) = created(eng.snapshot(&vault_only(&vault), 1).await.unwrap());
        let again = eng.snapshot(&vault_only(&vault), 2).await.unwrap();
        assert_eq!(
            again,
            Snapshot::Unchanged {
                head: first,
                skipped: vec![]
            }
        );
        assert_eq!(rig.count(ObjectKind::Blob).await, 1);
        assert_eq!(rig.count(ObjectKind::Manifest).await, 1);
    }

    #[tokio::test]
    async fn changing_one_of_five_files_uploads_exactly_one_blob() {
        let rig = Rig::new();
        let vault = rig.dir("vault");
        for i in 0..5 {
            write(
                &vault,
                &format!("f{i}.md"),
                format!("conteudo {i}").as_bytes(),
            );
        }
        let eng = rig.engine("pc");
        let (id1, m1, n1, _) = created(eng.snapshot(&vault_only(&vault), 10).await.unwrap());
        assert_eq!(n1, 5);

        write(&vault, "f2.md", b"conteudo novo");
        let (_, m2, n2, _) = created(eng.snapshot(&vault_only(&vault), 20).await.unwrap());
        assert_eq!(n2, 1);
        assert_eq!(rig.count(ObjectKind::Blob).await, 6);
        assert_eq!(rig.count(ObjectKind::Manifest).await, 2);
        assert_eq!((m2.counter, m2.created_at), (2, 20));
        assert_eq!(m2.base_version, Some(Hash32(id1.hash)));
        for (old, new) in m1.files.iter().zip(&m2.files) {
            assert_eq!(old.path, new.path);
            assert_eq!(old.blob == new.blob, old.path != "f2.md", "{}", old.path);
        }
        let heads = eng.heads().await.unwrap();
        assert_eq!(heads.tips.len(), 1);
    }

    #[tokio::test]
    async fn deleted_created_and_renamed_files_follow_the_directory() {
        let rig = Rig::new();
        let vault = rig.dir("vault");
        write(&vault, "apagar.md", b"x");
        write(&vault, "mover.md", b"y");
        write(&vault, "fica.md", b"z");
        let eng = rig.engine("pc");
        created(eng.snapshot(&vault_only(&vault), 1).await.unwrap());

        fs::remove_file(vault.join("apagar.md")).unwrap();
        fs::create_dir(vault.join("novo")).unwrap();
        fs::rename(vault.join("mover.md"), vault.join("novo/lugar.md")).unwrap();
        write(&vault, "criar.md", b"w");
        let (_, m, _, _) = created(eng.snapshot(&vault_only(&vault), 2).await.unwrap());
        assert_eq!(file_paths(&m), ["criar.md", "fica.md", "novo/lugar.md"]);
    }

    #[tokio::test]
    async fn a_disabled_class_keeps_the_entries_of_the_previous_tip() {
        let rig = Rig::new();
        let vault = rig.dir("vault");
        let config = rig.dir("config");
        write(&vault, "nota.md", b"v1");
        write(&config, "config.toml", b"a = 1");
        let both = Roots {
            vault: Some(vault.clone()),
            config: Some(config.clone()),
            secret: None,
        };
        let eng = rig.engine("pc");
        let (_, m1, _, _) = created(eng.snapshot(&both, 1).await.unwrap());

        // só o vault ligado; a config mudou no disco mas ninguém está olhando para ela
        write(&vault, "nota.md", b"v2");
        write(&config, "config.toml", b"a = 2");
        let (_, m2, n, _) = created(eng.snapshot(&vault_only(&vault), 2).await.unwrap());
        assert_eq!(n, 1);
        let cfg = |m: &Manifest| m.files.iter().find(|f| f.class == Class::Config).cloned();
        assert_eq!(cfg(&m2), cfg(&m1));
        assert_eq!(
            file_paths(&m2),
            ["nota.md", "config.toml"],
            "vault antes de config"
        );
    }

    #[tokio::test]
    async fn a_missing_root_is_an_error_and_writes_nothing() {
        let rig = Rig::new();
        let vault = rig.dir("vault");
        write(&vault, "a.md", b"1");
        let eng = rig.engine("pc");
        created(eng.snapshot(&vault_only(&vault), 1).await.unwrap());

        let gone = rig.tmp.path().join("desmontado");
        assert_eq!(
            eng.snapshot(&vault_only(&gone), 2).await,
            Err(EngineError::Io(io::ErrorKind::NotFound))
        );
        assert_eq!(rig.count(ObjectKind::Manifest).await, 1);
    }

    #[tokio::test]
    async fn two_tips_make_snapshot_refuse() {
        let rig = Rig::new();
        let vault = rig.dir("vault");
        write(&vault, "a.md", b"1");
        let eng = rig.engine("pc");
        created(eng.snapshot(&vault_only(&vault), 1).await.unwrap());
        // dois dispositivos enviaram a partir da mesma base: duas filhas da ponta
        let base = eng.heads().await.unwrap().tips.remove(0);
        for device in ["pc-a", "pc-b"] {
            let mut child = base.manifest.clone();
            child.base_version = Some(Hash32(base.id.hash));
            child.counter = 2;
            child.device = device.into();
            let sealed = child.seal(&keys(1)).unwrap();
            rig.store.put(&sealed.id, &sealed.bytes).await.unwrap();
        }

        assert_eq!(eng.heads().await.unwrap().tips.len(), 2);
        write(&vault, "a.md", b"2");
        assert_eq!(
            eng.snapshot(&vault_only(&vault), 9).await,
            Err(EngineError::Diverged {
                tips: 2,
                rejected: 0
            })
        );
        assert_eq!(rig.count(ObjectKind::Manifest).await, 3);
    }

    #[tokio::test]
    async fn an_unreadable_manifest_makes_snapshot_refuse_and_is_reported() {
        let rig = Rig::new();
        let vault = rig.dir("vault");
        write(&vault, "a.md", b"1");
        let eng = rig.engine("pc");
        let (id, ..) = created(eng.snapshot(&vault_only(&vault), 1).await.unwrap());
        fs::write(rig.store_dir().join(id.to_string()), b"adulterado").unwrap();

        let heads = eng.heads().await.unwrap();
        assert!(heads.tips.is_empty());
        assert_eq!(
            heads.rejected,
            [Rejected {
                id,
                error: EngineError::Store(StoreError::Corrupt)
            }]
        );
        write(&vault, "a.md", b"2");
        assert_eq!(
            eng.snapshot(&vault_only(&vault), 2).await,
            Err(EngineError::Diverged {
                tips: 0,
                rejected: 1
            })
        );
        assert_eq!(rig.count(ObjectKind::Blob).await, 1);
    }

    #[tokio::test]
    async fn a_manifest_from_another_epoch_is_rejected_not_a_tip() {
        let rig = Rig::new();
        let vault = rig.dir("vault");
        write(&vault, "a.md", b"1");
        let (id, ..) = created(
            rig.engine("pc")
                .snapshot(&vault_only(&vault), 1)
                .await
                .unwrap(),
        );
        let other = Engine::new(rig.store.clone(), keys(2), "pc").unwrap();
        let heads = other.heads().await.unwrap();
        assert!(heads.tips.is_empty());
        assert_eq!(
            heads.rejected,
            [Rejected {
                id,
                error: EngineError::Object(Error::WrongKeyEpoch {
                    found: 1,
                    expected: 2
                })
            }]
        );
    }

    #[tokio::test]
    async fn the_same_path_in_two_classes_is_backed_up_and_restored_to_each_class() {
        let rig = Rig::new();
        let vault = rig.dir("vault");
        let config = rig.dir("config");
        let secret = rig.dir("secret");
        write(&vault, "config.toml", b"nota com nome de config");
        write(&config, "config.toml", b"a = 1");
        write(&secret, "config.toml", b"{}");
        // caixa diferente entre classes também não colide: são pastas diferentes
        write(&vault, "Slack.json", b"nota");
        write(&secret, "slack.json", b"token");
        let roots = Roots {
            vault: Some(vault.clone()),
            config: Some(config.clone()),
            secret: Some(secret.clone()),
        };
        let eng = rig.engine("pc");
        let (id, m, new_blobs, skipped) = created(eng.snapshot(&roots, 1).await.unwrap());
        assert!(skipped.is_empty(), "{skipped:?}");
        assert_eq!(new_blobs, 5);
        let entries: Vec<(Class, &str)> =
            m.files.iter().map(|f| (f.class, f.path.as_str())).collect();
        assert_eq!(
            entries,
            [
                (Class::Vault, "Slack.json"),
                (Class::Vault, "config.toml"),
                (Class::Config, "config.toml"),
                (Class::Secret, "config.toml"),
                (Class::Secret, "slack.json"),
            ]
        );

        let target = rig.dir("restaurado");
        let restored_roots = Roots {
            vault: Some(target.join("vault")),
            config: Some(target.join("config")),
            secret: Some(target.join("secret")),
        };
        assert_eq!(
            eng.restore(&id, &restored_roots).await.unwrap(),
            Restored { files: 5 }
        );
        assert_eq!(tree(&target.join("vault")), tree(&vault));
        assert_eq!(tree(&target.join("config")), tree(&config));
        assert_eq!(tree(&target.join("secret")), tree(&secret));
    }

    /// Um segundo snapshot sem mudança não pode tratar o mesmo `path` de outra classe como
    /// "já visto": a entrada carregada da ponta é por `(classe, path)`.
    #[tokio::test]
    async fn an_unchanged_two_class_vault_snapshots_as_unchanged() {
        let rig = Rig::new();
        let vault = rig.dir("vault");
        let config = rig.dir("config");
        write(&vault, "config.toml", b"nota");
        write(&config, "config.toml", b"a = 1");
        let roots = Roots {
            vault: Some(vault),
            config: Some(config),
            secret: None,
        };
        let eng = rig.engine("pc");
        created(eng.snapshot(&roots, 1).await.unwrap());
        assert!(matches!(
            eng.snapshot(&roots, 2).await.unwrap(),
            Snapshot::Unchanged { .. }
        ));
    }

    #[tokio::test]
    async fn editing_one_class_keeps_the_same_path_of_the_other_class() {
        let rig = Rig::new();
        let vault = rig.dir("vault");
        let config = rig.dir("config");
        write(&vault, "config.toml", b"nota");
        write(&config, "config.toml", b"a = 1");
        let roots = Roots {
            vault: Some(vault.clone()),
            config: Some(config.clone()),
            secret: None,
        };
        let eng = rig.engine("pc");
        created(eng.snapshot(&roots, 1).await.unwrap());
        write(&config, "config.toml", b"a = 2");
        let (id, m, new_blobs, _) = created(eng.snapshot(&roots, 2).await.unwrap());
        assert_eq!(new_blobs, 1, "só o config mudou");

        let target = rig.dir("restaurado");
        let restored_roots = Roots {
            vault: Some(target.join("vault")),
            config: Some(target.join("config")),
            secret: None,
        };
        eng.restore(&id, &restored_roots).await.unwrap();
        assert_eq!(m.files.len(), 2);
        assert_eq!(fs::read(target.join("vault/config.toml")).unwrap(), b"nota");
        assert_eq!(
            fs::read(target.join("config/config.toml")).unwrap(),
            b"a = 2"
        );
    }

    #[tokio::test]
    async fn paths_differing_only_by_case_keep_the_first_and_skip_the_other() {
        let rig = Rig::new();
        let vault = rig.dir("vault");
        write(&vault, "A.md", b"1");
        write(&vault, "a.md", b"2");
        if fs::read_dir(&vault).unwrap().count() != 2 {
            return; // sistema de arquivos que ignora caixa: o caso não existe aqui
        }
        let (_, m, _, skipped) = created(
            rig.engine("pc")
                .snapshot(&vault_only(&vault), 1)
                .await
                .unwrap(),
        );
        assert_eq!(file_paths(&m), ["A.md"]);
        assert_eq!(skipped.len(), 1);
        assert_eq!(
            (skipped[0].path.as_str(), skipped[0].class),
            ("a.md", Class::Vault)
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn files_that_cannot_enter_are_skipped_and_do_not_block_the_snapshot() {
        let rig = Rig::new();
        let vault = rig.dir("vault");
        let outside = rig.dir("fora");
        write(&outside, "segredo.txt", b"nao deve entrar");
        write(&vault, "ok.md", b"1");
        write(&vault, "CON.md", b"2");
        write(&vault, "cafe\u{301}.md", b"3");
        std::os::unix::fs::symlink(outside.join("segredo.txt"), vault.join("link.md")).unwrap();
        fs::File::create(vault.join("grande.bin"))
            .unwrap()
            .set_len(crate::scan::MAX_FILE_BYTES + 1)
            .unwrap();

        let (_, m, new_blobs, mut skipped) = created(
            rig.engine("pc")
                .snapshot(&vault_only(&vault), 1)
                .await
                .unwrap(),
        );
        assert_eq!(file_paths(&m), ["ok.md"]);
        assert_eq!(new_blobs, 1);
        skipped.sort_by(|a, b| a.path.cmp(&b.path));
        let names: Vec<&str> = skipped.iter().map(|s| s.path.as_str()).collect();
        assert_eq!(names, ["CON.md", "cafe\u{301}.md", "grande.bin", "link.md"]);
        assert!(skipped
            .iter()
            .all(|s| s.class == Class::Vault && !s.reason.is_empty()));
    }

    #[tokio::test]
    async fn the_manifest_goes_last_so_a_failed_send_leaves_no_new_tip() {
        let rig = Rig::new();
        let vault = rig.dir("vault");
        write(&vault, "a.md", b"1");
        write(&vault, "b.md", b"2");
        let failing = Engine::new(FailManifestPut(rig.store.clone()), keys(1), "pc").unwrap();
        assert_eq!(
            failing.snapshot(&vault_only(&vault), 1).await,
            Err(EngineError::Store(StoreError::Unavailable(
                io::ErrorKind::TimedOut
            )))
        );
        // os blobs foram, o manifest não: o armazenamento segue válido e sem ponta
        assert_eq!(rig.count(ObjectKind::Blob).await, 2);
        assert_eq!(rig.count(ObjectKind::Manifest).await, 0);
        assert!(rig.engine("pc").heads().await.unwrap().tips.is_empty());

        // e um envio saudável depois funciona
        let (id, m, _, _) = created(
            rig.engine("pc")
                .snapshot(&vault_only(&vault), 2)
                .await
                .unwrap(),
        );
        assert_eq!(m.base_version, None);
        assert_eq!(rig.engine("pc").heads().await.unwrap().tips[0].id, id);
    }

    // ---------- restore ----------

    async fn snapshot_tree(rig: &Rig, vault: &Path) -> ObjectId {
        created(
            rig.engine("pc")
                .snapshot(&vault_only(vault), 1)
                .await
                .unwrap(),
        )
        .0
    }

    fn sample_tree(vault: &Path) {
        write(vault, "vazio.md", b"");
        write(vault, "notas/ação.md", "olá, ação\n".as_bytes());
        write(vault, ".dotfile", b"oculto");
        write(vault, "a/b/c/fundo.md", b"fundo");
        let big: Vec<u8> = (0..3 * 1024 * 1024).map(|i| (i % 251) as u8).collect();
        write(vault, "grande.bin", &big);
    }

    #[tokio::test]
    async fn restore_gives_back_every_byte() {
        let rig = Rig::new();
        let vault = rig.dir("vault");
        sample_tree(&vault);
        let id = snapshot_tree(&rig, &vault).await;

        let target = rig.tmp.path().join("restaurado");
        let restored = rig
            .engine("pc")
            .restore(&id, &vault_only(&target))
            .await
            .unwrap();
        assert_eq!(restored, Restored { files: 5 });
        assert_eq!(tree(&target), tree(&vault));
    }

    #[tokio::test]
    async fn restore_into_an_existing_empty_folder_works() {
        let rig = Rig::new();
        let vault = rig.dir("vault");
        write(&vault, "a.md", b"1");
        let id = snapshot_tree(&rig, &vault).await;
        let target = rig.dir("vazia");
        rig.engine("pc")
            .restore(&id, &vault_only(&target))
            .await
            .unwrap();
        assert_eq!(tree(&target), tree(&vault));
    }

    #[tokio::test]
    async fn restore_of_an_older_version_gives_the_older_content() {
        let rig = Rig::new();
        let vault = rig.dir("vault");
        write(&vault, "a.md", b"antigo");
        let eng = rig.engine("pc");
        let (first, ..) = created(eng.snapshot(&vault_only(&vault), 1).await.unwrap());
        write(&vault, "a.md", b"novo");
        write(&vault, "b.md", b"extra");
        created(eng.snapshot(&vault_only(&vault), 2).await.unwrap());

        let target = rig.tmp.path().join("v1");
        eng.restore(&first, &vault_only(&target)).await.unwrap();
        assert_eq!(
            tree(&target),
            BTreeMap::from([("a.md".to_string(), b"antigo".to_vec())])
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn restored_files_are_0600_and_created_folders_0700() {
        use std::os::unix::fs::PermissionsExt;
        let rig = Rig::new();
        let secret = rig.dir("logins");
        write(&secret, "oauth/token.json", b"{\"t\":1}");
        let roots = Roots {
            secret: Some(secret),
            ..Roots::default()
        };
        let eng = rig.engine("pc");
        let (id, ..) = created(eng.snapshot(&roots, 1).await.unwrap());

        let target = rig.tmp.path().join("novos-logins");
        eng.restore(
            &id,
            &Roots {
                secret: Some(target.clone()),
                ..Roots::default()
            },
        )
        .await
        .unwrap();
        let mode = |p: &Path| fs::metadata(p).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode(&target), 0o700);
        assert_eq!(mode(&target.join("oauth")), 0o700);
        assert_eq!(mode(&target.join("oauth/token.json")), 0o600);
    }

    #[tokio::test]
    async fn a_non_empty_target_is_refused_and_nothing_is_written_anywhere() {
        let rig = Rig::new();
        let vault = rig.dir("vault");
        let config = rig.dir("config");
        write(&vault, "a.md", b"1");
        write(&config, "config.toml", b"x");
        let both = Roots {
            vault: Some(vault.clone()),
            config: Some(config),
            secret: None,
        };
        let eng = rig.engine("pc");
        let (id, ..) = created(eng.snapshot(&both, 1).await.unwrap());

        let busy = rig.dir("ocupada");
        write(&busy, "dono.txt", b"nao mexa");
        let fresh = rig.tmp.path().join("nova-config");
        let roots = Roots {
            vault: Some(busy.clone()),
            config: Some(fresh.clone()),
            secret: None,
        };
        assert_eq!(
            eng.restore(&id, &roots).await,
            Err(EngineError::TargetNotEmpty)
        );
        assert_eq!(
            tree(&busy),
            BTreeMap::from([("dono.txt".to_string(), b"nao mexa".to_vec())])
        );
        assert!(!fresh.exists());
    }

    #[tokio::test]
    async fn a_tampered_blob_fails_the_restore_and_leaves_the_target_clean() {
        let rig = Rig::new();
        let vault = rig.dir("vault");
        write(&vault, "a.md", b"1");
        write(&vault, "b.md", b"2");
        let id = snapshot_tree(&rig, &vault).await;
        let tip = rig.engine("pc").heads().await.unwrap().tips.remove(0);
        let last = &tip.manifest.files[1];
        let blob = ObjectId {
            kind: ObjectKind::Blob,
            hash: last.blob.0,
        };
        fs::write(rig.store_dir().join(blob.to_string()), b"lixo").unwrap();

        let fresh = rig.tmp.path().join("novo");
        assert_eq!(
            rig.engine("pc").restore(&id, &vault_only(&fresh)).await,
            Err(EngineError::Store(StoreError::Corrupt))
        );
        assert!(
            !fresh.exists(),
            "a pasta criada pela restauração é removida"
        );

        let empty = rig.dir("ja-existia-vazia");
        assert!(rig
            .engine("pc")
            .restore(&id, &vault_only(&empty))
            .await
            .is_err());
        assert_eq!(
            fs::read_dir(&empty).unwrap().count(),
            0,
            "e a que já existia volta vazia"
        );
    }

    #[tokio::test]
    async fn a_manifest_that_lies_about_size_or_sha256_is_an_integrity_error() {
        let rig = Rig::new();
        let vault = rig.dir("vault");
        write(&vault, "a.md", b"conteudo");
        snapshot_tree(&rig, &vault).await;
        let bad_sha = put_forged(&rig, |m| m.files[0].sha256 = Hash32([9; 32])).await;
        let bad_size = put_forged(&rig, |m| m.files[0].size += 1).await;

        for id in [bad_sha, bad_size] {
            let fresh = rig.tmp.path().join("nova");
            assert_eq!(
                rig.engine("pc").restore(&id, &vault_only(&fresh)).await,
                Err(EngineError::Integrity)
            );
            assert!(!fresh.exists());
        }
    }

    #[tokio::test]
    async fn a_missing_blob_or_manifest_is_missing_object() {
        let rig = Rig::new();
        let vault = rig.dir("vault");
        write(&vault, "a.md", b"1");
        let id = snapshot_tree(&rig, &vault).await;
        let tip = rig.engine("pc").heads().await.unwrap().tips.remove(0);
        let blob = ObjectId {
            kind: ObjectKind::Blob,
            hash: tip.manifest.files[0].blob.0,
        };
        rig.store.delete(&blob).await.unwrap();

        let fresh = rig.tmp.path().join("novo");
        assert_eq!(
            rig.engine("pc").restore(&id, &vault_only(&fresh)).await,
            Err(EngineError::MissingObject)
        );
        assert!(!fresh.exists());
        let unknown = ObjectId::of(ObjectKind::Manifest, b"nunca existiu");
        assert_eq!(
            rig.engine("pc")
                .restore(&unknown, &vault_only(&fresh))
                .await,
            Err(EngineError::MissingObject)
        );
    }

    #[tokio::test]
    async fn the_wrong_epoch_or_the_wrong_key_cannot_restore() {
        let rig = Rig::new();
        let vault = rig.dir("vault");
        write(&vault, "a.md", b"1");
        let id = snapshot_tree(&rig, &vault).await;
        let fresh = rig.tmp.path().join("novo");

        let other_epoch = Engine::new(rig.store.clone(), keys(2), "pc").unwrap();
        assert_eq!(
            other_epoch.restore(&id, &vault_only(&fresh)).await,
            Err(EngineError::Object(Error::WrongKeyEpoch {
                found: 1,
                expected: 2
            }))
        );
        let other_key = Engine::new(
            rig.store.clone(),
            EpochKeys::derive(&VaultKey::from_bytes([4u8; 32]), 1),
            "pc",
        )
        .unwrap();
        assert_eq!(
            other_key.restore(&id, &vault_only(&fresh)).await,
            Err(EngineError::Object(Error::Decrypt))
        );
        assert!(!fresh.exists());
    }

    #[tokio::test]
    async fn a_forged_manifest_with_an_escaping_path_is_refused_and_nothing_escapes() {
        let rig = Rig::new();
        let vault = rig.dir("vault");
        write(&vault, "a.md", b"1");
        snapshot_tree(&rig, &vault).await;
        // contorna `Manifest::to_vec`/`seal` (que validam): serializa e cifra direto
        let tip = rig.engine("pc").heads().await.unwrap().tips.remove(0);
        let mut m = tip.manifest.clone();
        m.counter += 1;
        m.files[0].path = "../escapou.md".to_string();
        let json = serde_json::to_vec(&m).unwrap();
        let sealed = seal(&keys(1), ObjectKind::Manifest, &json).unwrap();
        rig.store.put(&sealed.id, &sealed.bytes).await.unwrap();

        let target = rig.dir("destino");
        let result = rig
            .engine("pc")
            .restore(&sealed.id, &vault_only(&target))
            .await;
        assert!(
            matches!(result, Err(EngineError::Object(Error::InvalidPath(_)))),
            "{result:?}"
        );
        assert!(!rig.tmp.path().join("escapou.md").exists());
        assert_eq!(fs::read_dir(&target).unwrap().count(), 0);
    }

    #[test]
    fn safe_join_accepts_normal_paths_and_stays_inside_the_root() {
        let root = Path::new("/tmp/raiz");
        for rel in ["a.md", "sub/dir/arquivo.md", ".oculto/x", "notas/ação.md"] {
            let joined = safe_join(root, rel).unwrap();
            assert!(joined.starts_with(root), "{rel}");
            assert_eq!(joined, root.join(rel));
        }
    }

    #[test]
    fn safe_join_refuses_anything_that_could_leave_the_root() {
        let root = Path::new("/tmp/raiz");
        for rel in [
            "",
            "/etc/passwd",
            "../x",
            "a/../../x",
            "a/..",
            "a//b",
            "./a",
            "a/./b",
            "a\\b",
            "a\0b",
        ] {
            assert!(
                matches!(
                    safe_join(root, rel),
                    Err(EngineError::Object(Error::InvalidPath(_)))
                ),
                "{rel:?}"
            );
        }
    }

    #[tokio::test]
    async fn a_class_without_a_target_is_not_restored() {
        let rig = Rig::new();
        let vault = rig.dir("vault");
        let secret = rig.dir("logins");
        write(&vault, "nota.md", b"1");
        write(&secret, "token.json", b"{}");
        let both = Roots {
            vault: Some(vault),
            config: None,
            secret: Some(secret),
        };
        let eng = rig.engine("pc");
        let (id, ..) = created(eng.snapshot(&both, 1).await.unwrap());

        let only_vault = rig.tmp.path().join("so-vault");
        let never = rig.tmp.path().join("nunca-logins");
        let restored = eng.restore(&id, &vault_only(&only_vault)).await.unwrap();
        assert_eq!(restored, Restored { files: 1 });
        assert_eq!(tree(&only_vault).keys().collect::<Vec<_>>(), ["nota.md"]);
        assert!(!never.exists());
    }

    /// O critério da Fase 1: perder a máquina e restaurar do zero em outra, só com o
    /// armazenamento copiado e a chave.
    #[tokio::test]
    async fn restore_from_zero_on_a_clean_machine() {
        let a = Rig::new();
        let vault = a.dir("vault");
        let config = a.dir("config");
        let secret = a.dir("logins");
        sample_tree(&vault);
        write(&config, "config.toml", b"modelo = \"x\"");
        write(&secret, "mcp/token.json", b"{\"t\":\"abc\"}");
        let roots = Roots {
            vault: Some(vault.clone()),
            config: Some(config.clone()),
            secret: Some(secret.clone()),
        };
        let (id, ..) = created(a.engine("maquina-a").snapshot(&roots, 1).await.unwrap());

        // máquina B: só a cópia do diretório do armazenamento e a chave
        let b = TempDir::new();
        copy_dir(&a.store_dir(), &b.path().join("store"));
        let store_b = Backend::Disk(DiskStore::open(b.path().join("store")).unwrap());
        let engine_b = Engine::new(store_b.clone(), keys(1), "maquina-b").unwrap();

        // a ponta aparece sem ninguém dizer o id
        let heads = engine_b.heads().await.unwrap();
        assert_eq!(heads.tips.iter().map(|t| t.id).collect::<Vec<_>>(), [id]);

        let targets = Roots {
            vault: Some(b.path().join("vault")),
            config: Some(b.path().join("config")),
            secret: Some(b.path().join("logins")),
        };
        let restored = engine_b.restore(&heads.tips[0].id, &targets).await.unwrap();
        assert_eq!(restored.files, 7);
        assert_eq!(tree(&b.path().join("vault")), tree(&vault));
        assert_eq!(tree(&b.path().join("config")), tree(&config));
        assert_eq!(tree(&b.path().join("logins")), tree(&secret));
        // tudo o que veio do armazenamento passou pela verificação de hash
        let manifest_id = heads.tips[0].id;
        assert!(verified_get(&store_b, &manifest_id)
            .await
            .unwrap()
            .is_some());
    }

    #[tokio::test]
    async fn engine_errors_never_leak_paths_or_file_names() {
        let rig = Rig::new();
        let vault = rig.dir("vault");
        write(&vault, "nome-secreto.md", b"1");
        let eng = rig.engine("pc");
        let (id, ..) = created(eng.snapshot(&vault_only(&vault), 1).await.unwrap());
        let busy = rig.dir("pasta-ocupada");
        write(&busy, "x", b"1");

        let errors = [
            eng.restore(&id, &vault_only(&busy)).await.unwrap_err(),
            eng.snapshot(&vault_only(&rig.tmp.path().join("sumiu-daqui")), 2)
                .await
                .unwrap_err(),
            EngineError::Diverged {
                tips: 2,
                rejected: 0,
            },
            EngineError::Integrity,
            EngineError::MissingObject,
        ];
        for e in errors {
            let text = format!("{e} {e:?}");
            for forbidden in [
                "arca-test",
                "nome-secreto",
                "pasta-ocupada",
                "sumiu-daqui",
                "/tmp",
            ] {
                assert!(!text.contains(forbidden), "{forbidden} em {text}");
            }
        }
    }
}
