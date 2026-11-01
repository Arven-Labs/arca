//! Núcleo local: junta cripto, manifest e `BlobStore` para **guardar o Vault e restaurá-lo
//! do zero** (`SPEC-TECNICA.md` §6, "Envio" e "Recebimento", sem a parte de rede). Sync entre
//! dispositivos, junção de pontas e regra de conflito são da Fase 2.

use crate::apply::{commit, finish, is_reserved_name, plan, stage, undo_all, Item, Kind, Undo};
use crate::crypto::{open, seal, EpochKeys};
use crate::disk::{create_private_dir, write_atomic};
use crate::envelope::Header;
use crate::error::Error;
use crate::heads::{tips, Head};
use crate::manifest::{validate_device, validate_path, Class, FileEntry, Hash32, Manifest};
use crate::merge::{common_ancestor, merge_files, Conflict, Side};
use crate::object::{ObjectId, ObjectKind};
use crate::scan::{scan_class, Candidate, Skipped, MAX_FILE_BYTES};
use crate::store::{verified_get, BlobStore, StoreError};
use crate::FORMAT_VERSION;
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet, HashSet};
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
    /// divergência silenciosa. Várias pontas se juntam com `Engine::merge`; ilegíveis
    /// continuam bloqueando (P-14).
    #[error(
        "storage has {tips} tips and {rejected} unreadable manifests; refusing to build on it \
         (join tips with merge)"
    )]
    Diverged { tips: usize, rejected: usize },
    /// Existe manifest de uma época mais nova que a das chaves deste dispositivo: ele precisa
    /// da chave nova (re-parear, ADR 0006) e não deve construir sobre uma época velha.
    #[error("storage has a newer key epoch ({found}) than this device's keys ({have})")]
    NeedsNewKey { have: u32, found: u32 },
    /// O nome da cópia `.conflito-*` não cabe num caminho válido (P-26): nada foi gravado.
    #[error("conflict copy name does not fit in a valid path")]
    ConflictCopyName,
    /// Um arquivo que o `apply` mudaria já difere do estado de origem no disco (edição local
    /// ainda sem snapshot), ou o caminho passa por um symlink: nada foi escrito nem apagado.
    #[error("the folder has local changes in files the apply would touch; take a snapshot first")]
    LocalChanges,
    /// Uma entrada do manifest tem nome de temporário da arca (`.arca-new-*`, `.arca-old-*`,
    /// `.tmp-*`): o `apply` recusa em vez de colidir com os próprios arquivos de trabalho.
    #[error("a manifest entry uses a name reserved for temporary files")]
    ReservedName,
    /// O `apply` falhou e, ao desfazer, o original não pôde voltar (a pessoa recriou o arquivo
    /// no meio-tempo, ou a pasta não deixa): ele **ficou ao lado** como `.arca-old-<n>`, e
    /// nada foi sobrescrito.
    #[error(
        "the apply failed and a previous version could not be put back; it was kept beside it"
    )]
    OriginalKept,
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

/// Resultado de ler um manifest com as chaves do dispositivo.
enum Read {
    Opened(Manifest),
    /// É da época das chaves, mas não abriu (tag, JSON ou regra do manifest).
    Unreadable(EngineError),
    /// Cabeçalho de outra época: não é aberto aqui.
    OtherEpoch(u32),
}

/// Resultado de `Engine::load`: o grafo legível e o que ficou de fora.
struct Loaded {
    readable: Vec<(ObjectId, Manifest)>,
    rejected: Vec<Rejected>,
    superseded: Vec<ObjectId>,
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
    /// Manifests de épocas anteriores à maior vista (P-24): histórico, nunca pontas, não
    /// bloqueiam o snapshot. Ordenados por hash.
    pub superseded: Vec<ObjectId>,
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
pub enum MergeOutcome {
    /// Zero ou uma ponta: nada a fundir, nada gravado.
    Nothing { head: Option<ObjectId> },
    /// Gravou um manifest de fusão para as duas primeiras pontas (ordem `(counter, hash)`).
    Merged {
        id: ObjectId,
        manifest: Manifest,
        conflicts: Vec<Conflict>,
        /// Pontas que existem depois desta fusão; repetir `merge` até chegar a 1.
        tips_after: usize,
    },
}

/// Resultado de `Engine::apply`: o que mudou na pasta viva, por entrada do manifest.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Applied {
    pub created: usize,
    pub updated: usize,
    pub removed: usize,
    /// Já estava no estado de destino (igual nos dois manifests, ou já aplicado antes).
    pub unchanged: usize,
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

    /// Lê todos os manifests do armazenamento e calcula as pontas. Só a **maior época** vista
    /// (lida do cabeçalho, em claro) conta (P-24): as anteriores vão para `superseded`, sem
    /// serem abertas. Se a maior época é mais nova que a das chaves, `NeedsNewKey`. Um manifest
    /// ilegível da época que conta vai para `rejected` e nunca vira ponta; backend fora do ar
    /// aborta (não é "ilegível").
    pub async fn heads(&self) -> Result<Heads, EngineError> {
        let Loaded {
            readable,
            rejected,
            superseded,
        } = self.load().await?;
        Ok(Heads {
            tips: tips(readable),
            rejected,
            superseded,
        })
    }

    /// Lê todos os manifests e os classifica (ver `heads`). `readable` é o grafo inteiro da
    /// época que conta, não só as pontas.
    async fn load(&self) -> Result<Loaded, EngineError> {
        let mine = self.keys.epoch();
        let mut readable = Vec::new();
        let mut rejected = Vec::new();
        // (id, época do cabeçalho) dos que este dispositivo não abriu
        let mut others: Vec<(ObjectId, u32)> = Vec::new();
        let mut mine_seen = false;
        for id in self.store.list(ObjectKind::Manifest).await? {
            match self.classify_manifest(&id).await {
                Ok(Read::Opened(manifest)) => {
                    mine_seen = true;
                    readable.push((id, manifest));
                }
                Ok(Read::Unreadable(error)) => {
                    mine_seen = true;
                    rejected.push(Rejected { id, error });
                }
                Ok(Read::OtherEpoch(epoch)) => others.push((id, epoch)),
                Err(e @ EngineError::Store(StoreError::Unavailable(_))) => return Err(e),
                // sem época conhecida (corrompido, ausente, cabeçalho inválido)
                Err(error) => rejected.push(Rejected { id, error }),
            }
        }
        let newest_other = others.iter().map(|(_, epoch)| *epoch).max();
        if let Some(found) = newest_other.filter(|epoch| *epoch > mine) {
            return Err(EngineError::NeedsNewKey { have: mine, found });
        }
        // Daqui em diante nenhuma época em `others` passa da minha. Se há manifest meu (aberto
        // ou ilegível na minha época), a minha é a maior; senão, a maior de `others` conta e,
        // sem as chaves dela, é `rejected` com `WrongKeyEpoch`, como antes do P-24.
        let counting = if mine_seen { Some(mine) } else { newest_other };
        let mut superseded = Vec::new();
        for (id, epoch) in others {
            if Some(epoch) == counting {
                let error = EngineError::Object(Error::WrongKeyEpoch {
                    found: epoch,
                    expected: mine,
                });
                rejected.push(Rejected { id, error });
            } else {
                superseded.push(id);
            }
        }
        rejected.sort_by_key(|r| r.id.hash);
        superseded.sort_by_key(|id| id.hash);
        Ok(Loaded {
            readable,
            rejected,
            superseded,
        })
    }

    /// Abre o manifest com as chaves do dispositivo (erro se for de outra época).
    async fn read_manifest(&self, id: &ObjectId) -> Result<Manifest, EngineError> {
        let bytes = verified_get(&self.store, id)
            .await?
            .ok_or(EngineError::MissingObject)?;
        Ok(Manifest::open(&self.keys, &bytes)?)
    }

    /// Para `heads`: abre o manifest se ele é da época das chaves; senão devolve só a época
    /// do cabeçalho.
    async fn classify_manifest(&self, id: &ObjectId) -> Result<Read, EngineError> {
        let bytes = verified_get(&self.store, id)
            .await?
            .ok_or(EngineError::MissingObject)?;
        let (header, _) = Header::decode(&bytes)?;
        if header.key_epoch != self.keys.epoch() {
            return Ok(Read::OtherEpoch(header.key_epoch));
        }
        Ok(match Manifest::open(&self.keys, &bytes) {
            Ok(manifest) => Read::Opened(manifest),
            Err(e) => Read::Unreadable(e.into()),
        })
    }

    /// Funde as duas primeiras pontas (ordem `(counter, hash)`) num manifest de fusão
    /// (`SPEC-TECNICA.md` §7, ADR 0014). Uma chamada resolve um par; repetir até `tips_after`
    /// chegar a 1.
    pub async fn merge(&self, now: u64) -> Result<MergeOutcome, EngineError> {
        let Loaded {
            readable, rejected, ..
        } = self.load().await?;
        let heads = tips(readable.clone());
        // sem o grafo completo a base pode estar errada: mesma recusa do `snapshot`
        if !rejected.is_empty() {
            return Err(EngineError::Diverged {
                tips: heads.len(),
                rejected: rejected.len(),
            });
        }
        let [first, second, ..] = heads.as_slice() else {
            return Ok(MergeOutcome::Nothing {
                head: heads.first().map(|h| h.id),
            });
        };

        // `first` vem antes de `second` em (counter, hash): `second` é a maior ponta
        let base_files: &[FileEntry] = common_ancestor(&readable, &first.id, &second.id)
            .and_then(|id| readable.iter().find(|(other, _)| *other == id))
            .map_or(&[], |(_, manifest)| &manifest.files);
        fn side(head: &Head) -> Side<'_> {
            Side {
                id: head.id,
                manifest: &head.manifest,
            }
        }
        let merged = merge_files(base_files, side(first), side(second)).map_err(|e| match e {
            Error::InvalidPath(_) => EngineError::ConflictCopyName,
            other => EngineError::Object(other),
        })?;

        // o manifest vai por último, e nunca cita um blob que não está aqui
        for file in &merged.files {
            let blob = ObjectId {
                kind: ObjectKind::Blob,
                hash: file.blob.0,
            };
            if !self.store.has(&blob).await? {
                return Err(EngineError::MissingObject);
            }
        }
        let manifest = Manifest {
            format_version: FORMAT_VERSION as u16,
            key_epoch: self.keys.epoch(),
            base_version: Some(Hash32(second.id.hash)),
            merged_from: vec![Hash32(first.id.hash)],
            counter: first
                .manifest
                .counter
                .max(second.manifest.counter)
                .saturating_add(1),
            device: self.device.clone(),
            created_at: now,
            files: merged.files,
        };
        let sealed = manifest.seal(&self.keys)?;
        self.store.put(&sealed.id, &sealed.bytes).await?;
        Ok(MergeOutcome::Merged {
            id: sealed.id,
            manifest,
            conflicts: merged.conflicts,
            // duas pontas viram uma
            tips_after: heads.len() - 1,
        })
    }

    /// Fotografa as pastas de `roots` e grava o estado como um manifest novo (blobs primeiro,
    /// manifest por último). `now` são segundos Unix (UTC), injetados para testes determinísticos.
    pub async fn snapshot(&self, roots: &Roots, now: u64) -> Result<Snapshot, EngineError> {
        let Heads { tips, rejected, .. } = self.heads().await?;
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

    /// Leva a pasta viva do estado `from` (o manifest que ela reflete; `None` = nenhum) ao de
    /// `to`: cria, atualiza e remove só o que difere, tudo ou nada. Recusa com `LocalChanges`
    /// se um arquivo do diff já foi alterado no disco, sem escrever nada.
    pub async fn apply(
        &self,
        from: Option<&ObjectId>,
        to: &ObjectId,
        roots: &Roots,
    ) -> Result<Applied, EngineError> {
        let from_manifest = match from {
            Some(id) => Some(self.read_manifest(id).await?),
            None => None,
        };
        let to_manifest = self.read_manifest(to).await?;
        let enabled = roots.enabled();

        // o diff por (classe, path), só das classes com pasta ligada
        let index = |files: &'_ [FileEntry]| -> BTreeMap<(Class, String), FileEntry> {
            files
                .iter()
                .map(|f| ((f.class, f.path.clone()), f.clone()))
                .collect()
        };
        let from_ix = index(from_manifest.as_ref().map_or(&[], |m| &m.files));
        let to_ix = index(&to_manifest.files);
        let keys: BTreeSet<&(Class, String)> = from_ix.keys().chain(to_ix.keys()).collect();
        let mut items = Vec::new();
        let mut same = 0;
        for key in keys {
            let Some((_, root)) = enabled.iter().find(|(class, _)| *class == key.0) else {
                continue;
            };
            // o nome de um temporário do próprio `apply` não pode ser destino nem origem
            if is_reserved_name(key.1.rsplit('/').next().unwrap_or(&key.1)) {
                return Err(EngineError::ReservedName);
            }
            let (f, t) = (from_ix.get(key), to_ix.get(key));
            let kind = match (f, t) {
                (Some(f), Some(t)) if f.sha256 == t.sha256 => {
                    same += 1;
                    continue;
                }
                (Some(_), Some(_)) => Kind::Update,
                (None, Some(_)) => Kind::Create,
                (Some(_), None) => Kind::Remove,
                (None, None) => continue,
            };
            items.push(Item {
                kind,
                root: root.to_path_buf(),
                dest: safe_join(root, &key.1)?,
                from: f.cloned(),
                to: t.cloned(),
            });
        }

        // 1. conferir o disco sem escrever
        let (planned, already) = run(move || plan(items)).await?;
        let counts = |kind: Kind| planned.iter().filter(|p| p.kind == kind).count();
        let applied = Applied {
            created: counts(Kind::Create),
            updated: counts(Kind::Update),
            removed: counts(Kind::Remove),
            unchanged: same + already,
        };

        // 2. buscar, verificar e gravar o conteúdo novo ao lado dos destinos
        let mut log = Vec::new();
        for p in planned.iter().filter(|p| p.kind != Kind::Remove) {
            let Some(entry) = &p.to else { continue };
            let plain = match self.plain_blob(entry).await {
                Ok(plain) => plain,
                Err(e) => return Err(rollback(log, e).await),
            };
            let one = p.clone();
            let (back, result) = with_log(log, move |log| stage(log, &one, &plain)).await;
            log = back;
            if let Err(e) = result {
                return Err(rollback(log, e).await);
            }
        }

        // 3. trocar; qualquer falha devolve tudo como estava
        let all = planned.clone();
        let (log, result) = with_log(log, move |log| commit(log, &all)).await;
        if let Err(e) = result {
            return Err(rollback(log, e).await);
        }
        run(move || {
            finish(&planned);
            Ok(())
        })
        .await?;
        Ok(applied)
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

    /// Busca o blob da entrada, decifra e confere `size` e `sha256` do texto claro.
    async fn plain_blob(&self, entry: &FileEntry) -> Result<Zeroizing<Vec<u8>>, EngineError> {
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
        Ok(plain)
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
            let plain = self.plain_blob(entry).await?;
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

/// Como `run`, mas passa o diário de desfazer para dentro e o devolve junto com o resultado.
/// Se a tarefa morrer (pânico), o diário se perde: o erro é genérico.
async fn with_log<T, F>(log: Vec<Undo>, f: F) -> (Vec<Undo>, Result<T, EngineError>)
where
    T: Send + 'static,
    F: FnOnce(&mut Vec<Undo>) -> Result<T, EngineError> + Send + 'static,
{
    let task = tokio::task::spawn_blocking(move || {
        let mut log = log;
        let result = f(&mut log);
        (log, result)
    });
    match task.await {
        Ok(pair) => pair,
        Err(_) => (Vec::new(), Err(EngineError::Io(io::ErrorKind::Other))),
    }
}

/// Desfaz o diário e devolve o erro que causou a volta.
/// Se algum original não pôde voltar (o destino foi recriado), o erro passa a ser `OriginalKept`.
async fn rollback(log: Vec<Undo>, error: EngineError) -> EngineError {
    match tokio::task::spawn_blocking(move || undo_all(log)).await {
        Ok(kept) if !kept.is_empty() => EngineError::OriginalKept,
        _ => error,
    }
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

    // ---------- épocas (P-24) ----------

    /// Grava um manifest vazio da época `epoch` (como se fosse o primeiro de uma rotação, ou
    /// um publicado por um dispositivo que ficou na chave velha).
    async fn put_epoch_manifest(
        rig: &Rig,
        epoch: u32,
        base: Option<ObjectId>,
        counter: u64,
    ) -> ObjectId {
        let m = Manifest {
            format_version: FORMAT_VERSION as u16,
            key_epoch: epoch,
            base_version: base.map(|id| Hash32(id.hash)),
            merged_from: vec![],
            counter,
            device: "pc".to_string(),
            created_at: 1,
            files: vec![],
        };
        let sealed = m.seal(&keys(epoch)).unwrap();
        rig.store.put(&sealed.id, &sealed.bytes).await.unwrap();
        sealed.id
    }

    fn engine_at(rig: &Rig, epoch: u32) -> Engine<Backend> {
        Engine::new(rig.store.clone(), keys(epoch), "pc").unwrap()
    }

    /// Época 1 com um snapshot real e, depois da rotação, a época 2 (cadeia nova, base nula).
    async fn rotated(rig: &Rig, vault: &Path) -> (ObjectId, ObjectId) {
        write(vault, "a.md", b"1");
        let (old, ..) = created(
            rig.engine("pc")
                .snapshot(&vault_only(vault), 1)
                .await
                .unwrap(),
        );
        let new = put_epoch_manifest(rig, 2, None, 1).await;
        (old, new)
    }

    #[tokio::test]
    async fn after_a_rotation_only_the_newest_epoch_has_tips_and_the_old_ones_are_superseded() {
        let rig = Rig::new();
        let vault = rig.dir("vault");
        let (old, new) = rotated(&rig, &vault).await;

        let heads = engine_at(&rig, 2).heads().await.unwrap();
        assert_eq!(ids_of(&heads), [new]);
        assert!(heads.rejected.is_empty());
        assert_eq!(heads.superseded, [old]);
    }

    #[tokio::test]
    async fn snapshot_builds_on_the_new_epoch_despite_the_superseded_ones() {
        let rig = Rig::new();
        let vault = rig.dir("vault");
        let (_, new) = rotated(&rig, &vault).await;

        let (_, m, _, _) = created(
            engine_at(&rig, 2)
                .snapshot(&vault_only(&vault), 2)
                .await
                .unwrap(),
        );
        assert_eq!(m.key_epoch, 2);
        assert_eq!(m.base_version, Some(Hash32(new.hash)));
        assert_eq!(file_paths(&m), ["a.md"]);
    }

    /// Um dispositivo que ficou na chave velha (e publicou depois da rotação, com contador
    /// maior) nunca vira ponta nem desfaz a época nova.
    #[tokio::test]
    async fn an_old_epoch_manifest_published_after_the_rotation_never_becomes_a_tip() {
        let rig = Rig::new();
        let vault = rig.dir("vault");
        let (old, new) = rotated(&rig, &vault).await;
        let late = put_epoch_manifest(&rig, 1, Some(old), 99).await;

        let heads = engine_at(&rig, 2).heads().await.unwrap();
        assert_eq!(ids_of(&heads), [new]);
        let mut superseded = vec![old, late];
        superseded.sort_by_key(|id| id.hash);
        assert_eq!(heads.superseded, superseded);
    }

    #[tokio::test]
    async fn a_device_on_an_old_key_is_told_it_needs_the_new_key_and_writes_nothing() {
        let rig = Rig::new();
        let vault = rig.dir("vault");
        rotated(&rig, &vault).await;
        let before = rig.count(ObjectKind::Manifest).await;

        let stale = rig.engine("pc"); // chaves da época 1
        let expected = EngineError::NeedsNewKey { have: 1, found: 2 };
        assert_eq!(stale.heads().await, Err(expected.clone()));
        write(&vault, "b.md", b"2");
        assert_eq!(stale.snapshot(&vault_only(&vault), 3).await, Err(expected));
        assert_eq!(rig.count(ObjectKind::Manifest).await, before);
        assert_eq!(rig.count(ObjectKind::Blob).await, 1);
    }

    /// A chave velha ainda lê o passado: restaurar uma versão da época 1 funciona depois da
    /// rotação (ADR 0006: o que foi publicado continua legível por quem tem a chave antiga).
    #[tokio::test]
    async fn restoring_an_old_epoch_version_still_works_with_the_old_keys() {
        let rig = Rig::new();
        let vault = rig.dir("vault");
        let (old, _) = rotated(&rig, &vault).await;

        let target = rig.tmp.path().join("restaurado");
        rig.engine("pc")
            .restore(&old, &vault_only(&target))
            .await
            .unwrap();
        assert_eq!(tree(&target), tree(&vault));
    }

    /// Um manifest ilegível dentro da maior época continua bloqueando o snapshot (P-14).
    #[tokio::test]
    async fn an_unreadable_manifest_in_the_newest_epoch_still_blocks() {
        let rig = Rig::new();
        let vault = rig.dir("vault");
        let (_, new) = rotated(&rig, &vault).await;
        let broken = put_epoch_manifest(&rig, 2, Some(new), 2).await;
        fs::write(rig.store_dir().join(broken.to_string()), b"adulterado").unwrap();

        let eng = engine_at(&rig, 2);
        let heads = eng.heads().await.unwrap();
        assert_eq!(ids_of(&heads), [new]);
        assert_eq!(heads.rejected.len(), 1);
        assert_eq!(
            eng.snapshot(&vault_only(&vault), 2).await,
            Err(EngineError::Diverged {
                tips: 1,
                rejected: 1
            })
        );
    }

    // ---------- fusão (Fase 2a, PR 2) ----------

    /// Cópia do armazenamento de `rig` num `Rig` novo: outro dispositivo, outro disco.
    fn forked(rig: &Rig) -> Rig {
        let other = Rig::new();
        copy_dir(&rig.store_dir(), &other.store_dir());
        other
    }

    /// Junta os objetos de `from` no armazenamento de `into`, como uma replicação.
    fn absorb(into: &Rig, from: &Rig) {
        copy_dir(&from.store_dir(), &into.store_dir());
    }

    struct Diverged {
        rig: Rig,
        _others: Vec<Rig>,
        tips: Vec<ObjectId>,
    }

    type Branch<'a> = (&'a str, u64, Vec<(&'a str, &'a [u8])>);

    /// Um Vault base e vários dispositivos que partem dele sem se ver: cada ramo é
    /// `(dispositivo, created_at, edições)`. No fim, `rig` tem o base e todas as pontas.
    async fn diverge(base: &[(&str, &[u8])], branches: Vec<Branch<'_>>) -> Diverged {
        let rig = Rig::new();
        let vault = rig.dir("vault");
        for (path, data) in base {
            write(&vault, path, data);
        }
        let (base_id, ..) = created(
            rig.engine("base")
                .snapshot(&vault_only(&vault), 1)
                .await
                .unwrap(),
        );
        let (mut others, mut tips) = (Vec::new(), Vec::new());
        for (device, at, edits) in branches {
            let other = forked(&rig);
            let dir = other.tmp.path().join("vault-ramo");
            let eng = other.engine(device);
            eng.restore(&base_id, &vault_only(&dir)).await.unwrap();
            for (path, data) in edits {
                write(&dir, path, data);
            }
            let (id, ..) = created(eng.snapshot(&vault_only(&dir), at).await.unwrap());
            tips.push(id);
            others.push(other);
        }
        for other in &others {
            absorb(&rig, other);
        }
        Diverged {
            rig,
            _others: others,
            tips,
        }
    }

    fn contents(pairs: &[(&str, &[u8])]) -> BTreeMap<String, Vec<u8>> {
        pairs
            .iter()
            .map(|(path, data)| (path.to_string(), data.to_vec()))
            .collect()
    }

    async fn restored_tree(rig: &Rig, id: &ObjectId, name: &str) -> BTreeMap<String, Vec<u8>> {
        let target = rig.tmp.path().join(name);
        rig.engine("pc")
            .restore(id, &vault_only(&target))
            .await
            .unwrap();
        tree(&target)
    }

    #[tokio::test]
    async fn merge_without_divergence_does_nothing() {
        let rig = Rig::new();
        let eng = rig.engine("pc");
        assert_eq!(
            eng.merge(5).await.unwrap(),
            MergeOutcome::Nothing { head: None }
        );
        let vault = rig.dir("vault");
        write(&vault, "a.md", b"1");
        let (id, ..) = created(eng.snapshot(&vault_only(&vault), 1).await.unwrap());
        assert_eq!(
            eng.merge(5).await.unwrap(),
            MergeOutcome::Nothing { head: Some(id) }
        );
        assert_eq!(rig.count(ObjectKind::Manifest).await, 1);
    }

    #[tokio::test]
    async fn merging_two_tips_with_disjoint_edits_joins_the_graph() {
        let d = diverge(
            &[("a.md", b"1"), ("b.md", b"1")],
            vec![
                ("pa", 10, vec![("a.md", b"A")]),
                ("pb", 20, vec![("b.md", b"B")]),
            ],
        )
        .await;
        let eng = d.rig.engine("pc");
        let MergeOutcome::Merged {
            id,
            manifest,
            conflicts,
            tips_after,
        } = eng.merge(30).await.unwrap()
        else {
            panic!("esperava Merged");
        };
        assert!(conflicts.is_empty());
        assert_eq!(tips_after, 1);

        // a maior ponta por (counter, hash) é a base; a outra vem em `merged_from`
        let mut parents = d.tips.clone(); // os dois têm counter 2: decide o hash
        parents.sort_by_key(|i| i.hash);
        assert_eq!(manifest.base_version, Some(Hash32(parents[1].hash)));
        assert_eq!(manifest.merged_from, vec![Hash32(parents[0].hash)]);
        assert_eq!(
            (
                manifest.counter,
                manifest.created_at,
                manifest.device.as_str(),
                manifest.key_epoch
            ),
            (3, 30, "pc", 1)
        );

        let heads = eng.heads().await.unwrap();
        assert_eq!(ids_of(&heads), [id]);
        assert!(heads.rejected.is_empty());
        assert_eq!(
            restored_tree(&d.rig, &id, "restaurado").await,
            contents(&[("a.md", b"A"), ("b.md", b"B")])
        );
    }

    #[tokio::test]
    async fn merging_a_conflict_keeps_the_newer_and_a_dated_copy_of_the_other() {
        use crate::merge::ConflictKind;
        let d = diverge(
            &[("a.md", b"1")],
            vec![
                ("pa", 10, vec![("a.md", b"from-a")]),
                ("pb", 20, vec![("a.md", b"from-b")]),
            ],
        )
        .await;
        let MergeOutcome::Merged { id, conflicts, .. } =
            d.rig.engine("pc").merge(30).await.unwrap()
        else {
            panic!("esperava Merged");
        };
        let copy = "a.md.conflito-pa-19700101T000010Z.md";
        assert_eq!(conflicts.len(), 1);
        assert_eq!(conflicts[0].kind, ConflictKind::BothEdited);
        assert_eq!(conflicts[0].loser_device, "pa");
        assert_eq!(conflicts[0].copy_path.as_deref(), Some(copy));
        // o mais novo (pb, 20) fica no lugar; nada se perde
        assert_eq!(
            restored_tree(&d.rig, &id, "restaurado").await,
            contents(&[("a.md", b"from-b"), (copy, b"from-a")])
        );
    }

    /// Três dispositivos publicaram sem se ver: cada chamada resolve um par.
    #[tokio::test]
    async fn three_tips_take_two_merge_calls() {
        let d = diverge(
            &[("a.md", b"1")],
            vec![
                ("pa", 10, vec![("x.md", b"X")]),
                ("pb", 20, vec![("y.md", b"Y")]),
                ("pc", 30, vec![("z.md", b"Z")]),
            ],
        )
        .await;
        let eng = d.rig.engine("pd");
        let heads = eng.heads().await.unwrap();
        assert_eq!(heads.tips.len(), 3);

        // o par é o das duas primeiras pontas na ordem (counter, hash)
        let lowest = (heads.tips[0].id, heads.tips[1].id);
        let MergeOutcome::Merged {
            manifest,
            tips_after,
            ..
        } = eng.merge(40).await.unwrap()
        else {
            panic!("esperava Merged");
        };
        assert_eq!(tips_after, 2);
        assert_eq!(manifest.base_version, Some(Hash32(lowest.1.hash)));
        assert_eq!(manifest.merged_from, vec![Hash32(lowest.0.hash)]);
        assert_eq!(eng.heads().await.unwrap().tips.len(), 2);

        let MergeOutcome::Merged { id, tips_after, .. } = eng.merge(41).await.unwrap() else {
            panic!("esperava Merged");
        };
        assert_eq!(tips_after, 1);
        assert_eq!(
            eng.merge(42).await.unwrap(),
            MergeOutcome::Nothing { head: Some(id) }
        );
        assert_eq!(
            restored_tree(&d.rig, &id, "restaurado").await,
            contents(&[
                ("a.md", b"1"),
                ("x.md", b"X"),
                ("y.md", b"Y"),
                ("z.md", b"Z")
            ])
        );
    }

    /// Dois dispositivos fundem o mesmo par ao mesmo tempo, em armazenamentos separados; ao
    /// juntar os objetos, o grafo tem uma ponta só (ADR 0014) e o snapshot seguinte funciona.
    #[tokio::test]
    async fn two_devices_merging_the_same_pair_converge_to_one_tip() {
        let d = diverge(
            &[("a.md", b"1"), ("b.md", b"1")],
            vec![
                ("pa", 10, vec![("a.md", b"A")]),
                ("pb", 20, vec![("b.md", b"B")]),
            ],
        )
        .await;
        let other = forked(&d.rig);
        let merged = |out: MergeOutcome| match out {
            MergeOutcome::Merged { id, .. } => id,
            other => panic!("esperava Merged, veio {other:?}"),
        };
        let first = merged(d.rig.engine("pa").merge(30).await.unwrap());
        let second = merged(other.engine("pb").merge(31).await.unwrap());
        assert_ne!(first, second, "nonce aleatório: ids diferentes");
        absorb(&d.rig, &other);

        let eng = d.rig.engine("pc");
        let heads = eng.heads().await.unwrap();
        let expected = if first.hash < second.hash {
            first
        } else {
            second
        };
        assert_eq!(ids_of(&heads), [expected]);

        // quem continua a partir daí enxerga o estado fundido e constrói em cima dele
        let work = d.rig.dir("trabalho");
        eng.restore(&expected, &vault_only(&work)).await.unwrap();
        assert!(matches!(
            eng.snapshot(&vault_only(&work), 40).await.unwrap(),
            Snapshot::Unchanged { head, .. } if head == expected
        ));
        write(&work, "c.md", b"C");
        let (_, next, ..) = created(eng.snapshot(&vault_only(&work), 41).await.unwrap());
        assert_eq!(next.base_version, Some(Hash32(expected.hash)));
        assert_eq!(eng.heads().await.unwrap().tips.len(), 1);
    }

    #[tokio::test]
    async fn an_unreadable_manifest_blocks_the_merge() {
        let d = diverge(
            &[("a.md", b"1")],
            vec![
                ("pa", 10, vec![("a.md", b"A")]),
                ("pb", 20, vec![("a.md", b"B")]),
            ],
        )
        .await;
        fs::write(d.rig.store_dir().join(d.tips[0].to_string()), b"adulterado").unwrap();
        assert_eq!(
            d.rig.engine("pc").merge(30).await,
            Err(EngineError::Diverged {
                tips: 1,
                rejected: 1
            })
        );
        assert_eq!(d.rig.count(ObjectKind::Manifest).await, 3);
    }

    /// Um manifest que cita um blob que não está aqui nunca é publicado.
    #[tokio::test]
    async fn a_missing_blob_stops_the_merge_before_writing() {
        let d = diverge(
            &[("a.md", b"1")],
            vec![
                ("pa", 10, vec![("a.md", b"A")]),
                ("pb", 20, vec![("b.md", b"B")]),
            ],
        )
        .await;
        let eng = d.rig.engine("pc");
        let heads = eng.heads().await.unwrap();
        let tip = heads.tips.iter().find(|h| h.id == d.tips[1]).unwrap();
        let blob = tip
            .manifest
            .files
            .iter()
            .find(|f| f.path == "b.md")
            .unwrap()
            .blob;
        let blob_id = ObjectId {
            kind: ObjectKind::Blob,
            hash: blob.0,
        };
        fs::remove_file(d.rig.store_dir().join(blob_id.to_string())).unwrap();

        assert_eq!(eng.merge(30).await, Err(EngineError::MissingObject));
        assert_eq!(d.rig.count(ObjectKind::Manifest).await, 3);
    }

    /// P-26: o nome da cópia não cabe num componente de caminho. Erro claro, nada gravado.
    #[tokio::test]
    async fn a_conflict_copy_name_that_does_not_fit_is_an_error_and_writes_nothing() {
        let name = format!("{}.md", "n".repeat(250));
        let d = diverge(
            &[(name.as_str(), b"1")],
            vec![
                ("pa", 10, vec![(name.as_str(), b"A")]),
                ("pb", 20, vec![(name.as_str(), b"B")]),
            ],
        )
        .await;
        assert_eq!(
            d.rig.engine("pc").merge(30).await,
            Err(EngineError::ConflictCopyName)
        );
        assert_eq!(d.rig.count(ObjectKind::Manifest).await, 3);
    }

    #[test]
    fn the_diverged_error_points_to_merge() {
        let text = EngineError::Diverged {
            tips: 2,
            rejected: 0,
        }
        .to_string();
        assert!(text.contains("merge"), "{text}");
    }

    // ---------- aplicar numa pasta viva (Fase 2a, PR 3) ----------

    /// Dois estados do mesmo Vault e uma pasta `target` que reflete o primeiro.
    ///
    /// v1: a.md, b.md, sub/c.md, sub/other.md, gone/z.md, keep.md
    /// v2: a.md muda; b.md, sub/c.md e gone/z.md somem; new.md e deep/new/x.md aparecem;
    ///     sub/other.md e keep.md ficam iguais.
    struct Hist {
        rig: Rig,
        v1: ObjectId,
        v2: ObjectId,
        /// Pasta que reflete v1 (é onde o `apply` roda).
        target: PathBuf,
        /// Árvore esperada depois de aplicar v2.
        v2_tree: BTreeMap<String, Vec<u8>>,
    }

    async fn hist() -> Hist {
        let rig = Rig::new();
        let work = rig.dir("work");
        for (path, data) in [
            ("a.md", b"1" as &[u8]),
            ("b.md", b"1"),
            ("sub/c.md", b"1"),
            ("sub/other.md", b"1"),
            ("gone/z.md", b"1"),
            ("keep.md", b"1"),
        ] {
            write(&work, path, data);
        }
        let eng = rig.engine("pc");
        let (v1, ..) = created(eng.snapshot(&vault_only(&work), 1).await.unwrap());
        let target = rig.tmp.path().join("target");
        copy_dir(&work, &target);

        write(&work, "a.md", b"2");
        fs::remove_file(work.join("b.md")).unwrap();
        fs::remove_file(work.join("sub/c.md")).unwrap();
        fs::remove_dir_all(work.join("gone")).unwrap();
        write(&work, "new.md", b"N");
        write(&work, "deep/new/x.md", b"X");
        let (v2, ..) = created(eng.snapshot(&vault_only(&work), 2).await.unwrap());
        let v2_tree = tree(&work);
        Hist {
            rig,
            v1,
            v2,
            target,
            v2_tree,
        }
    }

    fn no_leftovers(tree: &BTreeMap<String, Vec<u8>>) {
        assert!(
            tree.keys().all(|k| !k.contains(".arca-")),
            "sobras temporárias: {:?}",
            tree.keys().collect::<Vec<_>>()
        );
    }

    #[tokio::test]
    async fn apply_takes_the_folder_from_one_manifest_to_the_next() {
        let h = hist().await;
        let applied = h
            .rig
            .engine("pc")
            .apply(Some(&h.v1), &h.v2, &vault_only(&h.target))
            .await
            .unwrap();
        assert_eq!(
            applied,
            Applied {
                created: 2,
                updated: 1,
                removed: 3,
                unchanged: 2
            }
        );
        let after = tree(&h.target);
        no_leftovers(&after);
        assert_eq!(after, h.v2_tree);
        // pasta esvaziada pela remoção some; a que ainda tem arquivo e a raiz ficam
        assert!(!h.target.join("gone").exists());
        assert!(h.target.join("sub/other.md").exists());
        assert!(h.target.is_dir());
    }

    #[tokio::test]
    async fn applying_the_same_change_twice_changes_nothing_the_second_time() {
        let h = hist().await;
        let eng = h.rig.engine("pc");
        let roots = vault_only(&h.target);
        eng.apply(Some(&h.v1), &h.v2, &roots).await.unwrap();
        let second = eng.apply(Some(&h.v1), &h.v2, &roots).await.unwrap();
        assert_eq!(
            second,
            Applied {
                created: 0,
                updated: 0,
                removed: 0,
                unchanged: 8
            }
        );
        assert_eq!(tree(&h.target), h.v2_tree);
    }

    #[tokio::test]
    async fn applying_to_nothing_creates_everything() {
        let h = hist().await;
        let fresh = h.rig.tmp.path().join("novo");
        let applied = h
            .rig
            .engine("pc")
            .apply(None, &h.v2, &vault_only(&fresh))
            .await
            .unwrap();
        assert_eq!(
            applied,
            Applied {
                created: 5, // a.md, sub/other.md, keep.md, new.md, deep/new/x.md
                updated: 0,
                removed: 0,
                unchanged: 0
            }
        );
        assert_eq!(tree(&fresh), h.v2_tree);
    }

    #[tokio::test]
    async fn applying_a_manifest_onto_itself_touches_nothing() {
        let h = hist().await;
        let before = tree(&h.target);
        let applied = h
            .rig
            .engine("pc")
            .apply(Some(&h.v1), &h.v1, &vault_only(&h.target))
            .await
            .unwrap();
        assert_eq!(applied.created + applied.updated + applied.removed, 0);
        assert_eq!(tree(&h.target), before);
    }

    // ---------- aplicar: edição local ----------

    /// Recusa e a pasta fica exatamente como estava, sem nenhuma sobra temporária.
    async fn assert_refused_and_untouched(h: &Hist) {
        let before = tree(&h.target);
        let result = h
            .rig
            .engine("pc")
            .apply(Some(&h.v1), &h.v2, &vault_only(&h.target))
            .await;
        assert_eq!(result, Err(EngineError::LocalChanges));
        let after = tree(&h.target);
        no_leftovers(&after);
        assert_eq!(after, before);
    }

    #[tokio::test]
    async fn a_local_edit_in_a_file_the_apply_would_update_is_refused() {
        let h = hist().await;
        write(&h.target, "a.md", b"edicao local");
        assert_refused_and_untouched(&h).await;
    }

    #[tokio::test]
    async fn a_local_edit_in_a_file_the_apply_would_remove_is_refused() {
        let h = hist().await;
        write(&h.target, "b.md", b"edicao local");
        assert_refused_and_untouched(&h).await;
    }

    #[tokio::test]
    async fn a_file_deleted_locally_that_the_apply_would_update_is_refused() {
        let h = hist().await;
        fs::remove_file(h.target.join("a.md")).unwrap();
        assert_refused_and_untouched(&h).await;
    }

    #[tokio::test]
    async fn a_file_deleted_locally_that_the_apply_would_remove_is_fine() {
        let h = hist().await;
        fs::remove_file(h.target.join("b.md")).unwrap();
        h.rig
            .engine("pc")
            .apply(Some(&h.v1), &h.v2, &vault_only(&h.target))
            .await
            .unwrap();
        assert_eq!(tree(&h.target), h.v2_tree);
    }

    #[tokio::test]
    async fn a_local_edit_outside_the_diff_is_kept() {
        let h = hist().await;
        write(&h.target, "keep.md", b"minha edicao");
        write(&h.target, "extra.md", b"arquivo que ninguem sincronizou");
        h.rig
            .engine("pc")
            .apply(Some(&h.v1), &h.v2, &vault_only(&h.target))
            .await
            .unwrap();
        let after = tree(&h.target);
        assert_eq!(after["keep.md"], b"minha edicao");
        assert_eq!(after["extra.md"], b"arquivo que ninguem sincronizou");
        assert_eq!(after["a.md"], b"2");
    }

    #[tokio::test]
    async fn a_file_to_create_that_already_exists_is_fine_only_with_the_same_content() {
        let h = hist().await;
        write(&h.target, "new.md", b"N"); // mesmo conteúdo de v2
        let applied = h
            .rig
            .engine("pc")
            .apply(Some(&h.v1), &h.v2, &vault_only(&h.target))
            .await
            .unwrap();
        assert_eq!((applied.created, applied.unchanged), (1, 3));

        let h = hist().await;
        write(&h.target, "new.md", b"outro conteudo");
        assert_refused_and_untouched(&h).await;
    }

    #[tokio::test]
    async fn a_disabled_class_is_not_touched() {
        let h = hist().await;
        let before = tree(&h.target);
        let applied = h
            .rig
            .engine("pc")
            .apply(Some(&h.v1), &h.v2, &Roots::default())
            .await
            .unwrap();
        assert_eq!(
            applied,
            Applied {
                created: 0,
                updated: 0,
                removed: 0,
                unchanged: 0
            }
        );
        assert_eq!(tree(&h.target), before);
    }

    // ---------- aplicar: symlinks (nunca sair da pasta) ----------

    #[cfg(unix)]
    #[tokio::test]
    async fn a_symlink_in_place_of_a_file_to_update_is_refused() {
        let h = hist().await;
        let outside = h.rig.dir("fora");
        write(&outside, "alvo.md", b"nao mexa");
        fs::remove_file(h.target.join("a.md")).unwrap();
        std::os::unix::fs::symlink(outside.join("alvo.md"), h.target.join("a.md")).unwrap();
        let result = h
            .rig
            .engine("pc")
            .apply(Some(&h.v1), &h.v2, &vault_only(&h.target))
            .await;
        assert_eq!(result, Err(EngineError::LocalChanges));
        assert_eq!(fs::read(outside.join("alvo.md")).unwrap(), b"nao mexa");
    }

    /// `deep` aponta para fora: escrever `deep/new/x.md` iria parar fora do Vault.
    #[cfg(unix)]
    #[tokio::test]
    async fn a_symlinked_folder_on_the_way_is_refused_and_nothing_leaks_outside() {
        let h = hist().await;
        let outside = h.rig.dir("fora");
        std::os::unix::fs::symlink(&outside, h.target.join("deep")).unwrap();
        let result = h
            .rig
            .engine("pc")
            .apply(Some(&h.v1), &h.v2, &vault_only(&h.target))
            .await;
        assert_eq!(result, Err(EngineError::LocalChanges));
        assert!(fs::read_dir(&outside).unwrap().next().is_none());
        // e nada do resto foi aplicado
        assert_eq!(fs::read(h.target.join("a.md")).unwrap(), b"1");
    }

    // ---------- aplicar: blob ruim e falha no commit ----------

    /// Um manifest autenticado com uma entrada chamada `.arca-old-0` colidiria com os
    /// temporários do próprio `apply`: recusa clara, antes de tocar no disco.
    #[tokio::test]
    async fn an_entry_named_like_an_arca_temporary_is_refused_before_touching_anything() {
        let h = hist().await;
        let forged = put_forged(&h.rig, |m| {
            m.files.push(FileEntry {
                path: ".arca-old-0".to_string(),
                class: Class::Vault,
                blob: Hash32([1; 32]),
                size: 1,
                sha256: Hash32([1; 32]),
            });
            m.files
                .sort_by(|a, b| (a.class, &a.path).cmp(&(b.class, &b.path)));
        })
        .await;
        let before = tree(&h.target);
        let result = h
            .rig
            .engine("pc")
            .apply(Some(&h.v1), &forged, &vault_only(&h.target))
            .await;
        assert_eq!(result, Err(EngineError::ReservedName));
        assert_eq!(tree(&h.target), before);
    }

    /// Se o desfazer não pôde devolver o original porque a pessoa recriou o arquivo, o erro
    /// diz isso (e o original continua ao lado), em vez do erro que provocou a volta.
    #[tokio::test]
    async fn a_rollback_that_had_to_keep_an_original_says_so() {
        let rig = Rig::new();
        let dir = rig.dir("pasta");
        write(&dir, ".arca-old-0", b"original");
        write(&dir, "a.md", b"da pessoa");
        let log = vec![Undo::RenameBack {
            aside: dir.join(".arca-old-0"),
            orig: dir.join("a.md"),
        }];
        assert_eq!(
            rollback(log, EngineError::LocalChanges).await,
            EngineError::OriginalKept
        );
        assert_eq!(fs::read(dir.join("a.md")).unwrap(), b"da pessoa");
        assert_eq!(fs::read(dir.join(".arca-old-0")).unwrap(), b"original");

        // sem nada a manter, o erro original é preservado
        assert_eq!(
            rollback(Vec::new(), EngineError::LocalChanges).await,
            EngineError::LocalChanges
        );
    }

    #[tokio::test]
    async fn a_corrupt_or_missing_blob_stops_the_apply_before_touching_anything() {
        for corrupt in [true, false] {
            let h = hist().await;
            let eng = h.rig.engine("pc");
            let heads = eng.heads().await.unwrap();
            let tip = &heads.tips[0];
            let blob = tip
                .manifest
                .files
                .iter()
                .find(|f| f.path == "new.md")
                .unwrap()
                .blob;
            let blob_path = h.rig.store_dir().join(
                ObjectId {
                    kind: ObjectKind::Blob,
                    hash: blob.0,
                }
                .to_string(),
            );
            if corrupt {
                fs::write(&blob_path, b"adulterado").unwrap();
            } else {
                fs::remove_file(&blob_path).unwrap();
            }
            let before = tree(&h.target);
            let result = eng.apply(Some(&h.v1), &h.v2, &vault_only(&h.target)).await;
            let expected = if corrupt {
                EngineError::Store(StoreError::Corrupt)
            } else {
                EngineError::MissingObject
            };
            assert_eq!(result, Err(expected));
            let after = tree(&h.target);
            no_leftovers(&after);
            assert_eq!(after, before);
        }
    }

    /// Uma pasta somente leitura faz o commit falhar no meio: tudo volta como estava.
    #[cfg(unix)]
    #[tokio::test]
    async fn a_failure_in_the_middle_of_the_commit_undoes_everything() {
        use std::os::unix::fs::PermissionsExt;
        let h = hist().await;
        let sub = h.target.join("sub");
        fs::set_permissions(&sub, fs::Permissions::from_mode(0o500)).unwrap();
        // como root a permissão não vale: o teste não prova nada, então não roda
        if fs::File::create(sub.join("sonda")).is_ok() {
            fs::remove_file(sub.join("sonda")).unwrap();
            fs::set_permissions(&sub, fs::Permissions::from_mode(0o700)).unwrap();
            return;
        }
        let before = tree(&h.target);
        let result = h
            .rig
            .engine("pc")
            .apply(Some(&h.v1), &h.v2, &vault_only(&h.target))
            .await;
        fs::set_permissions(&sub, fs::Permissions::from_mode(0o700)).unwrap();
        assert_eq!(
            result,
            Err(EngineError::Io(io::ErrorKind::PermissionDenied))
        );
        let after = tree(&h.target);
        no_leftovers(&after);
        assert_eq!(after, before);
        assert!(
            !h.target.join("deep").exists(),
            "pastas criadas são desfeitas"
        );
        assert!(h.target.join("gone/z.md").exists());
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn applied_files_are_0600_and_created_folders_0700() {
        use std::os::unix::fs::PermissionsExt;
        let h = hist().await;
        h.rig
            .engine("pc")
            .apply(Some(&h.v1), &h.v2, &vault_only(&h.target))
            .await
            .unwrap();
        let mode = |p: &str| fs::metadata(h.target.join(p)).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode("a.md"), 0o600);
        assert_eq!(mode("new.md"), 0o600);
        assert_eq!(mode("deep/new/x.md"), 0o600);
        assert_eq!(mode("deep"), 0o700);
        assert_eq!(mode("deep/new"), 0o700);
    }

    fn ids_of(heads: &Heads) -> Vec<ObjectId> {
        heads.tips.iter().map(|h| h.id).collect()
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
            EngineError::ConflictCopyName,
            EngineError::LocalChanges,
            EngineError::ReservedName,
            EngineError::OriginalKept,
            EngineError::NeedsNewKey { have: 1, found: 2 },
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
