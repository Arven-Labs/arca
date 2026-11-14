//! Operações de disco do `Engine::apply` (Fase 2a, PR 3): conferir, preparar, trocar e
//! desfazer. Tudo aqui é bloqueante e roda fora do executor; quem orquestra é o engine.
//!
//! Três fases, tudo ou nada:
//! 1. `plan`: confere, sem escrever, que cada arquivo do diff ainda está como o estado de
//!    origem diz (e que nenhum caminho passa por symlink);
//! 2. `stage`: grava o conteúdo novo ao lado do destino (`.arca-new-<n>`), criando as pastas;
//! 3. `commit`: move o original para o lado (`.arca-old-<n>`), relê-o para provar que era o
//!    esperado e põe o novo no lugar. Qualquer falha desfaz o diário (`Undo`) ao contrário.

use crate::dirfd::{Folder, OpenError};
use crate::disk::create_private_dir;
use crate::engine::EngineError;
use crate::manifest::{FileEntry, Hash32};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::fs;
use std::io::{self, Read};
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub(crate) enum Kind {
    Create,
    Update,
    Remove,
}

/// Uma diferença entre o estado de origem e o de destino, ainda sem olhar o disco.
pub(crate) struct Item {
    pub kind: Kind,
    pub root: PathBuf,
    pub dest: PathBuf,
    pub from: Option<FileEntry>,
    pub to: Option<FileEntry>,
}

/// Uma operação que passou na conferência e vai ser executada.
#[derive(Clone)]
pub(crate) struct Planned {
    pub n: usize,
    pub kind: Kind,
    pub root: PathBuf,
    pub dest: PathBuf,
    /// `sha256` que o arquivo tem de ter no disco antes de ser trocado ou removido.
    pub from_sha: Option<Hash32>,
    pub to: Option<FileEntry>,
}

/// O que desfazer, em ordem de criação; o desfazer percorre ao contrário. Cada passo leva a
/// `root` da classe: o desfazer reabre a pasta-pai descendo dela, sem seguir link (P-29 item 1).
pub(crate) enum Undo {
    RemoveFile {
        root: PathBuf,
        path: PathBuf,
    },
    RemoveDir {
        root: PathBuf,
        path: PathBuf,
    },
    RenameBack {
        root: PathBuf,
        aside: PathBuf,
        orig: PathBuf,
    },
}

fn io_error(e: io::Error) -> EngineError {
    EngineError::Io(e.kind())
}

fn parent_of(path: &Path) -> Result<&Path, EngineError> {
    path.parent()
        .ok_or(EngineError::Io(io::ErrorKind::InvalidInput))
}

pub(crate) fn new_name(n: usize) -> String {
    format!(".arca-new-{n}")
}

pub(crate) fn old_name(n: usize) -> String {
    format!(".arca-old-{n}")
}

/// `sha256` do arquivo, lido em blocos.
fn hash_file(path: &Path) -> io::Result<Hash32> {
    let mut file = fs::File::open(path)?;
    let mut hasher = Sha256::new();
    let mut buffer = [0u8; 64 * 1024];
    loop {
        let read = file.read(&mut buffer)?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
    }
    Ok(Hash32(hasher.finalize().into()))
}

/// `None` se o caminho não existe; `LocalChanges` se é symlink ou algo que não é arquivo
/// regular (nunca seguimos link); senão o `sha256` do conteúdo.
pub(crate) fn current_sha(path: &Path) -> Result<Option<Hash32>, EngineError> {
    match fs::symlink_metadata(path) {
        Ok(meta) if meta.file_type().is_file() => hash_file(path).map(Some).map_err(io_error),
        Ok(_) => Err(EngineError::LocalChanges),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(io_error(e)),
    }
}

/// Abre a pasta-pai de `dest` descendo de `root` sem seguir link (P-29 item 1): quem confere e
/// quem age usam o mesmo `Folder`, então uma troca por link entre os dois não muda onde se age.
/// `None` se a raiz ou uma pasta do caminho não existe (nada abaixo existe); `LocalChanges` se
/// um componente é link ou não é pasta.
pub(crate) fn open_parent(root: &Path, dest: &Path) -> Result<Option<Folder>, EngineError> {
    let relative = dest
        .strip_prefix(root)
        .map_err(|_| EngineError::Io(io::ErrorKind::InvalidInput))?;
    let parent = relative
        .parent()
        .ok_or(EngineError::Io(io::ErrorKind::InvalidInput))?;
    match Folder::open_beneath(root, parent) {
        Ok(folder) => Ok(Some(folder)),
        Err(OpenError::NotAFolder) => Err(EngineError::LocalChanges),
        Err(OpenError::Io(e)) if e.kind() == io::ErrorKind::NotFound => Ok(None),
        // a raiz existe mas não é pasta
        Err(OpenError::Io(e)) if e.kind() == io::ErrorKind::NotADirectory => {
            Err(EngineError::LocalChanges)
        }
        Err(OpenError::Io(e)) => Err(io_error(e)),
    }
}

/// O nome do arquivo de `dest` (vem de um `path` do manifest já validado, que é texto UTF-8).
pub(crate) fn name_of(dest: &Path) -> Result<&str, EngineError> {
    dest.file_name()
        .and_then(|n| n.to_str())
        .ok_or(EngineError::Io(io::ErrorKind::InvalidInput))
}

/// Como `current_sha`, mas para `name` dentro de `folder`: `None` se não existe; `LocalChanges`
/// se é link ou algo que não é arquivo regular.
pub(crate) fn sha_in(folder: &Folder, name: &str) -> Result<Option<Hash32>, EngineError> {
    match folder.sha256(name) {
        Ok(sha) => Ok(Some(sha)),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(e) if e.kind() == io::ErrorKind::InvalidInput => Err(EngineError::LocalChanges),
        Err(e) => Err(io_error(e)),
    }
}

/// `Ok` se `name` não existe em `folder` (nem como link); senão `AlreadyExists`.
pub(crate) fn absent_in(folder: &Folder, name: &str) -> Result<(), EngineError> {
    match folder.stat(name) {
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(()),
        Ok(_) => Err(EngineError::Io(io::ErrorKind::AlreadyExists)),
        Err(e) => Err(io_error(e)),
    }
}

/// Fase 1: confere o disco contra o estado de origem, sem escrever. Devolve o que executar e
/// quantas entradas já estavam no estado de destino (idempotência).
pub(crate) fn plan(items: Vec<Item>) -> Result<(Vec<Planned>, usize), EngineError> {
    let mut planned = Vec::new();
    let mut unchanged = 0;
    for item in items {
        let current = match open_parent(&item.root, &item.dest)? {
            Some(folder) => sha_in(&folder, name_of(&item.dest)?)?,
            None => None,
        };
        let from_sha = item.from.as_ref().map(|f| f.sha256);
        let to_sha = item.to.as_ref().map(|t| t.sha256);
        let run = match (item.kind, current) {
            (Kind::Create, None) => true,
            (Kind::Create, Some(sha)) if Some(sha) == to_sha => false,
            (Kind::Update, Some(sha)) if Some(sha) == from_sha => true,
            (Kind::Update, Some(sha)) if Some(sha) == to_sha => false,
            (Kind::Remove, None) => false,
            (Kind::Remove, Some(sha)) if Some(sha) == from_sha => true,
            _ => return Err(EngineError::LocalChanges),
        };
        if run {
            planned.push(Planned {
                n: planned.len(),
                kind: item.kind,
                root: item.root,
                dest: item.dest,
                from_sha,
                to: item.to,
            });
        } else {
            unchanged += 1;
        }
    }
    Ok((planned, unchanged))
}

/// Fase 2: cria as pastas que faltam (uma a uma, no diário) e grava o conteúdo novo ao lado
/// do destino. Em caso de erro quem chama desfaz o diário.
pub(crate) fn stage(
    log: &mut Vec<Undo>,
    planned: &Planned,
    plain: &[u8],
) -> Result<(), EngineError> {
    // o caminho era seguro no `plan`, mas depois vieram o download e a decifragem dos blobs
    let relative = planned
        .dest
        .strip_prefix(&planned.root)
        .map_err(|_| EngineError::Io(io::ErrorKind::InvalidInput))?;
    let parent_relative = relative
        .parent()
        .ok_or(EngineError::Io(io::ErrorKind::InvalidInput))?;
    // a raiz que falta (e as de cima dela) é escolha de quem chama: criada por caminho, no diário
    let mut missing = Vec::new();
    let mut dir = Some(planned.root.as_path());
    while let Some(d) = dir {
        if fs::symlink_metadata(d).is_ok() {
            break;
        }
        missing.push(d.to_path_buf());
        dir = d.parent();
    }
    for d in missing.into_iter().rev() {
        create_private_dir(&d).map_err(io_error)?;
        log.push(Undo::RemoveDir {
            root: planned.root.clone(),
            path: d,
        });
    }
    // as pastas de dentro da raiz são criadas descendo sem seguir link; cada uma entra no diário
    // mesmo que uma seguinte falhe
    let mut created = Vec::new();
    let folder = Folder::create_beneath(&planned.root, parent_relative, &mut created);
    for path in created {
        log.push(Undo::RemoveDir {
            root: planned.root.clone(),
            path,
        });
    }
    let folder = match folder {
        Ok(folder) => folder,
        Err(OpenError::NotAFolder) => return Err(EngineError::LocalChanges),
        Err(OpenError::Io(e)) if e.kind() == io::ErrorKind::NotADirectory => {
            return Err(EngineError::LocalChanges)
        }
        Err(OpenError::Io(e)) => return Err(io_error(e)),
    };
    let tmp = new_name(planned.n);
    absent_in(&folder, &tmp)?;
    folder.write_atomic(&tmp, plain).map_err(io_error)?;
    log.push(Undo::RemoveFile {
        root: planned.root.clone(),
        path: parent_of(&planned.dest)?.join(tmp),
    });
    Ok(())
}

/// Põe `tmp` em `name` sem sobrescrever: `link` falha se `name` apareceu no meio-tempo.
fn place(folder: &Folder, tmp: &str, name: &str) -> Result<(), EngineError> {
    match folder.link(tmp, name) {
        Ok(()) => {
            let _ = folder.unlink(tmp);
            Ok(())
        }
        Err(e) if e.kind() == io::ErrorKind::AlreadyExists => Err(EngineError::LocalChanges),
        // sistema de arquivos sem hard link: confere e renomeia
        Err(e) if e.kind() == io::ErrorKind::Unsupported => {
            absent_in(folder, name).map_err(|_| EngineError::LocalChanges)?;
            folder.rename(tmp, name).map_err(io_error)
        }
        Err(e) => Err(io_error(e)),
    }
}

/// Fase 3: troca. O original vai para `.arca-old-<n>` (nunca é apagado de cara) e é relido
/// para provar que era o esperado; o novo entra no lugar. Em caso de erro quem chama desfaz.
pub(crate) fn commit(log: &mut Vec<Undo>, planned: &[Planned]) -> Result<(), EngineError> {
    for p in planned {
        let parent = parent_of(&p.dest)?;
        let name = name_of(&p.dest)?;
        // a pasta é aberta uma vez por arquivo, sem seguir link, e conferir, mover e pôr no lugar
        // agem nela (P-29 item 1)
        let folder =
            open_parent(&p.root, &p.dest)?.ok_or(EngineError::Io(io::ErrorKind::NotFound))?;
        if matches!(p.kind, Kind::Update | Kind::Remove) {
            let aside = old_name(p.n);
            absent_in(&folder, &aside)?;
            folder.rename(name, &aside).map_err(io_error)?;
            log.push(Undo::RenameBack {
                root: p.root.clone(),
                aside: parent.join(&aside),
                orig: p.dest.clone(),
            });
            // segunda conferência: uma edição entre o `plan` e agora é detectada aqui
            if sha_in(&folder, &aside)? != p.from_sha {
                return Err(EngineError::LocalChanges);
            }
        }
        if matches!(p.kind, Kind::Create | Kind::Update) {
            place(&folder, &new_name(p.n), name)?;
            log.push(Undo::RemoveFile {
                root: p.root.clone(),
                path: p.dest.clone(),
            });
        }
    }
    Ok(())
}

/// Depois do sucesso: apaga os originais postos de lado e as pastas que as remoções
/// esvaziaram (nunca a raiz). Melhor esforço: o commit já valeu.
pub(crate) fn finish(planned: &[Planned]) {
    for p in planned {
        let Ok(Some(folder)) = open_parent(&p.root, &p.dest) else {
            continue;
        };
        if matches!(p.kind, Kind::Update | Kind::Remove) {
            let _ = folder.unlink(&old_name(p.n));
        }
        if p.kind == Kind::Remove {
            remove_emptied_folders(&p.root, &p.dest);
        }
    }
}

/// Remove as pastas que a remoção de `dest` esvaziou, de baixo para cima, nunca a raiz. Cada uma
/// é removida pela pasta-pai aberta sem seguir link, não por caminho.
pub(crate) fn remove_emptied_folders(root: &Path, dest: &Path) {
    let Ok(relative) = dest.strip_prefix(root) else {
        return;
    };
    let mut folder = relative.parent();
    while let Some(rel) = folder {
        let (Some(parent), Some(name)) = (rel.parent(), rel.file_name().and_then(|n| n.to_str()))
        else {
            break; // chegou na raiz
        };
        let removed = Folder::open_beneath(root, parent)
            .ok()
            .is_some_and(|parent_folder| parent_folder.rmdir(name).is_ok());
        if !removed {
            break;
        }
        folder = Some(parent);
    }
}

/// Nomes que o `apply` usa para temporários (`.arca-new-<n>`, `.arca-old-<n>`) e que o
/// `write_atomic` usa (`.tmp-<16 hex>`), mais o diário e o original que a recuperação não pôde
/// devolver. Não entram no snapshot e não podem ser destino. É pública para quem valida a pasta de
/// uma classe por conta própria (o Warden com os logins) não recusar os arquivos de trabalho do arca.
pub fn is_reserved_name(name: &str) -> bool {
    if name == crate::journal::JOURNAL_NAME {
        return true;
    }
    // original que a recuperação não pôde devolver (`.arca-kept-<16 hex>`)
    if let Some(hex) = name.strip_prefix(".arca-kept-") {
        return hex.len() == 16 && hex.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'));
    }
    if let Some(n) = name
        .strip_prefix(".arca-new-")
        .or_else(|| name.strip_prefix(".arca-old-"))
    {
        return !n.is_empty() && n.bytes().all(|b| b.is_ascii_digit());
    }
    if let Some(hex) = name.strip_prefix(".tmp-") {
        // o `write_atomic` sufixa 8 bytes aleatórios em hex minúsculo
        return hex.len() == 16 && hex.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'));
    }
    false
}

/// Desfaz o diário ao contrário, sem parar em erro. Um original que não pôde voltar (o destino
/// foi recriado no meio-tempo, ou a pasta não deixa) **nunca sobrescreve nada**: fica onde
/// está (`.arca-old-<n>`) e o caminho dele é devolvido.
pub(crate) fn undo_all(log: Vec<Undo>) -> Vec<PathBuf> {
    let mut kept = Vec::new();
    for step in log.into_iter().rev() {
        match step {
            Undo::RemoveFile { root, path } => {
                if let (Ok(Some(folder)), Ok(name)) = (open_parent(&root, &path), name_of(&path)) {
                    let _ = folder.unlink(name);
                }
            }
            Undo::RemoveDir { root, path } => {
                // uma pasta de dentro da raiz sai pela pasta-pai aberta sem seguir link; a raiz
                // (e as de cima, que foram criadas por caminho) saem por caminho
                match (open_parent(&root, &path), name_of(&path)) {
                    (Ok(Some(folder)), Ok(name)) if path != root => {
                        let _ = folder.rmdir(name);
                    }
                    _ => {
                        let _ = fs::remove_dir(&path);
                    }
                }
            }
            Undo::RenameBack { root, aside, orig } => {
                // `link` falha se `orig` existe; `rename` o sobrescreveria. Se a pasta não abre
                // sem link, o original fica de lado e é devolvido em `kept`.
                let restored = match (open_parent(&root, &orig), name_of(&aside), name_of(&orig)) {
                    (Ok(Some(folder)), Ok(aside_name), Ok(orig_name)) => {
                        match folder.link(aside_name, orig_name) {
                            Ok(()) => {
                                let _ = folder.unlink(aside_name);
                                true
                            }
                            Err(e) if e.kind() == io::ErrorKind::Unsupported => {
                                absent_in(&folder, orig_name).is_ok()
                                    && folder.rename(aside_name, orig_name).is_ok()
                            }
                            Err(_) => false,
                        }
                    }
                    _ => false,
                };
                if !restored {
                    kept.push(aside);
                }
            }
        }
    }
    kept
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::TempDir;

    fn sha(data: &[u8]) -> Hash32 {
        Hash32(Sha256::digest(data).into())
    }

    fn planned(kind: Kind, root: &Path, name: &str, from: Option<&[u8]>) -> Planned {
        Planned {
            n: 0,
            kind,
            root: root.to_path_buf(),
            dest: root.join(name),
            from_sha: from.map(sha),
            to: None,
        }
    }

    /// A segunda conferência: o arquivo foi editado **depois** do `plan` e **antes** do
    /// `commit`. O commit percebe ao reler o original que moveu, recusa, e o desfazer devolve
    /// a edição ao lugar sem deixar sobras.
    #[test]
    fn an_edit_between_plan_and_commit_is_detected_and_undone() {
        for kind in [Kind::Update, Kind::Remove] {
            let dir = TempDir::new();
            let root = dir.path();
            fs::write(root.join("a.md"), b"original").unwrap();
            let p = planned(kind, root, "a.md", Some(b"original"));

            // o que o `plan` viu era "original"; agora a pessoa editou
            fs::write(root.join("a.md"), b"editado agora").unwrap();

            let mut log = Vec::new();
            if kind == Kind::Update {
                stage(&mut log, &p, b"novo").unwrap();
            }
            let result = commit(&mut log, std::slice::from_ref(&p));
            assert_eq!(result, Err(EngineError::LocalChanges), "{kind:?}");
            undo_all(log);

            assert_eq!(fs::read(root.join("a.md")).unwrap(), b"editado agora");
            let names: Vec<_> = fs::read_dir(root)
                .unwrap()
                .map(|e| e.unwrap().file_name())
                .collect();
            assert_eq!(names.len(), 1, "sobras: {names:?}");
        }
    }

    /// Sem edição nenhuma, o mesmo caminho dá certo (o teste acima não passa por acaso).
    #[test]
    fn the_same_path_succeeds_when_nothing_was_edited() {
        let dir = TempDir::new();
        let root = dir.path();
        fs::write(root.join("a.md"), b"original").unwrap();
        let p = planned(Kind::Update, root, "a.md", Some(b"original"));
        let mut log = Vec::new();
        stage(&mut log, &p, b"novo").unwrap();
        commit(&mut log, std::slice::from_ref(&p)).unwrap();
        finish(std::slice::from_ref(&p));
        assert_eq!(fs::read(root.join("a.md")).unwrap(), b"novo");
        assert_eq!(fs::read_dir(root).unwrap().count(), 1);
    }

    // ---------- auditoria: desfazer sem sobrescrever (I-1) ----------

    /// A pessoa recriou `a.md` (um editor que salva por rename, por exemplo) enquanto o original
    /// estava de lado. O desfazer **não** pode trocar o arquivo dela pelo original.
    #[test]
    fn undo_never_overwrites_a_file_the_person_recreated() {
        let dir = TempDir::new();
        let root = dir.path();
        fs::write(root.join(".arca-old-0"), b"original").unwrap();
        fs::write(root.join("a.md"), b"da pessoa").unwrap();
        let kept = undo_all(vec![Undo::RenameBack {
            root: root.to_path_buf(),
            aside: root.join(".arca-old-0"),
            orig: root.join("a.md"),
        }]);
        assert_eq!(kept, [root.join(".arca-old-0")]);
        assert_eq!(fs::read(root.join("a.md")).unwrap(), b"da pessoa");
        assert_eq!(fs::read(root.join(".arca-old-0")).unwrap(), b"original");
    }

    #[test]
    fn undo_puts_the_original_back_when_the_destination_is_free() {
        let dir = TempDir::new();
        let root = dir.path();
        fs::write(root.join(".arca-old-0"), b"original").unwrap();
        let kept = undo_all(vec![Undo::RenameBack {
            root: root.to_path_buf(),
            aside: root.join(".arca-old-0"),
            orig: root.join("a.md"),
        }]);
        assert!(kept.is_empty());
        assert_eq!(fs::read(root.join("a.md")).unwrap(), b"original");
        assert_eq!(fs::read_dir(root).unwrap().count(), 1);
    }

    // ---------- P-29 (1): o desfazer e o `plan` não seguem link trocado ----------

    /// A pasta do original foi trocada por um link depois de o `commit` pôr o original de lado:
    /// o desfazer não pode "devolvê-lo" para dentro da pasta de fora. O original fica de lado,
    /// na pasta de verdade, e vai para `kept`.
    #[cfg(unix)]
    #[test]
    fn undo_does_not_restore_through_a_folder_swapped_for_a_symlink() {
        let dir = TempDir::new();
        let root = dir.path().join("raiz");
        let outside = dir.path().join("fora");
        fs::create_dir_all(root.join("sub")).unwrap();
        fs::create_dir_all(&outside).unwrap();
        fs::write(root.join("sub/.arca-old-0"), b"original").unwrap();
        fs::write(outside.join(".arca-old-0"), b"da pessoa").unwrap();
        let log = vec![Undo::RenameBack {
            root: root.clone(),
            aside: root.join("sub/.arca-old-0"),
            orig: root.join("sub/a.md"),
        }];

        fs::rename(root.join("sub"), dir.path().join("sub-de-verdade")).unwrap();
        std::os::unix::fs::symlink(&outside, root.join("sub")).unwrap();
        let kept = undo_all(log);

        assert_eq!(kept, [root.join("sub/.arca-old-0")]);
        assert!(!outside.join("a.md").exists());
        assert_eq!(fs::read(outside.join(".arca-old-0")).unwrap(), b"da pessoa");
        assert_eq!(
            fs::read(dir.path().join("sub-de-verdade/.arca-old-0")).unwrap(),
            b"original"
        );
    }

    /// Um FIFO onde o arquivo deveria estar não trava o `plan`: abrir um FIFO sem `O_NONBLOCK`
    /// esperaria um escritor para sempre.
    #[cfg(unix)]
    #[test]
    fn plan_refuses_a_fifo_where_the_file_should_be_instead_of_blocking() {
        use rustix::fs::{mknodat, FileType, Mode, CWD};
        let dir = TempDir::new();
        let root = dir.path();
        mknodat(
            CWD,
            root.join("a.md"),
            FileType::Fifo,
            Mode::RUSR | Mode::WUSR,
            0,
        )
        .unwrap();
        let item = Item {
            kind: Kind::Update,
            root: root.to_path_buf(),
            dest: root.join("a.md"),
            from: None,
            to: None,
        };
        assert!(matches!(plan(vec![item]), Err(EngineError::LocalChanges)));
    }

    /// Criar e remover dentro de subpastas: as pastas que a remoção esvazia saem, a raiz não.
    #[test]
    fn finish_removes_the_folders_a_removal_emptied_but_never_the_root() {
        let dir = TempDir::new();
        let root = dir.path().join("raiz");
        fs::create_dir_all(root.join("a/b")).unwrap();
        fs::write(root.join("a/b/c.md"), b"x").unwrap();
        fs::write(root.join("a/fica.md"), b"y").unwrap();
        let p = planned(Kind::Remove, &root, "a/b/c.md", Some(b"x"));
        let mut log = Vec::new();
        commit(&mut log, std::slice::from_ref(&p)).unwrap();
        finish(std::slice::from_ref(&p));
        assert!(!root.join("a/b").exists(), "a pasta esvaziada sai");
        assert!(
            root.join("a/fica.md").exists(),
            "a que ainda tem arquivo fica"
        );
        assert!(root.exists());
    }

    // ---------- auditoria: symlink trocado depois do `plan` (I-5) ----------

    /// O caminho era seguro no `plan`; antes do `stage`, `sub` virou um link para fora.
    #[cfg(unix)]
    #[test]
    fn stage_refuses_a_folder_swapped_for_a_symlink_after_the_plan() {
        let dir = TempDir::new();
        let root = dir.path().join("raiz");
        let outside = dir.path().join("fora");
        fs::create_dir_all(&outside).unwrap();
        fs::create_dir_all(&root).unwrap();
        let p = planned(Kind::Create, &root, "sub/novo.md", None);
        std::os::unix::fs::symlink(&outside, root.join("sub")).unwrap();
        let mut log = Vec::new();
        assert_eq!(
            stage(&mut log, &p, b"do manifest"),
            Err(EngineError::LocalChanges)
        );
        assert_eq!(fs::read_dir(&outside).unwrap().count(), 0);
    }

    /// Idem entre o `stage` e o `commit`: nada é movido nem ligado através do link.
    #[cfg(unix)]
    #[test]
    fn commit_refuses_a_folder_swapped_for_a_symlink_after_the_stage() {
        let dir = TempDir::new();
        let root = dir.path().join("raiz");
        let outside = dir.path().join("fora");
        fs::create_dir_all(root.join("sub")).unwrap();
        fs::create_dir_all(&outside).unwrap();
        fs::write(outside.join("novo.md"), b"da pessoa").unwrap();
        let p = planned(Kind::Create, &root, "sub/novo.md", None);
        let mut log = Vec::new();
        stage(&mut log, &p, b"do manifest").unwrap();

        fs::rename(root.join("sub"), dir.path().join("sub-de-verdade")).unwrap();
        std::os::unix::fs::symlink(&outside, root.join("sub")).unwrap();
        assert_eq!(
            commit(&mut log, std::slice::from_ref(&p)),
            Err(EngineError::LocalChanges)
        );
        assert_eq!(fs::read(outside.join("novo.md")).unwrap(), b"da pessoa");
        assert_eq!(fs::read_dir(&outside).unwrap().count(), 1);
    }

    // ---------- auditoria: nomes reservados (I-3 e I-4) ----------

    #[test]
    fn reserved_names_are_exactly_the_temporaries_arca_creates() {
        for name in [
            ".arca-new-0",
            ".arca-old-12",
            ".tmp-0123456789abcdef",
            ".tmp-ffffffffffffffff",
        ] {
            assert!(is_reserved_name(name), "{name}");
        }
        for name in [
            "notes.md",
            ".arca-new-",
            ".arca-new-x",
            ".arca-old",
            ".arca-old-1a",
            ".tmp-notes",
            ".tmp-0123456789ABCDEF", // o write_atomic só gera minúsculas
            ".tmp-0123456789abcde",  // 15 dígitos
            "a.arca-new-0",
            ".arca-new-0.md",
        ] {
            assert!(!is_reserved_name(name), "{name}");
        }
    }

    /// Alguém criou o destino entre o `plan` e o `commit`: nunca é sobrescrito.
    #[test]
    fn a_file_created_in_the_meantime_is_never_overwritten() {
        let dir = TempDir::new();
        let root = dir.path();
        let p = planned(Kind::Create, root, "novo.md", None);
        let mut log = Vec::new();
        stage(&mut log, &p, b"do manifest").unwrap();
        fs::write(root.join("novo.md"), b"da pessoa").unwrap();
        assert_eq!(
            commit(&mut log, std::slice::from_ref(&p)),
            Err(EngineError::LocalChanges)
        );
        undo_all(log);
        assert_eq!(fs::read(root.join("novo.md")).unwrap(), b"da pessoa");
        assert_eq!(fs::read_dir(root).unwrap().count(), 1);
    }
}
