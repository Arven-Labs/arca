//! Listar e limpar as cópias de perdedores de conflito das classes `config` e `secret`
//! (Fase 2a, P-28, ADR 0010). `Engine::save_conflict_losers` grava o texto claro do perdedor em
//! `dir/<classe>/<path>.conflito-<dispositivo>-<16 hex do sha256>`; aqui se vê o que existe e se
//! apaga o que o consumidor escolher.
//!
//! O `clear` **não confia no que o chamador mandou**: refaz tudo a partir do disco, e só apaga
//! um arquivo regular, dentro de `dir/config` ou `dir/secret`, com nome no padrão e conteúdo que
//! ainda tem o hash do nome. Uma cópia que a pessoa editou nunca é apagada.

use crate::apply::{current_sha, ensure_absent};
use crate::engine::EngineError;
use crate::manifest::{validate_device, validate_path, Class};
use crate::scan::MAX_FILE_BYTES;
use std::fs;
use std::io;
use std::path::{Component, Path, PathBuf};

/// Uma cópia de perdedor que existe em disco.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LoserCopy {
    pub class: Class,
    /// Caminho original do arquivo em conflito (o do manifest).
    pub path: String,
    /// Dispositivo cuja versão perdeu.
    pub device: String,
    /// Primeiros 8 bytes do `sha256` do texto claro, como no nome.
    pub sha_prefix: [u8; 8],
    /// Onde a cópia está.
    pub file: PathBuf,
    /// O conteúdo atual não bate mais com o hash do nome: a pessoa editou. Nunca é apagada.
    pub edited: bool,
}

/// Por que o `clear` não apagou um item.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Skip {
    /// A pessoa editou a cópia (o conteúdo não tem mais o hash do nome).
    Edited,
    /// Não é uma cópia que a arca gravou: fora de `dir/config` e `dir/secret`, nome fora do
    /// padrão, link, pasta, ou caminho com link no meio.
    NotACopy,
    /// Já não existe.
    Gone,
    /// Erro de E/S neste item (permissão, sistema de arquivos sem hard link...): os outros
    /// itens seguem. Se o conteúdo ficou num arquivo de trabalho, o caminho está em `kept`.
    Failed(io::ErrorKind),
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ClearReport {
    pub removed: usize,
    pub skipped: Vec<(PathBuf, Skip)>,
    /// Arquivos de trabalho (`.tmp-<hex>`) que guardam conteúdo que não pôde ser devolvido nem
    /// apagado. O `list` não os mostra: quem chama precisa avisar a pessoa. Pode ser um segredo.
    pub kept: Vec<PathBuf>,
}

/// Profundidade máxima de subpastas percorrida (o `path` do manifest tem teto de tamanho).
const MAX_DEPTH: usize = 64;

/// As duas pastas de classe onde `save_conflict_losers` grava.
const CLASS_DIRS: [(&str, Class); 2] = [("config", Class::Config), ("secret", Class::Secret)];

fn io_error(e: io::Error) -> EngineError {
    EngineError::Io(e.kind())
}

/// `<base>.conflito-<dispositivo>-<16 hex minúsculos>` -> (base, dispositivo, 8 bytes do hash).
fn parse_name(name: &str) -> Option<(&str, &str, [u8; 8])> {
    let split = name.len().checked_sub(16)?;
    // o sufixo é ASCII; um nome cujo corte cai no meio de um caractere não é uma cópia
    // (`split_at` entraria em pânico)
    if !name.is_char_boundary(split) {
        return None;
    }
    let (rest, hex) = name.split_at(split);
    let rest = rest.strip_suffix('-')?;
    let mut prefix = [0u8; 8];
    for (byte, pair) in prefix.iter_mut().zip(hex.as_bytes().chunks(2)) {
        let digit = |c: u8| match c {
            b'0'..=b'9' => Some(c - b'0'),
            b'a'..=b'f' => Some(c - b'a' + 10),
            _ => None,
        };
        *byte = (digit(pair[0])? << 4) | digit(pair[1])?;
    }
    let (base, device) = rest.rsplit_once(".conflito-")?;
    if base.is_empty() || validate_device(device).is_err() {
        return None;
    }
    Some((base, device, prefix))
}

/// Pastas de verdade, sem link, entre a pasta de conflitos e o arquivo (inclusive a da classe).
/// `Ok(false)` se algum componente é link ou não é pasta; `Ok(true)` com tudo em ordem;
/// `Err(NotFound)` se faltar uma pasta.
fn real_folders(dir: &Path, relative_parent: &Path) -> io::Result<bool> {
    let mut walk = dir.to_path_buf();
    for part in relative_parent.components() {
        walk.push(part);
        if !fs::symlink_metadata(&walk)?.file_type().is_dir() {
            return Ok(false);
        }
    }
    Ok(true)
}

fn collect(
    class_dir: &Path,
    class: Class,
    current: &Path,
    depth: usize,
    out: &mut Vec<LoserCopy>,
) -> Result<(), EngineError> {
    // o `path` do manifest tem teto de componentes; mais fundo que isso não é cópia da arca
    // (e uma árvore profunda de propósito estouraria a pilha)
    if depth > MAX_DEPTH {
        return Ok(());
    }
    let entries = match fs::read_dir(current) {
        Ok(entries) => entries,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(e) => return Err(io_error(e)),
    };
    for entry in entries {
        let entry = entry.map_err(io_error)?;
        let path = entry.path();
        // `DirEntry::file_type` não segue link: link e o que não é arquivo nem pasta são ignorados
        let kind = entry.file_type().map_err(io_error)?;
        if kind.is_dir() {
            collect(class_dir, class, &path, depth + 1, out)?;
            continue;
        }
        if !kind.is_file() {
            continue;
        }
        let Some(name) = path.file_name().and_then(|n| n.to_str()) else {
            continue;
        };
        let Some((base, device, prefix)) = parse_name(name) else {
            continue;
        };
        let Ok(relative) = path.strip_prefix(class_dir) else {
            continue;
        };
        // pasta com nome que não é UTF-8 não vira `path` de manifest: não é cópia da arca
        let Some(mut parts) = relative
            .parent()
            .into_iter()
            .flat_map(|p| p.iter())
            .map(|c| c.to_str())
            .collect::<Option<Vec<&str>>>()
        else {
            continue;
        };
        parts.push(base);
        let original = parts.join("/");
        if validate_path(&original).is_err() {
            continue;
        }
        // acima do teto da arca não é cópia dela: não se lê (seria só custo), conta como editada
        let edited = match fs::symlink_metadata(&path) {
            Ok(meta) if meta.len() > MAX_FILE_BYTES => true,
            // some entre o `read_dir` e a leitura: não está mais lá
            Ok(_) => match current_sha(&path)? {
                Some(sha) => sha.0[..8] != prefix,
                None => continue,
            },
            Err(e) if e.kind() == io::ErrorKind::NotFound => continue,
            Err(e) => return Err(io_error(e)),
        };
        out.push(LoserCopy {
            class,
            path: original,
            device: device.to_string(),
            sha_prefix: prefix,
            file: path,
            edited,
        });
    }
    Ok(())
}

/// As cópias que existem em `dir`, em ordem estável (por caminho do arquivo).
pub(crate) fn list(dir: &Path) -> Result<Vec<LoserCopy>, EngineError> {
    let mut out = Vec::new();
    for (name, class) in CLASS_DIRS {
        let class_dir = dir.join(name);
        // a pasta da classe tem de ser uma pasta de verdade (um link não é seguido)
        match fs::symlink_metadata(&class_dir) {
            Ok(meta) if meta.file_type().is_dir() => {}
            Ok(_) => continue,
            Err(e) if e.kind() == io::ErrorKind::NotFound => continue,
            Err(e) => return Err(io_error(e)),
        }
        collect(&class_dir, class, &class_dir, 0, &mut out)?;
    }
    out.sort_by(|a, b| a.file.cmp(&b.file));
    Ok(out)
}

/// Apaga as cópias de `copies` que ainda são o que a arca gravou.
pub(crate) fn clear(dir: &Path, copies: &[LoserCopy]) -> Result<ClearReport, EngineError> {
    let mut report = ClearReport::default();
    let mut tidy: Vec<(PathBuf, PathBuf)> = Vec::new(); // (pasta da classe, pasta do arquivo)
    for copy in copies {
        // tudo vem do disco e do nome; `class`, `sha_prefix` e `edited` do chamador não valem
        let file = &copy.file;
        let skip = |report: &mut ClearReport, why| report.skipped.push((file.clone(), why));
        let Ok(relative) = file.strip_prefix(dir) else {
            skip(&mut report, Skip::NotACopy);
            continue;
        };
        let parts: Vec<_> = relative.components().collect();
        let normal = parts.iter().all(|c| matches!(c, Component::Normal(_)));
        let class_dir = match parts.first() {
            Some(Component::Normal(first)) if parts.len() >= 2 && normal => {
                CLASS_DIRS.iter().find(|(name, _)| first == name)
            }
            _ => None,
        };
        let Some((class_name, _)) = class_dir else {
            skip(&mut report, Skip::NotACopy);
            continue;
        };
        let Some((base, _, prefix)) = file
            .file_name()
            .and_then(|n| n.to_str())
            .and_then(parse_name)
        else {
            skip(&mut report, Skip::NotACopy);
            continue;
        };
        // o mesmo critério do `list`: o que ele não mostraria, o `clear` também não apaga
        if validate_path(base).is_err() {
            skip(&mut report, Skip::NotACopy);
            continue;
        }
        // um erro de E/S num item não aborta o lote: os já apagados continuam no relatório
        let parent_relative = relative.parent().unwrap_or(Path::new(""));
        match real_folders(dir, parent_relative) {
            Ok(true) => {}
            Ok(false) => {
                skip(&mut report, Skip::NotACopy);
                continue;
            }
            Err(e) if e.kind() == io::ErrorKind::NotFound => {
                skip(&mut report, Skip::Gone);
                continue;
            }
            Err(e) => {
                skip(&mut report, Skip::Failed(e.kind()));
                continue;
            }
        }
        match fs::symlink_metadata(file) {
            Ok(meta) if meta.file_type().is_file() => {}
            Ok(_) => {
                skip(&mut report, Skip::NotACopy);
                continue;
            }
            Err(e) if e.kind() == io::ErrorKind::NotFound => {
                skip(&mut report, Skip::Gone);
                continue;
            }
            Err(e) => {
                skip(&mut report, Skip::Failed(e.kind()));
                continue;
            }
        }
        match remove_if_unchanged(file, prefix, &mut report.kept) {
            Ok(None) => {
                report.removed += 1;
                if let Some(parent) = file.parent() {
                    tidy.push((dir.join(class_name), parent.to_path_buf()));
                }
            }
            Ok(Some(why)) => skip(&mut report, why),
            Err(EngineError::Io(kind)) => skip(&mut report, Skip::Failed(kind)),
            Err(_) => skip(&mut report, Skip::Failed(io::ErrorKind::Other)),
        }
    }
    // subpastas esvaziadas, de baixo para cima; nunca `dir` nem a pasta da classe
    for (class_dir, mut folder) in tidy {
        while folder != class_dir && folder.starts_with(&class_dir) {
            if fs::remove_dir(&folder).is_err() {
                break;
            }
            if !folder.pop() {
                break;
            }
        }
    }
    Ok(report)
}

/// Apaga `file` se, e só se, o que está lá agora tem `sha_prefix` como início do `sha256`.
/// Move para um nome de trabalho, confere o que moveu e só então apaga; o que não bate volta
/// sem sobrescrever nada. `Ok(None)` = apagado. O que ficou num arquivo de trabalho (não pôde
/// ser devolvido nem apagado) vai para `kept`.
pub(crate) fn remove_if_unchanged(
    file: &Path,
    sha_prefix: [u8; 8],
    kept: &mut Vec<PathBuf>,
) -> Result<Option<Skip>, EngineError> {
    let parent = file
        .parent()
        .ok_or(EngineError::Io(io::ErrorKind::InvalidInput))?;
    // a arca nunca grava uma cópia acima do teto: maior que isso não é cópia nossa, e movê-la
    // e hasheá-la só alargaria a janela contra uma edição da pessoa
    match fs::symlink_metadata(file) {
        Ok(meta) if meta.len() > MAX_FILE_BYTES => return Ok(Some(Skip::Edited)),
        Ok(_) => {}
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(Some(Skip::Gone)),
        Err(e) => return Err(io_error(e)),
    }
    let mut random = [0u8; 8];
    getrandom::fill(&mut random).map_err(|_| EngineError::Io(io::ErrorKind::Other))?;
    let hex: String = random.iter().map(|b| format!("{b:02x}")).collect();
    // nome de trabalho reservado (`.tmp-<16 hex>`): nem o snapshot nem o `list` o enxergam
    let scratch = parent.join(format!(".tmp-{hex}"));
    ensure_absent(&scratch)?;
    match fs::rename(file, &scratch) {
        Ok(()) => {}
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(Some(Skip::Gone)),
        Err(e) => return Err(io_error(e)),
    }
    if matches!(current_sha(&scratch), Ok(Some(sha)) if sha.0[..8] == sha_prefix) {
        return match fs::remove_file(&scratch) {
            Ok(()) => Ok(None),
            // não apagou: o segredo continua em disco, e quem chamou precisa saber onde
            Err(e) => {
                kept.push(scratch);
                Ok(Some(Skip::Failed(e.kind())))
            }
        };
    }
    Ok(Some(give_back(&scratch, file, kept)))
}

/// Devolve `scratch` a `file` sem sobrescrever. Se o nome foi recriado no meio-tempo (um editor
/// que salva por rename) ou o sistema de arquivos não tem hard link, o conteúdo **fica em
/// `scratch`** e o caminho vai para `kept`: nada se perde e nada fica sem aviso.
fn give_back(scratch: &Path, file: &Path, kept: &mut Vec<PathBuf>) -> Skip {
    match fs::hard_link(scratch, file) {
        Ok(()) => {
            if fs::remove_file(scratch).is_err() {
                kept.push(scratch.to_path_buf());
            }
            Skip::Edited
        }
        Err(e) => {
            kept.push(scratch.to_path_buf());
            Skip::Failed(e.kind())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::disk::{create_private_dir, write_atomic};
    use crate::testutil::TempDir;
    use sha2::{Digest, Sha256};
    use std::fs;

    fn prefix(content: &[u8]) -> [u8; 8] {
        Sha256::digest(content)[..8].try_into().unwrap()
    }

    fn hex(content: &[u8]) -> String {
        prefix(content).iter().map(|b| format!("{b:02x}")).collect()
    }

    /// Grava uma cópia como o `save_conflict_losers` grava: `dir/<classe>/<path>.conflito-...`.
    fn put(dir: &Path, class_dir: &str, path: &str, device: &str, content: &[u8]) -> PathBuf {
        let file = dir
            .join(class_dir)
            .join(format!("{path}.conflito-{device}-{}", hex(content)));
        create_private_dir(file.parent().unwrap()).unwrap();
        write_atomic(&file, content).unwrap();
        file
    }

    fn files_under(dir: &Path) -> Vec<String> {
        fn walk(dir: &Path, prefix: &str, out: &mut Vec<String>) {
            for e in fs::read_dir(dir).unwrap() {
                let e = e.unwrap();
                let name = e.file_name().into_string().unwrap();
                let rel = if prefix.is_empty() {
                    name
                } else {
                    format!("{prefix}/{name}")
                };
                if e.file_type().unwrap().is_dir() {
                    out.push(format!("{rel}/"));
                    walk(&e.path(), &rel, out);
                } else {
                    out.push(rel);
                }
            }
        }
        let mut out = Vec::new();
        walk(dir, "", &mut out);
        out.sort();
        out
    }

    // ---------- list ----------

    #[test]
    fn list_finds_the_copies_in_a_stable_order_and_ignores_everything_else() {
        let tmp = TempDir::new();
        let dir = tmp.path();
        let a = put(dir, "config", "settings.toml", "pa", b"perdedor A");
        let b = put(dir, "secret", "oauth/slack.json", "pb", b"{token}");
        // o que não é uma cópia da arca
        fs::write(dir.join("config/anotacao-da-pessoa.txt"), b"x").unwrap();
        fs::write(dir.join("config/settings.toml.conflito-pa-nao-e-hex"), b"x").unwrap();
        fs::write(dir.join("config/.tmp-0123456789abcdef"), b"x").unwrap();
        fs::write(
            dir.join("config/sem-dispositivo.conflito--0011223344556677"),
            b"x",
        )
        .unwrap();
        fs::write(
            dir.join("fora-das-classes.conflito-pa-0011223344556677"),
            b"x",
        )
        .unwrap();
        create_private_dir(&dir.join("config/pasta.conflito-pa-0011223344556677")).unwrap();
        #[cfg(unix)]
        std::os::unix::fs::symlink(&a, dir.join("config/link.conflito-pa-0011223344556677"))
            .unwrap();

        let found = list(dir).unwrap();
        assert_eq!(
            found,
            vec![
                LoserCopy {
                    class: Class::Config,
                    path: "settings.toml".to_string(),
                    device: "pa".to_string(),
                    sha_prefix: prefix(b"perdedor A"),
                    file: a,
                    edited: false,
                },
                LoserCopy {
                    class: Class::Secret,
                    path: "oauth/slack.json".to_string(),
                    device: "pb".to_string(),
                    sha_prefix: prefix(b"{token}"),
                    file: b,
                    edited: false,
                },
            ]
        );
        assert_eq!(list(dir).unwrap(), found, "ordem estável");
    }

    #[test]
    fn list_of_a_missing_or_empty_folder_is_empty() {
        let tmp = TempDir::new();
        assert_eq!(list(&tmp.path().join("nao-existe")).unwrap(), vec![]);
        assert_eq!(list(tmp.path()).unwrap(), vec![]);
    }

    #[test]
    fn a_copy_the_person_edited_is_listed_as_edited() {
        let tmp = TempDir::new();
        let file = put(tmp.path(), "config", "a.toml", "pa", b"original");
        fs::write(&file, b"resolvido pela pessoa").unwrap();
        let found = list(tmp.path()).unwrap();
        assert_eq!(found.len(), 1);
        assert!(found[0].edited);
    }

    // ---------- clear ----------

    #[test]
    fn clear_removes_only_what_was_passed_and_tidies_the_emptied_folders() {
        let tmp = TempDir::new();
        let dir = tmp.path();
        put(dir, "secret", "oauth/slack.json", "pa", b"um");
        put(dir, "secret", "oauth/github.json", "pa", b"dois");
        put(dir, "config", "a.toml", "pa", b"tres");
        let all = list(dir).unwrap();
        let (slack, rest): (Vec<_>, Vec<_>) = all
            .iter()
            .cloned()
            .partition(|c| c.path.ends_with("slack.json"));

        let report = clear(dir, &slack).unwrap();
        assert_eq!(
            report,
            ClearReport {
                removed: 1,
                ..ClearReport::default()
            }
        );
        assert_eq!(list(dir).unwrap(), rest);

        // a última de `oauth/` sai: a subpasta some, a pasta da classe e `dir` ficam
        let github: Vec<_> = rest
            .iter()
            .filter(|c| c.path.ends_with("github.json"))
            .cloned()
            .collect();
        clear(dir, &github).unwrap();
        let left = files_under(dir);
        assert!(
            !left.iter().any(|p| p.starts_with("secret/oauth")),
            "{left:?}"
        );
        assert!(left.contains(&"secret/".to_string()) && left.contains(&"config/".to_string()));
        assert!(dir.is_dir());
    }

    #[test]
    fn clear_is_idempotent_and_reports_what_is_gone() {
        let tmp = TempDir::new();
        put(tmp.path(), "config", "a.toml", "pa", b"x");
        let all = list(tmp.path()).unwrap();
        assert_eq!(clear(tmp.path(), &all).unwrap().removed, 1);
        let again = clear(tmp.path(), &all).unwrap();
        assert_eq!(again.removed, 0);
        assert_eq!(again.skipped, vec![(all[0].file.clone(), Skip::Gone)]);
    }

    #[test]
    fn an_edited_copy_is_never_removed_even_when_the_caller_asks() {
        let tmp = TempDir::new();
        let file = put(tmp.path(), "secret", "a.json", "pa", b"original");
        let mut listed = list(tmp.path()).unwrap();
        fs::write(&file, b"resolvido pela pessoa").unwrap();
        // o chamador mente dizendo que não foi editada
        listed[0].edited = false;
        let report = clear(tmp.path(), &listed).unwrap();
        assert_eq!(
            report,
            ClearReport {
                removed: 0,
                skipped: vec![(file.clone(), Skip::Edited)],
                kept: vec![]
            }
        );
        assert_eq!(fs::read(&file).unwrap(), b"resolvido pela pessoa");
    }

    #[test]
    fn a_forged_list_removes_nothing_outside_the_copies_the_arca_wrote() {
        let tmp = TempDir::new();
        let dir = tmp.path().join("conflitos");
        let valid = put(&dir, "config", "a.toml", "pa", b"x");
        let outside = tmp.path().join("importante.txt");
        fs::write(&outside, b"importante").unwrap();
        let named_ok_outside = tmp
            .path()
            .join(format!("fora.conflito-pa-{}", hex(b"importante")));
        fs::write(&named_ok_outside, b"importante").unwrap();
        // um arquivo de nome certo dentro de `dir`, mas na pasta `vault` (não é classe de perdedor)
        let in_vault = dir.join(format!("vault/n.md.conflito-pa-{}", hex(b"v")));
        create_private_dir(in_vault.parent().unwrap()).unwrap();
        fs::write(&in_vault, b"v").unwrap();
        // e um diretório com nome de cópia
        let folder = dir.join("config/pasta.conflito-pa-0011223344556677");
        create_private_dir(&folder).unwrap();

        let base = list(&dir).unwrap().remove(0);
        let forge = |file: PathBuf| LoserCopy {
            file,
            ..base.clone()
        };
        let mut forged = vec![
            forge(outside.clone()),
            forge(named_ok_outside.clone()),
            forge(dir.join("config/../../importante.txt")),
            forge(dir.join("config/../../fora.conflito-pa-0011223344556677")),
            forge(in_vault.clone()),
            forge(folder.clone()),
            forge(dir.clone()),
            forge(PathBuf::from("/etc/passwd")),
            forge(PathBuf::from(
                "config/relativo.conflito-pa-0011223344556677",
            )),
        ];
        #[cfg(unix)]
        {
            // link com nome de cópia apontando para uma cópia válida, e pasta-link no caminho
            let link = dir.join(format!("config/atalho.conflito-pa-{}", hex(b"x")));
            std::os::unix::fs::symlink(&valid, &link).unwrap();
            forged.push(forge(link));
            let real = dir.join("config/real");
            create_private_dir(&real).unwrap();
            let inside = real.join(format!("b.toml.conflito-pa-{}", hex(b"y")));
            fs::write(&inside, b"y").unwrap();
            std::os::unix::fs::symlink(&real, dir.join("config/via-link")).unwrap();
            forged.push(forge(
                dir.join("config/via-link")
                    .join(inside.file_name().unwrap()),
            ));
        }
        let before_outside = files_under(tmp.path());

        let report = clear(&dir, &forged).unwrap();
        assert_eq!(report.removed, 0, "{report:?}");
        assert!(report
            .skipped
            .iter()
            .all(|(_, why)| matches!(why, Skip::NotACopy | Skip::Gone)));
        assert_eq!(
            files_under(tmp.path()),
            before_outside,
            "nada pode ter mudado"
        );
        assert_eq!(fs::read(&outside).unwrap(), b"importante");
        assert_eq!(fs::read(&named_ok_outside).unwrap(), b"importante");
        assert_eq!(fs::read(&valid).unwrap(), b"x");
    }

    // ---------- mover e conferir ----------

    #[test]
    fn remove_if_unchanged_removes_what_matches_and_puts_back_what_does_not() {
        let tmp = TempDir::new();
        let file = tmp.path().join("c.conflito-pa-0011223344556677");

        let mut kept = Vec::new();
        fs::write(&file, b"igual").unwrap();
        assert_eq!(
            remove_if_unchanged(&file, prefix(b"igual"), &mut kept).unwrap(),
            None
        );
        assert!(!file.exists());

        // o conteúdo mudou entre a conferência de quem chamou e a remoção
        fs::write(&file, b"edicao da pessoa").unwrap();
        assert_eq!(
            remove_if_unchanged(&file, prefix(b"igual"), &mut kept).unwrap(),
            Some(Skip::Edited)
        );
        assert_eq!(fs::read(&file).unwrap(), b"edicao da pessoa");
        // nenhum arquivo de trabalho sobrou
        assert_eq!(fs::read_dir(tmp.path()).unwrap().count(), 1);

        fs::remove_file(&file).unwrap();
        assert_eq!(
            remove_if_unchanged(&file, prefix(b"igual"), &mut kept).unwrap(),
            Some(Skip::Gone)
        );
        assert!(kept.is_empty());
    }

    // ---------- auditoria do P-28 ----------

    /// Achado 1: um nome não ASCII cujo corte cai no meio de um caractere fazia `split_at`
    /// entrar em pânico, e com ele `list` e `clear` inteiros.
    #[test]
    fn a_non_ascii_file_name_is_not_a_copy_and_never_panics() {
        assert_eq!(parse_name("ç123456789012345"), None);
        assert_eq!(parse_name("notaçãoção-final"), None);
        let tmp = TempDir::new();
        let valid = put(tmp.path(), "config", "a.toml", "pa", b"x");
        fs::write(tmp.path().join("config/ç123456789012345"), b"nota").unwrap();
        fs::write(tmp.path().join("config/anotação-importante-ção"), b"nota").unwrap();
        let found = list(tmp.path()).unwrap();
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].file, valid);

        let mut forged = found[0].clone();
        forged.file = tmp.path().join("config/ç123456789012345");
        let report = clear(tmp.path(), &[forged, found[0].clone()]).unwrap();
        assert_eq!(report.removed, 1);
        assert_eq!(report.skipped.len(), 1);
        assert_eq!(report.skipped[0].1, Skip::NotACopy);
    }

    /// Achado 4: o `..` tem de ser barrado pela validação, e não por acaso (o hash do arquivo
    /// de fora bate com o do nome, então só a checagem do caminho o protege).
    #[test]
    fn a_dotdot_path_to_a_file_with_the_right_name_and_hash_is_not_removed() {
        let tmp = TempDir::new();
        let dir = tmp.path().join("conflitos");
        put(&dir, "config", "a.toml", "pa", b"x");
        let target = tmp
            .path()
            .join(format!("fora.conflito-pa-{}", hex(b"importante")));
        fs::write(&target, b"importante").unwrap();
        let mut forged = list(&dir).unwrap().remove(0);
        forged.file = dir.join(format!(
            "config/../../fora.conflito-pa-{}",
            hex(b"importante")
        ));
        assert!(
            forged.file.exists(),
            "o caminho forjado chega mesmo ao arquivo"
        );

        let report = clear(&dir, &[forged.clone()]).unwrap();
        assert_eq!(report.removed, 0);
        assert_eq!(report.skipped, vec![(forged.file, Skip::NotACopy)]);
        assert_eq!(fs::read(&target).unwrap(), b"importante");
    }

    /// Achado 2: se não dá para devolver (o nome foi recriado, ou o sistema de arquivos não tem
    /// hard link), o conteúdo da pessoa fica no arquivo de trabalho e o caminho é reportado.
    #[test]
    fn what_cannot_be_given_back_is_reported_instead_of_lost() {
        let tmp = TempDir::new();
        let file = tmp.path().join("c.conflito-pa-0011223344556677");
        let scratch = tmp.path().join(".tmp-aaaaaaaaaaaaaaaa");
        fs::write(&scratch, b"edicao da pessoa").unwrap();
        fs::write(&file, b"salvamento novo do editor").unwrap(); // o nome foi recriado
        let mut kept = Vec::new();
        assert_eq!(
            give_back(&scratch, &file, &mut kept),
            Skip::Failed(io::ErrorKind::AlreadyExists)
        );
        assert_eq!(kept, vec![scratch.clone()]);
        assert_eq!(fs::read(&scratch).unwrap(), b"edicao da pessoa");
        assert_eq!(fs::read(&file).unwrap(), b"salvamento novo do editor");
    }

    /// Achado 3: um erro de E/S num item não pode esconder o que o `clear` já apagou.
    #[cfg(unix)]
    #[test]
    fn an_io_error_on_one_item_does_not_abort_the_batch() {
        use std::os::unix::fs::PermissionsExt;
        let tmp = TempDir::new();
        let dir = tmp.path();
        put(dir, "config", "a.toml", "pa", b"x");
        let locked = dir.join("config/trancada");
        create_private_dir(&locked).unwrap();
        let hidden = put(dir, "config", "trancada/b.toml", "pa", b"y");
        let copies = list(dir).unwrap();
        assert_eq!(copies.len(), 2);
        fs::set_permissions(&locked, fs::Permissions::from_mode(0o000)).unwrap();
        // como root a permissão não vale: o teste não prova nada, então não roda
        if fs::symlink_metadata(&hidden).is_ok() {
            fs::set_permissions(&locked, fs::Permissions::from_mode(0o700)).unwrap();
            return;
        }
        let report = clear(dir, &copies).unwrap();
        fs::set_permissions(&locked, fs::Permissions::from_mode(0o700)).unwrap();
        assert_eq!(report.removed, 1);
        assert_eq!(
            report.skipped,
            vec![(
                hidden.clone(),
                Skip::Failed(io::ErrorKind::PermissionDenied)
            )]
        );
        assert!(hidden.exists());
    }

    /// Achado 6: acima do teto da arca não é cópia dela; não se lê, não se move, não se apaga.
    #[test]
    fn a_file_above_the_size_cap_is_edited_and_never_read_or_moved() {
        let tmp = TempDir::new();
        let name = format!("grande.conflito-pa-{}", hex(b"x"));
        let file = tmp.path().join("config").join(&name);
        create_private_dir(file.parent().unwrap()).unwrap();
        let big = fs::File::create(&file).unwrap();
        big.set_len(MAX_FILE_BYTES + 1).unwrap(); // esparso: não ocupa disco
        let found = list(tmp.path()).unwrap();
        assert_eq!(found.len(), 1);
        assert!(found[0].edited);

        let report = clear(tmp.path(), &found).unwrap();
        assert_eq!(report.removed, 0);
        assert_eq!(report.skipped, vec![(file.clone(), Skip::Edited)]);
        assert_eq!(fs::metadata(&file).unwrap().len(), MAX_FILE_BYTES + 1);
        assert_eq!(fs::read_dir(file.parent().unwrap()).unwrap().count(), 1);
    }

    /// Achado 5: árvore funda de propósito não pode estourar a pilha nem esconder as cópias.
    #[test]
    fn a_very_deep_folder_tree_is_cut_off_and_the_shallow_copies_are_still_found() {
        let tmp = TempDir::new();
        let near = put(tmp.path(), "config", "a/b/c.toml", "pa", b"x");
        let mut deep = tmp.path().join("config");
        for _ in 0..(MAX_DEPTH + 20) {
            deep.push("d");
        }
        create_private_dir(&deep).unwrap();
        fs::write(deep.join(format!("fundo.conflito-pa-{}", hex(b"y"))), b"y").unwrap();
        let found = list(tmp.path()).unwrap();
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].file, near);
    }

    /// Achado 8: pasta com nome que não é UTF-8 não vira `path`; a cópia dentro dela é ignorada
    /// em vez de ganhar um `path` errado que poderia colidir com outro conflito.
    #[cfg(unix)]
    #[test]
    fn a_copy_under_a_non_utf8_folder_is_ignored_by_list() {
        use std::ffi::OsStr;
        use std::os::unix::ffi::OsStrExt;
        let tmp = TempDir::new();
        put(tmp.path(), "config", "a.toml", "pa", b"x");
        let odd = tmp.path().join("config").join(OsStr::from_bytes(b"x\xff"));
        create_private_dir(&odd).unwrap();
        fs::write(odd.join(format!("b.toml.conflito-pa-{}", hex(b"y"))), b"y").unwrap();
        let found = list(tmp.path()).unwrap();
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].path, "a.toml");
    }
}
