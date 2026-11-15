//! Diário em disco do `Engine::apply` (Fase 2a, PR 3c, P-27): o que fazer para voltar uma pasta
//! ao estado anterior se o processo morrer no meio da troca de arquivos.
//!
//! Um diário por raiz de classe (`<raiz>/.arca-apply-journal`, `0600`, só metadados, nunca
//! conteúdo). Fase `open`: a troca pode estar pela metade, a recuperação desfaz. Fase
//! `committed`: todos os arquivos já foram trocados, a recuperação só termina (apaga os
//! originais postos de lado). A recuperação nunca confia no texto do diário para escolher um
//! caminho e nunca sobrescreve nem apaga um arquivo cujo conteúdo não seja o que o diário
//! espera.

use crate::apply::{
    absent_in, is_reserved_name, name_of, new_name, old_name, open_parent, remove_emptied_folders,
    sha_in, Kind, Planned,
};
use crate::dirfd::{Folder, OpenError};
use crate::disk::write_atomic;
use crate::engine::EngineError;
use crate::manifest::{validate_path, Hash32};
use serde::{Deserialize, Serialize};
use std::collections::HashSet;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

/// Nome do diário dentro da raiz. Reservado: não entra no snapshot nem pode ser destino.
pub(crate) const JOURNAL_NAME: &str = ".arca-apply-journal";
const JOURNAL_VERSION: u8 = 1;
/// Um diário tem uns 230 bytes por arquivo trocado (caminho típico): o manifest aceita até
/// `MAX_FILES` arquivos e 64 MiB (`MAX_MANIFEST_JSON_BYTES`), e uma entrada do diário (caminho e
/// dois hashes) pesa quase o mesmo que a do manifest, então o diário tem o mesmo teto: se o
/// manifest coube, o `apply` dele também cabe (P-33 item 8). O mesmo teto vale ao gravar e ao ler,
/// para nunca gravar o que a recuperação não consegue ler.
pub(crate) const MAX_JOURNAL_BYTES: u64 = 16 << 20;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub(crate) enum Phase {
    Open,
    Committed,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Entry {
    pub n: usize,
    pub kind: Kind,
    /// Caminho relativo à raiz, com `/`.
    pub path: String,
    pub from_sha: Option<Hash32>,
    pub to_sha: Option<Hash32>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Journal {
    pub version: u8,
    pub phase: Phase,
    pub entries: Vec<Entry>,
    /// Id do `apply` que gravou este diário e quantas raízes ele tem (P-33 item 2): a recuperação
    /// decide por `apply`, não por raiz. Os dois presentes ou os dois ausentes; um diário sem eles
    /// (gravado antes disto) vale como um `apply` de uma raiz só.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub apply_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub peers: Option<usize>,
    /// Pastas (caminhos relativos à raiz, pai antes do filho) que **não existiam** quando o diário
    /// foi gravado e que o `apply` vai criar (P-33 item 4). Só a recuperação em fase `open` as usa,
    /// e só com `rmdir` (pasta vazia): uma que já existia nunca entra aqui.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub dirs: Vec<String>,
}

/// O `apply` que reúne os diários de várias raízes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ApplyGroup {
    id: String,
    peers: usize,
}

impl ApplyGroup {
    /// Id novo (16 bytes aleatórios em hex) para um `apply` de `peers` raízes.
    pub(crate) fn new(peers: usize) -> Result<ApplyGroup, EngineError> {
        let mut random = [0u8; 16];
        getrandom::fill(&mut random).map_err(|_| EngineError::Io(io::ErrorKind::Other))?;
        let id = random.iter().map(|b| format!("{b:02x}")).collect();
        Ok(ApplyGroup { id, peers })
    }
}

/// O que a recuperação de uma raiz fez.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub(crate) struct RootRecovery {
    /// Entradas devolvidas ao estado de origem.
    pub rolled_back: usize,
    /// Entradas terminadas (fase `committed`).
    pub completed: usize,
    /// Arquivos que a recuperação não tocou porque o disco não bate com o diário.
    pub kept: Vec<PathBuf>,
}

impl Journal {
    /// Diário `open` com as entradas de `planned` que ficam sob `root`.
    pub(crate) fn open(root: &Path, planned: &[Planned]) -> Journal {
        let entries = planned
            .iter()
            .filter(|p| p.root == root)
            .filter_map(|p| {
                let relative = p.dest.strip_prefix(root).ok()?;
                let path: Vec<_> = relative.iter().map(|c| c.to_string_lossy()).collect();
                Some(Entry {
                    n: p.n,
                    kind: p.kind,
                    path: path.join("/"),
                    from_sha: p.from_sha,
                    to_sha: p.to.as_ref().map(|t| t.sha256),
                })
            })
            .collect();
        Journal {
            version: JOURNAL_VERSION,
            phase: Phase::Open,
            entries,
            apply_id: None,
            peers: None,
            dirs: Vec::new(),
        }
    }

    /// Registra as pastas que o `apply` vai criar (ver `dirs`).
    pub(crate) fn with_dirs(mut self, dirs: Vec<String>) -> Journal {
        self.dirs = dirs;
        self
    }

    /// Marca o diário como parte de um `apply` de várias raízes.
    pub(crate) fn in_apply(mut self, group: &ApplyGroup) -> Journal {
        self.apply_id = Some(group.id.clone());
        self.peers = Some(group.peers);
        self
    }

    pub(crate) fn path(root: &Path) -> PathBuf {
        root.join(JOURNAL_NAME)
    }

    fn encode(&self) -> Result<Vec<u8>, EngineError> {
        let json = serde_json::to_vec(self).map_err(|_| EngineError::CorruptJournal)?;
        if json.len() as u64 > MAX_JOURNAL_BYTES {
            // antes de qualquer `stage`: o `apply` falha sem tocar em nada
            return Err(EngineError::JournalTooLarge);
        }
        Ok(json)
    }

    /// Grava de forma atômica (`0600`), trocando o diário que existir. Só para o `apply` que já
    /// é dono dele (marcar `committed`); para criar, `create`.
    pub(crate) fn save(&self, root: &Path) -> Result<(), EngineError> {
        write_atomic(&Journal::path(root), &self.encode()?).map_err(io_error)
    }

    /// Cria o diário **sem sobrescrever** um que já exista (`Interrupted`): outro `apply` ou uma
    /// queda anterior é dono daquela raiz. Grava num arquivo de trabalho e o liga ao nome final
    /// com `hard_link`, que falha se o nome existe.
    pub(crate) fn create(&self, root: &Path) -> Result<(), EngineError> {
        let json = self.encode()?;
        let mut random = [0u8; 8];
        getrandom::fill(&mut random).map_err(|_| EngineError::Io(io::ErrorKind::Other))?;
        let hex: String = random.iter().map(|b| format!("{b:02x}")).collect();
        let staging = root.join(format!(".tmp-{hex}"));
        write_atomic(&staging, &json).map_err(io_error)?;
        let linked = fs::hard_link(&staging, Journal::path(root));
        let _ = fs::remove_file(&staging);
        match linked {
            // o nome do diário precisa ser durável antes de o `apply` mexer em qualquer arquivo
            // (P-33 item 3); o `fsync` do `write_atomic` cobriu o temporário, não este link
            Ok(()) => sync_root(root),
            Err(e) if e.kind() == io::ErrorKind::AlreadyExists => Err(EngineError::Interrupted),
            Err(e) => Err(io_error(e)),
        }
    }

    /// `None` se não existe. Link no lugar, grande demais, de outro dono ou legível por grupo ou
    /// outros (Unix), JSON inválido, versão ou fase desconhecida, campo a mais ou caminho que sai
    /// da raiz: `CorruptJournal`.
    pub(crate) fn load(root: &Path) -> Result<Option<Journal>, EngineError> {
        // abre o arquivo sem seguir link e confere tipo e tamanho no que foi aberto, na mesma
        // leitura (P-33 item 6): link, FIFO, pasta ou grande demais vale como corrupção
        let folder = match Folder::open_beneath(root, Path::new("")) {
            Ok(folder) => folder,
            Err(OpenError::NotAFolder) => return Err(EngineError::CorruptJournal),
            Err(OpenError::Io(e)) if e.kind() == io::ErrorKind::NotFound => return Ok(None),
            Err(OpenError::Io(e)) => return Err(io_error(e)),
        };
        let read = match folder.read_limited(JOURNAL_NAME, MAX_JOURNAL_BYTES) {
            Ok(read) => read,
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(None),
            Err(e) if e.kind() == io::ErrorKind::InvalidInput => {
                return Err(EngineError::CorruptJournal)
            }
            Err(e) => return Err(io_error(e)),
        };
        // P-33 item 1: o diário não é autenticado; só vale se for deste usuário e privado (a arca
        // sempre grava `0600`). Quem escreve na pasta sem ser o dono só cria arquivo de outro
        // dono, e esse é recusado aqui, antes de o conteúdo valer qualquer coisa
        if !read.private {
            return Err(EngineError::CorruptJournal);
        }
        let journal: Journal =
            serde_json::from_slice(&read.bytes).map_err(|_| EngineError::CorruptJournal)?;
        journal.validate()?;
        Ok(Some(journal))
    }

    /// Versão conhecida, caminhos que ficam dentro da raiz e não são nomes de trabalho da
    /// arca, `n` sem repetição (entra no nome do temporário) e hashes coerentes com o tipo.
    fn validate(&self) -> Result<(), EngineError> {
        if self.version != JOURNAL_VERSION {
            return Err(EngineError::CorruptJournal);
        }
        let group_fits = match (&self.apply_id, self.peers) {
            (None, None) => true,
            (Some(id), Some(peers)) => {
                id.len() == 32
                    && id.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
                    && peers >= 1
            }
            _ => false,
        };
        if !group_fits {
            return Err(EngineError::CorruptJournal);
        }
        let mut dir_seen = HashSet::new();
        for d in &self.dirs {
            let reserved = d.rsplit('/').next().is_some_and(is_reserved_name);
            if validate_path(d).is_err() || reserved || !dir_seen.insert(d.as_str()) {
                return Err(EngineError::CorruptJournal);
            }
        }
        let mut seen = HashSet::new();
        let mut paths = HashSet::new();
        for e in &self.entries {
            let name = e.path.rsplit('/').next().unwrap_or(&e.path);
            let hashes_fit = match e.kind {
                Kind::Create => e.from_sha.is_none() && e.to_sha.is_some(),
                Kind::Update => e.from_sha.is_some() && e.to_sha.is_some(),
                Kind::Remove => e.from_sha.is_some() && e.to_sha.is_none(),
            };
            if validate_path(&e.path).is_err()
                || is_reserved_name(name)
                || !seen.insert(e.n)
                // o mesmo arquivo duas vezes só serviria para multiplicar leituras (DoS)
                || !paths.insert(e.path.as_str())
                || !hashes_fit
            {
                return Err(EngineError::CorruptJournal);
            }
        }
        Ok(())
    }

    /// Existe algo no nome do diário (arquivo, link ou o que for)?
    pub(crate) fn exists(root: &Path) -> bool {
        fs::symlink_metadata(Journal::path(root)).is_ok()
    }

    pub(crate) fn remove(root: &Path) {
        let _ = fs::remove_file(Journal::path(root));
    }
}

fn io_error(e: io::Error) -> EngineError {
    EngineError::Io(e.kind())
}

/// `fsync` da pasta da raiz.
fn sync_root(root: &Path) -> Result<(), EngineError> {
    match Folder::open_beneath(root, Path::new("")) {
        Ok(folder) => folder.sync().map_err(io_error),
        Err(OpenError::NotAFolder) => Err(EngineError::Io(io::ErrorKind::NotADirectory)),
        Err(OpenError::Io(e)) => Err(io_error(e)),
    }
}

fn dest_of(root: &Path, entry: &Entry) -> PathBuf {
    let mut dest = root.to_path_buf();
    dest.extend(entry.path.split('/'));
    dest
}

/// Uma pasta já aberta (sem seguir link) e o caminho dela, só para montar o que vai em `kept`.
/// Toda conferência e toda ação da recuperação agem em `folder`, nunca num caminho refeito do
/// zero (P-29 item 1).
struct Slot<'a> {
    folder: &'a Folder,
    dir: &'a Path,
}

/// Remove `name` se for um arquivo regular (nunca segue link, nunca apaga pasta).
fn remove_if_regular(folder: &Folder, name: &str) {
    if folder.stat(name).is_ok_and(|e| e.is_file) {
        let _ = folder.unlink(name);
    }
}

/// Recupera as raízes de `roots` (P-33 item 2). Lê **todos** os diários antes de tocar em
/// qualquer raiz (um ilegível em qualquer uma recusa tudo) e decide por `apply`, não por raiz:
/// o marcador `committed` só é gravado depois de **todos** os arquivos de **todas** as raízes
/// estarem trocados, então se alguma raiz do `apply` o tem, todas terminam; senão, todas voltam.
/// Se faltam raízes do `apply` entre as de `roots` e nenhuma tem o marcador, a que falta pode
/// tê-lo: `IncompleteApply`, sem tocar em nada. Os diários só saem depois de **todas** as raízes
/// recuperadas, para uma falha no meio poder ser refeita com a mesma decisão.
pub(crate) fn recover_all(roots: &[PathBuf]) -> Result<RootRecovery, EngineError> {
    let mut loaded = Vec::new();
    for root in roots {
        if let Some(journal) = Journal::load(root)? {
            loaded.push((root, journal));
        }
    }
    let phases = decide_phases(&loaded)?;
    let mut total = RootRecovery::default();
    for ((root, journal), phase) in loaded.iter().zip(phases) {
        let done = recover_with(root, journal, phase)?;
        total.rolled_back += done.rolled_back;
        total.completed += done.completed;
        total.kept.extend(done.kept);
    }
    for (root, _) in &loaded {
        Journal::remove(root);
    }
    Ok(total)
}

/// A fase com que cada diário de `loaded` é recuperado, na mesma ordem.
fn decide_phases(loaded: &[(&PathBuf, Journal)]) -> Result<Vec<Phase>, EngineError> {
    let mut phases: Vec<Phase> = loaded.iter().map(|(_, j)| j.phase).collect();
    let mut seen: Vec<&str> = Vec::new();
    for (_, journal) in loaded {
        let Some(id) = journal.apply_id.as_deref() else {
            continue; // sem id: um `apply` de uma raiz só, decide pelo próprio marcador
        };
        if seen.contains(&id) {
            continue;
        }
        seen.push(id);
        let group: Vec<usize> = (0..loaded.len())
            .filter(|&i| loaded[i].1.apply_id.as_deref() == Some(id))
            .collect();
        let peers = journal.peers.unwrap_or(1);
        // o mesmo `apply` não pode ter números de raízes diferentes nem mais diários que raízes
        if group.iter().any(|&i| loaded[i].1.peers != journal.peers) || group.len() > peers {
            return Err(EngineError::CorruptJournal);
        }
        let committed = group.iter().any(|&i| loaded[i].1.phase == Phase::Committed);
        if !committed && group.len() < peers {
            return Err(EngineError::IncompleteApply);
        }
        let phase = if committed {
            Phase::Committed
        } else {
            Phase::Open
        };
        for &i in &group {
            phases[i] = phase;
        }
    }
    Ok(phases)
}

/// Recupera uma raiz sozinha, decidindo pelo próprio marcador. `None` se não há diário. Atalho
/// dos testes de uma raiz só; o motor usa `recover_all`.
#[cfg(test)]
pub(crate) fn recover_root(root: &Path) -> Result<Option<RootRecovery>, EngineError> {
    let Some(journal) = Journal::load(root)? else {
        return Ok(None);
    };
    let recovery = recover_with(root, &journal, journal.phase)?;
    Journal::remove(root);
    Ok(Some(recovery))
}

/// Recupera uma raiz como se o diário estivesse em `phase`. Não apaga o diário.
fn recover_with(root: &Path, journal: &Journal, phase: Phase) -> Result<RootRecovery, EngineError> {
    let mut recovery = RootRecovery::default();
    match phase {
        Phase::Committed => {
            // todos os arquivos foram trocados: falta o que o `finish` faria. Mas o marcador pode
            // ter sobrevivido a uma volta em memória interrompida (erro em outra raiz), então o
            // original de lado só sai se o destino está mesmo no estado novo.
            for e in &journal.entries {
                let dest = dest_of(root, e);
                // sem a pasta (ou a raiz) não há nada dela para terminar; um link no caminho é
                // `LocalChanges`
                let Some(folder) = open_parent(root, &dest)? else {
                    continue;
                };
                let dir = dest
                    .parent()
                    .ok_or(EngineError::Io(io::ErrorKind::InvalidInput))?;
                let slot = Slot {
                    folder: &folder,
                    dir,
                };
                let name = name_of(&dest)?;
                remove_if_regular(&folder, &new_name(e.n));
                if e.kind == Kind::Create {
                    continue;
                }
                let aside = old_name(e.n);
                if folder.stat(&aside).is_err() {
                    continue;
                }
                let in_place = match (e.kind, sha_in(&folder, name)) {
                    (Kind::Update, Ok(Some(sha))) => Some(sha) == e.to_sha,
                    (Kind::Remove, Ok(None)) => true,
                    _ => false,
                };
                if in_place {
                    remove_if_regular(&folder, &aside);
                    if e.kind == Kind::Remove {
                        remove_emptied_folders(root, &dest);
                    }
                } else {
                    keep(&slot, &aside, &mut recovery.kept);
                }
            }
            recovery.completed = journal.entries.len();
        }
        Phase::Open => {
            for e in journal.entries.iter().rev() {
                let dest = dest_of(root, e);
                let Some(folder) = open_parent(root, &dest)? else {
                    continue;
                };
                let dir = dest
                    .parent()
                    .ok_or(EngineError::Io(io::ErrorKind::InvalidInput))?;
                let slot = Slot {
                    folder: &folder,
                    dir,
                };
                let name = name_of(&dest)?;
                let scratch = new_name(e.n);
                remove_if_regular(&folder, &scratch);
                match e.kind {
                    Kind::Create => {
                        // só o que nós pusemos: conteúdo diferente é da pessoa
                        if matches!(sha_in(&folder, name), Ok(Some(sha)) if Some(sha) == e.to_sha) {
                            remove_if_ours(&slot, name, &scratch, e.to_sha, &mut recovery.kept)?;
                        }
                    }
                    Kind::Update | Kind::Remove => {
                        restore_original(e, &slot, name, &mut recovery)?;
                    }
                }
            }
            recovery.rolled_back = journal.entries.len();
            remove_created_dirs(root, &journal.dirs);
        }
    }
    Ok(recovery)
}

/// Remove as pastas que o `apply` interrompido ia criar (P-33 item 4), do filho para o pai, só
/// com `rmdir`: pasta com qualquer coisa dentro, que sumiu ou com link no caminho fica como está.
fn remove_created_dirs(root: &Path, dirs: &[String]) {
    for dir in dirs.iter().rev() {
        let relative = Path::new(dir);
        let (Some(parent), Some(name)) = (
            relative.parent(),
            relative.file_name().and_then(|n| n.to_str()),
        ) else {
            continue;
        };
        if let Ok(folder) = Folder::open_beneath(root, parent) {
            let _ = folder.rmdir(name);
        }
    }
}

/// Devolve `.arca-old-<n>` a `name` se, e só se, o disco bate com o diário. Nunca sobrescreve
/// conteúdo que não seja o que a própria arca pôs.
fn restore_original(
    entry: &Entry,
    slot: &Slot,
    name: &str,
    recovery: &mut RootRecovery,
) -> Result<(), EngineError> {
    let folder = slot.folder;
    let aside = old_name(entry.n);
    let scratch = new_name(entry.n);
    match folder.stat(&aside) {
        // o original nem chegou a ser movido
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(e) => return Err(io_error(e)),
        Ok(meta) if !meta.is_file => {
            keep(slot, &aside, &mut recovery.kept);
            return Ok(());
        }
        Ok(_) => {}
    }
    if !matches!(sha_in(folder, &aside), Ok(Some(sha)) if Some(sha) == entry.from_sha) {
        keep(slot, &aside, &mut recovery.kept);
        return Ok(());
    }
    let free = match sha_in(folder, name) {
        Ok(None) => true,
        // o novo que a arca pôs: tira de lá conferindo o que saiu, nunca por cima
        Ok(Some(sha)) if entry.kind == Kind::Update && Some(sha) == entry.to_sha => {
            remove_if_ours(slot, name, &scratch, entry.to_sha, &mut recovery.kept)?
        }
        // qualquer outro conteúdo (ou link, ou pasta) é da pessoa: o original fica de lado
        _ => false,
    };
    if !free {
        keep(slot, &aside, &mut recovery.kept);
        return Ok(());
    }
    // `link` falha se alguém criou o destino no meio-tempo
    match folder.link(&aside, name) {
        Ok(()) => {
            let _ = folder.unlink(&aside);
        }
        Err(e) if e.kind() == io::ErrorKind::AlreadyExists => {
            keep(slot, &aside, &mut recovery.kept);
        }
        Err(e) => return Err(io_error(e)),
    }
    Ok(())
}

/// Tira `name` do caminho **se for o que a arca pôs** (`expected`). Conferir o hash e depois
/// remover deixaria uma janela para uma edição concorrente; por isso se move primeiro para
/// `scratch` (um `rename` atômico), confere o que foi movido e só então apaga. O que não for o
/// esperado volta, sem sobrescrever (se o destino foi recriado, vai para `kept`).
fn remove_if_ours(
    slot: &Slot,
    name: &str,
    scratch: &str,
    expected: Option<Hash32>,
    kept: &mut Vec<PathBuf>,
) -> Result<bool, EngineError> {
    let folder = slot.folder;
    absent_in(folder, scratch)?;
    match folder.rename(name, scratch) {
        Ok(()) => {}
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(false),
        Err(e) => return Err(io_error(e)),
    }
    if matches!(sha_in(folder, scratch), Ok(Some(sha)) if Some(sha) == expected) {
        let _ = folder.unlink(scratch);
        return Ok(true);
    }
    match folder.link(scratch, name) {
        Ok(()) => {
            let _ = folder.unlink(scratch);
            Ok(false)
        }
        Err(e) => {
            keep(slot, scratch, kept);
            if e.kind() == io::ErrorKind::AlreadyExists {
                Ok(false)
            } else {
                Err(io_error(e))
            }
        }
    }
}

/// Registra em `kept` um arquivo que a recuperação não tocou. Se for um arquivo regular, ele é
/// renomeado para `.arca-kept-<16 hex do sha256>` (nome reservado, único pelo conteúdo): o nome
/// `.arca-old-<n>` colidiria com o próximo `apply`. Sem sucesso, fica onde está.
fn keep(slot: &Slot, name: &str, kept: &mut Vec<PathBuf>) {
    let folder = slot.folder;
    let renamed = (|| {
        if !folder.stat(name).ok()?.is_file {
            return None;
        }
        let sha = folder.sha256(name).ok()?;
        let target = format!(".arca-kept-{}", &sha.to_string()[..16]);
        folder.link(name, &target).ok()?;
        folder.unlink(name).ok()?;
        Some(slot.dir.join(target))
    })();
    kept.push(renamed.unwrap_or_else(|| slot.dir.join(name)));
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::apply::{commit, plan, stage, Item};
    use crate::manifest::{Class, FileEntry};
    use crate::testutil::TempDir;
    use sha2::{Digest, Sha256};
    use std::collections::BTreeMap;
    use std::fs;

    fn sha(data: &[u8]) -> Hash32 {
        Hash32(Sha256::digest(data).into())
    }

    fn entry(path: &str, data: &[u8]) -> FileEntry {
        FileEntry {
            path: path.to_string(),
            class: Class::Vault,
            blob: Hash32([0u8; 32]),
            size: data.len() as u64,
            sha256: sha(data),
        }
    }

    fn tree(root: &Path) -> BTreeMap<String, Vec<u8>> {
        fn walk(dir: &Path, prefix: &str, out: &mut BTreeMap<String, Vec<u8>>) {
            for e in fs::read_dir(dir).unwrap() {
                let e = e.unwrap();
                let name = e.file_name().into_string().unwrap();
                let rel = if prefix.is_empty() {
                    name
                } else {
                    format!("{prefix}/{name}")
                };
                if e.file_type().unwrap().is_dir() {
                    walk(&e.path(), &rel, out);
                } else {
                    out.insert(rel, fs::read(e.path()).unwrap());
                }
            }
        }
        let mut out = BTreeMap::new();
        walk(root, "", &mut out);
        out
    }

    fn files(pairs: &[(&str, &[u8])]) -> BTreeMap<String, Vec<u8>> {
        pairs
            .iter()
            .map(|(p, d)| (p.to_string(), d.to_vec()))
            .collect()
    }

    /// Estado de origem: a.md, sub/b.md e c.md. O `apply` vai atualizar a.md e sub/b.md,
    /// remover c.md e criar d.md.
    fn before() -> [(&'static str, &'static [u8]); 3] {
        [("a.md", b"A0"), ("sub/b.md", b"B0"), ("c.md", b"C0")]
    }

    struct Crash {
        _dir: TempDir,
        root: PathBuf,
        planned: Vec<Planned>,
        journal: Journal,
    }

    /// Monta o disco e o diário como estariam logo antes do `commit`: tudo no lugar, mais o
    /// diário `open` e o novo conteúdo ao lado (`stage`). Devolve também o que o `commit`
    /// faria, para os testes pararem no ponto que quiserem.
    fn prepared() -> Crash {
        let dir = TempDir::new();
        let root = dir.path().join("raiz");
        for (rel, data) in before() {
            let p = root.join(rel);
            fs::create_dir_all(p.parent().unwrap()).unwrap();
            fs::write(p, data).unwrap();
        }
        let item = |kind, rel: &str, from: Option<&[u8]>, to: Option<&[u8]>| Item {
            kind,
            root: root.clone(),
            dest: root.join(rel),
            from: from.map(|d| entry(rel, d)),
            to: to.map(|d| entry(rel, d)),
        };
        let (planned, _) = plan(vec![
            item(Kind::Update, "a.md", Some(b"A0"), Some(b"A1")),
            item(Kind::Update, "sub/b.md", Some(b"B0"), Some(b"B1")),
            item(Kind::Remove, "c.md", Some(b"C0"), None),
            item(Kind::Create, "d.md", None, Some(b"D1")),
        ])
        .unwrap();
        assert_eq!(planned.len(), 4);
        let journal = Journal::open(&root, &planned);
        journal.save(&root).unwrap();
        let mut log = Vec::new();
        for (p, new) in planned.iter().zip([&b"A1"[..], b"B1", b"", b"D1"]) {
            if p.kind != Kind::Remove {
                stage(&mut log, p, new).unwrap();
            }
        }
        Crash {
            _dir: dir,
            root,
            planned,
            journal,
        }
    }

    fn original() -> BTreeMap<String, Vec<u8>> {
        files(&before())
    }

    /// Onde a recuperação deixa, na raiz, um original que não pôde pôr de volta (achado 7):
    /// nome reservado e único pelo conteúdo, para não travar o próximo `apply` nem entrar no
    /// snapshot.
    fn kept_at(dir: &Path, content: &[u8]) -> PathBuf {
        dir.join(format!(".arca-kept-{}", &sha(content).to_string()[..16]))
    }

    // ---------- o formato do diário ----------

    #[test]
    fn the_journal_round_trips_and_holds_only_metadata() {
        let c = prepared();
        let loaded = Journal::load(&c.root).unwrap().unwrap();
        assert_eq!(loaded, c.journal);
        assert_eq!(loaded.phase, Phase::Open);
        assert_eq!(loaded.entries.len(), 4);
        let raw = fs::read_to_string(Journal::path(&c.root)).unwrap();
        for secret in ["A0", "A1", "B0", "C0", "D1"] {
            // nenhum conteúdo de arquivo; só caminhos e hashes
            assert!(!raw.contains(&format!("\"{secret}\"")), "{raw}");
        }
        assert!(raw.contains("a.md") && raw.contains(&sha(b"A0").to_string()));
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = fs::metadata(Journal::path(&c.root))
                .unwrap()
                .permissions()
                .mode();
            assert_eq!(mode & 0o777, 0o600);
        }
    }

    #[test]
    fn a_missing_journal_is_none_and_remove_is_idempotent() {
        let dir = TempDir::new();
        assert_eq!(Journal::load(dir.path()), Ok(None));
        assert!(!Journal::exists(dir.path()));
        Journal::remove(dir.path());
        assert_eq!(recover_root(dir.path()), Ok(None));
    }

    #[test]
    fn a_bad_journal_is_corrupt_and_nothing_is_touched() {
        let good = prepared();
        let text = fs::read_to_string(Journal::path(&good.root)).unwrap();
        let bad: Vec<(&str, Vec<u8>)> = vec![
            ("lixo", b"isto nao e json".to_vec()),
            ("vazio", Vec::new()),
            (
                "versao",
                text.replacen("\"version\":1", "\"version\":2", 1)
                    .into_bytes(),
            ),
            (
                "fase",
                text.replacen("\"open\"", "\"meio\"", 1).into_bytes(),
            ),
            (
                "campo a mais",
                text.replacen("{\"version\"", "{\"extra\":1,\"version\"", 1)
                    .into_bytes(),
            ),
            (
                "caminho fora da raiz",
                text.replacen("\"a.md\"", "\"../fora.md\"", 1).into_bytes(),
            ),
            (
                "caminho absoluto",
                text.replacen("\"a.md\"", "\"/etc/passwd\"", 1).into_bytes(),
            ),
            (
                "caminho reservado",
                text.replacen("\"a.md\"", "\".arca-old-0\"", 1).into_bytes(),
            ),
            ("grande demais", vec![b' '; MAX_JOURNAL_BYTES as usize + 1]),
        ];
        for (name, bytes) in bad {
            let c = prepared();
            fs::write(Journal::path(&c.root), &bytes).unwrap();
            let snapshot = tree(&c.root);
            assert_eq!(
                Journal::load(&c.root),
                Err(EngineError::CorruptJournal),
                "{name}"
            );
            assert_eq!(
                recover_root(&c.root),
                Err(EngineError::CorruptJournal),
                "{name}"
            );
            assert_eq!(
                tree(&c.root),
                snapshot,
                "{name}: recover não pode tocar em nada"
            );
        }
    }

    #[cfg(unix)]
    #[test]
    fn a_journal_that_is_a_symlink_is_corrupt_and_not_followed() {
        let c = prepared();
        let real = c.root.parent().unwrap().join("real.json");
        fs::rename(Journal::path(&c.root), &real).unwrap();
        std::os::unix::fs::symlink(&real, Journal::path(&c.root)).unwrap();
        let before = tree(&c.root);
        assert_eq!(recover_root(&c.root), Err(EngineError::CorruptJournal));
        assert_eq!(tree(&c.root), before);
        assert!(real.exists());
    }

    /// P-33 item 1: a arca sempre grava o diário `0600`. Um diário que grupo ou outros podem ler
    /// ou alterar não foi gravado por ela (ou foi mexido): não vale, e nada é tocado.
    #[cfg(unix)]
    #[test]
    fn a_journal_that_others_can_touch_is_refused_and_nothing_is_changed() {
        use std::os::unix::fs::PermissionsExt;
        for mode in [0o644, 0o660, 0o602, 0o666] {
            let c = prepared();
            fs::set_permissions(Journal::path(&c.root), fs::Permissions::from_mode(mode)).unwrap();
            let before = tree(&c.root);
            assert_eq!(
                Journal::load(&c.root),
                Err(EngineError::CorruptJournal),
                "{mode:o}"
            );
            assert_eq!(
                recover_root(&c.root),
                Err(EngineError::CorruptJournal),
                "{mode:o}"
            );
            assert_eq!(
                tree(&c.root),
                before,
                "{mode:o}: recover não pode tocar em nada"
            );
        }
    }

    /// Um FIFO no nome do diário é corrupção e não trava a leitura (abrir um FIFO sem
    /// `O_NONBLOCK` esperaria um escritor para sempre).
    #[cfg(unix)]
    #[test]
    fn a_journal_that_is_a_fifo_is_corrupt_instead_of_blocking() {
        use rustix::fs::{mknodat, FileType, Mode, CWD};
        let c = prepared();
        fs::remove_file(Journal::path(&c.root)).unwrap();
        mknodat(
            CWD,
            Journal::path(&c.root),
            FileType::Fifo,
            Mode::RUSR | Mode::WUSR,
            0,
        )
        .unwrap();
        assert_eq!(Journal::load(&c.root), Err(EngineError::CorruptJournal));
    }

    // ---------- queda em cada ponto: volta ao estado de origem ----------

    #[test]
    fn a_crash_while_staging_only_leaves_new_files_and_recover_removes_them() {
        let c = prepared();
        assert!(tree(&c.root).keys().any(|k| k.contains(".arca-new-")));
        let r = recover_root(&c.root).unwrap().unwrap();
        assert_eq!(r.kept, Vec::<PathBuf>::new());
        assert_eq!(tree(&c.root), original());
        assert!(!Journal::exists(&c.root));
    }

    #[test]
    fn a_crash_in_the_middle_of_the_commit_goes_back_to_the_origin_state() {
        // a.md já trocado; sub/b.md movido de lado e ainda sem o novo; o resto intocado
        let c = prepared();
        let mut log = Vec::new();
        commit(&mut log, &c.planned[..1]).unwrap();
        let b = &c.planned[1];
        fs::rename(&b.dest, b.dest.parent().unwrap().join(".arca-old-1")).unwrap();
        drop(log); // o processo morreu: o diário em memória se perdeu

        assert_eq!(fs::read(c.root.join("a.md")).unwrap(), b"A1");
        let r = recover_root(&c.root).unwrap().unwrap();
        assert_eq!(r.kept, Vec::<PathBuf>::new());
        assert_eq!(r.rolled_back, 4);
        assert_eq!(tree(&c.root), original());
        assert!(!Journal::exists(&c.root));
    }

    #[test]
    fn a_crash_after_every_file_was_swapped_but_before_the_marker_rolls_everything_back() {
        // o `commit` terminou, o processo morreu antes de gravar `committed`
        let c = prepared();
        let mut log = Vec::new();
        commit(&mut log, &c.planned).unwrap();
        drop(log);
        assert_eq!(fs::read(c.root.join("a.md")).unwrap(), b"A1");
        assert!(c.root.join("d.md").exists() && !c.root.join("c.md").exists());

        let r = recover_root(&c.root).unwrap().unwrap();
        assert_eq!(r.kept, Vec::<PathBuf>::new());
        assert_eq!(tree(&c.root), original());
    }

    #[test]
    fn a_crash_after_the_committed_marker_rolls_forward_and_keeps_the_new_state() {
        let c = prepared();
        let mut log = Vec::new();
        commit(&mut log, &c.planned).unwrap();
        drop(log);
        let mut journal = c.journal.clone();
        journal.phase = Phase::Committed;
        journal.save(&c.root).unwrap();

        let r = recover_root(&c.root).unwrap().unwrap();
        assert_eq!((r.rolled_back, r.completed), (0, 4));
        assert_eq!(r.kept, Vec::<PathBuf>::new());
        assert_eq!(
            tree(&c.root),
            files(&[("a.md", b"A1"), ("sub/b.md", b"B1"), ("d.md", b"D1")])
        );
        assert!(!Journal::exists(&c.root));
    }

    // ---------- a pessoa mexeu depois da queda: nada é sobrescrito ----------

    #[test]
    fn a_destination_edited_after_the_crash_is_never_touched() {
        let c = prepared();
        let mut log = Vec::new();
        commit(&mut log, &c.planned).unwrap();
        drop(log);
        // a pessoa abriu o editor e mexeu em a.md (o conteúdo não é nem o novo nem o antigo)
        fs::write(c.root.join("a.md"), b"da pessoa").unwrap();

        let r = recover_root(&c.root).unwrap().unwrap();
        assert_eq!(fs::read(c.root.join("a.md")).unwrap(), b"da pessoa");
        assert_eq!(r.kept, vec![kept_at(&c.root, b"A0")]);
        assert_eq!(fs::read(kept_at(&c.root, b"A0")).unwrap(), b"A0");
        // o resto voltou
        assert_eq!(fs::read(c.root.join("sub/b.md")).unwrap(), b"B0");
        assert_eq!(fs::read(c.root.join("c.md")).unwrap(), b"C0");
        assert!(!c.root.join("d.md").exists());
    }

    #[test]
    fn a_file_the_person_recreated_while_the_original_was_aside_is_not_overwritten() {
        let c = prepared();
        let mut log = Vec::new();
        commit(&mut log, &c.planned[..3]).unwrap(); // c.md (remoção) já de lado
        drop(log);
        fs::write(c.root.join("c.md"), b"recriado pela pessoa").unwrap();

        let r = recover_root(&c.root).unwrap().unwrap();
        assert_eq!(
            fs::read(c.root.join("c.md")).unwrap(),
            b"recriado pela pessoa"
        );
        assert_eq!(r.kept, vec![kept_at(&c.root, b"C0")]);
        assert_eq!(fs::read(kept_at(&c.root, b"C0")).unwrap(), b"C0");
    }

    #[test]
    fn a_tampered_original_aside_is_not_put_back() {
        let c = prepared();
        let mut log = Vec::new();
        commit(&mut log, &c.planned[..1]).unwrap();
        drop(log);
        fs::write(c.root.join(".arca-old-0"), b"adulterado").unwrap();

        let r = recover_root(&c.root).unwrap().unwrap();
        assert_eq!(r.kept, vec![kept_at(&c.root, b"adulterado")]);
        assert_eq!(
            fs::read(kept_at(&c.root, b"adulterado")).unwrap(),
            b"adulterado"
        );
        assert_eq!(fs::read(c.root.join("a.md")).unwrap(), b"A1"); // o novo fica, não há o que pôr
    }

    #[cfg(unix)]
    #[test]
    fn a_folder_swapped_for_a_symlink_after_the_crash_is_refused() {
        let c = prepared();
        let mut log = Vec::new();
        commit(&mut log, &c.planned).unwrap();
        drop(log);
        let outside = c.root.parent().unwrap().join("fora");
        fs::create_dir_all(&outside).unwrap();
        fs::write(outside.join("b.md"), b"B1").unwrap();
        fs::rename(
            c.root.join("sub"),
            c.root.parent().unwrap().join("sub-velha"),
        )
        .unwrap();
        std::os::unix::fs::symlink(&outside, c.root.join("sub")).unwrap();

        assert!(recover_root(&c.root).is_err());
        assert_eq!(fs::read(outside.join("b.md")).unwrap(), b"B1"); // nada escrito fora
        assert_eq!(fs::read_dir(&outside).unwrap().count(), 1);
    }

    /// P-29 item 1 (fatia 3): o mesmo, na fase `open` (a volta). Uma pasta virada link para um
    /// lugar que tem os nomes que a recuperação procura não pode ser tocada.
    #[cfg(unix)]
    #[test]
    fn a_folder_swapped_for_a_symlink_is_refused_by_the_rollback_too() {
        let c = prepared();
        let mut log = Vec::new();
        commit(&mut log, &c.planned).unwrap();
        drop(log);
        let outside = c.root.parent().unwrap().join("fora");
        fs::create_dir_all(&outside).unwrap();
        fs::write(outside.join("b.md"), b"B1").unwrap();
        fs::write(outside.join(".arca-old-1"), b"B0").unwrap();
        fs::rename(
            c.root.join("sub"),
            c.root.parent().unwrap().join("sub-velha"),
        )
        .unwrap();
        std::os::unix::fs::symlink(&outside, c.root.join("sub")).unwrap();

        assert!(recover_root(&c.root).is_err());
        assert_eq!(fs::read(outside.join("b.md")).unwrap(), b"B1");
        assert_eq!(fs::read(outside.join(".arca-old-1")).unwrap(), b"B0");
        assert_eq!(fs::read_dir(&outside).unwrap().count(), 2);
    }

    /// Um FIFO no lugar do destino não trava a recuperação (abrir um FIFO sem `O_NONBLOCK`
    /// esperaria um escritor para sempre): fica onde está e o original vai para `kept`.
    #[cfg(unix)]
    #[test]
    fn a_fifo_in_place_of_the_destination_does_not_block_the_recovery() {
        use rustix::fs::{mknodat, FileType, Mode, CWD};
        for phase in [Phase::Open, Phase::Committed] {
            let c = prepared();
            let mut log = Vec::new();
            commit(&mut log, &c.planned).unwrap();
            drop(log);
            if phase == Phase::Committed {
                let mut journal = c.journal.clone();
                journal.phase = Phase::Committed;
                journal.save(&c.root).unwrap();
            }
            fs::remove_file(c.root.join("a.md")).unwrap();
            mknodat(
                CWD,
                c.root.join("a.md"),
                FileType::Fifo,
                Mode::RUSR | Mode::WUSR,
                0,
            )
            .unwrap();

            let r = recover_root(&c.root).unwrap().unwrap();
            let kind = fs::symlink_metadata(c.root.join("a.md"))
                .unwrap()
                .file_type();
            assert!(
                !kind.is_file() && !kind.is_dir() && !kind.is_symlink(),
                "{phase:?}"
            );
            assert_eq!(fs::read(kept_at(&c.root, b"A0")).unwrap(), b"A0");
            assert!(r.kept.contains(&kept_at(&c.root, b"A0")), "{phase:?}");
        }
    }

    // ---------- auditoria do PR 3c ----------

    /// Achado 1: o marcador `committed` foi gravado, mas a volta em memória (erro em outra raiz)
    /// foi interrompida entre tirar o novo e devolver o original. O diário diz `committed`, o
    /// destino não está no estado novo: o original de lado NÃO pode ser apagado.
    #[test]
    fn committed_never_deletes_an_original_when_the_new_file_is_not_in_place() {
        let c = prepared();
        let mut log = Vec::new();
        commit(&mut log, &c.planned).unwrap();
        drop(log);
        let mut journal = c.journal.clone();
        journal.phase = Phase::Committed;
        journal.save(&c.root).unwrap();
        // a volta começou: a.md (novo) saiu, o original ainda está em .arca-old-0
        fs::remove_file(c.root.join("a.md")).unwrap();
        // e a pessoa recriou c.md (que a troca tinha removido) com outro conteúdo
        fs::write(c.root.join("c.md"), b"recriado").unwrap();

        let r = recover_root(&c.root).unwrap().unwrap();
        assert_eq!(fs::read(kept_at(&c.root, b"A0")).unwrap(), b"A0");
        assert_eq!(fs::read(kept_at(&c.root, b"C0")).unwrap(), b"C0");
        assert_eq!(fs::read(c.root.join("c.md")).unwrap(), b"recriado");
        assert!(r.kept.contains(&kept_at(&c.root, b"A0")));
        assert!(r.kept.contains(&kept_at(&c.root, b"C0")));
        // o que está em ordem foi terminado: sub/b.md já é o novo, então o original sai
        assert!(!c.root.join("sub/.arca-old-1").exists());
        assert_eq!(fs::read(c.root.join("sub/b.md")).unwrap(), b"B1");
    }

    /// Achado 2: o que o `save` grava o `load` tem de conseguir ler, mesmo num apply grande.
    #[test]
    fn a_journal_for_thousands_of_files_loads_and_one_too_big_to_load_is_not_saved() {
        let dir = TempDir::new();
        let entry = |i: usize, path: String| Entry {
            n: i,
            kind: Kind::Update,
            path,
            from_sha: Some(sha(b"a")),
            to_sha: Some(sha(b"b")),
        };
        let path = |i: usize| format!("notas/{}/{i}.md", "x".repeat(180));
        let big = Journal {
            version: 1,
            phase: Phase::Open,
            entries: (0..8000).map(|i| entry(i, path(i))).collect(),
            apply_id: None,
            peers: None,
            dirs: vec![],
        };
        big.save(dir.path()).unwrap();
        assert_eq!(Journal::load(dir.path()), Ok(Some(big)));

        // acima do teto mesmo (P-33 item 8): 20 mil arquivos com o caminho mais longo que o
        // manifest aceita (~4 KiB) passam de 64 MiB
        let long = |i: usize| format!("{}/{i}.md", vec!["x".repeat(250); 15].join("/"));
        let huge = Journal {
            version: 1,
            phase: Phase::Open,
            entries: (0..20_000).map(|i| entry(i, long(i))).collect(),
            apply_id: None,
            peers: None,
            dirs: vec![],
        };
        let other = TempDir::new();
        assert_eq!(huge.save(other.path()), Err(EngineError::JournalTooLarge));
        assert_eq!(huge.create(other.path()), Err(EngineError::JournalTooLarge));
        assert!(!Journal::exists(other.path()));
    }

    /// P-33 item 8: o manifest aceita até `MAX_FILES` arquivos, então o diário de um `apply` com
    /// todos eles (caminho típico) tem de caber. Com 16 MiB (uns 60 mil arquivos) não cabia.
    #[test]
    fn a_journal_for_a_manifest_at_the_file_limit_fits_and_loads_back() {
        let dir = TempDir::new();
        let journal = Journal {
            version: 1,
            phase: Phase::Open,
            entries: (0..crate::manifest::MAX_FILES)
                .map(|i| Entry {
                    n: i,
                    kind: Kind::Update,
                    path: format!("notas/projeto-{:04}/arquivo-{i:07}.md", i % 1000),
                    from_sha: Some(sha(b"a")),
                    to_sha: Some(sha(b"b")),
                })
                .collect(),
            apply_id: None,
            peers: None,
            dirs: vec![],
        };
        journal.save(dir.path()).unwrap();
        assert!(fs::metadata(Journal::path(dir.path())).unwrap().len() > 16 << 20);
        assert_eq!(Journal::load(dir.path()), Ok(Some(journal)));
    }

    /// Achado 6: o mesmo caminho repetido faria a recuperação hashear o mesmo arquivo N vezes.
    #[test]
    fn a_journal_that_repeats_a_path_is_corrupt() {
        let c = prepared();
        let text = fs::read_to_string(Journal::path(&c.root)).unwrap();
        fs::write(
            Journal::path(&c.root),
            text.replacen("\"sub/b.md\"", "\"a.md\"", 1),
        )
        .unwrap();
        assert_eq!(recover_root(&c.root), Err(EngineError::CorruptJournal));
    }

    /// Achado 7: o que a recuperação não devolve não pode bloquear o `apply` seguinte (o nome
    /// `.arca-old-<n>` colidia) nem ser lido pelo snapshot (um segredo antigo seria sincronizado).
    #[test]
    fn what_is_kept_does_not_block_the_next_apply_and_is_not_a_vault_file() {
        let c = prepared();
        let mut log = Vec::new();
        commit(&mut log, &c.planned[..1]).unwrap();
        drop(log);
        fs::write(c.root.join(".arca-old-0"), b"adulterado").unwrap();
        recover_root(&c.root).unwrap();
        assert!(is_reserved_name(
            kept_at(&c.root, b"adulterado")
                .file_name()
                .unwrap()
                .to_str()
                .unwrap()
        ));

        // a.md está em A1; um novo apply (de novo com n = 0) atualiza para A2
        let item = Item {
            kind: Kind::Update,
            root: c.root.clone(),
            dest: c.root.join("a.md"),
            from: Some(entry("a.md", b"A1")),
            to: Some(entry("a.md", b"A2")),
        };
        let (planned, _) = plan(vec![item]).unwrap();
        let mut log = Vec::new();
        stage(&mut log, &planned[0], b"A2").unwrap();
        commit(&mut log, &planned).unwrap();
        assert_eq!(fs::read(c.root.join("a.md")).unwrap(), b"A2");
    }

    /// Achado 3: a conferência do hash e a remoção não são atômicas; por isso se move primeiro e
    /// se confere o que foi movido. O que não for o esperado volta, sem sobrescrever.
    #[test]
    fn remove_if_ours_puts_back_what_turned_out_not_to_be_ours() {
        let dir = TempDir::new();
        let (dest, tmp) = (dir.path().join("a.md"), dir.path().join(".arca-new-0"));
        let mut kept = Vec::new();
        let folder = Folder::open_beneath(dir.path(), Path::new("")).unwrap();
        let slot = Slot {
            folder: &folder,
            dir: dir.path(),
        };
        let (name, scratch) = ("a.md", ".arca-new-0");

        fs::write(&dest, b"nosso").unwrap();
        assert!(remove_if_ours(&slot, name, scratch, Some(sha(b"nosso")), &mut kept).unwrap());
        assert!(!dest.exists() && !tmp.exists());

        // a pessoa salvou por cima entre a conferência e a remoção: o conteúdo é outro
        fs::write(&dest, b"edicao da pessoa").unwrap();
        assert!(!remove_if_ours(&slot, name, scratch, Some(sha(b"nosso")), &mut kept).unwrap());
        assert_eq!(fs::read(&dest).unwrap(), b"edicao da pessoa");
        assert!(!tmp.exists() && kept.is_empty());

        // e se o destino foi recriado enquanto o arquivo estava de lado, nada é sobrescrito
        fs::write(&dest, b"edicao da pessoa").unwrap();
        fs::write(&tmp, b"x").unwrap(); // tmp ocupado: não dá para mover para lá
        assert!(remove_if_ours(&slot, name, scratch, Some(sha(b"nosso")), &mut kept).is_err());
        assert_eq!(fs::read(&dest).unwrap(), b"edicao da pessoa");
        assert_eq!(fs::read(&tmp).unwrap(), b"x");
    }

    /// Achado 5: dois `apply` na mesma raiz não podem pisar no diário um do outro.
    #[test]
    fn creating_a_journal_never_overwrites_one_that_is_already_there() {
        let c = prepared();
        let theirs = fs::read(Journal::path(&c.root)).unwrap();
        let mine = Journal {
            version: 1,
            phase: Phase::Open,
            entries: vec![],
            apply_id: None,
            peers: None,
            dirs: vec![],
        };
        assert_eq!(mine.create(&c.root), Err(EngineError::Interrupted));
        assert_eq!(fs::read(Journal::path(&c.root)).unwrap(), theirs);

        let free = TempDir::new();
        mine.create(free.path()).unwrap();
        assert_eq!(Journal::load(free.path()), Ok(Some(mine)));
        // sem sobras do arquivo de trabalho
        assert_eq!(fs::read_dir(free.path()).unwrap().count(), 1);
    }

    /// P-33 item 3: o nome do diário precisa estar no disco antes de o `apply` mexer em qualquer
    /// arquivo, então `create` sincroniza a pasta da raiz (e só quando criou de fato).
    #[test]
    fn creating_a_journal_syncs_the_root_folder_only_when_it_created_it() {
        let journal = Journal {
            version: 1,
            phase: Phase::Open,
            entries: vec![],
            apply_id: None,
            peers: None,
            dirs: vec![],
        };
        let free = TempDir::new();
        let before = crate::dirfd::probe::syncs();
        journal.create(free.path()).unwrap();
        assert_eq!(crate::dirfd::probe::syncs() - before, 1);

        let before = crate::dirfd::probe::syncs();
        assert_eq!(journal.create(free.path()), Err(EngineError::Interrupted));
        assert_eq!(crate::dirfd::probe::syncs(), before);
    }

    // ---------- pastas criadas pelo `apply` (P-33 item 4) ----------

    fn journal_with_dirs(phase: Phase, dirs: &[&str]) -> Journal {
        Journal {
            version: 1,
            phase,
            entries: vec![],
            apply_id: None,
            peers: None,
            dirs: dirs.iter().map(|d| (*d).to_string()).collect(),
        }
    }

    #[test]
    fn recovering_an_open_journal_removes_the_empty_folders_it_lists_child_first() {
        let dir = TempDir::new();
        fs::create_dir_all(dir.path().join("a/b")).unwrap();
        fs::create_dir_all(dir.path().join("fica")).unwrap(); // não está no diário
        journal_with_dirs(Phase::Open, &["a", "a/b"])
            .save(dir.path())
            .unwrap();
        recover_root(dir.path()).unwrap();
        assert!(!dir.path().join("a").exists());
        assert!(dir.path().join("fica").is_dir());
    }

    #[test]
    fn a_listed_folder_with_a_file_of_the_person_stays_and_only_the_empty_one_goes() {
        let dir = TempDir::new();
        fs::create_dir_all(dir.path().join("a/b")).unwrap();
        fs::write(dir.path().join("a/da-pessoa.md"), b"meu").unwrap();
        journal_with_dirs(Phase::Open, &["a", "a/b"])
            .save(dir.path())
            .unwrap();
        recover_root(dir.path()).unwrap();
        assert!(!dir.path().join("a/b").exists());
        assert_eq!(fs::read(dir.path().join("a/da-pessoa.md")).unwrap(), b"meu");
    }

    #[test]
    fn a_committed_journal_keeps_its_listed_folders() {
        let dir = TempDir::new();
        fs::create_dir_all(dir.path().join("a/b")).unwrap();
        journal_with_dirs(Phase::Committed, &["a", "a/b"])
            .save(dir.path())
            .unwrap();
        recover_root(dir.path()).unwrap();
        assert!(dir.path().join("a/b").is_dir());
    }

    #[test]
    fn a_listed_path_through_a_link_touches_nothing_outside_the_root() {
        let dir = TempDir::new();
        let outside = TempDir::new();
        fs::create_dir_all(outside.path().join("x")).unwrap();
        std::os::unix::fs::symlink(outside.path(), dir.path().join("link")).unwrap();
        journal_with_dirs(Phase::Open, &["link/x"])
            .save(dir.path())
            .unwrap();
        recover_root(dir.path()).unwrap();
        assert!(outside.path().join("x").is_dir());
    }

    #[test]
    fn a_journal_with_bad_dirs_is_corrupt() {
        for dirs in [
            vec!["../fora"],
            vec!["/abs"],
            vec!["a", "a"],
            vec!["a/.arca-new-1"],
            vec![".arca-apply-journal"],
            vec![""],
        ] {
            let dir = TempDir::new();
            journal_with_dirs(Phase::Open, &dirs)
                .save(dir.path())
                .unwrap();
            assert_eq!(
                Journal::load(dir.path()),
                Err(EngineError::CorruptJournal),
                "{dirs:?}"
            );
        }
    }

    /// Um diário gravado antes do campo existir continua valendo.
    #[test]
    fn a_journal_written_before_dirs_existed_still_loads() {
        let dir = TempDir::new();
        let old = br#"{"version":1,"phase":"open","entries":[]}"#;
        let path = Journal::path(dir.path());
        fs::write(&path, old).unwrap();
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).unwrap();
        }
        let loaded = Journal::load(dir.path()).unwrap().unwrap();
        assert!(loaded.dirs.is_empty());
    }

    // ---------- idempotência ----------

    #[test]
    fn recover_twice_is_the_same_as_once() {
        let c = prepared();
        let mut log = Vec::new();
        commit(&mut log, &c.planned[..2]).unwrap();
        drop(log);
        assert!(recover_root(&c.root).unwrap().is_some());
        let after = tree(&c.root);
        assert_eq!(after, original());
        assert_eq!(recover_root(&c.root), Ok(None));
        assert_eq!(tree(&c.root), after);
    }

    // ---------- um `apply` de várias raízes (P-33 item 2) ----------

    struct Pair {
        _dir: TempDir,
        a: PathBuf,
        b: PathBuf,
    }

    /// Duas raízes, um `apply` que atualiza `x.md` em `a` e `y.md` em `b`, parado depois do
    /// `commit` e antes de qualquer marcador `committed`: os dois diários `open`, mesmo id.
    fn pair() -> Pair {
        let dir = TempDir::new();
        let (a, b) = (dir.path().join("a"), dir.path().join("b"));
        for (root, rel, data) in [(&a, "x.md", b"X0"), (&b, "y.md", b"Y0")] {
            fs::create_dir_all(root).unwrap();
            fs::write(root.join(rel), data).unwrap();
        }
        let item = |root: &PathBuf, rel: &str, from: &[u8], to: &[u8]| Item {
            kind: Kind::Update,
            root: root.clone(),
            dest: root.join(rel),
            from: Some(entry(rel, from)),
            to: Some(entry(rel, to)),
        };
        let (planned, _) = plan(vec![
            item(&a, "x.md", b"X0", b"X1"),
            item(&b, "y.md", b"Y0", b"Y1"),
        ])
        .unwrap();
        let group = ApplyGroup::new(2).unwrap();
        for root in [&a, &b] {
            Journal::open(root, &planned)
                .in_apply(&group)
                .save(root)
                .unwrap();
        }
        let mut log = Vec::new();
        for (p, new) in planned.iter().zip([&b"X1"[..], b"Y1"]) {
            stage(&mut log, p, new).unwrap();
        }
        commit(&mut log, &planned).unwrap();
        drop(log);
        Pair { _dir: dir, a, b }
    }

    fn mark_committed_in(root: &Path) {
        let mut journal = Journal::load(root).unwrap().unwrap();
        journal.phase = Phase::Committed;
        journal.save(root).unwrap();
    }

    fn contents(p: &Pair) -> (Vec<u8>, Vec<u8>) {
        (
            fs::read(p.a.join("x.md")).unwrap(),
            fs::read(p.b.join("y.md")).unwrap(),
        )
    }

    #[test]
    fn two_open_journals_of_the_same_apply_both_go_back() {
        let p = pair();
        let done = recover_all(&[p.a.clone(), p.b.clone()]).unwrap();
        assert_eq!(done.rolled_back, 2);
        assert_eq!(contents(&p), (b"X0".to_vec(), b"Y0".to_vec()));
        assert!(!Journal::exists(&p.a) && !Journal::exists(&p.b));
    }

    /// O problema do item 2: uma queda entre os dois marcadores. Quem tem o marcador prova que
    /// todos os arquivos de todas as raízes já foram trocados, então a outra termina também.
    #[test]
    fn a_committed_journal_in_one_root_makes_the_whole_apply_go_forward() {
        for committed_first in [true, false] {
            let p = pair();
            mark_committed_in(if committed_first { &p.a } else { &p.b });
            let done = recover_all(&[p.a.clone(), p.b.clone()]).unwrap();
            assert_eq!((done.completed, done.rolled_back), (2, 0));
            assert_eq!(contents(&p), (b"X1".to_vec(), b"Y1".to_vec()));
            assert!(!p.a.join(".arca-old-0").exists() && !p.b.join(".arca-old-1").exists());
            assert!(!Journal::exists(&p.a) && !Journal::exists(&p.b));
        }
    }

    /// Falta uma raiz do `apply` e as que aparecem estão `open`: a que falta pode ter o
    /// marcador, então nada é tocado (e o diário fica para quando a raiz aparecer).
    #[test]
    fn a_missing_root_with_only_open_journals_refuses_and_touches_nothing() {
        let p = pair();
        let before = (tree(&p.a), tree(&p.b));
        assert_eq!(
            recover_all(std::slice::from_ref(&p.b)),
            Err(EngineError::IncompleteApply)
        );
        assert_eq!((tree(&p.a), tree(&p.b)), before);
        assert!(Journal::exists(&p.a) && Journal::exists(&p.b));
        // com as duas, a recuperação segue normalmente
        assert!(recover_all(&[p.a.clone(), p.b.clone()]).is_ok());
        assert_eq!(contents(&p), (b"X0".to_vec(), b"Y0".to_vec()));
    }

    /// Com o marcador à vista, uma raiz ausente não impede terminar a que aparece.
    #[test]
    fn a_missing_root_is_fine_when_a_visible_journal_is_committed() {
        let p = pair();
        mark_committed_in(&p.b);
        let done = recover_all(std::slice::from_ref(&p.b)).unwrap();
        assert_eq!((done.completed, done.rolled_back), (1, 0));
        assert_eq!(fs::read(p.b.join("y.md")).unwrap(), b"Y1");
        assert!(!Journal::exists(&p.b) && Journal::exists(&p.a));
    }

    /// Um diário ilegível em qualquer raiz recusa tudo antes de tocar em qualquer uma.
    #[test]
    fn an_unreadable_journal_in_the_second_root_leaves_the_first_untouched() {
        let p = pair();
        fs::write(Journal::path(&p.b), b"lixo").unwrap();
        let before = (tree(&p.a), tree(&p.b));
        assert_eq!(
            recover_all(&[p.a.clone(), p.b.clone()]),
            Err(EngineError::CorruptJournal)
        );
        assert_eq!((tree(&p.a), tree(&p.b)), before);
    }

    #[test]
    fn journals_that_disagree_about_the_apply_are_corrupt_and_nothing_is_touched() {
        // mais diários do que raízes declaradas
        let p = pair();
        for root in [&p.a, &p.b] {
            let mut journal = Journal::load(root).unwrap().unwrap();
            journal.peers = Some(1);
            journal.save(root).unwrap();
        }
        let before = (tree(&p.a), tree(&p.b));
        assert_eq!(
            recover_all(&[p.a.clone(), p.b.clone()]),
            Err(EngineError::CorruptJournal)
        );
        assert_eq!((tree(&p.a), tree(&p.b)), before);
        // números de raízes diferentes para o mesmo id
        let p = pair();
        let mut journal = Journal::load(&p.a).unwrap().unwrap();
        journal.peers = Some(3);
        journal.save(&p.a).unwrap();
        assert_eq!(
            recover_all(&[p.a.clone(), p.b.clone()]),
            Err(EngineError::CorruptJournal)
        );
    }

    /// Um diário sem `apply_id` (gravado antes disto) vale como um `apply` de uma raiz só.
    #[test]
    fn a_journal_without_an_apply_id_is_recovered_on_its_own_marker() {
        let c = prepared();
        let mut log = Vec::new();
        commit(&mut log, &c.planned).unwrap();
        drop(log);
        let raw = fs::read_to_string(Journal::path(&c.root)).unwrap();
        assert!(!raw.contains("apply_id") && !raw.contains("peers"), "{raw}");
        let done = recover_all(std::slice::from_ref(&c.root)).unwrap();
        assert_eq!(done.rolled_back, 4);
        assert_eq!(tree(&c.root), original());
    }

    #[test]
    fn a_malformed_apply_id_or_a_half_group_is_corrupt() {
        let good = "0123456789abcdef0123456789abcdef";
        for (id, peers) in [
            (Some("curto"), Some(2)),
            (Some("0123456789ABCDEF0123456789ABCDEF"), Some(2)),
            (Some("zzzz456789abcdef0123456789abcdef"), Some(2)),
            (Some(good), Some(0)),
            (Some(good), None),
            (None, Some(2)),
        ] {
            let c = prepared();
            let mut journal = c.journal.clone();
            journal.apply_id = id.map(str::to_string);
            journal.peers = peers;
            journal.save(&c.root).unwrap();
            assert_eq!(
                Journal::load(&c.root),
                Err(EngineError::CorruptJournal),
                "{id:?} {peers:?}"
            );
        }
        let c = prepared();
        let mut journal = c.journal.clone();
        journal.apply_id = Some(good.to_string());
        journal.peers = Some(2);
        journal.save(&c.root).unwrap();
        assert!(Journal::load(&c.root).unwrap().is_some());
    }
}
