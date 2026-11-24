//! Núcleo local: junta cripto, manifest e `BlobStore` para **guardar o Vault e restaurá-lo
//! do zero** (`SPEC-TECNICA.md` §6, "Envio" e "Recebimento", sem a parte de rede). Sync entre
//! dispositivos, junção de pontas e regra de conflito são da Fase 2.

mod verify;
pub use verify::{RestoreCheck, VerifyMode, VerifyReport};

use crate::apply::{
    commit, finish, is_reserved_name, lock_roots, missing_dirs, plan, stage, undo_all, Item, Kind,
    Planned, Undo,
};
use crate::crypto::{open, seal, EpochKeys, VaultKey};
use crate::dirfd::{Folder, FolderLock, OpenError};
use crate::disk::{create_private_dir, write_atomic};
use crate::envelope::Header;
use crate::error::Error;
use crate::foreign::ForeignAcks;
use crate::heads::{tips, Head};
use crate::journal::{recover_all, ApplyGroup, Journal, Phase};
use crate::kept::{self, KeptOriginal};
use crate::losers::{self, ClearReport, LoserCopy};
use crate::manifest::{validate_device, validate_path, Class, FileEntry, Hash32, Manifest};
use crate::merge::{common_ancestor, merge_files, reachable, Conflict, Side};
use crate::object::{ObjectId, ObjectKind};
use crate::replicate::{replicate, replicate_filtered, ReplicateReport, Verdict};
use crate::scan::{scan_class_with, Candidate, ScanFilter, Skipped, MAX_FILE_BYTES};
use crate::seen::{Seen, MAX_SEEN};
use crate::state::{descends, satisfied, DeviceState};
use crate::store::{verified_get, BlobStore, StoreError};
use crate::FORMAT_VERSION;
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet, HashSet};
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
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

    /// A pasta da classe, se ela está ligada.
    fn root_of(&self, class: Class) -> Option<&Path> {
        self.enabled()
            .into_iter()
            .find(|(c, _)| *c == class)
            .map(|(_, root)| root)
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
    /// A pasta de conflitos não é segura: está dentro de uma pasta de classe ligada, tem `..`
    /// no caminho, ou ela (ou a pasta da classe nela) é um symlink. Um segredo perdedor ao lado
    /// dos logins vivos poderia ser lido como outro login.
    #[error("the conflicts folder must be a real folder outside every enabled class folder")]
    UnsafeConflictsDir,
    /// O armazenamento não continua a história que este dispositivo já aceitou (voltou no
    /// tempo, foi trocado ou esvaziado): nada foi escrito. Para trocar de armazenamento de
    /// propósito, apague o arquivo de estado.
    #[error("the storage does not continue the history this device already accepted")]
    Rollback,
    /// O snapshot deixaria uma classe ligada sem nenhuma entrada, tendo `had` na ponta: uma
    /// pasta recriada vazia propagaria a remoção de tudo aos outros dispositivos. Nada foi
    /// gravado. Para esvaziar de propósito, use `Engine::snapshot_allowing_empty`.
    #[error("the snapshot would leave class {class:?} empty ({had} entries on the tip)")]
    ClassEmptied { class: Class, had: usize },
    #[error("sem-correcao: a variante existe so para os testes compilarem")]
    HeadNotAccepted { class: Class },
    /// O arquivo de estado local existe mas não é válido. Nunca vale como "sem estado".
    #[error("the device state file is corrupt")]
    CorruptState,
    /// A operação precisa guardar uma decisão ao lado do estado local (ADR 0018) e o `Engine`
    /// não tem `with_state`: não há onde gravá-la.
    #[error("no device state file is configured; this operation needs one")]
    NoState,
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
    /// Há um diário de `apply` numa pasta de classe: um `apply` anterior foi interrompido no
    /// meio da troca de arquivos. Nada foi lido nem escrito; chame `Engine::recover` antes.
    #[error("an interrupted apply left a journal; run recover first")]
    Interrupted,
    /// O diário de `apply` existe mas não é válido (ilegível, grande demais, versão desconhecida,
    /// caminho que sai da raiz, link no lugar, de outro dono ou legível por grupo ou outros em
    /// Unix: a arca sempre grava o diário `0600`, então um diário assim não foi gravado por ela).
    /// A recuperação não tocou em nada. Um `apply` interrompido que rodou como outro usuário (por
    /// exemplo `sudo`) cai aqui: a pessoa apaga o diário à mão depois de conferir a pasta.
    #[error("the apply journal is corrupt")]
    CorruptJournal,
    /// O `apply` interrompido tinha mais pastas do que as de `Engine::recover`, e nenhuma das
    /// que aparecem tem o marcador `committed`: a que falta pode tê-lo, então voltar só as
    /// visíveis deixaria o conjunto meio voltado. Nada foi tocado; ligue as classes que faltam
    /// (as pastas do `apply` interrompido) e chame `recover` de novo.
    #[error("the interrupted apply has journals in folders that were not given to recover")]
    IncompleteApply,
    /// Outra operação (deste ou de outro processo) está usando uma das pastas ou o estado do
    /// dispositivo: um `apply` ou `recover` na pasta (P-33 item 7), ou um `snapshot`, `merge` ou
    /// `apply` com o mesmo arquivo de estado (P-31 item 4). Nada foi lido nem escrito; tente de
    /// novo depois.
    #[error("another operation is using the folders or the device state; try again later")]
    Busy,
    /// O diário deste `apply` passaria de `MAX_JOURNAL_BYTES` (P-33 item 8): arquivos demais, ou
    /// caminhos muito longos. Nada foi tocado e nenhum diário foi gravado.
    #[error("the apply touches too many files to journal; nothing was changed")]
    JournalTooLarge,
    /// A operação (`snapshot` que cria manifest, `merge` ou `apply`) **valeu**, mas o estado do
    /// dispositivo (o último manifest aceito) não pôde ser gravado (P-31 item 3). O resultado
    /// (`id`, `conflicts`, `Applied`) se perde, mas refazer é seguro: o `snapshot` vira
    /// `Unchanged`, o `apply` conta tudo como `unchanged` e o `merge` vira `Nothing`. Só a
    /// proteção contra rollback fica um passo atrás, até uma gravação dar certo.
    #[error(
        "the operation took effect, but the device state could not be saved; repeating it is safe"
    )]
    StateNotSaved,
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

/// O que `Engine::scan_manifests` achou no armazenamento, sem política.
struct Scanned {
    readable: Vec<(ObjectId, Manifest)>,
    rejected: Vec<Rejected>,
    /// `(id, época do cabeçalho)` dos que este dispositivo não abriu por serem de outra época.
    others: Vec<(ObjectId, u32)>,
    /// Há manifest da época das chaves (aberto ou ilegível).
    mine_seen: bool,
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

/// Resultado de `Engine::save_conflict_losers`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SavedLosers {
    /// Cópias gravadas agora.
    pub saved: usize,
    /// Já estavam lá com o mesmo conteúdo (chamada repetida).
    pub already: usize,
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

/// Resultado de `Engine::recover`.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Recovered {
    /// Entradas devolvidas ao estado de antes do `apply` interrompido.
    pub rolled_back: usize,
    /// Entradas de um `apply` que já tinha trocado todos os arquivos, apenas terminadas.
    pub completed: usize,
    /// Arquivos que a recuperação **não tocou** porque o disco não bate com o diário (a pessoa
    /// mexeu depois da queda): ficam onde estão, para ela decidir.
    pub kept: Vec<PathBuf>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Restored {
    pub files: usize,
}

/// Decide se uma entrada de manifest que veio de fora pode ser aplicada (`with_entry_filter`).
pub type EntryFilter = fn(&FileEntry) -> bool;

/// Um manifest de época **mais nova** que a das chaves do dispositivo, que ele não consegue
/// abrir e que a pessoa ainda não reconheceu (ADR 0018). O cabeçalho é em claro e não é
/// autenticado: pode ser a rotação de verdade ou um objeto forjado por quem tem a chave velha.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ForeignEpoch {
    pub epoch: u32,
    pub id: ObjectId,
}

/// O que bloqueia o dispositivo de época `mine` (ADR 0018): manifests de época mais nova **não
/// reconhecidos**, vistos no armazenamento local (`others`, réplica já contaminada) ou
/// anotados pelo `pull` (`acks.pending`). Sem repetição, em ordem `(época, hash)`.
fn unacknowledged(mine: u32, others: &[(ObjectId, u32)], acks: &ForeignAcks) -> Vec<ForeignEpoch> {
    let local = others.iter().map(|(id, epoch)| (*epoch, id.hash));
    let noted = acks.pending_above(mine).into_iter().map(|(e, h)| (e, h.0));
    let mut found: Vec<ForeignEpoch> = local
        .chain(noted)
        .filter(|(epoch, _)| *epoch > mine)
        .map(|(epoch, hash)| ForeignEpoch {
            epoch,
            id: ObjectId {
                kind: ObjectKind::Manifest,
                hash,
            },
        })
        .filter(|f| !acks.contains(f.epoch, &f.id))
        .collect();
    found.sort_by_key(|f| (f.epoch, f.id.hash));
    found.dedup();
    found
}

pub struct Engine<S> {
    store: S,
    keys: EpochKeys,
    device: String,
    /// Arquivo do último manifest aceito (`state.rs`). Sem ele o engine **não** protege contra
    /// um armazenamento que volte no tempo.
    state: Option<PathBuf>,
    /// Filtros de entradas por classe (`with_entry_filter`). Vazio: tudo é aceito.
    filters: Vec<(Class, EntryFilter)>,
    /// Filtros de arquivos locais por classe (`with_scan_filter`).
    scan_filters: Vec<(Class, ScanFilter)>,
    /// Pendentes e reconhecidos **sem** `with_state` (ADR 0018, P-47b): vivem só neste
    /// `Engine`. Com estado, o arquivo `<estado>.foreign` é a fonte e isto não é usado.
    foreign_memory: Mutex<ForeignAcks>,
    /// Cache negativo do `pull`: manifests de outra época já classificados (só memória, com teto).
    foreign_seen: Mutex<Seen>,
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
            state: None,
            filters: Vec::new(),
            scan_filters: Vec::new(),
            foreign_memory: Mutex::new(ForeignAcks::default()),
            foreign_seen: Mutex::new(Seen::new(MAX_SEEN)),
        })
    }

    /// Liga a proteção contra rollback: `snapshot`, `merge` e `apply` passam a consultar e a
    /// atualizar o último manifest aceito em `path` (`SPEC-TECNICA.md` §6). Para trocar de
    /// armazenamento de propósito, apague o arquivo.
    pub fn with_state(mut self, path: impl Into<PathBuf>) -> Engine<S> {
        self.state = Some(path.into());
        self
    }

    /// Diz quais entradas de `class` que **vêm de fora** (de outro dispositivo) este engine
    /// aceita aplicar. `apply` e `restore` ignoram as recusadas: nunca criam, atualizam nem
    /// apagam o arquivo local. Se a entrada de origem **ou** a de destino é recusada, o
    /// caminho inteiro é pulado (tratar "recusada" como "ausente" deixaria um dispositivo
    /// apagar um arquivo local publicando uma versão que o filtro recusa). `snapshot` não usa
    /// o filtro: ele reconstrói a classe ligada a partir do disco, então as entradas recusadas
    /// da ponta não são levadas para o manifest novo. Pode ser chamado mais de uma vez; todas
    /// as funções da classe precisam aceitar.
    pub fn with_entry_filter(mut self, class: Class, accept: EntryFilter) -> Engine<S> {
        self.filters.push((class, accept));
        self
    }

    /// Diz quais arquivos locais de `class` o `snapshot` pode levar (`scan_class_with`). Os
    /// recusados nunca são lidos nem enviados e voltam em `skipped`; `apply` e `restore` não os
    /// tocam (não estão no manifest, então não há o que apagar). Um filtro por classe: chamar de
    /// novo para a mesma classe substitui o anterior.
    pub fn with_scan_filter(mut self, class: Class, accept: ScanFilter) -> Engine<S> {
        self.scan_filters.retain(|(c, _)| *c != class);
        self.scan_filters.push((class, accept));
        self
    }

    fn accepts(&self, entry: &FileEntry) -> bool {
        self.filters
            .iter()
            .all(|(class, accept)| *class != entry.class || accept(entry))
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

    /// Lê todos os manifests e separa os que abrem com as chaves do dispositivo, os ilegíveis da
    /// época dele e os de outra época (só o cabeçalho em claro). Sem política: quem decide o que
    /// fazer com as outras épocas é `load`.
    async fn scan_manifests(&self) -> Result<Scanned, EngineError> {
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
        Ok(Scanned {
            readable,
            rejected,
            others,
            mine_seen,
        })
    }

    /// Lê todos os manifests e os classifica (ver `heads`). `readable` é o grafo inteiro da
    /// época que conta, não só as pontas. Manifest de época **mais nova** que a das chaves, que
    /// a pessoa ainda não reconheceu (`acknowledge_foreign`, ADR 0018), é `NeedsNewKey`; o
    /// reconhecido é ignorado (nem `superseded`, nem `rejected`).
    async fn load(&self) -> Result<Loaded, EngineError> {
        let mine = self.keys.epoch();
        let Scanned {
            readable,
            mut rejected,
            mut others,
            mine_seen,
        } = self.scan_manifests().await?;
        let acks = self.load_acks().await?;
        if let Some(found) = unacknowledged(mine, &others, &acks)
            .iter()
            .map(|f| f.epoch)
            .max()
        {
            return Err(EngineError::NeedsNewKey { have: mine, found });
        }
        others.retain(|(_, epoch)| *epoch <= mine);
        let newest_other = others.iter().map(|(_, epoch)| *epoch).max();
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

    /// Os manifests de época mais nova que a das chaves do dispositivo e **ainda não
    /// reconhecidos**, em ordem `(época, hash)` (ADR 0018). Enquanto houver algum, `heads`,
    /// `snapshot`, `merge` e `apply` recusam com `NeedsNewKey`. Inclui os anotados pelo `pull`
    /// (P-47b), que não estão no armazenamento. Só lê: não pede a trava do estado.
    pub async fn foreign_epochs(&self) -> Result<Vec<ForeignEpoch>, EngineError> {
        let mine = self.keys.epoch();
        let Scanned { others, .. } = self.scan_manifests().await?;
        let acks = self.load_acks().await?;
        Ok(unacknowledged(mine, &others, &acks))
    }

    /// Reconhece `acknowledged`: o que a pessoa **viu** em `foreign_epochs`, par a par
    /// `(época, id)` (ADR 0018). O consumidor deve mostrar a lista antes de chamar. Não apaga
    /// nada. Guarda os 256 mais recentes (passou, sai o mais antigo, que bloqueia de novo se
    /// ainda existir). Também tira do que está pendente (P-47b). Sem `with_state` o
    /// reconhecimento vale só na memória deste `Engine`. Estado ocupado por outra operação: `Busy`.
    pub async fn acknowledge_foreign(
        &self,
        acknowledged: &[ForeignEpoch],
    ) -> Result<(), EngineError> {
        let _state_lock = self.lock_state().await?;
        let mut acks = self.load_acks().await?;
        for f in acknowledged {
            acks.acknowledge(f.epoch, &f.id);
        }
        self.store_acks(acks).await
    }

    /// Pendentes e reconhecidos (ADR 0018): do arquivo `<estado>.foreign` com `with_state`
    /// (vazio sem arquivo), senão da memória deste `Engine`.
    async fn load_acks(&self) -> Result<ForeignAcks, EngineError> {
        let Some(path) = self.state.clone() else {
            return Ok(self.foreign_memory.lock().unwrap().clone());
        };
        run(move || ForeignAcks::load(&path)).await
    }

    /// Grava o que `load_acks` leu, já alterado. Quem chama segura a trava do estado.
    async fn store_acks(&self, acks: ForeignAcks) -> Result<(), EngineError> {
        let Some(path) = self.state.clone() else {
            *self.foreign_memory.lock().unwrap() = acks;
            return Ok(());
        };
        run(move || acks.save(&path)).await
    }

    /// Com o estado ligado, garante que o último manifest aceito esteja no grafo (P-31 item 2).
    /// O grafo vem só do `list`, e um `list` atrasado (um S3-compatível de listagem eventual, ou
    /// um hub que só alega consistência) pode não mostrar o manifest que este dispositivo acabou
    /// de aceitar: sem ele nenhuma ponta o alcança e a guarda daria `Rollback` falso. Então ele é
    /// lido pelo id. O id é o hash dos bytes cifrados: `classify_manifest` confere o hash e abre
    /// com a chave, então um backend hostil não forja o aceito. Ausente, corrompido, ilegível ou
    /// de outra época: o grafo fica como estava e a guarda decide (rollback de verdade). Só
    /// `Unavailable` é erro, porque pede tentar de novo e não acusa rollback. O estado de outra
    /// época que as chaves segue o caminho da rotação, sem leitura extra.
    async fn with_accepted(
        &self,
        mut loaded: Loaded,
        state: Option<&DeviceState>,
    ) -> Result<Loaded, EngineError> {
        let Some(state) = state else {
            return Ok(loaded);
        };
        if state.key_epoch != self.keys.epoch() {
            return Ok(loaded);
        }
        let hash = state.last_accepted.0;
        let known = loaded.readable.iter().any(|(id, _)| id.hash == hash)
            || loaded.rejected.iter().any(|r| r.id.hash == hash)
            || loaded.superseded.iter().any(|id| id.hash == hash);
        if known {
            return Ok(loaded);
        }
        let id = ObjectId {
            kind: ObjectKind::Manifest,
            hash,
        };
        match self.classify_manifest(&id).await {
            Ok(Read::Opened(manifest)) => loaded.readable.push((id, manifest)),
            Err(e @ EngineError::Store(StoreError::Unavailable(_))) => return Err(e),
            _ => {}
        }
        Ok(loaded)
    }

    /// Trava o arquivo de estado para a operação inteira (P-31 item 4), se a proteção está
    /// ligada: dois processos do mesmo dispositivo não podem intercalar o `load_state` e o
    /// `remember`, ou o que grava por último faria o estado recuar. Sem espera: ocupado é `Busy`.
    /// Vem **antes** da trava das raízes (nenhuma espera, então não há impasse).
    async fn lock_state(&self) -> Result<Option<FolderLock>, EngineError> {
        let Some(path) = self.state.clone() else {
            return Ok(None);
        };
        run(move || crate::state::lock(&path).map(Some)).await
    }

    /// O último manifest aceito, se a proteção está ligada e o arquivo existe. Arquivo
    /// corrompido é `CorruptState`: nunca vale como "sem estado".
    async fn load_state(&self) -> Result<Option<DeviceState>, EngineError> {
        let Some(path) = self.state.clone() else {
            return Ok(None);
        };
        run(move || DeviceState::load(&path)).await
    }

    /// Grava `id` como o último manifest aceito (se a proteção está ligada). Falhar aqui depois
    /// de uma operação que já valeu devolve `Io`; o estado antigo continua ancestral, então a
    /// próxima chamada não dá `Rollback` falso.
    async fn remember(&self, id: &ObjectId) -> Result<(), EngineError> {
        let Some(path) = self.state.clone() else {
            return Ok(());
        };
        let state = DeviceState {
            key_epoch: self.keys.epoch(),
            last_accepted: Hash32(id.hash),
        };
        run(move || state.save(&path)).await
    }

    /// Como `remember`, para depois de uma operação que **já valeu** (`snapshot` que criou
    /// manifest, `merge`, `apply`): se gravar falha, o erro é `StateNotSaved` e não `Io`, para o
    /// chamador saber que refazer é seguro e que nada foi desfeito (P-31 item 3).
    async fn remember_after(&self, id: &ObjectId) -> Result<(), EngineError> {
        self.remember(id)
            .await
            .map_err(|_| EngineError::StateNotSaved)
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

    /// Puxa de `remote` (não confiável) para o armazenamento deste dispositivo (confiável).
    /// Igual a `replicate(remote, store)`, mas um manifest da época das chaves que não abre com
    /// elas (forjado, corrompido ou de outro Vault) não é gravado e vem em `forged`: sem isso ele
    /// viraria `rejected` e travaria `snapshot` e `merge` (P-32). Manifest de **outra época**
    /// (maior ou menor) não pode ser verificado sem a chave dela: **nunca é gravado** e vem em
    /// `foreign_epochs`, classificado mesmo quando um blob ruim retém os manifests (ADR 0018, P-47b).
    /// As épocas maiores ainda não reconhecidas viram pendência e bloqueiam `snapshot`, `merge` e
    /// `apply` com `NeedsNewKey` até `acknowledge_foreign` (em arquivo com `with_state`, senão só
    /// na memória deste `Engine`). Toma a trava do estado: ocupado é `Busy`. É o único caminho
    /// público para trazer objetos de fora; o inverso é `push`.
    pub async fn pull<R: BlobStore>(&self, remote: &R) -> Result<ReplicateReport, EngineError> {
        // a trava do estado cobre a leitura, a rodada e a gravação da pendência (ADR 0018)
        let _state_lock = self.lock_state().await?;
        let mine = self.keys.epoch();
        let report = replicate_filtered(
            remote,
            &self.store,
            |id| self.foreign_seen.lock().unwrap().contains(id),
            |bytes| match Header::decode(bytes) {
                Ok((header, _)) if header.key_epoch == mine => {
                    if Manifest::open(&self.keys, bytes).is_ok() {
                        Verdict::Accept
                    } else {
                        Verdict::Forged
                    }
                }
                Ok((header, _)) => Verdict::Foreign(header.key_epoch),
                Err(_) => Verdict::Forged,
            },
        )
        .await?;
        self.note_foreign(mine, &report.foreign_epochs).await?;
        Ok(report)
    }

    /// Anota o que o `pull` viu de outra época: as maiores ainda não reconhecidas viram
    /// pendência (é o que passa a bloquear `snapshot`, `merge` e `apply`, já que o `pull` não
    /// grava mais o objeto); as menores e as já reconhecidas só entram no cache negativo. Um par
    /// que não coube no teto de pendências **não** entra no cache, para voltar na próxima rodada.
    async fn note_foreign(&self, mine: u32, seen: &[ForeignEpoch]) -> Result<(), EngineError> {
        let mut acks = self.load_acks().await?;
        let before = acks.clone();
        acks.drop_pending_up_to(mine);
        let mut cache = Vec::new();
        for f in seen {
            if f.epoch <= mine || acks.contains(f.epoch, &f.id) || acks.add_pending(f.epoch, &f.id)
            {
                cache.push(f.id);
            }
        }
        if acks != before {
            self.store_acks(acks).await?;
        }
        let mut seen_ids = self.foreign_seen.lock().unwrap();
        for id in cache {
            seen_ids.insert(id);
        }
        Ok(())
    }

    /// Empurra o armazenamento deste dispositivo (confiável) para `remote`: copia os blobs e
    /// depois os manifests que o remoto não tem, sem apagar nada. Não filtra, porque a origem é
    /// local. `Ok` com `manifests_withheld` não é sucesso (um blob local não pôde ser lido).
    /// O caminho inverso é `pull`.
    pub async fn push<R: BlobStore>(&self, remote: &R) -> Result<ReplicateReport, StoreError> {
        replicate(&self.store, remote).await
    }

    /// Os conflitos das fusões que `to` alcança e `from` (o manifest que a pasta reflete; `None`
    /// = nenhum) não alcança, recalculados a partir dos pais de cada manifest de fusão (M-1 b).
    /// Serve a quem perdeu um conflito sem fazer o merge: antes do `apply` sobrescrever o
    /// arquivo, o chamador passa o resultado a `save_conflict_losers` (idempotente). `merge_files`
    /// é determinística e simétrica, então o resultado é o mesmo que o dispositivo que fundiu
    /// obteve. Uma fusão cujos pais não estão no armazenamento (histórico parcial) é pulada.
    pub async fn conflicts_between(
        &self,
        from: Option<&ObjectId>,
        to: &ObjectId,
    ) -> Result<Vec<Conflict>, EngineError> {
        let Loaded { readable, .. } = self.load().await?;
        let graph: std::collections::HashMap<[u8; 32], &(ObjectId, Manifest)> =
            readable.iter().map(|entry| (entry.0.hash, entry)).collect();
        let already = from.map_or_else(HashSet::new, |id| reachable(&graph, id.hash));
        let mut fresh: Vec<[u8; 32]> = reachable(&graph, to.hash)
            .into_iter()
            .filter(|hash| !already.contains(hash))
            .filter(|hash| {
                graph
                    .get(hash)
                    .is_some_and(|(_, manifest)| !manifest.merged_from.is_empty())
            })
            .collect();
        fresh.sort_unstable();

        let mut found = Vec::new();
        for hash in fresh {
            let (_, manifest) = graph[&hash];
            let parents: Vec<&(ObjectId, Manifest)> = manifest
                .base_version
                .iter()
                .chain(&manifest.merged_from)
                .filter_map(|parent| graph.get(&parent.0).copied())
                .collect();
            let [left, right] = parents.as_slice() else {
                continue;
            };
            let base_files: &[FileEntry] = common_ancestor(&readable, &left.0, &right.0)
                .and_then(|id| readable.iter().find(|(other, _)| *other == id))
                .map_or(&[], |(_, manifest)| &manifest.files);
            fn side(entry: &(ObjectId, Manifest)) -> Side<'_> {
                Side {
                    id: entry.0,
                    manifest: &entry.1,
                }
            }
            let merged = merge_files(base_files, side(left), side(right)).map_err(|e| match e {
                Error::InvalidPath(_) => EngineError::ConflictCopyName,
                other => EngineError::Object(other),
            })?;
            found.extend(merged.conflicts);
        }
        Ok(found)
    }

    /// Como `merge`, mas grava os perdedores de conflito das classes `config` e `secret`
    /// (`save_conflict_losers`, em `dir`) **antes** de gravar o manifest de fusão (M-1). A fusão
    /// é determinística e a gravação é idempotente: se o save falhar ou o processo cair, nada
    /// da fusão foi gravado e a próxima chamada recalcula os mesmos conflitos. Se o save
    /// falhar, o erro volta e as pontas continuam como estavam. Classe sem raiz em `roots` não
    /// tem perdedor gravado (P-37).
    pub async fn merge_with_losers(
        &self,
        now: u64,
        dir: &Path,
        roots: &Roots,
    ) -> Result<(MergeOutcome, SavedLosers), EngineError> {
        self.merge_inner(now, Some((dir, roots))).await
    }

    /// Funde as duas primeiras pontas (ordem `(counter, hash)`) num manifest de fusão
    /// (`SPEC-TECNICA.md` §7, ADR 0014). Uma chamada resolve um par; repetir até `tips_after`
    /// chegar a 1. Os perdedores de `config` e `secret` só voltam em `conflicts`, em memória:
    /// quem os usa deve chamar `merge_with_losers`, que os grava antes do manifest.
    pub async fn merge(&self, now: u64) -> Result<MergeOutcome, EngineError> {
        Ok(self.merge_inner(now, None).await?.0)
    }

    async fn merge_inner(
        &self,
        now: u64,
        losers_to: Option<(&Path, &Roots)>,
    ) -> Result<(MergeOutcome, SavedLosers), EngineError> {
        let mut saved = SavedLosers {
            saved: 0,
            already: 0,
        };
        let _state_lock = self.lock_state().await?;
        let state = self.load_state().await?;
        let loaded = self.load().await?;
        let Loaded {
            readable, rejected, ..
        } = self.with_accepted(loaded, state.as_ref()).await?;
        let heads = tips(readable.clone());
        guard(state.as_ref(), &readable, &heads)?;
        // sem o grafo completo a base pode estar errada: mesma recusa do `snapshot`
        if !rejected.is_empty() {
            return Err(EngineError::Diverged {
                tips: heads.len(),
                rejected: rejected.len(),
            });
        }
        let [first, second, ..] = heads.as_slice() else {
            return Ok((
                MergeOutcome::Nothing {
                    head: heads.first().map(|h| h.id),
                },
                saved,
            ));
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
        // M-1: o perdedor de `config`/`secret` não está em nenhum manifest; grava-se antes do
        // manifest de fusão, para que uma falha aqui deixe o armazenamento como estava
        if let Some((dir, roots)) = losers_to {
            saved = self
                .save_conflict_losers(&merged.conflicts, dir, roots)
                .await?;
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
        // o estado só avança: `guard` aceitou porque ALGUMA ponta continua o aceito, mas o par
        // fundido são as duas primeiras por (counter, hash) e pode não incluí-la. Gravar uma
        // fusão que não descende do aceito faria o estado recuar (achado C1 da auditoria).
        let advances = state.as_ref().is_none_or(|state| {
            descends(state, &readable, &first.id) || descends(state, &readable, &second.id)
        });
        if advances {
            self.remember_after(&sealed.id).await?;
        }
        Ok((
            MergeOutcome::Merged {
                id: sealed.id,
                manifest,
                conflicts: merged.conflicts,
                // duas pontas viram uma
                tips_after: heads.len() - 1,
            },
            saved,
        ))
    }

    /// Fotografa as pastas de `roots` e grava o estado como um manifest novo (blobs primeiro,
    /// manifest por último). `now` são segundos Unix (UTC), injetados para testes determinísticos.
    pub async fn snapshot(&self, roots: &Roots, now: u64) -> Result<Snapshot, EngineError> {
        self.snapshot_allowing_empty(roots, now, &[]).await
    }

    /// Como `snapshot`, mas as classes em `allow_empty` podem ficar sem nenhuma entrada (a
    /// pessoa apagou tudo de propósito). Sem a autorização, uma classe ligada que tinha entradas
    /// na ponta e ficaria sem nenhuma dá `ClassEmptied`, sem gravar o manifest.
    pub async fn snapshot_allowing_empty(
        &self,
        roots: &Roots,
        now: u64,
        allow_empty: &[Class],
    ) -> Result<Snapshot, EngineError> {
        let _state_lock = self.lock_state().await?;
        ensure_not_interrupted(roots)?;
        let state = self.load_state().await?;
        let loaded = self.load().await?;
        let Loaded {
            readable, rejected, ..
        } = self.with_accepted(loaded, state.as_ref()).await?;
        let heads = match &state {
            // com proteção ligada o grafo é preciso depois do cálculo das pontas
            Some(_) => {
                let heads = tips(readable.clone());
                guard(state.as_ref(), &readable, &heads)?;
                heads
            }
            None => tips(readable),
        };
        if heads.len() > 1 || !rejected.is_empty() {
            return Err(EngineError::Diverged {
                tips: heads.len(),
                rejected: rejected.len(),
            });
        }
        let prev = heads.into_iter().next();
        self.build_snapshot(prev, roots, now, allow_empty).await
    }

    /// O miolo do `snapshot`: varre as pastas, envia os blobs que mudaram em relação a `prev` e
    /// grava o manifest novo (com as chaves deste `Engine`), por último. Quem chama já tem a
    /// trava do estado e já checou as pré-condições (`ensure_not_interrupted`, `guard`,
    /// divergência): é o que o `begin_epoch` reaproveita com `prev` da época nova.
    async fn build_snapshot(
        &self,
        prev: Option<Head>,
        roots: &Roots,
        now: u64,
        allow_empty: &[Class],
    ) -> Result<Snapshot, EngineError> {
        let prev_files: &[FileEntry] = prev.as_ref().map_or(&[], |h| &h.manifest.files);

        // 1. varrer as classes ligadas (nada é lido nem enviado ainda)
        let mut candidates = Vec::new();
        let mut skipped = Vec::new();
        for (class, root) in roots.enabled() {
            let root = root.to_path_buf();
            let accept = self
                .scan_filters
                .iter()
                .find(|(c, _)| *c == class)
                .map(|(_, accept)| *accept);
            let scan = run(move || scan_class_with(class, &root, accept).map_err(io_error)).await?;
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
            let root = roots
                .root_of(candidate.class)
                .ok_or(EngineError::Io(io::ErrorKind::InvalidInput))?;
            let Some(data) = read_file(&candidate, root, &mut skipped).await? else {
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

        // piso: uma classe ligada que tinha entradas e ficaria sem nenhuma só passa se o
        // consumidor autorizou. Raiz ausente já parou no scan; vazia propagaria N remoções.
        for (class, _) in roots.enabled() {
            if allow_empty.contains(&class) {
                continue;
            }
            // só conta o que este dispositivo aplicaria: uma entrada que o filtro recusa (um nome
            // estranho publicado por outro dispositivo) não faz da pasta vazia um "esvaziei a classe"
            let had = prev_files
                .iter()
                .filter(|f| f.class == class && self.accepts(f))
                .count();
            if had > 0 && !files.iter().any(|f| f.class == class) {
                return Err(EngineError::ClassEmptied { class, had });
            }
        }

        if let Some(head) = &prev {
            if head.manifest.files == files {
                // a pasta já é igual à cabeça: o dispositivo a aceita como estado atual
                self.remember(&head.id).await?;
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
        self.remember_after(&sealed.id).await?;
        Ok(Snapshot::Created {
            id: sealed.id,
            manifest,
            new_blobs,
            skipped,
        })
    }

    /// Inicia a época seguinte com `new_key` (ADR 0018, itens 4 e 5): publica um snapshot
    /// **completo** da pasta viva (`base_version = null`, todos os blobs recifrados) com as
    /// chaves da época atual + 1 e devolve um `Engine` novo com elas (as chaves de um `Engine`
    /// são imutáveis). `new_key` tem de derivar subchaves **diferentes** das atuais
    /// (`KeyNotRotated`); sem época seguinte, `EpochOverflow`. Pré-condições do `snapshot`:
    /// `Rollback`, `Diverged` (mais de uma ponta ou manifest ilegível), `Interrupted`, e nenhuma
    /// época mais nova sem reconhecer (`NeedsNewKey`). O estado, se ligado, passa para a época
    /// nova. **Retomável:** se uma tentativa anterior já publicou o manifest da época nova e
    /// caiu antes de gravar o estado, repetir com a mesma chave o adota (`Unchanged`) em vez de
    /// publicar outro; se a pasta mudou, o manifest novo vem sobre ele. O `Engine` antigo não
    /// deve mais ser usado depois do sucesso (ele vê a época nova como `NeedsNewKey`).
    pub async fn begin_epoch(
        &self,
        new_key: &VaultKey,
        roots: &Roots,
        now: u64,
    ) -> Result<(Engine<S>, Snapshot), EngineError>
    where
        S: Clone,
    {
        let next = Engine {
            store: self.store.clone(),
            keys: self.keys.next(new_key)?,
            device: self.device.clone(),
            state: self.state.clone(),
            filters: self.filters.clone(),
            scan_filters: self.scan_filters.clone(),
            foreign_memory: Mutex::new(ForeignAcks::default()),
            foreign_seen: Mutex::new(Seen::new(MAX_SEEN)),
        };
        // uma trava só: pedi-la de novo (pelo `snapshot` do `next`) daria `Busy` contra mim mesmo
        let _state_lock = self.lock_state().await?;
        ensure_not_interrupted(roots)?;

        // manifests da época nova que abrem com a chave nova (tentativa anterior que caiu)
        let Loaded { readable: own, .. } = next.load().await?;
        let prev = if own.is_empty() {
            // as pré-condições do `snapshot`, com as chaves e o estado da época atual
            let state = self.load_state().await?;
            let loaded = self.load().await?;
            let Loaded {
                readable, rejected, ..
            } = self.with_accepted(loaded, state.as_ref()).await?;
            let heads = match &state {
                Some(_) => {
                    let heads = tips(readable.clone());
                    guard(state.as_ref(), &readable, &heads)?;
                    heads
                }
                None => tips(readable),
            };
            if heads.len() > 1 || !rejected.is_empty() {
                return Err(EngineError::Diverged {
                    tips: heads.len(),
                    rejected: rejected.len(),
                });
            }
            None
        } else {
            // O `guard` não roda: o estado guarda uma ponta da época velha e `satisfied` sem
            // pontas dá `Rollback`. Objeto ilegível da época nova (forjado) não veta a rotação.
            let heads = tips(own);
            if heads.len() > 1 {
                return Err(EngineError::Diverged {
                    tips: heads.len(),
                    rejected: 0,
                });
            }
            heads.into_iter().next()
        };
        let snapshot = next.build_snapshot(prev, roots, now, &[]).await?;
        Ok((next, snapshot))
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
        let _state_lock = self.lock_state().await?;
        ensure_not_interrupted(roots)?;
        let state = self.load_state().await?;
        let from_manifest = match from {
            Some(id) => Some(self.read_manifest(id).await?),
            None => None,
        };
        let to_manifest = self.read_manifest(to).await?;
        // adotar um manifest só vale se ele continua a história já aceita (voltar a uma versão
        // mais velha é `restore`)
        if let Some(state) = &state {
            let loaded = self.load().await?;
            let Loaded { readable, .. } = self.with_accepted(loaded, Some(state)).await?;
            if !descends(state, &readable, to) {
                return Err(EngineError::Rollback);
            }
        }
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
            let (f, t) = (from_ix.get(key), to_ix.get(key));
            // entrada que o filtro recusa (de um lado ou do outro): o arquivo local fica como está
            if f.is_some_and(|e| !self.accepts(e)) || t.is_some_and(|e| !self.accepts(e)) {
                continue;
            }
            // o nome de um temporário do próprio `apply` não pode ser destino nem origem
            if is_reserved_name(key.1.rsplit('/').next().unwrap_or(&key.1)) {
                return Err(EngineError::ReservedName);
            }
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

        // 2. diário em disco (P-27): se o processo morrer daqui em diante, `recover` sabe voltar
        let journal_roots = distinct_roots(&planned);
        let (p, r) = (planned.clone(), journal_roots.clone());
        // o mesmo id em todos os diários: a recuperação decide por `apply`, não por raiz (P-33 2)
        let group = ApplyGroup::new(journal_roots.len())?;
        let g = group.clone();
        // trava as raízes que já existem **antes** do diário: um `recover` de outro processo que
        // visse o diário `open` deste `apply` o desfaria no meio (P-33 item 7); a trava fica até o
        // fim da função, inclusive nos erros
        let lock_dirs = journal_roots.clone();
        let locks = run(move || lock_roots(&lock_dirs)).await?;
        // raiz que ainda não existe é criada aqui (o diário mora nela, e a trava dela também) e
        // sai de novo se falhar
        let (created, _locks) = run(move || {
            let mut locks = locks;
            let created = open_journals(&r, &p, &g, &mut locks)?;
            Ok((created, locks))
        })
        .await?;
        let journals = Journals {
            roots: journal_roots,
            created,
        };

        // 3. buscar, verificar e gravar o conteúdo novo ao lado dos destinos
        let mut log = Vec::new();
        for p in planned.iter().filter(|p| p.kind != Kind::Remove) {
            let Some(entry) = &p.to else { continue };
            let plain = match self.plain_blob(entry).await {
                Ok(plain) => plain,
                Err(e) => return Err(abandon(log, e, journals).await),
            };
            let one = p.clone();
            let (back, result) = with_log(log, move |log| stage(log, &one, &plain)).await;
            log = back;
            if let Err(e) = result {
                return Err(abandon(log, e, journals).await);
            }
        }

        // 4. trocar e marcar o diário como `committed`; qualquer falha devolve tudo como estava
        let (all, marked) = (planned.clone(), journals.roots.clone());
        let (log, result) = with_log(log, move |log| {
            commit(log, &all)?;
            mark_committed(&marked, &all, &group)
        })
        .await;
        if let Err(e) = result {
            return Err(abandon(log, e, journals).await);
        }
        run(move || {
            finish(&planned);
            for root in &journals.roots {
                Journal::remove(root);
            }
            Ok(())
        })
        .await?;
        self.remember_after(to).await?;
        Ok(applied)
    }

    /// Recupera um `apply` interrompido nas pastas ligadas de `roots` (P-27): desfaz a troca
    /// pela metade, ou termina a que já valeu. Nunca sobrescreve nem apaga o que não bate com
    /// o diário. Sem diário, não faz nada. Idempotente.
    pub async fn recover(&self, roots: &Roots) -> Result<Recovered, EngineError> {
        let dirs: Vec<PathBuf> = roots
            .enabled()
            .iter()
            .map(|(_, root)| root.to_path_buf())
            .collect();
        run(move || {
            // um `apply` vivo (deste ou de outro processo) tem a trava: o diário dele não é de uma
            // queda, e desfazê-lo aqui quebraria o `apply` (P-33 item 7)
            let _locks = lock_roots(&dirs)?;
            let done = recover_all(&dirs)?;
            Ok(Recovered {
                rolled_back: done.rolled_back,
                completed: done.completed,
                kept: done.kept,
            })
        })
        .await
    }

    /// Grava, em `dir/<classe>/<path>.conflito-<dispositivo>-<16 hex do sha256>`, o texto claro
    /// dos perdedores de conflito das classes `config` e `secret` (ADR 0010), fora das pastas
    /// das classes. Idempotente; `dir` dentro de uma pasta de classe ligada é recusado.
    /// O perdedor de uma classe sem raiz em `roots` é pulado sem ser decifrado nem contado
    /// (P-37): quem não liga `secret` não guarda segredo em texto claro. Quem perdeu tem a
    /// raiz e grava a própria cópia ao aplicar (`conflicts_between`).
    pub async fn save_conflict_losers(
        &self,
        conflicts: &[Conflict],
        dir: &Path,
        roots: &Roots,
    ) -> Result<SavedLosers, EngineError> {
        // `vault` já tem a cópia no manifest; remoção-vs-edição não tem perdedor com texto;
        // classe sem raiz neste dispositivo não recebe cópia
        let losers: Vec<(&Conflict, &FileEntry)> = conflicts
            .iter()
            .filter(|c| matches!(c.class, Class::Config | Class::Secret))
            .filter(|c| roots.is_enabled(c.class))
            .filter_map(|c| c.loser.as_ref().map(|loser| (c, loser)))
            .collect();
        if losers.is_empty() {
            return Ok(SavedLosers {
                saved: 0,
                already: 0,
            });
        }

        let live_roots: Vec<PathBuf> = roots
            .enabled()
            .into_iter()
            .map(|(_, root)| root.to_path_buf())
            .collect();
        let conflicts_dir = dir.to_path_buf();
        let (checked_dir, checked_roots) = (conflicts_dir.clone(), live_roots.clone());
        run(move || check_conflicts_dir(&checked_dir, &checked_roots)).await?;

        // busca e confere tudo antes de gravar qualquer coisa
        let mut to_write: Vec<(PathBuf, Zeroizing<Vec<u8>>)> = Vec::new();
        for (conflict, loser) in losers {
            validate_device(&conflict.loser_device)?;
            let plain = self.plain_blob(loser).await?;
            let class_dir = match conflict.class {
                Class::Config => "config",
                _ => "secret",
            };
            let hex: String = loser.sha256.0[..8]
                .iter()
                .map(|b| format!("{b:02x}"))
                .collect();
            let relative = format!(
                "{class_dir}/{}.conflito-{}-{hex}",
                conflict.path, conflict.loser_device
            );
            to_write.push((safe_join(&conflicts_dir, &relative)?, plain));
        }

        run(move || {
            // depois do download dos blobs (que pode demorar): confere a pasta de novo
            check_conflicts_dir(&conflicts_dir, &live_roots)?;
            // passo 1: decide tudo, sem escrever. Um destino que já existe como arquivo conta
            // como já salvo (inclusive uma cópia que a pessoa editou: nunca é tocada); algo que
            // não é arquivo para o lote inteiro aqui, antes de gravar o primeiro.
            let mut pending: Vec<&(PathBuf, Zeroizing<Vec<u8>>)> = Vec::new();
            let mut seen = HashSet::new();
            let mut saved = SavedLosers {
                saved: 0,
                already: 0,
            };
            for item in &to_write {
                if !seen.insert(item.0.clone()) || !needs_write(&conflicts_dir, &item.0)? {
                    saved.already += 1;
                } else {
                    pending.push(item);
                }
            }
            // passo 2: grava
            for (dest, plain) in pending {
                // `false`: o destino apareceu depois do passo 1 e conta como já salvo
                if write_loser(dest, plain)? {
                    saved.saved += 1;
                } else {
                    saved.already += 1;
                }
            }
            Ok(saved)
        })
        .await
    }

    /// As cópias de perdedores (`config` e `secret`) que `save_conflict_losers` gravou em `dir`
    /// (P-28). `dir` passa pelas mesmas recusas da gravação (`UnsafeConflictsDir`).
    pub async fn list_conflict_losers(
        &self,
        dir: &Path,
        roots: &Roots,
    ) -> Result<Vec<LoserCopy>, EngineError> {
        let (dir, live) = (dir.to_path_buf(), live_dirs(roots));
        run(move || {
            check_conflicts_dir(&dir, &live)?;
            losers::list(&dir)
        })
        .await
    }

    /// Apaga as cópias de `copies` que ainda são o que a arca gravou (P-28). Revalida cada item
    /// no disco: fora de `dir/config` e `dir/secret`, link, nome fora do padrão ou cópia que a
    /// pessoa editou nunca são apagados (ficam em `skipped`). Idempotente.
    pub async fn clear_conflict_losers(
        &self,
        dir: &Path,
        roots: &Roots,
        copies: &[LoserCopy],
    ) -> Result<ClearReport, EngineError> {
        let (dir, live, copies) = (dir.to_path_buf(), live_dirs(roots), copies.to_vec());
        run(move || {
            check_conflicts_dir(&dir, &live)?;
            losers::clear(&dir, &copies)
        })
        .await
    }

    /// Os originais que a recuperação de um `apply` interrompido não pôde devolver
    /// (`.arca-kept-<hex>`) nas pastas ligadas de `roots` (P-33 item 5), em ordem estável. Um
    /// arquivo que a pessoa editou vem com `edited: true`.
    pub async fn list_kept(&self, roots: &Roots) -> Result<Vec<KeptOriginal>, EngineError> {
        let enabled = enabled_roots(roots);
        run(move || kept::list(&enabled)).await
    }

    /// Apaga os itens de `items` que ainda são o que a recuperação guardou (P-33 item 5).
    /// Revalida cada item no disco: fora das raízes ligadas, link, nome fora do padrão ou arquivo
    /// que a pessoa editou nunca são apagados (ficam em `skipped`). Idempotente. Apagar não
    /// sobrescreve o conteúdo (P-34).
    pub async fn clear_kept(
        &self,
        roots: &Roots,
        items: &[KeptOriginal],
    ) -> Result<ClearReport, EngineError> {
        let (enabled, items) = (enabled_roots(roots), items.to_vec());
        run(move || kept::clear(&enabled, &items)).await
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

        self.restore_files(&manifest, &enabled, &touched).await
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

    /// Cria as raízes, trava-as (P-43) e grava os arquivos. Se falha, desfaz **só o que criou**
    /// (arquivos, pastas e raízes que não existiam), ainda com a trava: o que a pessoa pôs na
    /// pasta depois da conferência de "vazia" nunca é apagado.
    async fn restore_files(
        &self,
        manifest: &Manifest,
        enabled: &[(Class, &Path)],
        touched: &[(PathBuf, bool)],
    ) -> Result<Restored, EngineError> {
        let mut created = Created::default();
        // as travas só caem no fim desta função, depois do desfazer
        let mut locks: Vec<FolderLock> = Vec::new();
        let result = async {
            for (root, existed) in touched {
                if !existed {
                    created.roots.push(root.clone());
                }
                let root = root.clone();
                run(move || create_private_dir(&root).map_err(io_error)).await?;
            }
            let roots: Vec<PathBuf> = touched.iter().map(|(root, _)| root.clone()).collect();
            locks = run(move || lock_roots(&roots)).await?;
            self.write_entries(manifest, enabled, &mut created).await
        }
        .await;
        if result.is_err() {
            let _ = run(move || {
                undo_restore(&created);
                Ok(())
            })
            .await;
        }
        drop(locks);
        result
    }

    async fn write_entries(
        &self,
        manifest: &Manifest,
        enabled: &[(Class, &Path)],
        created: &mut Created,
    ) -> Result<Restored, EngineError> {
        let mut files = 0;
        for entry in &manifest.files {
            let Some((_, root)) = enabled.iter().find(|(class, _)| *class == entry.class) else {
                continue;
            };
            if !self.accepts(entry) {
                continue;
            }
            let plain = self.plain_blob(entry).await?;
            let dest = safe_join(root, &entry.path)?;
            let root = root.to_path_buf();
            let (new_dirs, written) = run(move || Ok(place(&root, dest, &plain))).await?;
            // as pastas entram na lista mesmo que a gravação tenha falhado
            created.dirs.extend(new_dirs);
            created.files.push(written?);
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

/// Caminho com os links resolvidos: canonicaliza o ancestral existente mais próximo e reanexa
/// o resto (que ainda não existe). Compara-se isto, e não o texto, com as raízes das classes.
fn resolve(path: &Path) -> io::Result<PathBuf> {
    let mut existing = path.to_path_buf();
    let mut tail = Vec::new();
    loop {
        match fs::canonicalize(&existing) {
            Ok(mut resolved) => {
                resolved.extend(tail.iter().rev());
                return Ok(resolved);
            }
            Err(e) if e.kind() == io::ErrorKind::NotFound => {
                let Some(name) = existing.file_name().map(|n| n.to_os_string()) else {
                    return Err(e);
                };
                tail.push(name);
                if !existing.pop() {
                    return Err(e);
                }
            }
            Err(e) => return Err(e),
        }
    }
}

/// A pasta de conflitos tem de ser uma pasta real fora de toda pasta de classe ligada: sem
/// `..`, sem resolver (por links) para dentro de uma raiz, e sem links nas pastas de classe.
fn check_conflicts_dir(dir: &Path, roots: &[PathBuf]) -> Result<(), EngineError> {
    use std::path::Component;
    if dir.components().any(|c| c == Component::ParentDir) {
        return Err(EngineError::UnsafeConflictsDir);
    }
    let absolute = std::path::absolute(dir).map_err(io_error)?;
    let resolved = resolve(&absolute).map_err(io_error)?;
    let mut live = Vec::with_capacity(roots.len());
    for root in roots {
        live.push(resolve(&std::path::absolute(root).map_err(io_error)?).map_err(io_error)?);
    }
    // 1. `dir` não pode estar dentro de uma pasta de classe viva
    if live.iter().any(|root| resolved.starts_with(root)) {
        return Err(EngineError::UnsafeConflictsDir);
    }
    for class_dir in ["config", "secret"] {
        let target = resolved.join(class_dir);
        match fs::symlink_metadata(&target) {
            Ok(meta) if !meta.file_type().is_dir() => return Err(EngineError::UnsafeConflictsDir),
            Ok(meta) if writable_by_others(&meta) => return Err(EngineError::UnsafeConflictsDir),
            Ok(_) => {}
            Err(e) if e.kind() == io::ErrorKind::NotFound => {}
            Err(e) => return Err(io_error(e)),
        }
        // 2. o destino real é `dir/<classe>/...`: uma pasta viva dentro dele (ou igual) receberia
        //    o segredo, mesmo com `dir` fora da raiz
        if live.iter().any(|root| root.starts_with(&target)) {
            return Err(EngineError::UnsafeConflictsDir);
        }
    }
    // 3. pasta que grupo ou outros podem alterar permitiria trocar as cópias
    match fs::metadata(&resolved) {
        Ok(meta) if writable_by_others(&meta) => Err(EngineError::UnsafeConflictsDir),
        Ok(_) => Ok(()),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(io_error(e)),
    }
}

/// Grupo ou outros podem criar, apagar ou renomear dentro da pasta (unix).
#[cfg(unix)]
fn writable_by_others(meta: &fs::Metadata) -> bool {
    use std::os::unix::fs::PermissionsExt;
    meta.permissions().mode() & 0o022 != 0
}

#[cfg(not(unix))]
fn writable_by_others(_meta: &fs::Metadata) -> bool {
    false
}

/// Decide se um perdedor precisa ser gravado, sem escrever. `false` se o destino já é um
/// arquivo (o mesmo perdedor, ou a cópia que a pessoa editou: nunca é tocada). Algo que não é
/// arquivo no nome do destino é erro; link ou pasta gravável por outros no caminho é recusado.
fn needs_write(dir: &Path, dest: &Path) -> Result<bool, EngineError> {
    let parent = dest
        .parent()
        .ok_or(EngineError::Io(io::ErrorKind::InvalidInput))?;
    // pastas entre `dir` e o arquivo (a da classe e as do `path`) não podem ser links
    let relative = parent.strip_prefix(dir).unwrap_or(parent);
    let mut walk = dir.to_path_buf();
    for part in relative.components() {
        walk.push(part);
        match fs::symlink_metadata(&walk) {
            Ok(meta) if meta.file_type().is_dir() && !writable_by_others(&meta) => {}
            Ok(_) => return Err(EngineError::UnsafeConflictsDir),
            Err(e) if e.kind() == io::ErrorKind::NotFound => break,
            Err(e) => return Err(io_error(e)),
        }
    }
    match fs::symlink_metadata(dest) {
        Ok(meta) if meta.file_type().is_file() => Ok(false),
        Ok(_) => Err(EngineError::Io(io::ErrorKind::AlreadyExists)),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(true),
        Err(e) => Err(io_error(e)),
    }
}

/// Grava um perdedor (`needs_write` já decidiu): pastas `0700`, arquivo `0600`, atômico.
/// Nunca sobrescreve (P-30 (1)): o temporário entra no lugar por `hard_link`, que falha se o
/// destino apareceu depois do `needs_write`. Devolve `false` nesse caso, com o destino intocado.
fn write_loser(dest: &Path, plain: &[u8]) -> Result<bool, EngineError> {
    let parent = dest
        .parent()
        .ok_or(EngineError::Io(io::ErrorKind::InvalidInput))?;
    create_private_dir(parent).map_err(io_error)?;
    let mut random = [0u8; 8];
    getrandom::fill(&mut random).map_err(|_| EngineError::Io(io::ErrorKind::Other))?;
    let suffix: String = random.iter().map(|b| format!("{b:02x}")).collect();
    let tmp = parent.join(format!(".tmp-{suffix}"));
    write_atomic(&tmp, plain).map_err(io_error)?;

    let placed = match fs::hard_link(&tmp, dest) {
        Ok(()) => Ok(true),
        Err(e) if e.kind() == io::ErrorKind::AlreadyExists => Ok(false),
        // sistema de arquivos sem hard link: confere e renomeia, como o `place` do `apply`
        Err(e) if e.kind() == io::ErrorKind::Unsupported => match fs::symlink_metadata(dest) {
            Ok(_) => Ok(false),
            Err(e) if e.kind() == io::ErrorKind::NotFound => {
                fs::rename(&tmp, dest).map(|()| true).map_err(io_error)
            }
            Err(e) => Err(io_error(e)),
        },
        Err(e) => Err(io_error(e)),
    };
    let _ = fs::remove_file(&tmp);
    let placed = placed?;
    #[cfg(unix)]
    fs::File::open(parent)
        .and_then(|d| d.sync_all())
        .map_err(io_error)?;
    Ok(placed)
}

/// Com estado: alguma das pontas tem de continuar a história aceita, senão `Rollback`.
fn guard(
    state: Option<&DeviceState>,
    graph: &[(ObjectId, Manifest)],
    heads: &[Head],
) -> Result<(), EngineError> {
    let Some(state) = state else {
        return Ok(());
    };
    let ids: Vec<ObjectId> = heads.iter().map(|h| h.id).collect();
    if satisfied(state, graph, &ids) {
        Ok(())
    } else {
        Err(EngineError::Rollback)
    }
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

/// As pastas das classes ligadas.
fn enabled_roots(roots: &Roots) -> Vec<(Class, PathBuf)> {
    roots
        .enabled()
        .into_iter()
        .map(|(class, root)| (class, root.to_path_buf()))
        .collect()
}

fn live_dirs(roots: &Roots) -> Vec<PathBuf> {
    roots
        .enabled()
        .into_iter()
        .map(|(_, root)| root.to_path_buf())
        .collect()
}

/// Alguma pasta ligada tem um diário de `apply` interrompido (P-27)?
fn ensure_not_interrupted(roots: &Roots) -> Result<(), EngineError> {
    if roots
        .enabled()
        .iter()
        .any(|(_, root)| Journal::exists(root))
    {
        return Err(EngineError::Interrupted);
    }
    Ok(())
}

/// As raízes com alguma operação planejada, sem repetir, na ordem em que aparecem.
fn distinct_roots(planned: &[Planned]) -> Vec<PathBuf> {
    let mut roots: Vec<PathBuf> = Vec::new();
    for p in planned {
        if !roots.contains(&p.root) {
            roots.push(p.root.clone());
        }
    }
    roots
}

/// As raízes que têm diário num `apply` e as que o `apply` precisou criar para isso.
struct Journals {
    roots: Vec<PathBuf>,
    created: Vec<PathBuf>,
}

/// Grava o diário `open` de cada raiz (criando a raiz, privada, se ela ainda não existe) e
/// devolve as raízes criadas. Se algum falhar, nenhum diário fica e as raízes criadas saem.
fn open_journals(
    roots: &[PathBuf],
    planned: &[Planned],
    group: &ApplyGroup,
    locks: &mut Vec<FolderLock>,
) -> Result<Vec<PathBuf>, EngineError> {
    let mut created = Vec::new();
    let mut made: Vec<&PathBuf> = Vec::new();
    for root in roots {
        let result = (|| {
            if fs::symlink_metadata(root).is_err() {
                // cada pasta que vai faltar, da folha para cima: a falha as tira nessa ordem
                let mut dir = Some(root.as_path());
                while let Some(d) = dir.filter(|d| fs::symlink_metadata(d).is_err()) {
                    created.push(d.to_path_buf());
                    dir = d.parent();
                }
                create_private_dir(root).map_err(io_error)?;
                // a raiz criada agora ainda não tinha trava: pega antes de o diário existir
                locks.extend(lock_roots(std::slice::from_ref(root))?);
            }
            // sem sobrescrever: um diário que já existe é de outro `apply` ou de uma queda
            // as pastas que faltam agora, para a recuperação tirar as que uma queda deixou (P-33 4)
            Journal::open(root, planned)
                .with_dirs(missing_dirs(root, planned))
                .in_apply(group)
                .create(root)?;
            made.push(root);
            Ok(())
        })();
        if let Err(e) = result {
            // só os diários que este `apply` criou; o de outro dono não é tocado
            made.iter().for_each(|r| Journal::remove(r));
            created.iter().for_each(|r| {
                let _ = fs::remove_dir(r);
            });
            return Err(e);
        }
    }
    Ok(created)
}

/// Depois do `commit`: todos os arquivos já foram trocados, então a recuperação passa a
/// terminar em vez de desfazer.
fn mark_committed(
    roots: &[PathBuf],
    planned: &[Planned],
    group: &ApplyGroup,
) -> Result<(), EngineError> {
    for root in roots {
        let mut journal = Journal::open(root, planned).in_apply(group);
        journal.phase = Phase::Committed;
        journal.save(root)?;
    }
    Ok(())
}

/// Desfaz o diário em memória e, se o disco ficou limpo, apaga o diário em disco. Se a tarefa
/// de troca morreu (pânico: `Io(Other)`), o disco pode estar pela metade e o diário **fica**
/// para o `recover`.
async fn abandon(log: Vec<Undo>, error: EngineError, journals: Journals) -> EngineError {
    let died = error == EngineError::Io(io::ErrorKind::Other);
    let error = rollback(log, error).await;
    if !died {
        let _ = tokio::task::spawn_blocking(move || {
            journals.roots.iter().for_each(|r| Journal::remove(r));
            // raiz que o `apply` criou: sai de novo se ficou vazia (`remove_dir` não apaga o resto)
            journals.created.iter().for_each(|r| {
                let _ = fs::remove_dir(r);
            });
        })
        .await;
    }
    error
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

/// Motivo em `skipped` quando, entre a varredura e a leitura, o arquivo virou link, FIFO ou
/// pasta, um componente do caminho virou link, ou ele passou do teto (P-41).
const NOT_REGULAR: &str = "not a regular file or too large";

/// Lê o arquivo inteiro, **sem seguir link e com o teto valendo antes de ler** (P-41): a pasta-pai
/// é aberta descendo da raiz da classe sem seguir link e o arquivo é lido por ela, com tipo e
/// tamanho conferidos no descritor aberto. Some entre a varredura e a leitura, virou outra coisa
/// ou cresceu além do teto: vai para `skipped` em vez de abortar o snapshot.
async fn read_file(
    candidate: &Candidate,
    root: &Path,
    skipped: &mut Vec<Skipped>,
) -> Result<Option<Zeroizing<Vec<u8>>>, EngineError> {
    let root = root.to_path_buf();
    let path = candidate.path.clone();
    let read = tokio::task::spawn_blocking(move || read_beneath(&root, &path))
        .await
        .map_err(|_| EngineError::Io(io::ErrorKind::Other))?;
    let reason = match read {
        Ok(data) => return Ok(Some(Zeroizing::new(data))),
        Err(e) if e.kind() == io::ErrorKind::NotFound => "file disappeared",
        Err(e) if e.kind() == io::ErrorKind::InvalidInput => NOT_REGULAR,
        Err(e) => return Err(io_error(e)),
    };
    skipped.push(Skipped {
        class: candidate.class,
        path: candidate.path.clone(),
        reason,
    });
    Ok(None)
}

/// Lê `relative` (caminho do manifest, separador `/`, já validado) sob `root`. Link, FIFO, pasta
/// ou arquivo acima do teto: `InvalidInput`; um componente do caminho que virou link ou arquivo
/// também. Não existe mais: `NotFound`.
fn read_beneath(root: &Path, relative: &str) -> io::Result<Vec<u8>> {
    let relative = Path::new(relative);
    let invalid = || io::Error::from(io::ErrorKind::InvalidInput);
    let name = relative
        .file_name()
        .and_then(|n| n.to_str())
        .ok_or_else(invalid)?;
    let parent = relative.parent().unwrap_or(Path::new(""));
    let folder = Folder::open_beneath(root, parent).map_err(|e| match e {
        OpenError::NotAFolder => invalid(),
        OpenError::Io(e) if e.kind() == io::ErrorKind::NotADirectory => invalid(),
        OpenError::Io(e) => e,
    })?;
    Ok(folder.read_limited(name, MAX_FILE_BYTES)?.bytes)
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

/// O que uma restauração criou, para desfazer só isso se ela falhar (P-43). A pasta de destino
/// foi conferida como vazia no início, mas a restauração leva tempo e a pasta não está
/// protegida de quem a edita: o que não está aqui nunca é apagado.
#[derive(Default)]
struct Created {
    /// Raízes que não existiam.
    roots: Vec<PathBuf>,
    /// Pastas que não existiam, o pai antes do filho.
    dirs: Vec<PathBuf>,
    /// Arquivos gravados.
    files: Vec<PathBuf>,
}

/// Desfaz uma restauração que falhou: arquivos, depois pastas do fundo para a raiz, depois as
/// raízes criadas. Só remove pasta **vazia** (`remove_dir`): se a pessoa pôs algo dentro, a pasta
/// e o conteúdo ficam.
fn undo_restore(created: &Created) {
    for file in &created.files {
        let _ = fs::remove_file(file);
    }
    for dir in created.dirs.iter().rev() {
        let _ = fs::remove_dir(dir);
    }
    for root in &created.roots {
        let _ = fs::remove_dir(root);
    }
}

/// As pastas entre `root` (exclusive) e `parent` (inclusive) que não existem agora, o pai antes
/// do filho.
fn missing_ancestors(root: &Path, parent: &Path) -> Vec<PathBuf> {
    let mut missing = Vec::new();
    let mut current = parent;
    while current != root && fs::symlink_metadata(current).is_err() {
        missing.push(current.to_path_buf());
        match current.parent() {
            Some(up) => current = up,
            None => break,
        }
    }
    missing.reverse();
    missing
}

/// Cria as pastas que faltam e grava `plain` em `dest`. Devolve as pastas criadas (mesmo se a
/// gravação falhar) e o resultado da gravação.
fn place(root: &Path, dest: PathBuf, plain: &[u8]) -> (Vec<PathBuf>, Result<PathBuf, EngineError>) {
    let Some(parent) = dest.parent() else {
        return (
            Vec::new(),
            write_atomic(&dest, plain)
                .map(|()| dest.clone())
                .map_err(io_error),
        );
    };
    let new_dirs = missing_ancestors(root, parent);
    if let Err(e) = create_private_dir(parent) {
        return (new_dirs, Err(io_error(e)));
    }
    let written = write_atomic(&dest, plain)
        .map(|()| dest.clone())
        .map_err(io_error);
    (new_dirs, written)
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
        let m = forged_from_tip(rig, edit).await;
        let sealed = m.seal(&keys(1)).unwrap();
        rig.store.put(&sealed.id, &sealed.bytes).await.unwrap();
        sealed.id
    }

    /// Como `put_forged`, mas **sem** a validação do `Manifest::seal`: serializa e cifra direto,
    /// como faria quem tem a chave e publica o que `validate` recusa.
    async fn put_forged_raw(rig: &Rig, edit: impl FnOnce(&mut Manifest)) -> ObjectId {
        let m = forged_from_tip(rig, edit).await;
        let json = serde_json::to_vec(&m).unwrap();
        let sealed = seal(&keys(1), ObjectKind::Manifest, &json).unwrap();
        rig.store.put(&sealed.id, &sealed.bytes).await.unwrap();
        sealed.id
    }

    /// O manifest da ponta atual, filho dela, depois de `edit`.
    async fn forged_from_tip(rig: &Rig, edit: impl FnOnce(&mut Manifest)) -> Manifest {
        let heads = rig.engine("pc").heads().await.unwrap();
        let tip = heads.tips.last().unwrap();
        let mut m = tip.manifest.clone();
        m.base_version = Some(Hash32(tip.id.hash));
        m.counter += 1;
        edit(&mut m);
        m
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

    // ---------- épocas estrangeiras e reconhecimento (ADR 0018, P-47a) ----------

    fn foreign(epoch: u32, id: ObjectId) -> ForeignEpoch {
        ForeignEpoch { epoch, id }
    }

    fn needs_new_key() -> EngineError {
        EngineError::NeedsNewKey { have: 1, found: 2 }
    }

    #[tokio::test]
    async fn foreign_epochs_lists_only_manifests_of_a_newer_epoch() {
        let rig = Rig::new();
        let vault = rig.dir("vault");
        let (_, new) = rotated(&rig, &vault).await;

        assert_eq!(
            rig.engine("pc").foreign_epochs().await.unwrap(),
            [foreign(2, new)]
        );
        // com a chave da época 2 não há nada estrangeiro: a época 1 é histórico, a 2 é a dele
        assert!(engine_at(&rig, 2)
            .foreign_epochs()
            .await
            .unwrap()
            .is_empty());
    }

    #[tokio::test]
    async fn an_acknowledged_foreign_epoch_stops_blocking_and_is_ignored() {
        let rig = Rig::new();
        let vault = rig.dir("vault");
        let (old, _) = rotated(&rig, &vault).await;
        let stale = guarded(&rig, "pc", &state_path(&rig));
        assert_eq!(stale.heads().await, Err(needs_new_key()));

        let seen = stale.foreign_epochs().await.unwrap();
        assert_eq!(seen.len(), 1);
        stale.acknowledge_foreign(&seen).await.unwrap();

        let heads = stale.heads().await.unwrap();
        assert_eq!(ids_of(&heads), [old]);
        // o objeto reconhecido não vira `rejected` nem `superseded`: é só ignorado
        assert!(heads.rejected.is_empty() && heads.superseded.is_empty());
        assert!(stale.foreign_epochs().await.unwrap().is_empty());

        write(&vault, "b.md", b"2");
        let (id, ..) = created(stale.snapshot(&vault_only(&vault), 3).await.unwrap());
        assert_ne!(id, old);
        assert_eq!(ids_of(&stale.heads().await.unwrap()), [id]);
    }

    /// A próxima época é previsível: reconhecer só pela época deixaria um objeto forjado
    /// esconder a rotação de verdade. Com o id, um manifest novo da mesma época bloqueia de novo.
    #[tokio::test]
    async fn a_new_manifest_of_the_same_foreign_epoch_blocks_again() {
        let rig = Rig::new();
        let vault = rig.dir("vault");
        let (_, new) = rotated(&rig, &vault).await;
        let stale = guarded(&rig, "pc", &state_path(&rig));
        let seen = stale.foreign_epochs().await.unwrap();
        stale.acknowledge_foreign(&seen).await.unwrap();
        stale.heads().await.unwrap();

        let later = put_epoch_manifest(&rig, 2, Some(new), 2).await;
        assert_eq!(stale.heads().await, Err(needs_new_key()));
        assert_eq!(stale.foreign_epochs().await.unwrap(), [foreign(2, later)]);
    }

    #[tokio::test]
    async fn acknowledging_another_id_or_another_epoch_does_not_unblock() {
        let rig = Rig::new();
        let vault = rig.dir("vault");
        let (old, new) = rotated(&rig, &vault).await;
        let stale = guarded(&rig, "pc", &state_path(&rig));

        stale
            .acknowledge_foreign(&[foreign(2, old), foreign(3, new)])
            .await
            .unwrap();
        assert_eq!(stale.heads().await, Err(needs_new_key()));
        assert_eq!(stale.foreign_epochs().await.unwrap(), [foreign(2, new)]);
    }

    #[tokio::test]
    async fn without_a_state_file_the_acknowledgement_lives_only_in_the_engine() {
        let rig = Rig::new();
        let vault = rig.dir("vault");
        rotated(&rig, &vault).await;
        let stale = rig.engine("pc");
        let seen = stale.foreign_epochs().await.unwrap();
        assert_eq!(seen.len(), 1);

        // P-47b: sem estado o reconhecimento vale na memória deste `Engine` (e só nela)
        stale.acknowledge_foreign(&seen).await.unwrap();
        assert!(stale.heads().await.is_ok());
        assert_eq!(rig.engine("pc").heads().await, Err(needs_new_key()));
    }

    /// O reconhecimento é de arquivo, não de memória; e o `snapshot` (que regrava o estado do
    /// dispositivo a cada vez) não o apaga.
    #[tokio::test]
    async fn the_acknowledgement_survives_a_restart_and_a_state_write() {
        let rig = Rig::new();
        let vault = rig.dir("vault");
        rotated(&rig, &vault).await;
        let path = state_path(&rig);
        let first = guarded(&rig, "pc", &path);
        let seen = first.foreign_epochs().await.unwrap();
        first.acknowledge_foreign(&seen).await.unwrap();

        let second = guarded(&rig, "pc", &path);
        second.heads().await.unwrap();
        write(&vault, "b.md", b"2");
        created(second.snapshot(&vault_only(&vault), 3).await.unwrap());
        assert!(fs::metadata(crate::foreign::path(&path)).is_ok());

        let third = guarded(&rig, "pc", &path);
        third.heads().await.unwrap();
        assert!(third.foreign_epochs().await.unwrap().is_empty());
    }

    /// Passou do teto, sai o mais antigo: nunca é erro, e o que saiu bloqueia de novo.
    #[tokio::test]
    async fn past_the_cap_the_oldest_acknowledgements_leave_without_an_error() {
        let rig = Rig::new();
        let vault = rig.dir("vault");
        rotated(&rig, &vault).await;
        for counter in 2..=300u64 {
            put_epoch_manifest(&rig, 2, None, counter).await;
        }
        let stale = guarded(&rig, "pc", &state_path(&rig));
        let all = stale.foreign_epochs().await.unwrap();
        assert_eq!(all.len(), 300);

        stale.acknowledge_foreign(&all).await.unwrap();
        assert_eq!(stale.foreign_epochs().await.unwrap(), &all[..44]);
        assert_eq!(stale.heads().await, Err(needs_new_key()));
    }

    #[tokio::test]
    async fn a_corrupt_acknowledgement_file_always_fails_closed() {
        let rig = Rig::new();
        let vault = rig.dir("vault");
        write(&vault, "a.md", b"1");
        let path = state_path(&rig);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(crate::foreign::path(&path), b"lixo").unwrap();
        let stale = guarded(&rig, "pc", &path);
        // P-47b: o arquivo também guarda a pendência do `pull`, então é lido sempre; corrompido
        // nunca vale como "nenhuma pendência" (o 47a só o lia com época mais nova no disco)
        assert_eq!(stale.heads().await, Err(EngineError::CorruptState));
        assert_eq!(
            stale.snapshot(&vault_only(&vault), 1).await,
            Err(EngineError::CorruptState)
        );
    }

    #[tokio::test]
    async fn acknowledging_is_busy_while_another_process_holds_the_state() {
        let rig = Rig::new();
        let vault = rig.dir("vault");
        rotated(&rig, &vault).await;
        let path = state_path(&rig);
        let stale = guarded(&rig, "pc", &path);
        let seen = stale.foreign_epochs().await.unwrap();

        let held = state_busy(&path);
        assert_eq!(
            stale.acknowledge_foreign(&seen).await,
            Err(EngineError::Busy)
        );
        assert!(fs::metadata(crate::foreign::path(&path)).is_err());
        drop(held);
        stale.acknowledge_foreign(&seen).await.unwrap();
        stale.heads().await.unwrap();
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
        diverge_in(Class::Vault, base, branches).await
    }

    /// Só a pasta da classe `class` é ligada.
    fn roots_of(class: Class, path: &Path) -> Roots {
        let mut roots = Roots::default();
        match class {
            Class::Vault => roots.vault = Some(path.to_path_buf()),
            Class::Config => roots.config = Some(path.to_path_buf()),
            Class::Secret => roots.secret = Some(path.to_path_buf()),
        }
        roots
    }

    async fn diverge_in(
        class: Class,
        base: &[(&str, &[u8])],
        branches: Vec<Branch<'_>>,
    ) -> Diverged {
        let rig = Rig::new();
        let folder = rig.dir("pasta-da-classe");
        for (path, data) in base {
            write(&folder, path, data);
        }
        let (base_id, ..) = created(
            rig.engine("base")
                .snapshot(&roots_of(class, &folder), 1)
                .await
                .unwrap(),
        );
        let (mut others, mut tips) = (Vec::new(), Vec::new());
        for (device, at, edits) in branches {
            let other = forked(&rig);
            let dir = other.tmp.path().join("vault-ramo");
            let eng = other.engine(device);
            eng.restore(&base_id, &roots_of(class, &dir)).await.unwrap();
            for (path, data) in edits {
                write(&dir, path, data);
            }
            let (id, ..) = created(eng.snapshot(&roots_of(class, &dir), at).await.unwrap());
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

    /// P-26: o nome da cópia não cabe num componente de caminho: a cópia sai com o nome
    /// encurtado e a fusão não trava.
    #[tokio::test]
    async fn a_conflict_copy_name_that_does_not_fit_is_shortened_and_the_merge_goes_through() {
        let name = format!("{}.md", "n".repeat(250));
        let d = diverge(
            &[(name.as_str(), b"1")],
            vec![
                ("pa", 10, vec![(name.as_str(), b"A")]),
                ("pb", 20, vec![(name.as_str(), b"B")]),
            ],
        )
        .await;
        let MergeOutcome::Merged {
            manifest,
            conflicts,
            tips_after,
            ..
        } = d.rig.engine("pc").merge(30).await.unwrap()
        else {
            panic!("esperava Merged");
        };
        assert_eq!(tips_after, 1);
        assert_eq!(d.rig.count(ObjectKind::Manifest).await, 4);
        let copy = conflicts[0].copy_path.clone().expect("copy_path");
        assert!(copy.len() <= 255 && copy.contains('~'), "{copy}");
        assert_eq!(manifest.files.len(), 2);
        assert!(manifest.files.iter().any(|f| f.path == copy));
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

    // ---------- último manifest aceito e rollback (Fase 2a, PR 4) ----------

    use crate::state::DeviceState;

    fn state_path(rig: &Rig) -> PathBuf {
        rig.tmp.path().join("estado/estado.json")
    }

    fn accepted(id: &ObjectId, epoch: u32) -> DeviceState {
        DeviceState {
            key_epoch: epoch,
            last_accepted: Hash32(id.hash),
        }
    }

    fn saved(path: &Path) -> Option<DeviceState> {
        DeviceState::load(path).unwrap()
    }

    /// Engine do `rig` com a proteção ligada no arquivo `state`.
    fn guarded(rig: &Rig, device: &str, state: &Path) -> Engine<Backend> {
        rig.engine(device).with_state(state)
    }

    #[tokio::test]
    async fn without_a_state_file_nothing_is_protected_and_nothing_is_written() {
        let rig = Rig::new();
        let vault = rig.dir("vault");
        write(&vault, "a.md", b"1");
        created(
            rig.engine("pc")
                .snapshot(&vault_only(&vault), 1)
                .await
                .unwrap(),
        );
        let old = forked(&rig);
        write(&vault, "a.md", b"2");
        created(
            rig.engine("pc")
                .snapshot(&vault_only(&vault), 2)
                .await
                .unwrap(),
        );
        // um armazenamento que voltou no tempo é aceito: é o comportamento sem `with_state`
        write(&vault, "a.md", b"3");
        created(
            old.engine("pc")
                .snapshot(&vault_only(&vault), 3)
                .await
                .unwrap(),
        );
        assert!(!state_path(&rig).exists());
    }

    #[tokio::test]
    async fn the_state_follows_snapshot_unchanged_snapshot_merge_and_apply() {
        // snapshot e snapshot sem mudança
        let rig = Rig::new();
        let vault = rig.dir("vault");
        let path = state_path(&rig);
        let eng = guarded(&rig, "pc", &path);
        write(&vault, "a.md", b"1");
        let (s1, ..) = created(eng.snapshot(&vault_only(&vault), 1).await.unwrap());
        assert_eq!(saved(&path), Some(accepted(&s1, 1)));
        write(&vault, "a.md", b"2");
        let (s2, ..) = created(eng.snapshot(&vault_only(&vault), 2).await.unwrap());
        assert_eq!(saved(&path), Some(accepted(&s2, 1)));
        assert!(matches!(
            eng.snapshot(&vault_only(&vault), 3).await.unwrap(),
            Snapshot::Unchanged { head, .. } if head == s2
        ));
        assert_eq!(saved(&path), Some(accepted(&s2, 1)));

        // merge
        let d = diverge(
            &[("a.md", b"1"), ("b.md", b"1")],
            vec![
                ("pa", 10, vec![("a.md", b"A")]),
                ("pb", 20, vec![("b.md", b"B")]),
            ],
        )
        .await;
        let merge_state = state_path(&d.rig);
        let MergeOutcome::Merged { id, .. } =
            guarded(&d.rig, "pc", &merge_state).merge(30).await.unwrap()
        else {
            panic!("esperava Merged");
        };
        assert_eq!(saved(&merge_state), Some(accepted(&id, 1)));

        // apply
        let h = hist().await;
        let apply_state = state_path(&h.rig);
        accepted(&h.v1, 1).save(&apply_state).unwrap();
        guarded(&h.rig, "pc", &apply_state)
            .apply(Some(&h.v1), &h.v2, &vault_only(&h.target))
            .await
            .unwrap();
        assert_eq!(saved(&apply_state), Some(accepted(&h.v2, 1)));
    }

    // ---------- trava do arquivo de estado entre processos (P-31 item 4) ----------

    /// Outro processo do mesmo dispositivo está dentro de uma operação com este estado: um
    /// segundo `lock` do mesmo arquivo (abertura própria, como a de outro processo) o simula.
    fn state_busy(path: &Path) -> crate::dirfd::FolderLock {
        crate::state::lock(path).unwrap()
    }

    #[tokio::test]
    async fn a_snapshot_is_busy_while_another_process_holds_the_state_and_writes_nothing() {
        let rig = Rig::new();
        let vault = rig.dir("vault");
        let path = state_path(&rig);
        let eng = guarded(&rig, "pc", &path);
        write(&vault, "a.md", b"1");
        let held = state_busy(&path);
        assert_eq!(
            eng.snapshot(&vault_only(&vault), 1).await,
            Err(EngineError::Busy)
        );
        assert_eq!(rig.count(ObjectKind::Manifest).await, 0);
        assert_eq!(saved(&path), None);
        // o `Busy` também vem do estado, não só de `apply`/`recover`: a mensagem não pode dizer
        // que a culpa é de um deles (um consumidor mostra este texto à pessoa)
        let message = EngineError::Busy.to_string();
        assert!(message.contains("device state"), "{message}");
        assert!(!message.contains(path.to_str().unwrap()), "{message}");
        // solta a trava: a mesma chamada passa
        drop(held);
        created(eng.snapshot(&vault_only(&vault), 1).await.unwrap());
    }

    #[tokio::test]
    async fn a_merge_is_busy_while_another_process_holds_the_state_and_writes_nothing() {
        let d = diverge(
            &[("a.md", b"1"), ("b.md", b"1")],
            vec![
                ("pa", 10, vec![("a.md", b"A")]),
                ("pb", 20, vec![("b.md", b"B")]),
            ],
        )
        .await;
        let path = state_path(&d.rig);
        let eng = guarded(&d.rig, "pc", &path);
        let held = state_busy(&path);
        assert_eq!(eng.merge(30).await, Err(EngineError::Busy));
        assert_eq!(eng.heads().await.unwrap().tips.len(), 2);
        assert_eq!(saved(&path), None);
        drop(held);
        assert!(matches!(
            eng.merge(30).await.unwrap(),
            MergeOutcome::Merged { .. }
        ));
    }

    #[tokio::test]
    async fn an_apply_is_busy_while_another_process_holds_the_state_and_writes_nothing() {
        let h = hist().await;
        let path = state_path(&h.rig);
        accepted(&h.v1, 1).save(&path).unwrap();
        let eng = guarded(&h.rig, "pc", &path);
        let roots = vault_only(&h.target);
        let before = tree(&h.target);
        let held = state_busy(&path);
        assert_eq!(
            eng.apply(Some(&h.v1), &h.v2, &roots).await.map(|_| ()),
            Err(EngineError::Busy)
        );
        assert_eq!(tree(&h.target), before);
        assert_eq!(saved(&path), Some(accepted(&h.v1, 1)));
        drop(held);
        eng.apply(Some(&h.v1), &h.v2, &roots).await.unwrap();
        assert_eq!(tree(&h.target), h.v2_tree);
    }

    /// Backend que, a cada leitura e gravação, tenta pegar a trava do estado como um segundo
    /// processo faria e registra se ela estava ocupada: prova que a trava vale **durante** a
    /// operação, e não só no começo.
    #[derive(Clone)]
    struct StateLockSpy {
        inner: Backend,
        state: PathBuf,
        busy: std::sync::Arc<std::sync::Mutex<Vec<bool>>>,
    }

    impl StateLockSpy {
        fn new(inner: &Backend, state: &Path) -> StateLockSpy {
            StateLockSpy {
                inner: inner.clone(),
                state: state.to_path_buf(),
                busy: Default::default(),
            }
        }

        fn probe(&self) {
            let busy = crate::state::lock(&self.state).err() == Some(EngineError::Busy);
            self.busy.lock().unwrap().push(busy);
        }

        /// Ocupada em todas as sondagens, e houve ao menos uma.
        fn always_busy(&self) -> bool {
            let seen = self.busy.lock().unwrap();
            !seen.is_empty() && seen.iter().all(|busy| *busy)
        }
    }

    impl BlobStore for StateLockSpy {
        async fn put(&self, id: &ObjectId, bytes: &[u8]) -> Result<(), StoreError> {
            self.probe();
            self.inner.put(id, bytes).await
        }
        async fn get(&self, id: &ObjectId) -> Result<Option<Vec<u8>>, StoreError> {
            self.probe();
            self.inner.get(id).await
        }
        async fn has(&self, id: &ObjectId) -> Result<bool, StoreError> {
            self.inner.has(id).await
        }
        async fn list(&self, kind: ObjectKind) -> Result<Vec<ObjectId>, StoreError> {
            self.inner.list(kind).await
        }
        async fn delete(&self, id: &ObjectId) -> Result<(), StoreError> {
            self.inner.delete(id).await
        }
        fn capabilities(&self) -> Capabilities {
            self.inner.capabilities()
        }
    }

    #[tokio::test]
    async fn the_state_stays_locked_during_the_whole_snapshot_merge_and_apply() {
        // snapshot
        let rig = Rig::new();
        let vault = rig.dir("vault");
        let path = state_path(&rig);
        let spy = StateLockSpy::new(&rig.store, &path);
        let eng = Engine::new(spy.clone(), keys(1), "pc")
            .unwrap()
            .with_state(&path);
        write(&vault, "a.md", b"1");
        created(eng.snapshot(&vault_only(&vault), 1).await.unwrap());
        assert!(spy.always_busy(), "snapshot");

        // merge
        let d = diverge(
            &[("a.md", b"1"), ("b.md", b"1")],
            vec![
                ("pa", 10, vec![("a.md", b"A")]),
                ("pb", 20, vec![("b.md", b"B")]),
            ],
        )
        .await;
        let path = state_path(&d.rig);
        let spy = StateLockSpy::new(&d.rig.store, &path);
        let eng = Engine::new(spy.clone(), keys(1), "pc")
            .unwrap()
            .with_state(&path);
        assert!(matches!(
            eng.merge(30).await.unwrap(),
            MergeOutcome::Merged { .. }
        ));
        assert!(spy.always_busy(), "merge");

        // apply
        let h = hist().await;
        let path = state_path(&h.rig);
        accepted(&h.v1, 1).save(&path).unwrap();
        let spy = StateLockSpy::new(&h.rig.store, &path);
        let eng = Engine::new(spy.clone(), keys(1), "pc")
            .unwrap()
            .with_state(&path);
        eng.apply(Some(&h.v1), &h.v2, &vault_only(&h.target))
            .await
            .unwrap();
        assert!(spy.always_busy(), "apply");
    }

    /// A guarda sai de escopo também quando a operação falha: um erro não deixa o estado preso.
    #[tokio::test]
    async fn a_failed_operation_releases_the_state_lock() {
        let rig = Rig::new();
        let vault = rig.dir("vault");
        let path = state_path(&rig);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(&path, b"lixo").unwrap();
        let eng = guarded(&rig, "pc", &path);
        assert_eq!(
            eng.snapshot(&vault_only(&vault), 1).await,
            Err(EngineError::CorruptState)
        );
        drop(state_busy(&path));
    }

    #[tokio::test]
    async fn two_different_state_files_do_not_block_each_other() {
        let rig = Rig::new();
        let vault = rig.dir("vault");
        let other = rig.tmp.path().join("outro/estado.json");
        let held = state_busy(&state_path(&rig));
        write(&vault, "a.md", b"1");
        created(
            guarded(&rig, "pc", &other)
                .snapshot(&vault_only(&vault), 1)
                .await
                .unwrap(),
        );
        drop(held);
    }

    #[tokio::test]
    async fn an_engine_without_a_state_file_never_locks_and_never_creates_the_lock_file() {
        let rig = Rig::new();
        let vault = rig.dir("vault");
        let path = state_path(&rig);
        let held = state_busy(&path);
        write(&vault, "a.md", b"1");
        created(
            rig.engine("pc")
                .snapshot(&vault_only(&vault), 1)
                .await
                .unwrap(),
        );
        drop(held);
        fs::remove_file(crate::state::lock_path(&path)).unwrap();
        write(&vault, "a.md", b"2");
        created(
            rig.engine("pc")
                .snapshot(&vault_only(&vault), 2)
                .await
                .unwrap(),
        );
        assert!(!crate::state::lock_path(&path).exists());
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn the_lock_file_is_0600_next_to_the_state_in_a_0700_folder() {
        use std::os::unix::fs::PermissionsExt;
        let rig = Rig::new();
        let vault = rig.dir("vault");
        let path = state_path(&rig);
        write(&vault, "a.md", b"1");
        created(
            guarded(&rig, "pc", &path)
                .snapshot(&vault_only(&vault), 1)
                .await
                .unwrap(),
        );
        let lock = crate::state::lock_path(&path);
        assert_eq!(lock.file_name().unwrap(), "estado.json.lock");
        let mode = |p: &Path| fs::metadata(p).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode(&lock), 0o600);
        assert_eq!(mode(path.parent().unwrap()), 0o700);
    }

    /// Um link no lugar do `.lock` não é seguido: a operação recusa antes de agir.
    #[cfg(unix)]
    #[tokio::test]
    async fn a_symlink_where_the_lock_file_should_be_is_refused_before_anything_runs() {
        let rig = Rig::new();
        let vault = rig.dir("vault");
        let path = state_path(&rig);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        let elsewhere = rig.tmp.path().join("alvo");
        fs::write(&elsewhere, b"intacto").unwrap();
        std::os::unix::fs::symlink(&elsewhere, crate::state::lock_path(&path)).unwrap();
        write(&vault, "a.md", b"1");
        let result = guarded(&rig, "pc", &path)
            .snapshot(&vault_only(&vault), 1)
            .await;
        assert!(matches!(result, Err(EngineError::Io(_))), "{result:?}");
        assert_eq!(rig.count(ObjectKind::Manifest).await, 0);
        assert_eq!(fs::read(&elsewhere).unwrap(), b"intacto");
    }

    /// As duas travas são independentes e nenhuma fica presa pela tentativa que falhou na outra.
    #[tokio::test]
    async fn the_state_lock_and_the_root_lock_do_not_hold_each_other() {
        let h = hist().await;
        let path = state_path(&h.rig);
        accepted(&h.v1, 1).save(&path).unwrap();
        let eng = guarded(&h.rig, "pc", &path);
        let roots = vault_only(&h.target);

        // estado ocupado: a raiz não pode ter ficado presa
        let held = state_busy(&path);
        assert_eq!(
            eng.apply(Some(&h.v1), &h.v2, &roots).await.map(|_| ()),
            Err(EngineError::Busy)
        );
        drop(lock_roots(std::slice::from_ref(&h.target)).unwrap());
        drop(held);

        // raiz ocupada: o estado não pode ter ficado preso
        let held = lock_roots(std::slice::from_ref(&h.target)).unwrap();
        assert_eq!(
            eng.apply(Some(&h.v1), &h.v2, &roots).await.map(|_| ()),
            Err(EngineError::Busy)
        );
        drop(state_busy(&path));
        drop(held);
    }

    // ---------- o estado não grava depois da operação valer (P-31 item 3) ----------

    /// Cria (se faltar) a pasta do estado e a deixa sem escrita (`0o500`): o arquivo existente
    /// ainda lê, mas gravar um novo falha com `PermissionDenied`.
    #[cfg(unix)]
    fn lock_state_dir(path: &Path) {
        use std::os::unix::fs::PermissionsExt;
        let dir = path.parent().unwrap();
        fs::create_dir_all(dir).unwrap();
        // a trava do estado (P-31 item 4) precisa existir antes da pasta perder a escrita: abrir
        // um arquivo que já existe funciona, criar um novo não
        fs::File::create(crate::state::lock_path(path)).unwrap();
        fs::set_permissions(dir, fs::Permissions::from_mode(0o500)).unwrap();
    }

    #[cfg(unix)]
    fn unlock_state_dir(path: &Path) {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(path.parent().unwrap(), fs::Permissions::from_mode(0o700)).unwrap();
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn a_snapshot_that_created_a_manifest_says_so_when_the_state_cannot_be_saved() {
        let rig = Rig::new();
        let vault = rig.dir("vault");
        let path = state_path(&rig);
        let eng = guarded(&rig, "pc", &path);
        write(&vault, "a.md", b"1");
        lock_state_dir(&path);
        assert_eq!(
            eng.snapshot(&vault_only(&vault), 1).await,
            Err(EngineError::StateNotSaved)
        );
        // a operação valeu: o manifest está no armazenamento, só o estado não foi gravado
        let tips = eng.heads().await.unwrap().tips;
        assert_eq!(tips.len(), 1);
        assert_eq!(saved(&path), None);

        // refazer é seguro: vira `Unchanged` e grava o estado
        unlock_state_dir(&path);
        assert!(matches!(
            eng.snapshot(&vault_only(&vault), 2).await.unwrap(),
            Snapshot::Unchanged { head, .. } if head == tips[0].id
        ));
        assert_eq!(saved(&path), Some(accepted(&tips[0].id, 1)));
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn an_unchanged_snapshot_that_cannot_save_the_state_is_a_plain_io_error() {
        let rig = Rig::new();
        let vault = rig.dir("vault");
        let path = state_path(&rig);
        let eng = guarded(&rig, "pc", &path);
        write(&vault, "a.md", b"1");
        created(eng.snapshot(&vault_only(&vault), 1).await.unwrap());
        lock_state_dir(&path);
        // nada aconteceu além de aceitar a ponta como estado: não é "a operação valeu"
        assert_eq!(
            eng.snapshot(&vault_only(&vault), 2).await,
            Err(EngineError::Io(io::ErrorKind::PermissionDenied))
        );
        unlock_state_dir(&path);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn an_apply_that_took_effect_says_so_when_the_state_cannot_be_saved() {
        let h = hist().await;
        let path = state_path(&h.rig);
        accepted(&h.v1, 1).save(&path).unwrap();
        let eng = guarded(&h.rig, "pc", &path);
        let roots = vault_only(&h.target);
        lock_state_dir(&path);
        assert_eq!(
            eng.apply(Some(&h.v1), &h.v2, &roots).await.map(|_| ()),
            Err(EngineError::StateNotSaved)
        );
        // a pasta já é a nova, sem sobras e sem diário
        assert_eq!(tree(&h.target), h.v2_tree);
        assert!(!crate::journal::Journal::exists(&h.target));
        assert_eq!(saved(&path), Some(accepted(&h.v1, 1)));

        // refazer é seguro: nada a mudar, e o estado avança
        unlock_state_dir(&path);
        let again = eng.apply(Some(&h.v1), &h.v2, &roots).await.unwrap();
        assert_eq!(again.created + again.updated + again.removed, 0);
        assert_eq!(saved(&path), Some(accepted(&h.v2, 1)));
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn a_merge_that_was_written_says_so_when_the_state_cannot_be_saved() {
        let d = diverge(
            &[("a.md", b"1"), ("b.md", b"1")],
            vec![
                ("pa", 10, vec![("a.md", b"A")]),
                ("pb", 20, vec![("b.md", b"B")]),
            ],
        )
        .await;
        let path = state_path(&d.rig);
        let eng = guarded(&d.rig, "pc", &path);
        lock_state_dir(&path);
        assert_eq!(eng.merge(30).await, Err(EngineError::StateNotSaved));
        // a fusão existe: as duas pontas viraram uma
        let tips = eng.heads().await.unwrap().tips;
        assert_eq!(tips.len(), 1);
        // refazer é seguro
        unlock_state_dir(&path);
        assert_eq!(
            eng.merge(31).await.unwrap(),
            MergeOutcome::Nothing {
                head: Some(tips[0].id)
            }
        );
    }

    /// O ataque: o armazenamento volta a uma cópia velha. Nada é escrito, em nenhuma operação.
    #[tokio::test]
    async fn a_storage_that_went_back_in_time_is_a_rollback_everywhere() {
        let rig = Rig::new();
        let vault = rig.dir("vault");
        let path = state_path(&rig);
        let eng = guarded(&rig, "pc", &path);
        write(&vault, "a.md", b"1");
        let (s1, ..) = created(eng.snapshot(&vault_only(&vault), 1).await.unwrap());
        let old = forked(&rig); // só tem s1
        write(&vault, "a.md", b"2");
        let (s2, ..) = created(eng.snapshot(&vault_only(&vault), 2).await.unwrap());

        let stale = guarded(&old, "pc", &path);
        let manifests = old.count(ObjectKind::Manifest).await;
        let blobs = old.count(ObjectKind::Blob).await;

        write(&vault, "a.md", b"3");
        assert_eq!(
            stale.snapshot(&vault_only(&vault), 3).await,
            Err(EngineError::Rollback)
        );
        assert_eq!(stale.merge(4).await, Err(EngineError::Rollback));
        let fresh = old.dir("destino");
        assert_eq!(
            stale.apply(None, &s1, &vault_only(&fresh)).await,
            Err(EngineError::Rollback)
        );
        assert_eq!(fs::read_dir(&fresh).unwrap().count(), 0);

        assert_eq!(old.count(ObjectKind::Manifest).await, manifests);
        assert_eq!(old.count(ObjectKind::Blob).await, blobs);
        assert_eq!(saved(&path), Some(accepted(&s2, 1)), "o estado não recua");
    }

    #[tokio::test]
    async fn an_empty_or_unrelated_storage_is_a_rollback() {
        let rig = Rig::new();
        let vault = rig.dir("vault");
        let path = state_path(&rig);
        write(&vault, "a.md", b"1");
        created(
            guarded(&rig, "pc", &path)
                .snapshot(&vault_only(&vault), 1)
                .await
                .unwrap(),
        );

        // armazenamento vazio: o backend apagou tudo (ou o disco está vazio por engano)
        let empty = Rig::new();
        let eng = guarded(&empty, "pc", &path);
        assert_eq!(
            eng.snapshot(&vault_only(&vault), 2).await,
            Err(EngineError::Rollback)
        );
        assert_eq!(eng.merge(3).await, Err(EngineError::Rollback));

        // armazenamento com outra história, sem o manifest aceito
        let other = Rig::new();
        let other_vault = other.dir("vault");
        write(&other_vault, "x.md", b"outro");
        created(
            other
                .engine("pd")
                .snapshot(&vault_only(&other_vault), 1)
                .await
                .unwrap(),
        );
        assert_eq!(
            guarded(&other, "pc", &path)
                .snapshot(&vault_only(&vault), 4)
                .await,
            Err(EngineError::Rollback)
        );
        assert_eq!(other.count(ObjectKind::Manifest).await, 1);
    }

    #[tokio::test]
    async fn a_storage_that_moved_forward_is_accepted_and_remembered() {
        let rig = Rig::new();
        let vault = rig.dir("vault");
        let path = state_path(&rig);
        write(&vault, "a.md", b"1");
        created(
            guarded(&rig, "pc", &path)
                .snapshot(&vault_only(&vault), 1)
                .await
                .unwrap(),
        );
        // outro dispositivo avança a história
        write(&vault, "b.md", b"2");
        let (s2, ..) = created(
            rig.engine("pd")
                .snapshot(&vault_only(&vault), 2)
                .await
                .unwrap(),
        );
        assert!(matches!(
            guarded(&rig, "pc", &path)
                .snapshot(&vault_only(&vault), 3)
                .await
                .unwrap(),
            Snapshot::Unchanged { head, .. } if head == s2
        ));
        assert_eq!(saved(&path), Some(accepted(&s2, 1)));
    }

    // ---------- `list` atrasado (P-31 item 2) ----------

    /// Backend de `list` atrasado, como um S3-compatível de listagem eventual: `put` e `get`
    /// são fiéis, mas o `list` de manifests não mostra o que está em `hidden` (ou nada, com
    /// `list_empty`). Declara `consistent_list: false`. Também simula o backend que perdeu o
    /// objeto de verdade (`gone`), o hostil (`forged`) e o fora do ar (`failing`), e conta os
    /// `get` de manifest por hash.
    #[derive(Clone)]
    struct LaggedList {
        inner: Backend,
        script: std::sync::Arc<std::sync::Mutex<LagScript>>,
    }

    #[derive(Default)]
    struct LagScript {
        hidden: Vec<[u8; 32]>,
        list_empty: bool,
        gone: Vec<[u8; 32]>,
        forged: Vec<([u8; 32], Vec<u8>)>,
        failing: Vec<[u8; 32]>,
        manifest_gets: Vec<[u8; 32]>,
        /// Na primeira leitura de um blob, grava este arquivo (a pessoa mexendo na pasta durante
        /// uma restauração). Só roda uma vez.
        drop_in: Option<(PathBuf, Vec<u8>)>,
    }

    impl LaggedList {
        fn new(inner: &Backend) -> LaggedList {
            LaggedList {
                inner: inner.clone(),
                script: Default::default(),
            }
        }

        /// O `list` não mostra `id` (o `get` continua entregando).
        fn hide(&self, id: &ObjectId) {
            self.script.lock().unwrap().hidden.push(id.hash);
        }

        /// O `list` de manifests volta vazio.
        fn hide_every_manifest(&self) {
            self.script.lock().unwrap().list_empty = true;
        }

        /// O objeto sumiu de verdade: `get` devolve `None` e `has` devolve `false`.
        fn lose(&self, id: &ObjectId) {
            self.script.lock().unwrap().gone.push(id.hash);
        }

        /// O `get` de `id` devolve `bytes` (que não têm o hash do id).
        fn forge(&self, id: &ObjectId, bytes: &[u8]) {
            self.script
                .lock()
                .unwrap()
                .forged
                .push((id.hash, bytes.to_vec()));
        }

        /// O `get` de `id` falha com `Unavailable`.
        fn fail_get(&self, id: &ObjectId) {
            self.script.lock().unwrap().failing.push(id.hash);
        }

        /// Na primeira leitura de um blob, grava `bytes` em `path`.
        fn drop_file_on_first_blob_get(&self, path: &Path, bytes: &[u8]) {
            self.script.lock().unwrap().drop_in = Some((path.to_path_buf(), bytes.to_vec()));
        }

        fn gets_of(&self, id: &ObjectId) -> usize {
            let script = self.script.lock().unwrap();
            script
                .manifest_gets
                .iter()
                .filter(|hash| **hash == id.hash)
                .count()
        }
    }

    impl BlobStore for LaggedList {
        async fn put(&self, id: &ObjectId, bytes: &[u8]) -> Result<(), StoreError> {
            self.inner.put(id, bytes).await
        }
        async fn get(&self, id: &ObjectId) -> Result<Option<Vec<u8>>, StoreError> {
            {
                let mut script = self.script.lock().unwrap();
                if id.kind == ObjectKind::Manifest {
                    script.manifest_gets.push(id.hash);
                }
                if id.kind == ObjectKind::Blob {
                    if let Some((path, bytes)) = script.drop_in.take() {
                        fs::write(path, bytes).unwrap();
                    }
                }
                if script.failing.contains(&id.hash) {
                    return Err(StoreError::Unavailable(std::io::ErrorKind::Other));
                }
                if script.gone.contains(&id.hash) {
                    return Ok(None);
                }
                if let Some((_, bytes)) = script.forged.iter().find(|(hash, _)| *hash == id.hash) {
                    return Ok(Some(bytes.clone()));
                }
            }
            self.inner.get(id).await
        }
        async fn has(&self, id: &ObjectId) -> Result<bool, StoreError> {
            if self.script.lock().unwrap().gone.contains(&id.hash) {
                return Ok(false);
            }
            self.inner.has(id).await
        }
        async fn list(&self, kind: ObjectKind) -> Result<Vec<ObjectId>, StoreError> {
            let all = self.inner.list(kind).await?;
            if kind != ObjectKind::Manifest {
                return Ok(all);
            }
            let script = self.script.lock().unwrap();
            if script.list_empty {
                return Ok(Vec::new());
            }
            Ok(all
                .into_iter()
                .filter(|id| !script.hidden.contains(&id.hash))
                .collect())
        }
        async fn delete(&self, id: &ObjectId) -> Result<(), StoreError> {
            self.inner.delete(id).await
        }
        fn capabilities(&self) -> Capabilities {
            Capabilities {
                consistent_list: false,
            }
        }
    }

    /// Engine sobre o `LaggedList`, com a proteção ligada no arquivo `state`.
    fn lagged(lag: &LaggedList, state: &Path) -> Engine<LaggedList> {
        Engine::new(lag.clone(), keys(1), "pc")
            .unwrap()
            .with_state(state)
    }

    #[tokio::test]
    async fn a_list_that_does_not_show_the_accepted_manifest_is_not_a_rollback_for_snapshot() {
        let rig = Rig::new();
        let vault = rig.dir("vault");
        let path = state_path(&rig);
        let lag = LaggedList::new(&rig.store);
        let eng = lagged(&lag, &path);
        write(&vault, "a.md", b"1");
        let (s1, ..) = created(eng.snapshot(&vault_only(&vault), 1).await.unwrap());
        assert_eq!(saved(&path), Some(accepted(&s1, 1)));

        // o `list` ainda não mostra o s1 que acabamos de gravar; o `get` o entrega
        lag.hide(&s1);
        assert!(matches!(
            eng.snapshot(&vault_only(&vault), 2).await,
            Ok(Snapshot::Unchanged { head, .. }) if head == s1
        ));
        assert_eq!(saved(&path), Some(accepted(&s1, 1)));
        assert_eq!(rig.count(ObjectKind::Manifest).await, 1);
    }

    #[tokio::test]
    async fn an_empty_list_with_the_accepted_manifest_in_the_store_is_not_a_rollback() {
        let rig = Rig::new();
        let vault = rig.dir("vault");
        let path = state_path(&rig);
        let lag = LaggedList::new(&rig.store);
        let eng = lagged(&lag, &path);
        write(&vault, "a.md", b"1");
        let (s1, ..) = created(eng.snapshot(&vault_only(&vault), 1).await.unwrap());

        lag.hide_every_manifest();
        assert!(matches!(
            eng.snapshot(&vault_only(&vault), 2).await,
            Ok(Snapshot::Unchanged { head, .. }) if head == s1
        ));
    }

    #[tokio::test]
    async fn the_next_snapshot_builds_on_the_accepted_manifest_the_list_did_not_show() {
        let rig = Rig::new();
        let vault = rig.dir("vault");
        let path = state_path(&rig);
        let lag = LaggedList::new(&rig.store);
        let eng = lagged(&lag, &path);
        write(&vault, "a.md", b"1");
        let (s1, ..) = created(eng.snapshot(&vault_only(&vault), 1).await.unwrap());

        lag.hide(&s1);
        write(&vault, "a.md", b"2");
        let (s2, m2, ..) = created(eng.snapshot(&vault_only(&vault), 2).await.unwrap());
        // a história continua em linha reta (sem dois ramos) e o estado avança
        assert_eq!(m2.base_version, Some(Hash32(s1.hash)));
        assert_eq!(saved(&path), Some(accepted(&s2, 1)));
    }

    #[tokio::test]
    async fn a_list_that_does_not_show_the_accepted_manifest_is_not_a_rollback_for_merge() {
        let rig = Rig::new();
        let vault = rig.dir("vault");
        let path = state_path(&rig);
        let lag = LaggedList::new(&rig.store);
        let eng = lagged(&lag, &path);
        write(&vault, "a.md", b"1");
        let (s1, ..) = created(eng.snapshot(&vault_only(&vault), 1).await.unwrap());

        lag.hide(&s1);
        assert_eq!(
            eng.merge(3).await,
            Ok(MergeOutcome::Nothing { head: Some(s1) })
        );
    }

    #[tokio::test]
    async fn a_list_that_does_not_show_the_accepted_manifest_is_not_a_rollback_for_apply() {
        let h = hist().await;
        let path = state_path(&h.rig);
        // o aceito é a ponta (v2): ninguém o cita, então só o `get` o mostra
        accepted(&h.v2, 1).save(&path).unwrap();
        let lag = LaggedList::new(&h.rig.store);
        lag.hide(&h.v2);
        let eng = lagged(&lag, &path);

        eng.apply(Some(&h.v1), &h.v2, &vault_only(&h.target))
            .await
            .unwrap();
        assert_eq!(tree(&h.target), h.v2_tree);
        assert_eq!(saved(&path), Some(accepted(&h.v2, 1)));
    }

    /// O cenário do PENDING: dois dispositivos fundiram o mesmo par; a gêmea de menor hash está
    /// no `list`, a de maior hash (a que este dispositivo aceitou) ainda não.
    #[tokio::test]
    async fn a_twin_merge_accepted_but_not_yet_listed_is_not_a_rollback() {
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
        absorb(&d.rig, &other);
        let (smaller, larger) = if first.hash < second.hash {
            (first, second)
        } else {
            (second, first)
        };

        let path = state_path(&d.rig);
        accepted(&larger, 1).save(&path).unwrap();
        let lag = LaggedList::new(&d.rig.store);
        lag.hide(&larger);
        let eng = lagged(&lag, &path);
        assert_eq!(
            eng.merge(40).await,
            Ok(MergeOutcome::Nothing {
                head: Some(smaller)
            })
        );
    }

    /// Rollback de verdade: o aceito sumiu do armazenamento (também do `get`), não é atraso.
    #[tokio::test]
    async fn an_accepted_manifest_that_is_really_gone_is_still_a_rollback() {
        let rig = Rig::new();
        let vault = rig.dir("vault");
        let path = state_path(&rig);
        let lag = LaggedList::new(&rig.store);
        let eng = lagged(&lag, &path);
        write(&vault, "a.md", b"1");
        created(eng.snapshot(&vault_only(&vault), 1).await.unwrap());
        write(&vault, "a.md", b"2");
        let (s2, ..) = created(eng.snapshot(&vault_only(&vault), 2).await.unwrap());

        lag.hide(&s2);
        lag.lose(&s2);
        assert_eq!(
            eng.snapshot(&vault_only(&vault), 3).await,
            Err(EngineError::Rollback)
        );
        assert_eq!(eng.merge(4).await, Err(EngineError::Rollback));
        assert_eq!(saved(&path), Some(accepted(&s2, 1)), "o estado não recua");
    }

    /// Um backend hostil que serve outros bytes sob o id aceito não passa: o hash não bate.
    #[tokio::test]
    async fn bytes_that_do_not_match_the_accepted_hash_are_still_a_rollback() {
        let rig = Rig::new();
        let vault = rig.dir("vault");
        let path = state_path(&rig);
        let lag = LaggedList::new(&rig.store);
        let eng = lagged(&lag, &path);
        write(&vault, "a.md", b"1");
        let (s1, ..) = created(eng.snapshot(&vault_only(&vault), 1).await.unwrap());

        lag.hide(&s1);
        lag.forge(&s1, b"lixo que nao e o manifest");
        assert_eq!(
            eng.snapshot(&vault_only(&vault), 2).await,
            Err(EngineError::Rollback)
        );
        assert_eq!(saved(&path), Some(accepted(&s1, 1)));
    }

    /// Backend fora do ar na leitura do aceito: é erro de armazenamento (tentar de novo), não
    /// uma acusação de rollback.
    #[tokio::test]
    async fn an_unavailable_get_of_the_accepted_manifest_is_a_store_error_not_a_rollback() {
        let rig = Rig::new();
        let vault = rig.dir("vault");
        let path = state_path(&rig);
        let lag = LaggedList::new(&rig.store);
        let eng = lagged(&lag, &path);
        write(&vault, "a.md", b"1");
        let (s1, ..) = created(eng.snapshot(&vault_only(&vault), 1).await.unwrap());

        lag.hide(&s1);
        lag.fail_get(&s1);
        assert_eq!(
            eng.snapshot(&vault_only(&vault), 2).await,
            Err(EngineError::Store(StoreError::Unavailable(
                std::io::ErrorKind::Other
            )))
        );
    }

    /// O estado é de outra época que as chaves: não se tenta ler o aceito (rotação segue o
    /// caminho de sempre) e nenhum `get` extra sai.
    #[tokio::test]
    async fn a_state_from_another_epoch_does_not_read_the_accepted_manifest_by_id() {
        let rig = Rig::new();
        let vault = rig.dir("vault");
        let path = state_path(&rig);
        let lag = LaggedList::new(&rig.store);
        write(&vault, "a.md", b"1");
        let (s1, ..) = created(
            rig.engine("pc")
                .snapshot(&vault_only(&vault), 1)
                .await
                .unwrap(),
        );
        accepted(&s1, 2).save(&path).unwrap();

        lag.hide(&s1);
        assert_eq!(
            lagged(&lag, &path).snapshot(&vault_only(&vault), 2).await,
            Err(EngineError::Rollback)
        );
        assert_eq!(lag.gets_of(&s1), 0);
    }

    /// Com o aceito no `list` não há leitura extra: o `get` que a listagem já faz basta.
    #[tokio::test]
    async fn a_listed_accepted_manifest_is_read_once_not_twice() {
        let rig = Rig::new();
        let vault = rig.dir("vault");
        let path = state_path(&rig);
        let lag = LaggedList::new(&rig.store);
        let eng = lagged(&lag, &path);
        write(&vault, "a.md", b"1");
        let (s1, ..) = created(eng.snapshot(&vault_only(&vault), 1).await.unwrap());

        let before = lag.gets_of(&s1);
        assert_eq!(
            eng.merge(2).await,
            Ok(MergeOutcome::Nothing { head: Some(s1) })
        );
        assert_eq!(lag.gets_of(&s1) - before, 1);
    }

    /// Falso `Rollback` seria o pior efeito prático: dois dispositivos fundiram o mesmo par, a
    /// regra de colapso escolheu a gêmea de menor hash como ponta, e este dispositivo tinha
    /// aceito a de **maior** hash (a que ele próprio gerou).
    #[tokio::test]
    async fn accepting_the_larger_hash_twin_of_a_merge_is_not_a_rollback() {
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
        absorb(&d.rig, &other);
        let (smaller, larger) = if first.hash < second.hash {
            (first, second)
        } else {
            (second, first)
        };

        let path = state_path(&d.rig);
        accepted(&larger, 1).save(&path).unwrap();
        let eng = guarded(&d.rig, "pc", &path);
        assert_eq!(ids_of(&eng.heads().await.unwrap()), [smaller]);
        let work = d.rig.dir("trabalho");
        eng.restore(&smaller, &vault_only(&work)).await.unwrap();
        assert!(matches!(
            eng.snapshot(&vault_only(&work), 40).await.unwrap(),
            Snapshot::Unchanged { head, .. } if head == smaller
        ));
        assert_eq!(saved(&path), Some(accepted(&smaller, 1)));
    }

    #[tokio::test]
    async fn a_rotation_to_a_newer_epoch_is_accepted_and_an_older_epoch_is_a_rollback() {
        let rig = Rig::new();
        let vault = rig.dir("vault");
        let path = state_path(&rig);
        write(&vault, "a.md", b"1");
        created(
            guarded(&rig, "pc", &path)
                .snapshot(&vault_only(&vault), 1)
                .await
                .unwrap(),
        );
        // rotação: a cadeia da época 2 começa com `base_version = null`
        put_epoch_manifest(&rig, 2, None, 1).await;
        let (next, ..) = created(
            engine_at(&rig, 2)
                .with_state(&path)
                .snapshot(&vault_only(&vault), 5)
                .await
                .unwrap(),
        );
        assert_eq!(saved(&path), Some(accepted(&next, 2)));

        // o estado é da época 2; um armazenamento só com a época 1 não é aceito
        let old = Rig::new();
        let old_vault = old.dir("vault");
        write(&old_vault, "a.md", b"1");
        created(
            old.engine("pc")
                .snapshot(&vault_only(&old_vault), 1)
                .await
                .unwrap(),
        );
        assert_eq!(
            guarded(&old, "pc", &path)
                .snapshot(&vault_only(&old_vault), 6)
                .await,
            Err(EngineError::Rollback)
        );
    }

    /// O estado só avança (achado C1 da auditoria). `merge` junta as duas primeiras pontas por
    /// `(counter, hash)`, que podem não incluir a que o dispositivo aceitou; gravar a fusão como
    /// "aceita" faria o estado **recuar**, e um backend que escondesse a ponta aceita depois
    /// passaria sem `Rollback`.
    #[tokio::test]
    async fn a_merge_that_does_not_include_the_accepted_tip_leaves_the_state_where_it_was() {
        for accepted_index in [2usize, 1] {
            let d = diverge(
                &[("a.md", b"1")],
                vec![
                    ("pa", 10, vec![("x.md", b"X")]),
                    ("pb", 20, vec![("y.md", b"Y")]),
                    ("pc", 30, vec![("z.md", b"Z")]),
                ],
            )
            .await;
            let path = state_path(&d.rig);
            let eng = guarded(&d.rig, "pd", &path);
            let tips: Vec<ObjectId> = eng
                .heads()
                .await
                .unwrap()
                .tips
                .iter()
                .map(|h| h.id)
                .collect();
            // `merge` funde tips[0] e tips[1]; o índice 2 fica de fora, o 1 entra
            let accepted_id = tips[accepted_index];
            accepted(&accepted_id, 1).save(&path).unwrap();

            let MergeOutcome::Merged { id: merged, .. } = eng.merge(40).await.unwrap() else {
                panic!("esperava Merged");
            };
            if accepted_index == 2 {
                assert_eq!(saved(&path), Some(accepted(&accepted_id, 1)), "não recua");
                // o backend esconde a ponta aceita: o que sobra não a continua
                fs::remove_file(d.rig.store_dir().join(accepted_id.to_string())).unwrap();
                assert_eq!(eng.merge(41).await, Err(EngineError::Rollback));
                let vault = d.rig.dir("vault-do-teste");
                assert_eq!(
                    eng.snapshot(&vault_only(&vault), 42).await,
                    Err(EngineError::Rollback)
                );
            } else {
                // controle: a fusão inclui o aceito, então o estado avança para ela
                assert_eq!(saved(&path), Some(accepted(&merged, 1)));
            }
        }
    }

    /// `apply` adota um manifest: só um que continue a história aceita. Voltar a uma versão
    /// mais velha é `restore`, que não mexe no estado.
    #[tokio::test]
    async fn applying_a_manifest_that_does_not_descend_from_the_accepted_one_is_a_rollback() {
        let h = hist().await;
        let path = state_path(&h.rig);
        accepted(&h.v2, 1).save(&path).unwrap();
        let before = tree(&h.target);
        assert_eq!(
            guarded(&h.rig, "pc", &path)
                .apply(Some(&h.v1), &h.v1, &vault_only(&h.target))
                .await,
            Err(EngineError::Rollback)
        );
        assert_eq!(tree(&h.target), before);
        assert_eq!(saved(&path), Some(accepted(&h.v2, 1)));

        // restaurar a versão velha continua permitido e não mexe no estado
        let elsewhere = h.rig.tmp.path().join("versao-antiga");
        guarded(&h.rig, "pc", &path)
            .restore(&h.v1, &vault_only(&elsewhere))
            .await
            .unwrap();
        assert_eq!(saved(&path), Some(accepted(&h.v2, 1)));
    }

    #[tokio::test]
    async fn a_failed_operation_does_not_touch_the_state() {
        let rig = Rig::new();
        let vault = rig.dir("vault");
        let path = state_path(&rig);
        let eng = guarded(&rig, "pc", &path);
        // falha antes de existir estado: nenhum arquivo nasce
        let gone = rig.tmp.path().join("sumiu");
        assert_eq!(
            eng.snapshot(&vault_only(&gone), 1).await,
            Err(EngineError::Io(io::ErrorKind::NotFound))
        );
        assert!(!path.exists());

        write(&vault, "a.md", b"1");
        let (s1, ..) = created(eng.snapshot(&vault_only(&vault), 2).await.unwrap());
        assert_eq!(
            eng.snapshot(&vault_only(&gone), 3).await,
            Err(EngineError::Io(io::ErrorKind::NotFound))
        );
        assert_eq!(saved(&path), Some(accepted(&s1, 1)));
    }

    /// Estado corrompido nunca desliga a proteção, e o arquivo não é apagado.
    #[tokio::test]
    async fn a_corrupt_state_blocks_every_operation_and_is_kept() {
        let h = hist().await;
        let path = state_path(&h.rig);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(&path, b"lixo").unwrap();
        let eng = guarded(&h.rig, "pc", &path);
        assert_eq!(
            eng.snapshot(&vault_only(&h.target), 9).await,
            Err(EngineError::CorruptState)
        );
        assert_eq!(eng.merge(9).await, Err(EngineError::CorruptState));
        assert_eq!(
            eng.apply(Some(&h.v1), &h.v2, &vault_only(&h.target)).await,
            Err(EngineError::CorruptState)
        );
        assert_eq!(fs::read(&path).unwrap(), b"lixo");
    }

    // ---------- perdedores de config e secret (Fase 2a, PR 3b) ----------

    /// Grava um blob no armazenamento e devolve a entrada que o descreve (o perdedor).
    async fn stored_loser(rig: &Rig, class: Class, path: &str, data: &[u8]) -> FileEntry {
        let sealed = seal(&keys(1), ObjectKind::Blob, data).unwrap();
        rig.store.put(&sealed.id, &sealed.bytes).await.unwrap();
        FileEntry {
            path: path.to_string(),
            class,
            blob: Hash32(sealed.id.hash),
            size: data.len() as u64,
            sha256: sha(data),
        }
    }

    fn conflict(class: Class, path: &str, device: &str, loser: Option<FileEntry>) -> Conflict {
        Conflict {
            class,
            path: path.to_string(),
            kind: if loser.is_some() {
                crate::merge::ConflictKind::BothEdited
            } else {
                crate::merge::ConflictKind::DeletedVsEdited
            },
            loser,
            loser_device: device.to_string(),
            copy_path: None,
        }
    }

    /// Primeiros 8 bytes do sha256 do texto claro, em hex: o sufixo do nome gravado.
    fn short_hex(data: &[u8]) -> String {
        Sha256::digest(data)[..8]
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect()
    }

    /// Duas pastas de classe vivas e uma pasta de conflitos fora delas.
    struct Vault3 {
        rig: Rig,
        roots: Roots,
        dir: PathBuf,
    }

    fn vault3() -> Vault3 {
        let rig = Rig::new();
        let roots = Roots {
            vault: None,
            config: Some(rig.dir("cfg-vivo")),
            secret: Some(rig.dir("sec-vivo")),
        };
        let dir = rig.tmp.path().join("conflitos");
        Vault3 { rig, roots, dir }
    }

    #[tokio::test]
    async fn config_and_secret_losers_are_saved_outside_the_class_folders() {
        let v = vault3();
        let a = stored_loser(&v.rig, Class::Config, "settings.toml", b"perdedor A").await;
        let b = stored_loser(&v.rig, Class::Secret, "oauth/slack.json", b"{token-b}").await;
        let conflicts = [
            conflict(Class::Config, "settings.toml", "pa", Some(a)),
            conflict(Class::Secret, "oauth/slack.json", "pb", Some(b)),
        ];
        let saved = v
            .rig
            .engine("pc")
            .save_conflict_losers(&conflicts, &v.dir, &v.roots)
            .await
            .unwrap();
        assert_eq!(
            saved,
            SavedLosers {
                saved: 2,
                already: 0
            }
        );

        let name_a = format!(
            "config/settings.toml.conflito-pa-{}",
            short_hex(b"perdedor A")
        );
        let name_b = format!(
            "secret/oauth/slack.json.conflito-pb-{}",
            short_hex(b"{token-b}")
        );
        assert_eq!(fs::read(v.dir.join(&name_a)).unwrap(), b"perdedor A");
        assert_eq!(fs::read(v.dir.join(&name_b)).unwrap(), b"{token-b}");
        // nada caiu nas pastas vivas das classes
        assert_eq!(
            fs::read_dir(v.roots.config.as_ref().unwrap())
                .unwrap()
                .count(),
            0
        );
        assert_eq!(
            fs::read_dir(v.roots.secret.as_ref().unwrap())
                .unwrap()
                .count(),
            0
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn saved_losers_are_0600_and_their_folders_0700() {
        use std::os::unix::fs::PermissionsExt;
        let v = vault3();
        let loser = stored_loser(&v.rig, Class::Secret, "oauth/x.json", b"segredo").await;
        v.rig
            .engine("pc")
            .save_conflict_losers(
                &[conflict(Class::Secret, "oauth/x.json", "pa", Some(loser))],
                &v.dir,
                &v.roots,
            )
            .await
            .unwrap();
        let mode = |p: &Path| fs::metadata(p).unwrap().permissions().mode() & 0o777;
        let file = v.dir.join(format!(
            "secret/oauth/x.json.conflito-pa-{}",
            short_hex(b"segredo")
        ));
        assert_eq!(mode(&file), 0o600);
        assert_eq!(mode(&v.dir), 0o700);
        assert_eq!(mode(&v.dir.join("secret")), 0o700);
        assert_eq!(mode(&v.dir.join("secret/oauth")), 0o700);
    }

    /// `vault` já tem a cópia dentro do manifest; remoção vs edição não tem perdedor com texto.
    #[tokio::test]
    async fn vault_conflicts_and_deletions_have_nothing_to_save() {
        let v = vault3();
        let entry = stored_loser(&v.rig, Class::Vault, "a.md", b"x").await;
        let conflicts = [
            conflict(Class::Vault, "a.md", "pa", Some(entry)),
            conflict(Class::Config, "settings.toml", "pb", None),
        ];
        let saved = v
            .rig
            .engine("pc")
            .save_conflict_losers(&conflicts, &v.dir, &v.roots)
            .await
            .unwrap();
        assert_eq!(
            saved,
            SavedLosers {
                saved: 0,
                already: 0
            }
        );
        assert!(!v.dir.exists(), "não cria a pasta sem ter o que gravar");
    }

    #[tokio::test]
    async fn saving_twice_is_harmless() {
        let v = vault3();
        let loser = stored_loser(&v.rig, Class::Config, "settings.toml", b"A").await;
        let conflicts = [conflict(Class::Config, "settings.toml", "pa", Some(loser))];
        let eng = v.rig.engine("pc");
        eng.save_conflict_losers(&conflicts, &v.dir, &v.roots)
            .await
            .unwrap();
        let again = eng
            .save_conflict_losers(&conflicts, &v.dir, &v.roots)
            .await
            .unwrap();
        assert_eq!(
            again,
            SavedLosers {
                saved: 0,
                already: 1
            }
        );
    }

    /// A pessoa resolveu o conflito editando a cópia: o nome já está tomado, e a cópia dela
    /// vale. Conta como já salvo e **nunca** é sobrescrita.
    #[tokio::test]
    async fn a_copy_the_person_edited_is_left_alone_and_counted_as_saved() {
        let v = vault3();
        let loser = stored_loser(&v.rig, Class::Config, "settings.toml", b"A").await;
        let name = format!("settings.toml.conflito-pa-{}", short_hex(b"A"));
        write(&v.dir.join("config"), &name, b"conteudo de outra pessoa");
        let result = v
            .rig
            .engine("pc")
            .save_conflict_losers(
                &[conflict(Class::Config, "settings.toml", "pa", Some(loser))],
                &v.dir,
                &v.roots,
            )
            .await;
        assert_eq!(
            result,
            Ok(SavedLosers {
                saved: 0,
                already: 1
            })
        );
        assert_eq!(
            fs::read(v.dir.join("config").join(&name)).unwrap(),
            b"conteudo de outra pessoa"
        );
    }

    /// Uma cópia editada não pode travar o resto do lote (achado 3 da auditoria).
    #[tokio::test]
    async fn an_edited_copy_does_not_stop_the_rest_of_the_batch() {
        let v = vault3();
        let a = stored_loser(&v.rig, Class::Config, "a.toml", b"A").await;
        let b = stored_loser(&v.rig, Class::Secret, "b.json", b"B").await;
        let conflicts = [
            conflict(Class::Config, "a.toml", "pa", Some(a)),
            conflict(Class::Secret, "b.json", "pb", Some(b)),
        ];
        let eng = v.rig.engine("pc");
        eng.save_conflict_losers(&conflicts, &v.dir, &v.roots)
            .await
            .unwrap();
        let a_copy = v
            .dir
            .join(format!("config/a.toml.conflito-pa-{}", short_hex(b"A")));
        let b_copy = v
            .dir
            .join(format!("secret/b.json.conflito-pb-{}", short_hex(b"B")));
        fs::write(&a_copy, b"editada pela pessoa").unwrap();
        fs::remove_file(&b_copy).unwrap();

        let again = eng
            .save_conflict_losers(&conflicts, &v.dir, &v.roots)
            .await
            .unwrap();
        assert_eq!(
            again,
            SavedLosers {
                saved: 1,
                already: 1
            }
        );
        assert_eq!(fs::read(&a_copy).unwrap(), b"editada pela pessoa");
        assert_eq!(fs::read(&b_copy).unwrap(), b"B");
    }

    /// Algo que não é arquivo no nome do destino (uma pasta) para o lote **antes** de gravar
    /// o primeiro.
    #[tokio::test]
    async fn a_directory_at_a_destination_name_stops_the_batch_before_writing() {
        let v = vault3();
        let a = stored_loser(&v.rig, Class::Config, "a.toml", b"A").await;
        let b = stored_loser(&v.rig, Class::Secret, "b.json", b"B").await;
        fs::create_dir_all(
            v.dir
                .join(format!("secret/b.json.conflito-pb-{}", short_hex(b"B"))),
        )
        .unwrap();
        let conflicts = [
            conflict(Class::Config, "a.toml", "pa", Some(a)),
            conflict(Class::Secret, "b.json", "pb", Some(b)),
        ];
        let result = v
            .rig
            .engine("pc")
            .save_conflict_losers(&conflicts, &v.dir, &v.roots)
            .await;
        assert_eq!(result, Err(EngineError::Io(io::ErrorKind::AlreadyExists)));
        assert!(
            !v.dir.join("config").exists(),
            "o primeiro perdedor não pode ter sido gravado"
        );
    }

    /// Nomes e dispositivos de um `Conflict` montado à mão passam pelas mesmas validações.
    #[tokio::test]
    async fn hand_made_conflicts_with_unsafe_names_are_refused() {
        let v = vault3();
        let loser = stored_loser(&v.rig, Class::Config, "a.toml", b"A").await;
        let eng = v.rig.engine("pc");
        let bad_device = conflict(Class::Config, "a.toml", "pa/../x", Some(loser.clone()));
        assert!(matches!(
            eng.save_conflict_losers(&[bad_device], &v.dir, &v.roots)
                .await,
            Err(EngineError::Object(_))
        ));
        let bad_path = conflict(Class::Config, "../fora.toml", "pa", Some(loser));
        assert!(matches!(
            eng.save_conflict_losers(&[bad_path], &v.dir, &v.roots)
                .await,
            Err(EngineError::Object(_))
        ));
        assert!(!v.dir.exists());
    }

    /// Verifica tudo antes de gravar: um blob ausente ou adulterado não deixa nada para trás.
    #[tokio::test]
    async fn a_missing_or_tampered_loser_stops_everything_before_writing() {
        for tamper in [false, true] {
            let v = vault3();
            let good = stored_loser(&v.rig, Class::Config, "a.toml", b"bom").await;
            let mut bad = stored_loser(&v.rig, Class::Secret, "b.json", b"ruim").await;
            if tamper {
                bad.sha256 = sha(b"outro texto"); // o manifest mentiria sobre o conteúdo
            } else {
                bad.blob = Hash32([9; 32]); // blob que não existe
            }
            let conflicts = [
                conflict(Class::Config, "a.toml", "pa", Some(good)),
                conflict(Class::Secret, "b.json", "pb", Some(bad)),
            ];
            let result = v
                .rig
                .engine("pc")
                .save_conflict_losers(&conflicts, &v.dir, &v.roots)
                .await;
            let expected = if tamper {
                EngineError::Integrity
            } else {
                EngineError::MissingObject
            };
            assert_eq!(result, Err(expected));
            assert!(!v.dir.exists(), "nada gravado, nem o que estava bom");
        }
    }

    // ---------- perdedores: a pasta de conflitos nunca pode cair nas classes ----------

    async fn save_into(v: &Vault3, dir: &Path) -> Result<SavedLosers, EngineError> {
        let loser = stored_loser(&v.rig, Class::Secret, "x.json", b"segredo").await;
        v.rig
            .engine("pc")
            .save_conflict_losers(
                &[conflict(Class::Secret, "x.json", "pa", Some(loser))],
                dir,
                &v.roots,
            )
            .await
    }

    #[tokio::test]
    async fn a_conflicts_folder_inside_or_equal_to_a_class_folder_is_refused() {
        let v = vault3();
        let sec = v.roots.secret.clone().unwrap();
        for dir in [
            sec.clone(),
            sec.join("conflitos"),
            sec.join("a/b/c"),
            v.roots.config.clone().unwrap().join("x"),
        ] {
            assert_eq!(
                save_into(&v, &dir).await,
                Err(EngineError::UnsafeConflictsDir),
                "{dir:?}"
            );
        }
        assert_eq!(fs::read_dir(&sec).unwrap().count(), 0);
    }

    #[tokio::test]
    async fn dot_dot_in_the_conflicts_folder_is_refused() {
        let v = vault3();
        let sneaky = v.rig.tmp.path().join("conflitos/../sec-vivo/dentro");
        assert_eq!(
            save_into(&v, &sneaky).await,
            Err(EngineError::UnsafeConflictsDir)
        );
        assert_eq!(
            fs::read_dir(v.roots.secret.as_ref().unwrap())
                .unwrap()
                .count(),
            0
        );
    }

    /// `link` parece estar fora do Vault, mas aponta para dentro da pasta de logins.
    #[cfg(unix)]
    #[tokio::test]
    async fn a_symlink_that_leads_into_a_class_folder_is_refused() {
        let v = vault3();
        let link = v.rig.tmp.path().join("link");
        std::os::unix::fs::symlink(v.roots.secret.as_ref().unwrap(), &link).unwrap();
        assert_eq!(
            save_into(&v, &link.join("conflitos")).await,
            Err(EngineError::UnsafeConflictsDir)
        );
        assert_eq!(
            fs::read_dir(v.roots.secret.as_ref().unwrap())
                .unwrap()
                .count(),
            0
        );
    }

    /// `dir/secret` já existe como link para outro lugar: não se segue.
    #[cfg(unix)]
    #[tokio::test]
    async fn a_symlinked_class_subfolder_in_the_conflicts_folder_is_refused() {
        let v = vault3();
        let elsewhere = v.rig.dir("outro-lugar");
        fs::create_dir_all(&v.dir).unwrap();
        std::os::unix::fs::symlink(&elsewhere, v.dir.join("secret")).unwrap();
        assert_eq!(
            save_into(&v, &v.dir).await,
            Err(EngineError::UnsafeConflictsDir)
        );
        assert_eq!(fs::read_dir(&elsewhere).unwrap().count(), 0);
    }

    /// O destino real é `dir/<classe>/...`: uma pasta de classe VIVA dentro de `dir/config` ou
    /// `dir/secret` (ou igual a elas) receberia o segredo, mesmo com `dir` fora da raiz
    /// (achado 1 da auditoria). Layout comum: `~/arca/{vault,config,secret}` com `dir = ~/arca`.
    #[tokio::test]
    async fn a_live_class_folder_inside_the_conflicts_class_folders_is_refused() {
        let rig = Rig::new();
        let dir = rig.dir("arca");
        let live_secret = rig.dir("arca/secret");
        for root in [
            live_secret.clone(),
            live_secret.join("oauth"),
            dir.join("config"),
        ] {
            let roots = Roots {
                vault: None,
                config: None,
                secret: Some(root.clone()),
            };
            let loser = stored_loser(&rig, Class::Secret, "oauth/x.json", b"segredo").await;
            let result = rig
                .engine("pc")
                .save_conflict_losers(
                    &[conflict(Class::Secret, "oauth/x.json", "pa", Some(loser))],
                    &dir,
                    &roots,
                )
                .await;
            assert_eq!(result, Err(EngineError::UnsafeConflictsDir), "{root:?}");
        }
        assert_eq!(fs::read_dir(&live_secret).unwrap().count(), 0);
    }

    /// Uma pasta viva em outro lugar de `dir` (nem `config` nem `secret`) não é afetada.
    #[tokio::test]
    async fn a_live_folder_elsewhere_inside_the_conflicts_parent_is_fine() {
        let rig = Rig::new();
        let dir = rig.dir("arca");
        let roots = Roots {
            vault: None,
            config: None,
            secret: Some(rig.dir("arca/logins-vivos")),
        };
        let loser = stored_loser(&rig, Class::Secret, "x.json", b"segredo").await;
        let saved = rig
            .engine("pc")
            .save_conflict_losers(
                &[conflict(Class::Secret, "x.json", "pa", Some(loser))],
                &dir,
                &roots,
            )
            .await
            .unwrap();
        assert_eq!(saved.saved, 1);
        assert_eq!(
            fs::read_dir(rig.tmp.path().join("arca/logins-vivos"))
                .unwrap()
                .count(),
            0
        );
    }

    /// A raiz viva é um link para `dir/secret`.
    #[cfg(unix)]
    #[tokio::test]
    async fn a_live_class_folder_that_links_to_the_conflicts_folder_is_refused() {
        let rig = Rig::new();
        let dir = rig.dir("arca");
        let real = rig.dir("arca/secret");
        let live = rig.tmp.path().join("logins-vivos");
        std::os::unix::fs::symlink(&real, &live).unwrap();
        let roots = Roots {
            vault: None,
            config: None,
            secret: Some(live),
        };
        let loser = stored_loser(&rig, Class::Secret, "x.json", b"segredo").await;
        let result = rig
            .engine("pc")
            .save_conflict_losers(
                &[conflict(Class::Secret, "x.json", "pa", Some(loser))],
                &dir,
                &roots,
            )
            .await;
        assert_eq!(result, Err(EngineError::UnsafeConflictsDir));
        assert_eq!(fs::read_dir(&real).unwrap().count(), 0);
    }

    /// `dir/config` como link, e subpasta do `path` como link (só `dir/secret` era testado).
    #[cfg(unix)]
    #[tokio::test]
    async fn symlinks_in_the_config_folder_and_in_the_path_subfolders_are_refused() {
        let v = vault3();
        let elsewhere = v.rig.dir("outro-lugar");
        fs::create_dir_all(&v.dir).unwrap();
        std::os::unix::fs::symlink(&elsewhere, v.dir.join("config")).unwrap();
        let loser = stored_loser(&v.rig, Class::Config, "a.toml", b"A").await;
        let result = v
            .rig
            .engine("pc")
            .save_conflict_losers(
                &[conflict(Class::Config, "a.toml", "pa", Some(loser))],
                &v.dir,
                &v.roots,
            )
            .await;
        assert_eq!(result, Err(EngineError::UnsafeConflictsDir));

        // subpasta do `path` (dir/secret/oauth) é um link
        fs::remove_file(v.dir.join("config")).unwrap();
        fs::create_dir_all(v.dir.join("secret")).unwrap();
        std::os::unix::fs::symlink(&elsewhere, v.dir.join("secret/oauth")).unwrap();
        let loser = stored_loser(&v.rig, Class::Secret, "oauth/x.json", b"S").await;
        let result = v
            .rig
            .engine("pc")
            .save_conflict_losers(
                &[conflict(Class::Secret, "oauth/x.json", "pa", Some(loser))],
                &v.dir,
                &v.roots,
            )
            .await;
        assert_eq!(result, Err(EngineError::UnsafeConflictsDir));
        assert_eq!(fs::read_dir(&elsewhere).unwrap().count(), 0);
    }

    /// Pasta de conflitos que outros usuários podem alterar permitiria trocar ou renomear as
    /// cópias (achado 5). Leitura é aceita (os nomes aparecem); escrita por grupo/outros não.
    #[cfg(unix)]
    #[tokio::test]
    async fn a_conflicts_folder_that_others_can_write_to_is_refused() {
        use std::os::unix::fs::PermissionsExt;
        for (relative, mode) in [("", 0o777), ("", 0o775), ("secret", 0o770)] {
            let v = vault3();
            fs::create_dir_all(v.dir.join("secret")).unwrap();
            fs::set_permissions(v.dir.join(relative), fs::Permissions::from_mode(mode)).unwrap();
            let loser = stored_loser(&v.rig, Class::Secret, "x.json", b"segredo").await;
            let result = v
                .rig
                .engine("pc")
                .save_conflict_losers(
                    &[conflict(Class::Secret, "x.json", "pa", Some(loser))],
                    &v.dir,
                    &v.roots,
                )
                .await;
            assert_eq!(
                result,
                Err(EngineError::UnsafeConflictsDir),
                "{relative:?} {mode:o}"
            );
            assert_eq!(fs::read_dir(v.dir.join("secret")).unwrap().count(), 0);
        }
    }

    /// Pastas só legíveis por outros (0755) são aceitas: nada pode ser trocado.
    #[cfg(unix)]
    #[tokio::test]
    async fn a_conflicts_folder_readable_by_others_but_not_writable_is_accepted() {
        use std::os::unix::fs::PermissionsExt;
        let v = vault3();
        fs::create_dir_all(&v.dir).unwrap();
        fs::set_permissions(&v.dir, fs::Permissions::from_mode(0o755)).unwrap();
        let loser = stored_loser(&v.rig, Class::Secret, "x.json", b"segredo").await;
        let saved = v
            .rig
            .engine("pc")
            .save_conflict_losers(
                &[conflict(Class::Secret, "x.json", "pa", Some(loser))],
                &v.dir,
                &v.roots,
            )
            .await
            .unwrap();
        assert_eq!(saved.saved, 1);
    }

    /// Ponta a ponta: dois dispositivos editam o mesmo `config`; a fusão real devolve o
    /// perdedor, e é esse texto que acaba gravado.
    #[tokio::test]
    async fn a_real_merge_conflict_in_config_ends_up_saved() {
        let d = diverge_in(
            Class::Config,
            &[("settings.toml", b"1")],
            vec![
                ("pa", 10, vec![("settings.toml", b"perdedor A")]),
                ("pb", 20, vec![("settings.toml", b"vencedor B")]),
            ],
        )
        .await;
        let eng = d.rig.engine("pc");
        let MergeOutcome::Merged { id, conflicts, .. } = eng.merge(30).await.unwrap() else {
            panic!("esperava Merged");
        };
        let live = d.rig.dir("cfg-vivo");
        let roots = roots_of(Class::Config, &live);
        let dir = d.rig.tmp.path().join("conflitos");
        let saved = eng
            .save_conflict_losers(&conflicts, &dir, &roots)
            .await
            .unwrap();
        assert_eq!(
            saved,
            SavedLosers {
                saved: 1,
                already: 0
            }
        );
        let name = format!(
            "config/settings.toml.conflito-pa-{}",
            short_hex(b"perdedor A")
        );
        assert_eq!(fs::read(dir.join(name)).unwrap(), b"perdedor A");

        // o vencedor é o que a pasta viva recebe; o perdedor não aparece nela
        eng.restore(&id, &roots).await.unwrap();
        assert_eq!(tree(&live), contents(&[("settings.toml", b"vencedor B")]));
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
    /// temporários do próprio `apply`: recusa clara, antes de tocar no disco. Desde o P-42 quem
    /// recusa é o `validate_path` ao abrir o manifest (o `seal` já não o produz, por isso o
    /// `put_forged_raw`); a checagem do `apply` fica como defesa na fronteira do disco.
    #[tokio::test]
    async fn an_entry_named_like_an_arca_temporary_is_refused_before_touching_anything() {
        let h = hist().await;
        let forged = put_forged_raw(&h.rig, |m| {
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
        assert_eq!(
            result,
            Err(EngineError::Object(Error::InvalidPath("reserved name")))
        );
        assert_eq!(tree(&h.target), before);
    }

    /// P-42: o `restore` de um manifest autenticado com `.arca-apply-journal` gravava um diário
    /// falso na pasta do usuário (e o próximo `apply` ou `snapshot` dava `Interrupted` para
    /// sempre). A entrada aponta para um blob de verdade, então só o nome a denuncia. O nome
    /// reservado vale em qualquer componente: nome do arquivo e nome de pasta.
    #[tokio::test]
    async fn a_reserved_name_in_any_component_is_never_restored() {
        let h = hist().await;
        for reserved in [
            ".arca-apply-journal",
            ".arca-new-3/c.md",
            "sub/.tmp-0123456789abcdef",
        ] {
            let forged = put_forged_raw(&h.rig, |m| {
                m.files[0].path = reserved.to_string();
                m.files
                    .sort_by(|a, b| (a.class, &a.path).cmp(&(b.class, &b.path)));
            })
            .await;
            let target = h.rig.dir("restaurado");
            let result = h
                .rig
                .engine("pc")
                .restore(&forged, &vault_only(&target))
                .await;
            assert_eq!(
                result.err(),
                Some(EngineError::Object(Error::InvalidPath("reserved name"))),
                "{reserved}"
            );
            assert!(tree(&target).is_empty(), "{reserved}: {:?}", tree(&target));
        }
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
            root: dir.clone(),
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

    // ---------- limpeza das cópias de perdedores (P-28) ----------

    #[tokio::test]
    async fn saved_losers_can_be_listed_and_cleared_and_the_folder_ends_empty() {
        let v = vault3();
        let a = stored_loser(&v.rig, Class::Config, "settings.toml", b"perdedor A").await;
        let b = stored_loser(&v.rig, Class::Secret, "oauth/slack.json", b"{token-b}").await;
        let eng = v.rig.engine("pc");
        eng.save_conflict_losers(
            &[
                conflict(Class::Config, "settings.toml", "pa", Some(a)),
                conflict(Class::Secret, "oauth/slack.json", "pb", Some(b)),
            ],
            &v.dir,
            &v.roots,
        )
        .await
        .unwrap();

        let listed = eng.list_conflict_losers(&v.dir, &v.roots).await.unwrap();
        assert_eq!(listed.len(), 2);
        assert_eq!(
            (
                listed[0].class,
                listed[0].path.as_str(),
                listed[0].device.as_str()
            ),
            (Class::Config, "settings.toml", "pa")
        );
        assert_eq!(
            (
                listed[1].class,
                listed[1].path.as_str(),
                listed[1].device.as_str()
            ),
            (Class::Secret, "oauth/slack.json", "pb")
        );
        assert!(listed.iter().all(|c| !c.edited));
        assert_eq!(fs::read(&listed[1].file).unwrap(), b"{token-b}");

        let report = eng
            .clear_conflict_losers(&v.dir, &v.roots, &listed)
            .await
            .unwrap();
        assert_eq!(report.removed, 2);
        assert!(report.skipped.is_empty());
        assert_eq!(
            eng.list_conflict_losers(&v.dir, &v.roots).await.unwrap(),
            vec![]
        );
        // a pasta de conflitos e a da classe ficam; as subpastas esvaziadas somem
        assert!(v.dir.join("config").is_dir() && v.dir.join("secret").is_dir());
        assert!(!v.dir.join("secret/oauth").exists());
    }

    #[tokio::test]
    async fn list_and_clear_refuse_an_unsafe_conflicts_folder() {
        let v = vault3();
        let eng = v.rig.engine("pc");
        let inside_live = v.roots.config.clone().unwrap().join("conflitos");
        let fake = LoserCopy {
            class: Class::Config,
            path: "a".to_string(),
            device: "pa".to_string(),
            sha_prefix: [0; 8],
            file: inside_live.join("config/a.conflito-pa-0000000000000000"),
            edited: false,
        };
        for bad in [inside_live.clone(), v.dir.join("../conflitos")] {
            assert_eq!(
                eng.list_conflict_losers(&bad, &v.roots).await,
                Err(EngineError::UnsafeConflictsDir),
                "{bad:?}"
            );
            assert_eq!(
                eng.clear_conflict_losers(&bad, &v.roots, std::slice::from_ref(&fake))
                    .await
                    .map(|_| ()),
                Err(EngineError::UnsafeConflictsDir),
                "{bad:?}"
            );
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::create_dir_all(&v.dir).unwrap();
            fs::set_permissions(&v.dir, fs::Permissions::from_mode(0o777)).unwrap();
            assert_eq!(
                eng.list_conflict_losers(&v.dir, &v.roots).await,
                Err(EngineError::UnsafeConflictsDir)
            );
        }
    }

    // ---------- recuperação de um `apply` interrompido (PR 3c, P-27) ----------

    /// Backend que registra, a cada leitura de blob, se o diário já existia na raiz.
    #[derive(Clone)]
    struct JournalSpy {
        inner: Backend,
        root: PathBuf,
        seen: std::sync::Arc<std::sync::Mutex<Vec<bool>>>,
    }

    impl BlobStore for JournalSpy {
        async fn put(&self, id: &ObjectId, bytes: &[u8]) -> Result<(), StoreError> {
            self.inner.put(id, bytes).await
        }
        async fn get(&self, id: &ObjectId) -> Result<Option<Vec<u8>>, StoreError> {
            if id.kind == ObjectKind::Blob {
                let exists = crate::journal::Journal::exists(&self.root);
                self.seen.lock().unwrap().push(exists);
            }
            self.inner.get(id).await
        }
        async fn has(&self, id: &ObjectId) -> Result<bool, StoreError> {
            self.inner.has(id).await
        }
        async fn list(&self, kind: ObjectKind) -> Result<Vec<ObjectId>, StoreError> {
            self.inner.list(kind).await
        }
        async fn delete(&self, id: &ObjectId) -> Result<(), StoreError> {
            self.inner.delete(id).await
        }
        fn capabilities(&self) -> Capabilities {
            self.inner.capabilities()
        }
    }

    #[tokio::test]
    async fn apply_writes_the_journal_before_it_stages_and_removes_it_at_the_end() {
        let h = hist().await;
        let seen = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let spy = JournalSpy {
            inner: h.rig.store.clone(),
            root: h.target.clone(),
            seen: seen.clone(),
        };
        let eng = Engine::new(spy, keys(1), "pc").unwrap();
        eng.apply(Some(&h.v1), &h.v2, &vault_only(&h.target))
            .await
            .unwrap();
        let seen = seen.lock().unwrap().clone();
        assert!(
            !seen.is_empty() && seen.iter().all(|exists| *exists),
            "{seen:?}"
        );
        assert!(!crate::journal::Journal::exists(&h.target));
        no_leftovers(&tree(&h.target));
    }

    // ---------- trava entre processos (P-33 item 7) ----------

    /// Backend que, a cada leitura de blob, tenta travar a raiz como outro `Engine` faria e anota
    /// se estava ocupada.
    #[derive(Clone)]
    struct LockSpy {
        inner: Backend,
        root: PathBuf,
        busy: std::sync::Arc<std::sync::Mutex<Vec<bool>>>,
    }

    impl BlobStore for LockSpy {
        async fn put(&self, id: &ObjectId, bytes: &[u8]) -> Result<(), StoreError> {
            self.inner.put(id, bytes).await
        }
        async fn get(&self, id: &ObjectId) -> Result<Option<Vec<u8>>, StoreError> {
            if id.kind == ObjectKind::Blob {
                let busy =
                    lock_roots(std::slice::from_ref(&self.root)).err() == Some(EngineError::Busy);
                self.busy.lock().unwrap().push(busy);
            }
            self.inner.get(id).await
        }
        async fn has(&self, id: &ObjectId) -> Result<bool, StoreError> {
            self.inner.has(id).await
        }
        async fn list(&self, kind: ObjectKind) -> Result<Vec<ObjectId>, StoreError> {
            self.inner.list(kind).await
        }
        async fn delete(&self, id: &ObjectId) -> Result<(), StoreError> {
            self.inner.delete(id).await
        }
        fn capabilities(&self) -> Capabilities {
            self.inner.capabilities()
        }
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn recover_is_busy_while_another_operation_holds_the_folder_and_touches_nothing() {
        let h = hist().await;
        interrupt_apply(&h.target);
        let before = tree(&h.target);
        let eng = h.rig.engine("pc");
        let roots = vault_only(&h.target);

        let held = lock_roots(std::slice::from_ref(&h.target)).unwrap();
        assert_eq!(eng.recover(&roots).await, Err(EngineError::Busy));
        assert_eq!(tree(&h.target), before);
        assert!(crate::journal::Journal::exists(&h.target));

        // solta a trava: a mesma recuperação passa
        drop(held);
        let recovered = eng.recover(&roots).await.unwrap();
        assert!(recovered.rolled_back > 0);
        assert!(!crate::journal::Journal::exists(&h.target));
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn apply_is_busy_while_another_operation_holds_the_folder_and_creates_no_journal() {
        let h = hist().await;
        let before = tree(&h.target);
        let eng = h.rig.engine("pc");
        let roots = vault_only(&h.target);

        let held = lock_roots(std::slice::from_ref(&h.target)).unwrap();
        assert_eq!(
            eng.apply(Some(&h.v1), &h.v2, &roots).await.map(|_| ()),
            Err(EngineError::Busy)
        );
        assert_eq!(tree(&h.target), before);
        assert!(!crate::journal::Journal::exists(&h.target));

        drop(held);
        eng.apply(Some(&h.v1), &h.v2, &roots).await.unwrap();
        assert_eq!(tree(&h.target), h.v2_tree);
    }

    /// O `apply` segura a trava durante o `stage` (quando outro processo, vendo o diário `open`,
    /// o confundiria com uma queda) e a solta ao terminar, com sucesso ou com erro.
    #[cfg(unix)]
    #[tokio::test]
    async fn apply_holds_the_lock_while_it_works_and_releases_it_at_the_end() {
        let h = hist().await;
        let busy = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let spy = LockSpy {
            inner: h.rig.store.clone(),
            root: h.target.clone(),
            busy: busy.clone(),
        };
        let eng = Engine::new(spy, keys(1), "pc").unwrap();
        let roots = vault_only(&h.target);

        eng.apply(Some(&h.v1), &h.v2, &roots).await.unwrap();
        let seen = busy.lock().unwrap().clone();
        assert!(!seen.is_empty() && seen.iter().all(|b| *b), "{seen:?}");
        // terminou: a trava está livre
        drop(lock_roots(std::slice::from_ref(&h.target)).unwrap());

        // um `apply` que falha (edição local no meio do caminho) também solta
        let h = hist().await;
        fs::write(h.target.join("a.md"), b"da pessoa").unwrap();
        let eng = h.rig.engine("pc");
        assert_eq!(
            eng.apply(Some(&h.v1), &h.v2, &vault_only(&h.target))
                .await
                .map(|_| ()),
            Err(EngineError::LocalChanges)
        );
        drop(lock_roots(std::slice::from_ref(&h.target)).unwrap());
    }

    /// A raiz que o próprio `apply` cria também fica travada enquanto ele trabalha.
    #[cfg(unix)]
    #[tokio::test]
    async fn the_root_an_apply_creates_is_locked_while_it_works() {
        let h = hist().await;
        let fresh = h.rig.tmp.path().join("nova");
        let busy = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let spy = LockSpy {
            inner: h.rig.store.clone(),
            root: fresh.clone(),
            busy: busy.clone(),
        };
        let eng = Engine::new(spy, keys(1), "pc").unwrap();
        eng.apply(None, &h.v2, &vault_only(&fresh)).await.unwrap();
        let seen = busy.lock().unwrap().clone();
        assert!(!seen.is_empty() && seen.iter().all(|b| *b), "{seen:?}");
        drop(lock_roots(std::slice::from_ref(&fresh)).unwrap());
    }

    /// Com duas raízes e uma ocupada: `Busy`, e a outra não fica presa pela tentativa.
    #[cfg(unix)]
    #[tokio::test]
    async fn with_two_roots_one_busy_recover_is_busy_and_leaves_the_other_free() {
        let h = hist().await;
        let config = h.rig.tmp.path().join("config");
        copy_dir(&h.target, &config);
        let roots = Roots {
            vault: Some(h.target.clone()),
            config: Some(config.clone()),
            ..Roots::default()
        };
        let held = lock_roots(std::slice::from_ref(&config)).unwrap();
        assert_eq!(
            h.rig.engine("pc").recover(&roots).await,
            Err(EngineError::Busy)
        );
        // o `vault` (pego primeiro, em ordem de caminho) foi solto ao desistir
        drop(lock_roots(std::slice::from_ref(&h.target)).unwrap());
        drop(held);
    }

    /// P-33 item 2: os diários de todas as raízes de um `apply` levam o mesmo id e o número de
    /// raízes, tanto ao abrir quanto ao marcar `committed` (que reescreve o diário).
    #[tokio::test]
    async fn the_journals_of_all_roots_of_an_apply_share_one_id_and_the_root_count() {
        let h = hist().await;
        let config = h.rig.tmp.path().join("config");
        copy_dir(&h.target, &config);
        let roots = vec![h.target.clone(), config.clone()];
        let items: Vec<Item> = roots
            .iter()
            .map(|root| {
                let entry = |data: &[u8]| FileEntry {
                    path: "a.md".to_string(),
                    class: Class::Vault,
                    blob: Hash32([0u8; 32]),
                    size: data.len() as u64,
                    sha256: sha(data),
                };
                Item {
                    kind: Kind::Update,
                    root: root.clone(),
                    dest: root.join("a.md"),
                    from: Some(entry(b"1")),
                    to: Some(entry(b"2")),
                }
            })
            .collect();
        let (planned, _) = plan(items).unwrap();
        let group = ApplyGroup::new(roots.len()).unwrap();
        open_journals(&roots, &planned, &group, &mut Vec::new()).unwrap();
        let ids = |phase| {
            let journals: Vec<_> = roots
                .iter()
                .map(|r| Journal::load(r).unwrap().unwrap())
                .collect();
            assert!(journals.iter().all(|j| j.phase == phase));
            assert!(journals.iter().all(|j| j.peers == Some(2)));
            assert_eq!(journals[0].apply_id, journals[1].apply_id);
            journals[0].apply_id.clone().unwrap()
        };
        let opened = ids(Phase::Open);
        mark_committed(&roots, &planned, &group).unwrap();
        assert_eq!(ids(Phase::Committed), opened);
        assert_eq!(opened.len(), 32);
        // um `apply` novo tem outro id
        assert_ne!(ApplyGroup::new(2).unwrap(), group);
    }

    /// Achado 9: raiz aninhada que o próprio `apply` criou (`p1/p2/raiz`) sai inteira se falhar.
    #[tokio::test]
    async fn a_failed_apply_removes_every_folder_it_created_for_a_nested_root() {
        let h = hist().await;
        let eng = h.rig.engine("pc");
        let heads = eng.heads().await.unwrap();
        let blob = heads.tips[0]
            .manifest
            .files
            .iter()
            .find(|f| f.path == "new.md")
            .unwrap()
            .blob;
        let id = ObjectId {
            kind: ObjectKind::Blob,
            hash: blob.0,
        };
        fs::remove_file(h.rig.store_dir().join(id.to_string())).unwrap();
        let top = h.rig.tmp.path().join("p1");
        let root = top.join("p2").join("raiz");
        assert!(eng.apply(None, &h.v2, &vault_only(&root)).await.is_err());
        assert!(!top.exists(), "sobrou {:?}", tree_dirs(h.rig.tmp.path()));
    }

    fn tree_dirs(root: &Path) -> Vec<String> {
        fs::read_dir(root)
            .unwrap()
            .map(|e| e.unwrap().file_name().into_string().unwrap())
            .collect()
    }

    #[tokio::test]
    async fn a_failed_apply_removes_its_journal() {
        let h = hist().await;
        let eng = h.rig.engine("pc");
        let heads = eng.heads().await.unwrap();
        let blob = heads.tips[0]
            .manifest
            .files
            .iter()
            .find(|f| f.path == "new.md")
            .unwrap()
            .blob;
        let id = ObjectId {
            kind: ObjectKind::Blob,
            hash: blob.0,
        };
        fs::remove_file(h.rig.store_dir().join(id.to_string())).unwrap();
        assert!(eng
            .apply(Some(&h.v1), &h.v2, &vault_only(&h.target))
            .await
            .is_err());
        assert!(!crate::journal::Journal::exists(&h.target));
    }

    /// Deixa `target` como uma queda deixaria no meio da troca de `a.md` (atualizado) e `b.md`
    /// (removido): diário `open`, `.arca-new-*`, `a.md` já trocado, `b.md` de lado.
    fn interrupt_apply(target: &Path) {
        let item = |kind, rel: &str, from: &[u8], to: Option<&[u8]>| {
            let entry = |data: &[u8]| FileEntry {
                path: rel.to_string(),
                class: Class::Vault,
                blob: Hash32([0u8; 32]),
                size: data.len() as u64,
                sha256: sha(data),
            };
            Item {
                kind,
                root: target.to_path_buf(),
                dest: target.join(rel),
                from: Some(entry(from)),
                to: to.map(entry),
            }
        };
        let (planned, _) = plan(vec![
            item(Kind::Update, "a.md", b"1", Some(b"2")),
            item(Kind::Remove, "b.md", b"1", None),
        ])
        .unwrap();
        crate::journal::Journal::open(target, &planned)
            .save(target)
            .unwrap();
        let mut log = Vec::new();
        stage(&mut log, &planned[0], b"2").unwrap();
        commit(&mut log, &planned).unwrap();
        // o processo morreu aqui: nada de `finish`, nada de desfazer
    }

    #[tokio::test]
    async fn apply_and_snapshot_refuse_while_a_journal_is_there_and_write_nothing() {
        let h = hist().await;
        interrupt_apply(&h.target);
        let eng = h.rig.engine("pc");
        let roots = vault_only(&h.target);
        let before = tree(&h.target);
        assert_eq!(
            eng.apply(Some(&h.v1), &h.v2, &roots).await,
            Err(EngineError::Interrupted)
        );
        assert_eq!(
            eng.snapshot(&roots, 9).await.map(|_| ()),
            Err(EngineError::Interrupted)
        );
        assert_eq!(tree(&h.target), before);
    }

    #[tokio::test]
    async fn recover_puts_the_folder_back_and_the_next_apply_works() {
        let h = hist().await;
        let origin = tree(&h.target);
        interrupt_apply(&h.target);
        assert_ne!(tree(&h.target), origin);
        let eng = h.rig.engine("pc");
        let roots = vault_only(&h.target);

        let recovered = eng.recover(&roots).await.unwrap();
        assert_eq!(recovered.kept, Vec::<PathBuf>::new());
        assert_eq!(recovered.completed, 0);
        assert_eq!(recovered.rolled_back, 2);
        assert_eq!(tree(&h.target), origin);

        // idempotente
        assert_eq!(eng.recover(&roots).await, Ok(Recovered::default()));
        assert_eq!(tree(&h.target), origin);
        // e a pasta voltou a ser aplicável
        eng.apply(Some(&h.v1), &h.v2, &roots).await.unwrap();
        assert_eq!(tree(&h.target), h.v2_tree);
    }

    #[tokio::test]
    async fn recover_without_a_journal_does_nothing_and_ignores_disabled_classes() {
        let h = hist().await;
        let before = tree(&h.target);
        let eng = h.rig.engine("pc");
        assert_eq!(
            eng.recover(&vault_only(&h.target)).await,
            Ok(Recovered::default())
        );
        assert_eq!(
            eng.recover(&Roots::default()).await,
            Ok(Recovered::default())
        );
        assert_eq!(tree(&h.target), before);
    }

    #[tokio::test]
    async fn recover_reports_what_it_would_not_touch() {
        let h = hist().await;
        interrupt_apply(&h.target);
        fs::write(h.target.join("a.md"), b"da pessoa").unwrap();
        let recovered = h
            .rig
            .engine("pc")
            .recover(&vault_only(&h.target))
            .await
            .unwrap();
        assert_eq!(
            recovered.kept,
            vec![h
                .target
                .join(format!(".arca-kept-{}", &sha(b"1").to_string()[..16]))]
        );
        assert_eq!(fs::read(h.target.join("a.md")).unwrap(), b"da pessoa");
    }

    /// P-33 item 5, ponta a ponta: o que a recuperação deixou em `.arca-kept-*` aparece em
    /// `list_kept` (mesmo depois de reiniciar) e `clear_kept` o apaga; a segunda vez não há nada.
    #[tokio::test]
    async fn what_recover_kept_can_be_listed_and_cleared() {
        let h = hist().await;
        interrupt_apply(&h.target);
        fs::write(h.target.join("a.md"), b"da pessoa").unwrap();
        let eng = h.rig.engine("pc");
        let roots = vault_only(&h.target);
        let recovered = eng.recover(&roots).await.unwrap();
        assert_eq!(recovered.kept.len(), 1);

        let listed = eng.list_kept(&roots).await.unwrap();
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].file, recovered.kept[0]);
        assert_eq!(listed[0].class, Class::Vault);
        assert!(!listed[0].edited);

        let report = eng.clear_kept(&roots, &listed).await.unwrap();
        assert_eq!(report.removed, 1);
        assert!(!recovered.kept[0].exists());
        // o arquivo da pessoa não foi tocado
        assert_eq!(fs::read(h.target.join("a.md")).unwrap(), b"da pessoa");
        assert_eq!(eng.list_kept(&roots).await.unwrap(), vec![]);
        // idempotente: apagar de novo o que já foi é `Gone`, sem erro
        let again = eng.clear_kept(&roots, &listed).await.unwrap();
        assert_eq!(again.removed, 0);
    }

    /// P-33 item 2: um diário corrompido em qualquer raiz recusa a recuperação **antes** de tocar
    /// em qualquer raiz. Hoje a primeira raiz (vault) já foi recuperada quando a segunda falha, e
    /// o conjunto fica meio voltado.
    #[tokio::test]
    async fn a_corrupt_journal_in_a_later_root_stops_the_recovery_before_touching_any_root() {
        let h = hist().await;
        let config = h.rig.tmp.path().join("config");
        copy_dir(&h.target, &config);
        interrupt_apply(&h.target);
        interrupt_apply(&config);
        fs::write(crate::journal::Journal::path(&config), b"lixo").unwrap();
        let (before_vault, before_config) = (tree(&h.target), tree(&config));
        let roots = Roots {
            vault: Some(h.target.clone()),
            config: Some(config.clone()),
            ..Roots::default()
        };
        assert_eq!(
            h.rig.engine("pc").recover(&roots).await,
            Err(EngineError::CorruptJournal)
        );
        assert_eq!(tree(&h.target), before_vault);
        assert_eq!(tree(&config), before_config);
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

    // ---------- snapshot: leitura sem seguir link, com teto antes de ler (P-41) ----------
    //
    // O candidato é montado à mão, como o `scan` o viu; o disco já está diferente (a troca
    // acontece entre a varredura e a leitura).

    fn seen_by_scan(root: &Path, rel: &str) -> Candidate {
        Candidate {
            class: Class::Vault,
            path: rel.to_string(),
            abs: root.join(rel),
            len: 1,
        }
    }

    #[tokio::test]
    async fn read_file_returns_a_regular_file_whole() {
        let rig = Rig::new();
        let vault = rig.dir("vault");
        write(&vault, "sub/a.md", b"conteudo");
        let mut skipped = Vec::new();
        let data = read_file(&seen_by_scan(&vault, "sub/a.md"), &vault, &mut skipped)
            .await
            .unwrap();
        assert_eq!(
            data.as_deref().map(|d| d.as_slice()),
            Some(&b"conteudo"[..])
        );
        assert!(skipped.is_empty());
    }

    #[tokio::test]
    async fn read_file_skips_a_file_that_vanished_after_the_scan() {
        let rig = Rig::new();
        let vault = rig.dir("vault");
        let mut skipped = Vec::new();
        let data = read_file(&seen_by_scan(&vault, "a.md"), &vault, &mut skipped)
            .await
            .unwrap();
        assert!(data.is_none());
        assert_eq!(skipped.len(), 1);
        assert_eq!(skipped[0].reason, "file disappeared");
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn read_file_never_follows_a_link_put_in_place_of_the_file() {
        let rig = Rig::new();
        let vault = rig.dir("vault");
        let outside = rig.dir("fora");
        write(&outside, "id_ed25519", b"chave privada");
        std::os::unix::fs::symlink(outside.join("id_ed25519"), vault.join("a.md")).unwrap();
        let mut skipped = Vec::new();
        let data = read_file(&seen_by_scan(&vault, "a.md"), &vault, &mut skipped)
            .await
            .unwrap();
        assert!(data.is_none(), "leu o alvo do link: {data:?}");
        assert_eq!(skipped.len(), 1);
        assert_eq!(skipped[0].reason, NOT_REGULAR);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn read_file_never_follows_a_link_put_in_place_of_a_parent_folder() {
        let rig = Rig::new();
        let vault = rig.dir("vault");
        let outside = rig.dir("fora");
        write(&outside, "a.md", b"segredo de fora da raiz");
        std::os::unix::fs::symlink(&outside, vault.join("sub")).unwrap();
        let mut skipped = Vec::new();
        let data = read_file(&seen_by_scan(&vault, "sub/a.md"), &vault, &mut skipped)
            .await
            .unwrap();
        assert!(data.is_none(), "leu fora da raiz: {data:?}");
        assert_eq!(skipped.len(), 1);
        assert_eq!(skipped[0].reason, NOT_REGULAR);
    }

    /// Um FIFO no lugar do arquivo não pode travar a leitura nem entregar o que se escreve nele.
    /// A thread que escreve fica solta de propósito: com o `read_file` certo ninguém lê o FIFO.
    #[cfg(unix)]
    #[tokio::test]
    async fn read_file_skips_a_fifo_instead_of_blocking_or_reading_it() {
        use rustix::fs::{mknodat, FileType, Mode, CWD};
        let rig = Rig::new();
        let vault = rig.dir("vault");
        let fifo = vault.join("a.md");
        mknodat(CWD, &fifo, FileType::Fifo, Mode::RUSR | Mode::WUSR, 0).unwrap();
        std::thread::spawn(move || {
            let _ = fs::write(fifo, b"x");
        });
        let mut skipped = Vec::new();
        let data = read_file(&seen_by_scan(&vault, "a.md"), &vault, &mut skipped)
            .await
            .unwrap();
        assert!(data.is_none(), "leu o FIFO: {data:?}");
        assert_eq!(skipped[0].reason, NOT_REGULAR);
    }

    /// O arquivo cresceu depois da varredura: o teto vale antes de ler, não depois. (Guarda de
    /// regressão: o `fs::read` antigo já devolvia "file too large", só que depois de ler tudo.)
    #[cfg(unix)]
    #[tokio::test]
    async fn read_file_skips_a_file_that_grew_past_the_cap_after_the_scan() {
        let rig = Rig::new();
        let vault = rig.dir("vault");
        fs::File::create(vault.join("a.md"))
            .unwrap()
            .set_len(MAX_FILE_BYTES + 1)
            .unwrap();
        let mut skipped = Vec::new();
        let data = read_file(&seen_by_scan(&vault, "a.md"), &vault, &mut skipped)
            .await
            .unwrap();
        assert!(data.is_none());
        assert_eq!(skipped.len(), 1);
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

    /// P-43: a restauração baixa blob por blob e pode levar minutos. Um arquivo que a pessoa
    /// criou na pasta depois da conferência de "vazia" não é dela para apagar quando falha.
    #[tokio::test]
    async fn a_failed_restore_never_deletes_a_file_the_user_created_in_the_target() {
        for existed in [false, true] {
            let rig = Rig::new();
            let vault = rig.dir("vault");
            write(&vault, "a/x.md", b"1");
            write(&vault, "b.md", b"2");
            let id = snapshot_tree(&rig, &vault).await;
            let tip = rig.engine("pc").heads().await.unwrap().tips.remove(0);
            let last = &tip.manifest.files[1];
            let lag = LaggedList::new(&rig.store);
            lag.fail_get(&ObjectId {
                kind: ObjectKind::Blob,
                hash: last.blob.0,
            });

            let target = if existed {
                rig.dir("ja-existia-vazia")
            } else {
                rig.tmp.path().join("nova")
            };
            // o blob do primeiro arquivo é lido depois de a pasta existir: a pessoa cria um arquivo
            lag.drop_file_on_first_blob_get(&target.join("dono.txt"), b"nao mexa");
            let eng = Engine::new(lag.clone(), keys(1), "pc").unwrap();

            assert!(eng.restore(&id, &vault_only(&target)).await.is_err());
            assert_eq!(
                tree(&target),
                BTreeMap::from([("dono.txt".to_string(), b"nao mexa".to_vec())]),
                "o arquivo da pessoa fica (pasta que já existia: {existed}) e o que a restauração \
                 gravou some"
            );
        }
    }

    /// P-43: enquanto uma restauração roda, a raiz está travada; outra operação (aqui, a trava de
    /// um `apply`) recebe `Busy`, e nada é escrito.
    #[tokio::test]
    async fn a_restore_into_a_locked_root_is_busy_and_writes_nothing() {
        let rig = Rig::new();
        let vault = rig.dir("vault");
        write(&vault, "a.md", b"1");
        let id = snapshot_tree(&rig, &vault).await;
        let target = rig.dir("vazia");

        let held = lock_roots(std::slice::from_ref(&target)).unwrap();
        assert_eq!(
            rig.engine("pc").restore(&id, &vault_only(&target)).await,
            Err(EngineError::Busy)
        );
        assert_eq!(fs::read_dir(&target).unwrap().count(), 0);
        drop(held);
        rig.engine("pc")
            .restore(&id, &vault_only(&target))
            .await
            .unwrap();
        assert_eq!(tree(&target), tree(&vault));
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
            EngineError::NoState,
            EngineError::OriginalKept,
            EngineError::UnsafeConflictsDir,
            EngineError::Rollback,
            EngineError::CorruptState,
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

    // ---------- filtro de entradas por classe (I-2, P-36) ----------

    /// O filtro do Warden para os logins, em miniatura: nome simples `*.json` e até 10 bytes.
    fn small_flat_json(entry: &FileEntry) -> bool {
        !entry.path.contains('/') && entry.path.ends_with(".json") && entry.size <= 10
    }

    /// Um manifest com a nota do Vault e, na classe `secret`, `ok.json` e duas entradas que o filtro recusa.
    async fn manifest_with_strays(rig: &Rig) -> ObjectId {
        let (vault, secret) = (rig.dir("src-vault"), rig.dir("src-secret"));
        write(&vault, "nota.md", b"N");
        write(&secret, "ok.json", b"{}");
        write(&secret, "x/y.json", b"{}");
        write(&secret, "y.bak", b"b");
        let roots = Roots {
            vault: Some(vault),
            secret: Some(secret),
            ..Roots::default()
        };
        created(rig.engine("origin").snapshot(&roots, 1).await.unwrap()).0
    }

    fn flat_json_name(rel: &str) -> bool {
        !rel.contains('/') && rel.ends_with(".json")
    }

    #[tokio::test]
    async fn snapshot_leaves_out_the_local_files_the_scan_filter_refuses() {
        let rig = Rig::new();
        let secret = rig.dir("src-secret");
        write(&secret, "ok.json", b"{}");
        write(&secret, "ok.json.bak", b"old-token");
        write(&secret, "x/y.json", b"{}");
        let engine = rig
            .engine("pc")
            .with_scan_filter(Class::Secret, flat_json_name);

        let (_, manifest, _, skipped) = created(
            engine
                .snapshot(&roots_of(Class::Secret, &secret), 1)
                .await
                .unwrap(),
        );

        assert_eq!(file_paths(&manifest), ["ok.json"]);
        let mut left_out: Vec<&str> = skipped.iter().map(|s| s.path.as_str()).collect();
        left_out.sort();
        assert_eq!(left_out, ["ok.json.bak", "x/y.json"]);
        assert_eq!(
            rig.count(ObjectKind::Blob).await,
            1,
            "só o ok.json vira blob: o conteúdo do .bak não chega ao armazenamento"
        );
    }

    #[tokio::test]
    async fn the_scan_filter_does_not_touch_other_classes() {
        let rig = Rig::new();
        let (vault, secret) = (rig.dir("src-vault"), rig.dir("src-secret"));
        write(&vault, "nota.txt", b"N");
        write(&secret, "ok.json", b"{}");
        let engine = rig
            .engine("pc")
            .with_scan_filter(Class::Secret, flat_json_name);
        let roots = Roots {
            vault: Some(vault),
            secret: Some(secret),
            ..Roots::default()
        };

        let (_, manifest, _, skipped) = created(engine.snapshot(&roots, 1).await.unwrap());

        assert_eq!(file_paths(&manifest), ["nota.txt", "ok.json"]);
        assert!(skipped.is_empty());
    }

    #[tokio::test]
    async fn apply_never_deletes_nor_overwrites_a_local_file_the_scan_filter_refuses() {
        let rig = Rig::new();
        let src = rig.dir("src-secret");
        write(&src, "ok.json", b"a");
        let origin = rig.engine("origin");
        let (v1, ..) = created(
            origin
                .snapshot(&roots_of(Class::Secret, &src), 1)
                .await
                .unwrap(),
        );
        write(&src, "ok.json", b"b");
        let (v2, ..) = created(
            origin
                .snapshot(&roots_of(Class::Secret, &src), 2)
                .await
                .unwrap(),
        );

        let dst = rig.dir("dst-secret");
        let roots = roots_of(Class::Secret, &dst);
        let pc = rig
            .engine("pc")
            .with_scan_filter(Class::Secret, flat_json_name);
        pc.apply(None, &v1, &roots).await.unwrap();
        write(&dst, "forgotten.json.bak", b"old-token");

        pc.apply(Some(&v1), &v2, &roots).await.unwrap();

        let files = tree(&dst);
        assert_eq!(files["ok.json"], b"b");
        assert_eq!(files["forgotten.json.bak"], b"old-token");
    }

    #[tokio::test]
    async fn apply_ignores_the_entries_the_filter_refuses_and_applies_the_rest() {
        let rig = Rig::new();
        let to = manifest_with_strays(&rig).await;
        let (vault, secret) = (rig.dir("dst-vault"), rig.dir("dst-secret"));
        let roots = Roots {
            vault: Some(vault.clone()),
            secret: Some(secret.clone()),
            ..Roots::default()
        };
        let engine = rig
            .engine("pc")
            .with_entry_filter(Class::Secret, small_flat_json);

        let applied = engine.apply(None, &to, &roots).await.unwrap();

        assert_eq!(applied.created, 2, "nota.md e ok.json");
        assert_eq!(
            tree(&secret).keys().collect::<Vec<_>>(),
            ["ok.json"],
            "nem x/y.json nem y.bak"
        );
        assert_eq!(
            tree(&vault).keys().collect::<Vec<_>>(),
            ["nota.md"],
            "o Vault não é barrado por uma entrada estranha de outra classe"
        );
    }

    #[tokio::test]
    async fn a_refused_new_version_neither_updates_nor_deletes_the_local_file() {
        let rig = Rig::new();
        let secret = rig.dir("src-secret");
        write(&secret, "login.json", b"ab");
        let eng = rig.engine("origin");
        let roots = roots_of(Class::Secret, &secret);
        let (v1, ..) = created(eng.snapshot(&roots, 1).await.unwrap());
        write(&secret, "login.json", b"0123456789ABCDEF"); // 16 bytes: o filtro recusa
        let (v2, ..) = created(eng.snapshot(&roots, 2).await.unwrap());

        let target = rig.dir("dst-secret");
        let target_roots = roots_of(Class::Secret, &target);
        rig.engine("pc")
            .apply(None, &v1, &target_roots)
            .await
            .unwrap();
        let before = tree(&target);

        let applied = rig
            .engine("pc")
            .with_entry_filter(Class::Secret, small_flat_json)
            .apply(Some(&v1), &v2, &target_roots)
            .await
            .unwrap();

        assert_eq!(applied.created + applied.updated + applied.removed, 0);
        assert_eq!(tree(&target), before, "o login local não mudou nem sumiu");
    }

    #[tokio::test]
    async fn the_next_snapshot_drops_the_refused_entries_from_the_lineage() {
        let rig = Rig::new();
        let to = manifest_with_strays(&rig).await;
        let (vault, secret) = (rig.dir("dst-vault"), rig.dir("dst-secret"));
        let roots = Roots {
            vault: Some(vault),
            secret: Some(secret),
            ..Roots::default()
        };
        let engine = rig
            .engine("pc")
            .with_entry_filter(Class::Secret, small_flat_json);
        engine.apply(None, &to, &roots).await.unwrap();

        let (_, manifest, ..) = created(engine.snapshot(&roots, 2).await.unwrap());

        assert_eq!(file_paths(&manifest), ["nota.md", "ok.json"]);
    }

    #[tokio::test]
    async fn restore_writes_only_the_entries_the_filter_accepts() {
        let rig = Rig::new();
        let to = manifest_with_strays(&rig).await;
        let (vault, secret) = (
            rig.tmp.path().join("r-vault"),
            rig.tmp.path().join("r-secret"),
        );
        let roots = Roots {
            vault: Some(vault.clone()),
            secret: Some(secret.clone()),
            ..Roots::default()
        };

        let restored = rig
            .engine("pc")
            .with_entry_filter(Class::Secret, small_flat_json)
            .restore(&to, &roots)
            .await
            .unwrap();

        assert_eq!(restored.files, 2);
        assert_eq!(tree(&secret).keys().collect::<Vec<_>>(), ["ok.json"]);
        assert_eq!(tree(&vault).keys().collect::<Vec<_>>(), ["nota.md"]);
    }

    #[tokio::test]
    async fn a_refused_entry_that_the_next_manifest_drops_is_not_removed_from_disk() {
        let rig = Rig::new();
        let secret = rig.dir("src-secret");
        write(&secret, "ok.json", b"{}");
        write(&secret, "y.bak", b"b");
        let eng = rig.engine("origin");
        let roots = roots_of(Class::Secret, &secret);
        let (v1, ..) = created(eng.snapshot(&roots, 1).await.unwrap());
        fs::remove_file(secret.join("y.bak")).unwrap();
        let (v2, ..) = created(eng.snapshot(&roots, 2).await.unwrap());

        // aqui `y.bak` é um arquivo da pessoa, que nunca foi aplicado pela arca
        let target = rig.dir("dst-secret");
        write(&target, "ok.json", b"{}");
        write(&target, "y.bak", b"da pessoa");

        let applied = rig
            .engine("pc")
            .with_entry_filter(Class::Secret, small_flat_json)
            .apply(Some(&v1), &v2, &roots_of(Class::Secret, &target))
            .await
            .unwrap();

        assert_eq!(applied.created + applied.updated + applied.removed, 0);
        assert_eq!(fs::read(target.join("y.bak")).unwrap(), b"da pessoa");
    }

    // ---- M-1: o perdedor é gravado antes do manifest de fusão ----

    /// Dois dispositivos editaram o mesmo `settings.toml` da classe `config`: `pa` (10) perde
    /// para `pb` (20). Devolve também as raízes ligadas e uma pasta de conflitos segura.
    async fn config_conflict() -> (Diverged, Roots, PathBuf) {
        let d = diverge_in(
            Class::Config,
            &[("settings.toml", b"base")],
            vec![
                ("pa", 10, vec![("settings.toml", b"from-a")]),
                ("pb", 20, vec![("settings.toml", b"from-b")]),
            ],
        )
        .await;
        let roots = roots_of(Class::Config, &d.rig.dir("cfg-vivo"));
        let safe = d.rig.tmp.path().join("conflitos");
        (d, roots, safe)
    }

    fn loser_copy(dir: &Path) -> PathBuf {
        dir.join(format!(
            "config/settings.toml.conflito-pa-{}",
            short_hex(b"from-a")
        ))
    }

    #[tokio::test]
    async fn merge_with_losers_saves_the_loser_and_the_merge_manifest() {
        let (d, roots, dir) = config_conflict().await;
        let eng = d.rig.engine("pc");
        let (outcome, saved) = eng.merge_with_losers(30, &dir, &roots).await.unwrap();
        assert!(matches!(
            outcome,
            MergeOutcome::Merged { tips_after: 1, .. }
        ));
        assert_eq!(
            saved,
            SavedLosers {
                saved: 1,
                already: 0
            }
        );
        assert_eq!(fs::read(loser_copy(&dir)).unwrap(), b"from-a");
        assert_eq!(eng.heads().await.unwrap().tips.len(), 1);
    }

    #[tokio::test]
    async fn an_unsafe_conflicts_dir_stops_the_merge_before_the_manifest_is_stored() {
        let (d, roots, _) = config_conflict().await;
        let inside_a_live_root = d.rig.tmp.path().join("cfg-vivo/conflitos");
        let eng = d.rig.engine("pc");
        let err = eng
            .merge_with_losers(30, &inside_a_live_root, &roots)
            .await
            .unwrap_err();
        assert!(matches!(err, EngineError::UnsafeConflictsDir));
        // nada foi gravado: as duas pontas seguem lá e a fusão pode ser refeita
        assert_eq!(eng.heads().await.unwrap().tips.len(), 2);
        assert!(!loser_copy(&inside_a_live_root).exists());
    }

    #[tokio::test]
    async fn after_a_refused_merge_the_next_one_still_sees_the_conflict() {
        let (d, roots, dir) = config_conflict().await;
        let eng = d.rig.engine("pc");
        let inside_a_live_root = d.rig.tmp.path().join("cfg-vivo/conflitos");
        eng.merge_with_losers(30, &inside_a_live_root, &roots)
            .await
            .unwrap_err();

        let (outcome, saved) = eng.merge_with_losers(31, &dir, &roots).await.unwrap();
        assert!(matches!(
            outcome,
            MergeOutcome::Merged { tips_after: 1, .. }
        ));
        assert_eq!(saved.saved, 1);
        assert_eq!(fs::read(loser_copy(&dir)).unwrap(), b"from-a");
        assert_eq!(eng.heads().await.unwrap().tips.len(), 1);
    }

    #[tokio::test]
    async fn a_loser_already_saved_before_a_crash_is_not_duplicated() {
        let (d, roots, dir) = config_conflict().await;
        let eng = d.rig.engine("pc");
        // a queda aconteceu depois do save e antes do manifest: a cópia já está lá
        let copy = loser_copy(&dir);
        fs::create_dir_all(copy.parent().unwrap()).unwrap();
        fs::write(&copy, b"from-a").unwrap();

        let (outcome, saved) = eng.merge_with_losers(30, &dir, &roots).await.unwrap();
        assert!(matches!(outcome, MergeOutcome::Merged { .. }));
        assert_eq!(
            saved,
            SavedLosers {
                saved: 0,
                already: 1
            }
        );
        assert_eq!(fs::read(&copy).unwrap(), b"from-a");
    }

    #[tokio::test]
    async fn a_vault_conflict_has_nothing_to_save_and_merges_as_before() {
        let d = diverge(
            &[("a.md", b"1")],
            vec![
                ("pa", 10, vec![("a.md", b"from-a")]),
                ("pb", 20, vec![("a.md", b"from-b")]),
            ],
        )
        .await;
        let roots = vault_only(&d.rig.dir("vault-vivo"));
        let dir = d.rig.tmp.path().join("conflitos");
        let (outcome, saved) = d
            .rig
            .engine("pc")
            .merge_with_losers(30, &dir, &roots)
            .await
            .unwrap();
        let MergeOutcome::Merged { conflicts, .. } = outcome else {
            panic!("esperava Merged");
        };
        assert_eq!(conflicts.len(), 1);
        assert_eq!(
            saved,
            SavedLosers {
                saved: 0,
                already: 0
            }
        );
        assert!(!dir.exists());
    }

    // ---- P-37 (2): classe sem raiz não tem cópia do perdedor ----

    /// Um dispositivo que não liga `secret` (sem aceite dos logins) não grava o segredo em
    /// texto claro, nem cria `dir`.
    #[tokio::test]
    async fn a_secret_loser_is_not_saved_without_a_secret_root() {
        let rig = Rig::new();
        let roots = roots_of(Class::Config, &rig.dir("cfg-vivo"));
        let dir = rig.tmp.path().join("conflitos");
        let loser = stored_loser(&rig, Class::Secret, "oauth/slack.json", b"{token-b}").await;
        let saved = rig
            .engine("pc")
            .save_conflict_losers(
                &[conflict(
                    Class::Secret,
                    "oauth/slack.json",
                    "pb",
                    Some(loser),
                )],
                &dir,
                &roots,
            )
            .await
            .unwrap();
        assert_eq!(
            saved,
            SavedLosers {
                saved: 0,
                already: 0
            }
        );
        assert!(!dir.exists());
    }

    /// No mesmo lote, o perdedor da classe ligada é gravado e o da classe sem raiz não.
    #[tokio::test]
    async fn only_the_loser_of_a_class_with_a_root_is_saved() {
        let rig = Rig::new();
        let roots = roots_of(Class::Config, &rig.dir("cfg-vivo"));
        let dir = rig.tmp.path().join("conflitos");
        let a = stored_loser(&rig, Class::Config, "settings.toml", b"perdedor A").await;
        let b = stored_loser(&rig, Class::Secret, "oauth/slack.json", b"{token-b}").await;
        let saved = rig
            .engine("pc")
            .save_conflict_losers(
                &[
                    conflict(Class::Config, "settings.toml", "pa", Some(a)),
                    conflict(Class::Secret, "oauth/slack.json", "pb", Some(b)),
                ],
                &dir,
                &roots,
            )
            .await
            .unwrap();
        assert_eq!(
            saved,
            SavedLosers {
                saved: 1,
                already: 0
            }
        );
        let name_a = format!(
            "config/settings.toml.conflito-pa-{}",
            short_hex(b"perdedor A")
        );
        assert_eq!(fs::read(dir.join(name_a)).unwrap(), b"perdedor A");
        assert!(!dir.join("secret").exists());
    }

    /// Quem funde um conflito `secret` sem raiz `secret` funde, mas não guarda o perdedor.
    #[tokio::test]
    async fn merging_a_secret_conflict_without_a_secret_root_saves_no_copy() {
        let d = diverge_in(
            Class::Secret,
            &[("oauth/slack.json", b"base")],
            vec![
                ("pa", 10, vec![("oauth/slack.json", b"from-a")]),
                ("pb", 20, vec![("oauth/slack.json", b"from-b")]),
            ],
        )
        .await;
        let roots = roots_of(Class::Config, &d.rig.dir("cfg-vivo"));
        let dir = d.rig.tmp.path().join("conflitos");
        let eng = d.rig.engine("pc");
        let (outcome, saved) = eng.merge_with_losers(30, &dir, &roots).await.unwrap();
        assert!(matches!(
            outcome,
            MergeOutcome::Merged { tips_after: 1, .. }
        ));
        assert_eq!(
            saved,
            SavedLosers {
                saved: 0,
                already: 0
            }
        );
        assert!(!dir.exists());
    }

    /// Sem nada a gravar, `dir` nem é conferido: o filtro de raiz vem antes de tudo (antes de
    /// decifrar o perdedor e de checar a pasta).
    #[tokio::test]
    async fn without_a_root_the_loser_is_dropped_before_the_conflicts_dir_is_checked() {
        let rig = Rig::new();
        let roots = roots_of(Class::Config, &rig.dir("cfg-vivo"));
        let inside_a_live_root = rig.tmp.path().join("cfg-vivo/conflitos");
        let loser = stored_loser(&rig, Class::Secret, "x.json", b"segredo").await;
        let saved = rig
            .engine("pc")
            .save_conflict_losers(
                &[conflict(Class::Secret, "x.json", "pa", Some(loser))],
                &inside_a_live_root,
                &roots,
            )
            .await
            .unwrap();
        assert_eq!(
            saved,
            SavedLosers {
                saved: 0,
                already: 0
            }
        );
        assert!(!inside_a_live_root.exists());
    }

    // ---- P-30 (1): o perdedor nunca sobrescreve o que apareceu no destino ----

    /// Quantos temporários `.tmp-*` sobraram em `dir`.
    fn leftover_temps(dir: &Path) -> usize {
        fs::read_dir(dir)
            .unwrap()
            .filter(|entry| {
                entry
                    .as_ref()
                    .unwrap()
                    .file_name()
                    .to_string_lossy()
                    .starts_with(".tmp-")
            })
            .count()
    }

    /// Um arquivo criado no destino depois do `needs_write` (a cópia que a pessoa editou, ou
    /// outro processo) fica como está: gravar o perdedor não o substitui.
    #[test]
    fn a_loser_never_overwrites_a_file_that_appeared_after_the_check() {
        let tmp = TempDir::new();
        let dest = tmp
            .path()
            .join("conflitos/secret/oauth/slack.json.conflito-pb-0011223344556677");
        fs::create_dir_all(dest.parent().unwrap()).unwrap();
        fs::write(&dest, b"da pessoa").unwrap();

        let _ = write_loser(&dest, b"perdedor");

        assert_eq!(fs::read(&dest).unwrap(), b"da pessoa");
        assert_eq!(leftover_temps(dest.parent().unwrap()), 0);
    }

    /// Destino livre: o perdedor é gravado e nenhum temporário sobra.
    #[test]
    fn a_loser_is_written_when_the_destination_is_free() {
        let tmp = TempDir::new();
        let dest = tmp
            .path()
            .join("conflitos/secret/oauth/slack.json.conflito-pb-0011223344556677");

        write_loser(&dest, b"perdedor").unwrap();

        assert_eq!(fs::read(&dest).unwrap(), b"perdedor");
        assert_eq!(leftover_temps(dest.parent().unwrap()), 0);
    }

    #[tokio::test]
    async fn plain_merge_still_works_without_a_conflicts_dir() {
        let (d, _, _) = config_conflict().await;
        let eng = d.rig.engine("pc");
        let MergeOutcome::Merged { conflicts, .. } = eng.merge(30).await.unwrap() else {
            panic!("esperava Merged");
        };
        assert_eq!(conflicts.len(), 1);
        assert_eq!(eng.heads().await.unwrap().tips.len(), 1);
    }

    // ---- M-1 (b): quem perdeu sem fazer o merge deriva o perdedor do manifest de fusão ----

    /// `pc` funde o conflito de `config` (`pa` perde) e devolve o id da fusão.
    async fn merged_by_another_device() -> (Diverged, Roots, PathBuf, ObjectId) {
        let (d, roots, dir) = config_conflict().await;
        let MergeOutcome::Merged { id, .. } = d.rig.engine("pc").merge(30).await.unwrap() else {
            panic!("esperava Merged");
        };
        (d, roots, dir, id)
    }

    #[tokio::test]
    async fn the_device_that_lost_without_merging_gets_the_conflict_from_the_merge_manifest() {
        let (d, _, _, merged) = merged_by_another_device().await;
        // `pa` está em tips[0] (created_at 10) e perdeu para `pb`
        let found = d
            .rig
            .engine("pa")
            .conflicts_between(Some(&d.tips[0]), &merged)
            .await
            .unwrap();
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].path, "settings.toml");
        assert_eq!(found[0].loser_device, "pa");
        assert_eq!(found[0].loser.as_ref().unwrap().sha256, sha(b"from-a"));
    }

    #[tokio::test]
    async fn the_derived_loser_is_saved_once_and_matches_what_the_merging_device_saves() {
        let (d, roots, dir, merged) = merged_by_another_device().await;
        let eng = d.rig.engine("pa");
        let found = eng
            .conflicts_between(Some(&d.tips[0]), &merged)
            .await
            .unwrap();
        let first = eng
            .save_conflict_losers(&found, &dir, &roots)
            .await
            .unwrap();
        assert_eq!(
            first,
            SavedLosers {
                saved: 1,
                already: 0
            }
        );
        assert_eq!(fs::read(loser_copy(&dir)).unwrap(), b"from-a");
        let again = eng
            .save_conflict_losers(&found, &dir, &roots)
            .await
            .unwrap();
        assert_eq!(
            again,
            SavedLosers {
                saved: 0,
                already: 1
            }
        );
    }

    #[tokio::test]
    async fn a_merge_the_folder_already_reflects_has_nothing_to_derive() {
        let (d, _, _, merged) = merged_by_another_device().await;
        let found = d
            .rig
            .engine("pa")
            .conflicts_between(Some(&merged), &merged)
            .await
            .unwrap();
        assert!(found.is_empty());
    }

    #[tokio::test]
    async fn a_vault_conflict_derives_but_has_nothing_to_save() {
        let d = diverge(
            &[("a.md", b"1")],
            vec![
                ("pa", 10, vec![("a.md", b"from-a")]),
                ("pb", 20, vec![("a.md", b"from-b")]),
            ],
        )
        .await;
        let MergeOutcome::Merged { id, .. } = d.rig.engine("pc").merge(30).await.unwrap() else {
            panic!("esperava Merged");
        };
        let eng = d.rig.engine("pa");
        let found = eng.conflicts_between(Some(&d.tips[0]), &id).await.unwrap();
        assert_eq!(found.len(), 1);
        let dir = d.rig.tmp.path().join("conflitos");
        let saved = eng
            .save_conflict_losers(&found, &dir, &vault_only(&d.rig.dir("vault-vivo")))
            .await
            .unwrap();
        assert_eq!(
            saved,
            SavedLosers {
                saved: 0,
                already: 0
            }
        );
    }

    #[tokio::test]
    async fn a_merge_whose_parent_is_missing_is_skipped_without_error() {
        let (d, _, _, merged) = merged_by_another_device().await;
        d.rig.store.delete(&d.tips[0]).await.unwrap();
        let found = d
            .rig
            .engine("pa")
            .conflicts_between(None, &merged)
            .await
            .unwrap();
        assert!(found.is_empty());
    }

    // ---------- piso: o snapshot não esvazia uma classe sem autorização ----------

    /// Ponta com `files` em `class` (pasta `src`), vinda do dispositivo `pc`.
    async fn tip_with(rig: &Rig, class: Class, files: &[&str]) -> (PathBuf, Engine<Backend>) {
        let folder = rig.dir("pasta");
        for name in files {
            write(&folder, name, name.as_bytes());
        }
        let engine = rig.engine("pc");
        created(engine.snapshot(&roots_of(class, &folder), 1).await.unwrap());
        (folder, engine)
    }

    fn empty_the_folder(folder: &Path) {
        for entry in fs::read_dir(folder).unwrap() {
            fs::remove_file(entry.unwrap().path()).unwrap();
        }
    }

    #[tokio::test]
    async fn a_class_recreated_empty_is_refused_and_nothing_is_written() {
        let rig = Rig::new();
        let (folder, engine) = tip_with(&rig, Class::Secret, &["a.json", "b.json"]).await;
        empty_the_folder(&folder);

        assert_eq!(
            engine.snapshot(&roots_of(Class::Secret, &folder), 2).await,
            Err(EngineError::ClassEmptied {
                class: Class::Secret,
                had: 2
            })
        );
        assert_eq!(rig.count(ObjectKind::Manifest).await, 1);
    }

    #[tokio::test]
    async fn the_same_class_can_be_emptied_when_the_consumer_allows_it() {
        let rig = Rig::new();
        let (folder, engine) = tip_with(&rig, Class::Secret, &["a.json", "b.json"]).await;
        empty_the_folder(&folder);

        let (_, manifest, _, _) = created(
            engine
                .snapshot_allowing_empty(&roots_of(Class::Secret, &folder), 2, &[Class::Secret])
                .await
                .unwrap(),
        );
        assert!(file_paths(&manifest).is_empty());
    }

    #[tokio::test]
    async fn removing_only_some_of_the_files_is_not_blocked() {
        let rig = Rig::new();
        let (folder, engine) = tip_with(&rig, Class::Secret, &["a.json", "b.json", "c.json"]).await;
        fs::remove_file(folder.join("a.json")).unwrap();
        fs::remove_file(folder.join("b.json")).unwrap();

        let (_, manifest, _, _) = created(
            engine
                .snapshot(&roots_of(Class::Secret, &folder), 2)
                .await
                .unwrap(),
        );
        assert_eq!(file_paths(&manifest), ["c.json"]);
    }

    #[tokio::test]
    async fn a_class_that_was_already_empty_stays_empty_without_error() {
        let rig = Rig::new();
        let (folder, engine) = tip_with(&rig, Class::Secret, &[]).await;

        assert!(matches!(
            engine.snapshot(&roots_of(Class::Secret, &folder), 2).await,
            Ok(Snapshot::Unchanged { .. })
        ));
    }

    #[tokio::test]
    async fn the_first_snapshot_of_an_empty_folder_is_allowed() {
        let rig = Rig::new();
        let folder = rig.dir("pasta");
        let (_, manifest, _, _) = created(
            rig.engine("pc")
                .snapshot(&roots_of(Class::Secret, &folder), 1)
                .await
                .unwrap(),
        );
        assert!(file_paths(&manifest).is_empty());
    }

    #[tokio::test]
    async fn a_folder_left_with_only_files_the_scan_filter_refuses_counts_as_emptied() {
        let rig = Rig::new();
        let (folder, engine) = tip_with(&rig, Class::Secret, &["a.json"]).await;
        let engine = engine.with_scan_filter(Class::Secret, flat_json_name);
        fs::remove_file(folder.join("a.json")).unwrap();
        write(&folder, "a.json.bak", b"old-token");

        assert_eq!(
            engine.snapshot(&roots_of(Class::Secret, &folder), 2).await,
            Err(EngineError::ClassEmptied {
                class: Class::Secret,
                had: 1
            })
        );
    }

    #[tokio::test]
    async fn allowing_one_class_does_not_allow_another() {
        let rig = Rig::new();
        let (config, secret) = (rig.dir("cfg"), rig.dir("sec"));
        write(&config, "c.toml", b"c");
        write(&secret, "s.json", b"s");
        let roots = Roots {
            config: Some(config.clone()),
            secret: Some(secret.clone()),
            ..Roots::default()
        };
        let engine = rig.engine("pc");
        created(engine.snapshot(&roots, 1).await.unwrap());
        fs::remove_file(secret.join("s.json")).unwrap();

        assert_eq!(
            engine
                .snapshot_allowing_empty(&roots, 2, &[Class::Config])
                .await,
            Err(EngineError::ClassEmptied {
                class: Class::Secret,
                had: 1
            })
        );
    }

    /// Um outro dispositivo publicou `files` na classe `secret` (sem filtro); devolve a pasta, vazia, de um segundo
    /// dispositivo no mesmo armazenamento, que tem o filtro de entradas `small_flat_json`.
    async fn tip_from_a_peer_with_secret(rig: &Rig, files: &[&str]) -> (PathBuf, Engine<Backend>) {
        let theirs = rig.dir("src-secret");
        for name in files {
            write(&theirs, name, b"{}");
        }
        created(
            rig.engine("peer")
                .snapshot(&roots_of(Class::Secret, &theirs), 1)
                .await
                .unwrap(),
        );
        let mine = rig.dir("my-secret");
        let engine = rig
            .engine("pc")
            .with_entry_filter(Class::Secret, small_flat_json);
        (mine, engine)
    }

    #[tokio::test]
    async fn entries_the_entry_filter_refuses_do_not_count_as_a_class_that_had_entries() {
        // o dispositivo nunca aplicaria `y.bak`: a pasta vazia dele não é "esvaziei a classe"
        let rig = Rig::new();
        let (mine, engine) = tip_from_a_peer_with_secret(&rig, &["y.bak"]).await;

        let (_, manifest, _, _) = created(
            engine
                .snapshot(&roots_of(Class::Secret, &mine), 2)
                .await
                .unwrap(),
        );
        assert!(file_paths(&manifest).is_empty());
    }

    #[tokio::test]
    async fn an_entry_the_filter_accepts_still_counts_when_the_class_is_emptied() {
        let rig = Rig::new();
        let (mine, engine) = tip_from_a_peer_with_secret(&rig, &["ok.json", "y.bak"]).await;

        assert_eq!(
            engine.snapshot(&roots_of(Class::Secret, &mine), 2).await,
            Err(EngineError::ClassEmptied {
                class: Class::Secret,
                had: 1
            })
        );
    }

    #[tokio::test]
    async fn a_disabled_class_is_carried_and_never_counts_as_emptied() {
        let rig = Rig::new();
        let (vault, secret) = (rig.dir("vault"), rig.dir("sec"));
        write(&vault, "v.md", b"v");
        write(&secret, "s.json", b"s");
        let both = Roots {
            vault: Some(vault.clone()),
            secret: Some(secret),
            ..Roots::default()
        };
        let engine = rig.engine("pc");
        created(engine.snapshot(&both, 1).await.unwrap());
        write(&vault, "v.md", b"v2");

        let (_, manifest, _, _) = created(engine.snapshot(&vault_only(&vault), 2).await.unwrap());
        assert_eq!(file_paths(&manifest), ["v.md", "s.json"]);
    }

    // ---------- begin_epoch (ADR 0018, itens 4 e 5; P-48) ----------

    fn new_key() -> VaultKey {
        VaultKey::from_bytes([4u8; 32])
    }

    fn new_keys(epoch: u32) -> EpochKeys {
        EpochKeys::derive(&new_key(), epoch)
    }

    /// Um Vault da época 1 com `a.md` e `b.md`, e o id da ponta.
    async fn old_vault(rig: &Rig) -> (PathBuf, ObjectId) {
        let vault = rig.dir("vault");
        write(&vault, "a.md", b"1");
        write(&vault, "b.md", b"2");
        let (head, ..) = created(
            rig.engine("pc")
                .snapshot(&vault_only(&vault), 1)
                .await
                .unwrap(),
        );
        (vault, head)
    }

    async fn stored(rig: &Rig) -> (usize, usize) {
        (
            rig.count(ObjectKind::Blob).await,
            rig.count(ObjectKind::Manifest).await,
        )
    }

    #[tokio::test]
    async fn begin_epoch_publishes_a_full_snapshot_that_only_the_new_key_opens() {
        let rig = Rig::new();
        let (vault, old_head) = old_vault(&rig).await;
        let old = rig.store.get(&old_head).await.unwrap().unwrap();
        let old_manifest = Manifest::open(&keys(1), &old).unwrap();

        let (next, snap) = rig
            .engine("pc")
            .begin_epoch(&new_key(), &vault_only(&vault), 5)
            .await
            .unwrap();
        let (id, manifest, new_blobs, _) = created(snap);

        assert_eq!(manifest.key_epoch, 2);
        assert_eq!(manifest.base_version, None);
        assert_eq!(manifest.counter, 1);
        assert_eq!(file_paths(&manifest), ["a.md", "b.md"]);
        assert_eq!(new_blobs, 2);
        let bytes = rig.store.get(&id).await.unwrap().unwrap();
        assert_eq!(Manifest::open(&new_keys(2), &bytes), Ok(manifest.clone()));
        assert!(Manifest::open(&keys(1), &bytes).is_err());
        // a época nova não cita blob da anterior: todos recifrados, com ids novos
        for file in &manifest.files {
            assert!(old_manifest.files.iter().all(|o| o.blob != file.blob));
        }
        let heads = next.heads().await.unwrap();
        assert_eq!(heads.tips.iter().map(|t| t.id).collect::<Vec<_>>(), [id]);
        assert_eq!(next.keys.epoch(), 2);
    }

    #[tokio::test]
    async fn begin_epoch_refuses_the_same_vault_key_and_writes_nothing() {
        let rig = Rig::new();
        let (vault, _) = old_vault(&rig).await;
        let before = stored(&rig).await;

        let same = VaultKey::from_bytes([3u8; 32]);
        let refused = rig
            .engine("pc")
            .begin_epoch(&same, &vault_only(&vault), 5)
            .await;
        assert_eq!(
            refused.map(|_| ()),
            Err(EngineError::Object(Error::KeyNotRotated))
        );
        assert_eq!(stored(&rig).await, before);
    }

    #[tokio::test]
    async fn begin_epoch_refuses_when_there_is_no_next_epoch() {
        let rig = Rig::new();
        let vault = rig.dir("vault");
        write(&vault, "a.md", b"1");
        let top = Engine::new(rig.store.clone(), keys(u32::MAX), "pc").unwrap();
        assert_eq!(
            top.begin_epoch(&new_key(), &vault_only(&vault), 5)
                .await
                .map(|_| ()),
            Err(EngineError::Object(Error::EpochOverflow))
        );
        assert_eq!(stored(&rig).await, (0, 0));
    }

    #[tokio::test]
    async fn begin_epoch_keeps_the_snapshot_preconditions_and_writes_nothing() {
        // duas pontas
        let rig = Rig::new();
        let (vault, _) = old_vault(&rig).await;
        put_epoch_manifest(&rig, 1, None, 1).await;
        let before = stored(&rig).await;
        assert_eq!(
            rig.engine("pc")
                .begin_epoch(&new_key(), &vault_only(&vault), 5)
                .await
                .map(|_| ()),
            Err(EngineError::Diverged {
                tips: 2,
                rejected: 0
            })
        );
        assert_eq!(stored(&rig).await, before);

        // apply interrompido
        let h = hist().await;
        interrupt_apply(&h.target);
        let before = stored(&h.rig).await;
        assert_eq!(
            h.rig
                .engine("pc")
                .begin_epoch(&new_key(), &vault_only(&h.target), 9)
                .await
                .map(|_| ()),
            Err(EngineError::Interrupted)
        );
        assert_eq!(stored(&h.rig).await, before);
    }

    #[tokio::test]
    async fn begin_epoch_refuses_a_store_that_went_back_in_time() {
        let rig = Rig::new();
        let vault = rig.dir("vault");
        write(&vault, "a.md", b"1");
        let path = state_path(&rig);
        let engine = guarded(&rig, "pc", &path);
        let (head, ..) = created(engine.snapshot(&vault_only(&vault), 1).await.unwrap());
        rig.store.delete(&head).await.unwrap();
        let before = stored(&rig).await;

        assert_eq!(
            engine
                .begin_epoch(&new_key(), &vault_only(&vault), 5)
                .await
                .map(|_| ()),
            Err(EngineError::Rollback)
        );
        assert_eq!(stored(&rig).await, before);
    }

    #[tokio::test]
    async fn begin_epoch_waits_for_a_newer_epoch_the_person_has_not_acknowledged() {
        let rig = Rig::new();
        let (vault, _) = old_vault(&rig).await;
        // alguém já rotacionou com outra chave: o objeto da época 2 não abre com a nossa
        let theirs = put_epoch_manifest(&rig, 2, None, 1).await;
        let before = stored(&rig).await;
        let engine = rig.engine("pc");
        assert_eq!(
            engine
                .begin_epoch(&new_key(), &vault_only(&vault), 5)
                .await
                .map(|_| ()),
            Err(EngineError::NeedsNewKey { have: 1, found: 2 })
        );
        assert_eq!(stored(&rig).await, before);

        // reconhecido como forjado, ele convive sem vetar a rotação (nunca abre com a chave nova)
        engine
            .acknowledge_foreign(&[ForeignEpoch {
                epoch: 2,
                id: theirs,
            }])
            .await
            .unwrap();
        let (next, snap) = engine
            .begin_epoch(&new_key(), &vault_only(&vault), 5)
            .await
            .unwrap();
        let (id, ..) = created(snap);
        let heads = next.heads().await.unwrap();
        assert_eq!(heads.tips.iter().map(|t| t.id).collect::<Vec<_>>(), [id]);
    }

    #[tokio::test]
    async fn begin_epoch_moves_the_state_to_the_new_epoch() {
        let rig = Rig::new();
        let vault = rig.dir("vault");
        write(&vault, "a.md", b"1");
        let path = state_path(&rig);
        let engine = guarded(&rig, "pc", &path);
        created(engine.snapshot(&vault_only(&vault), 1).await.unwrap());

        let (next, snap) = engine
            .begin_epoch(&new_key(), &vault_only(&vault), 5)
            .await
            .unwrap();
        let (id, ..) = created(snap);
        assert_eq!(saved(&path), Some(accepted(&id, 2)));
        // o engine novo usa o mesmo estado e não dá `Rollback`
        write(&vault, "b.md", b"2");
        created(next.snapshot(&vault_only(&vault), 6).await.unwrap());
    }

    /// A tentativa caiu depois de gravar o manifest e antes de gravar o estado: repetir com a
    /// mesma chave adota o manifest que já existe, em vez de publicar outro.
    #[tokio::test]
    async fn begin_epoch_is_resumable_and_adopts_the_manifest_already_published() {
        let rig = Rig::new();
        let (vault, _) = old_vault(&rig).await;
        let (_, first) = rig
            .engine("pc")
            .begin_epoch(&new_key(), &vault_only(&vault), 5)
            .await
            .unwrap();
        let (first_id, ..) = created(first);
        let before = stored(&rig).await;

        let path = state_path(&rig);
        let retried = rig.engine("pc").with_state(&path);
        let (_, snap) = retried
            .begin_epoch(&new_key(), &vault_only(&vault), 6)
            .await
            .unwrap();
        match snap {
            Snapshot::Unchanged { head, .. } => assert_eq!(head, first_id),
            other => panic!("esperava Unchanged, veio {other:?}"),
        }
        assert_eq!(stored(&rig).await, before);
        assert_eq!(saved(&path), Some(accepted(&first_id, 2)));
    }

    #[tokio::test]
    async fn a_retry_after_the_folder_changed_builds_on_the_manifest_already_published() {
        let rig = Rig::new();
        let (vault, _) = old_vault(&rig).await;
        let (_, first) = rig
            .engine("pc")
            .begin_epoch(&new_key(), &vault_only(&vault), 5)
            .await
            .unwrap();
        let (first_id, ..) = created(first);
        write(&vault, "c.md", b"3");

        let (_, snap) = rig
            .engine("pc")
            .begin_epoch(&new_key(), &vault_only(&vault), 6)
            .await
            .unwrap();
        let (_, manifest, ..) = created(snap);
        assert_eq!(manifest.key_epoch, 2);
        assert_eq!(manifest.counter, 2);
        assert_eq!(manifest.base_version, Some(Hash32(first_id.hash)));
    }

    #[tokio::test]
    async fn the_new_engine_keeps_the_device_name_the_state_and_the_scan_filters() {
        fn no_skip(path: &str) -> bool {
            path != "skip.md"
        }
        let rig = Rig::new();
        let vault = rig.dir("vault");
        write(&vault, "a.md", b"1");
        let path = state_path(&rig);
        let engine = rig
            .engine("meu-pc")
            .with_state(&path)
            .with_scan_filter(Class::Vault, no_skip);
        created(engine.snapshot(&vault_only(&vault), 1).await.unwrap());

        let (next, snap) = engine
            .begin_epoch(&new_key(), &vault_only(&vault), 5)
            .await
            .unwrap();
        let (_, manifest, ..) = created(snap);
        assert_eq!(manifest.device, "meu-pc");
        write(&vault, "skip.md", b"x");
        write(&vault, "ok.md", b"y");
        let (_, later, ..) = created(next.snapshot(&vault_only(&vault), 6).await.unwrap());
        assert_eq!(file_paths(&later), ["a.md", "ok.md"]);
        assert_eq!(saved(&path).map(|s| s.key_epoch), Some(2));
    }
}
