//! Listar e apagar os originais que a recuperação de um `apply` interrompido não pôde devolver
//! (`.arca-kept-<16 hex do sha256>`, P-33 item 5). `Engine::recover` deixa o arquivo na pasta da
//! classe e devolve só o caminho; na classe `secret` pode ser um token antigo. Aqui se vê o que
//! existe e se apaga o que o consumidor escolher.
//!
//! O `clear` **não confia no que o chamador mandou**: refaz tudo a partir do disco e só apaga um
//! arquivo regular, sob uma raiz ligada, com nome no padrão e conteúdo que ainda tem o hash do
//! nome. Um arquivo que a pessoa editou nunca é apagado.

use crate::apply::current_sha;
use crate::dirfd::{Folder, OpenError};
use crate::engine::EngineError;
use crate::losers::{remove_in, ClearReport, Skip, MAX_DEPTH};
use crate::manifest::Class;
use crate::scan::MAX_FILE_BYTES;
use std::fs;
use std::io;
use std::path::{Component, Path, PathBuf};

fn io_error(e: io::Error) -> EngineError {
    EngineError::Io(e.kind())
}

/// `.arca-kept-<16 hex minúsculos>` -> os 8 bytes do hash do nome (o mesmo critério de
/// `is_reserved_name`).
fn parse_name(name: &str) -> Option<[u8; 8]> {
    let hex = name.strip_prefix(".arca-kept-")?;
    if hex.len() != 16 {
        return None;
    }
    let digit = |c: u8| match c {
        b'0'..=b'9' => Some(c - b'0'),
        b'a'..=b'f' => Some(c - b'a' + 10),
        _ => None,
    };
    let mut prefix = [0u8; 8];
    for (byte, pair) in prefix.iter_mut().zip(hex.as_bytes().chunks(2)) {
        *byte = (digit(pair[0])? << 4) | digit(pair[1])?;
    }
    Some(prefix)
}

fn collect(
    class: Class,
    current: &Path,
    depth: usize,
    out: &mut Vec<KeptOriginal>,
) -> Result<(), EngineError> {
    // mais fundo que o teto de um `path` do manifest não é nosso (e uma árvore profunda de
    // propósito estouraria a pilha)
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
        let kind = match fs::metadata(&path) {
            Ok(meta) => meta.file_type(),
            Err(_) => continue,
        };
        if kind.is_dir() {
            collect(class, &path, depth + 1, out)?;
            continue;
        }
        if !kind.is_file() {
            continue;
        }
        let Some(prefix) = path
            .file_name()
            .and_then(|n| n.to_str())
            .and_then(parse_name)
        else {
            continue;
        };
        // acima do teto da arca não é um original dela: não se lê (seria só custo)
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
        out.push(KeptOriginal {
            class,
            file: path,
            sha_prefix: prefix,
            edited,
        });
    }
    Ok(())
}

/// Um original guardado que existe em disco.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KeptOriginal {
    pub class: Class,
    /// Onde o arquivo está.
    pub file: PathBuf,
    /// Primeiros 8 bytes do `sha256` do conteúdo, como no nome.
    pub sha_prefix: [u8; 8],
    /// O conteúdo atual não bate mais com o hash do nome: a pessoa editou. Nunca é apagado.
    pub edited: bool,
}

/// Os originais guardados sob as raízes ligadas, em ordem estável (por caminho).
pub(crate) fn list(roots: &[(Class, PathBuf)]) -> Result<Vec<KeptOriginal>, EngineError> {
    let mut out = Vec::new();
    for (class, root) in roots {
        collect(*class, root, 0, &mut out)?;
    }
    out.sort_by(|a, b| a.file.cmp(&b.file));
    Ok(out)
}

/// Apaga os itens de `items` que ainda são o que a recuperação guardou. Nada do que o chamador
/// mandou vale (`class`, `sha_prefix`, `edited`): a raiz, o nome e o hash vêm do caminho e do
/// disco.
pub(crate) fn clear(
    roots: &[(Class, PathBuf)],
    items: &[KeptOriginal],
) -> Result<ClearReport, EngineError> {
    let mut report = ClearReport::default();
    for item in items {
        let file = &item.file;
        let skip = |report: &mut ClearReport, why| report.skipped.push((file.clone(), why));
        // sob alguma raiz ligada, só com componentes normais (sem `..`)
        let found = roots.iter().find_map(|(_, root)| {
            let relative = file.strip_prefix(root).ok()?;
            let normal = relative
                .components()
                .all(|c| matches!(c, Component::Normal(_)));
            (normal && relative.components().count() >= 1).then_some((root, relative))
        });
        let Some((root, relative)) = found else {
            skip(&mut report, Skip::NotACopy);
            continue;
        };
        let Some(name) = file.file_name().and_then(|n| n.to_str()) else {
            skip(&mut report, Skip::NotACopy);
            continue;
        };
        let Some(prefix) = parse_name(name) else {
            skip(&mut report, Skip::NotACopy);
            continue;
        };
        // a pasta é aberta uma vez, sem seguir link, e tudo o que vem depois age nela
        let parent_relative = relative.parent().unwrap_or(Path::new(""));
        let folder = match Folder::open_beneath(root, parent_relative) {
            Ok(folder) => folder,
            Err(OpenError::NotAFolder) => {
                skip(&mut report, Skip::NotACopy);
                continue;
            }
            Err(OpenError::Io(e)) if e.kind() == io::ErrorKind::NotFound => {
                skip(&mut report, Skip::Gone);
                continue;
            }
            Err(OpenError::Io(e)) => {
                skip(&mut report, Skip::Failed(e.kind()));
                continue;
            }
        };
        match folder.stat(name) {
            Ok(entry) if entry.is_file => {}
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
        let parent = file.parent().unwrap_or(root);
        // mover, conferir o hash do que moveu e só então apagar; o que não bate volta
        match remove_in(&folder, name, parent, prefix, &mut report.kept) {
            Ok(None) => report.removed += 1,
            Ok(Some(why)) => skip(&mut report, why),
            Err(EngineError::Io(kind)) => skip(&mut report, Skip::Failed(kind)),
            Err(_) => skip(&mut report, Skip::Failed(io::ErrorKind::Other)),
        }
    }
    Ok(report)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::losers::Skip;
    use crate::testutil::TempDir;
    use sha2::{Digest, Sha256};
    use std::fs;
    use std::path::Path;

    fn prefix(content: &[u8]) -> [u8; 8] {
        Sha256::digest(content)[..8].try_into().unwrap()
    }

    fn kept_name(content: &[u8]) -> String {
        let hex: String = prefix(content).iter().map(|b| format!("{b:02x}")).collect();
        format!(".arca-kept-{hex}")
    }

    /// Grava um original guardado como a recuperação o deixa: `<pasta>/.arca-kept-<hex>`.
    fn put(dir: &Path, content: &[u8]) -> PathBuf {
        fs::create_dir_all(dir).unwrap();
        let file = dir.join(kept_name(content));
        fs::write(&file, content).unwrap();
        file
    }

    fn roots(vault: &Path, secret: &Path) -> Vec<(Class, PathBuf)> {
        vec![
            (Class::Vault, vault.to_path_buf()),
            (Class::Secret, secret.to_path_buf()),
        ]
    }

    fn item(class: Class, file: &Path, content: &[u8]) -> KeptOriginal {
        KeptOriginal {
            class,
            file: file.to_path_buf(),
            sha_prefix: prefix(content),
            edited: false,
        }
    }

    fn names(dir: &Path) -> Vec<String> {
        let mut out: Vec<String> = fs::read_dir(dir)
            .unwrap()
            .map(|e| e.unwrap().file_name().into_string().unwrap())
            .collect();
        out.sort();
        out
    }

    // ---------- list ----------

    #[test]
    fn list_finds_the_kept_files_of_every_enabled_root_in_stable_order() {
        let tmp = TempDir::new();
        let (vault, secret) = (tmp.path().join("vault"), tmp.path().join("secret"));
        let in_secret = put(&secret, b"token-velho");
        let in_sub = put(&vault.join("sub"), b"nota-velha");
        let listed = list(&roots(&vault, &secret)).unwrap();
        assert_eq!(
            listed,
            // por caminho: `secret/...` vem antes de `vault/...`, qualquer que seja a ordem das raízes
            vec![
                item(Class::Secret, &in_secret, b"token-velho"),
                item(Class::Vault, &in_sub, b"nota-velha"),
            ]
        );
    }

    #[test]
    fn list_flags_a_file_the_person_edited() {
        let tmp = TempDir::new();
        let secret = tmp.path().join("secret");
        let file = put(&secret, b"token-velho");
        fs::write(&file, b"editado pela pessoa").unwrap();
        let listed = list(&[(Class::Secret, secret)]).unwrap();
        assert_eq!(listed.len(), 1);
        assert!(listed[0].edited);
        assert_eq!(listed[0].sha_prefix, prefix(b"token-velho"));
    }

    #[test]
    fn list_ignores_names_that_are_not_ours_and_things_that_are_not_files() {
        let tmp = TempDir::new();
        let outside = TempDir::new();
        let secret = tmp.path().join("secret");
        fs::create_dir_all(&secret).unwrap();
        for bad in [
            ".arca-kept-zzzzzzzzzzzzzzzz",  // não é hex
            ".arca-kept-0123456789ABCDEF",  // maiúsculo
            ".arca-kept-0123456789abcde",   // curto
            ".arca-kept-0123456789abcdef0", // longo
            "arca-kept-0123456789abcdef",   // sem o ponto
        ] {
            fs::write(secret.join(bad), b"x").unwrap();
        }
        // pasta com o nome certo, e link para fora com o nome certo
        fs::create_dir_all(secret.join(".arca-kept-0000000000000000")).unwrap();
        let target = put(outside.path(), b"de fora");
        std::os::unix::fs::symlink(&target, secret.join(".arca-kept-1111111111111111")).unwrap();
        // pasta que é link para fora, com um kept de verdade dentro
        std::os::unix::fs::symlink(outside.path(), secret.join("link")).unwrap();
        assert_eq!(list(&[(Class::Secret, secret)]).unwrap(), vec![]);
    }

    // ---------- clear ----------

    #[test]
    fn clear_deletes_a_valid_item_and_leaves_no_work_file() {
        let tmp = TempDir::new();
        let secret = tmp.path().join("secret");
        let file = put(&secret, b"token-velho");
        fs::write(secret.join("fica.json"), b"{}").unwrap();
        let r = [(Class::Secret, secret.clone())];
        let listed = list(&r).unwrap();
        let report = clear(&r, &listed).unwrap();
        assert_eq!(report.removed, 1);
        assert!(report.skipped.is_empty() && report.kept.is_empty());
        assert!(!file.exists());
        assert_eq!(names(&secret), vec!["fica.json"]);
    }

    #[test]
    fn clear_never_deletes_what_the_person_edited() {
        let tmp = TempDir::new();
        let secret = tmp.path().join("secret");
        let file = put(&secret, b"token-velho");
        let r = [(Class::Secret, secret.clone())];
        let listed = list(&r).unwrap();
        // editado depois do `list`, e o chamador ainda diz `edited: false`
        fs::write(&file, b"editado depois").unwrap();
        let report = clear(&r, &listed).unwrap();
        assert_eq!(report.removed, 0);
        assert_eq!(report.skipped, vec![(file.clone(), Skip::Edited)]);
        assert_eq!(fs::read(&file).unwrap(), b"editado depois");
        assert_eq!(names(&secret), vec![kept_name(b"token-velho")]);
    }

    #[test]
    fn clear_refuses_a_path_outside_the_enabled_roots() {
        let tmp = TempDir::new();
        let outside = TempDir::new();
        let (vault, secret) = (tmp.path().join("vault"), tmp.path().join("secret"));
        fs::create_dir_all(&vault).unwrap();
        let elsewhere = put(outside.path(), b"de fora");
        let in_vault = put(&vault, b"da raiz desligada");
        // só `secret` está ligada agora
        let r = [(Class::Secret, secret.clone())];
        fs::create_dir_all(&secret).unwrap();
        let dotdot = secret
            .join("..")
            .join("vault")
            .join(kept_name(b"da raiz desligada"));
        let items = [
            item(Class::Secret, &elsewhere, b"de fora"),
            item(Class::Secret, &in_vault, b"da raiz desligada"),
            item(Class::Secret, &dotdot, b"da raiz desligada"),
            item(Class::Secret, Path::new("relativo"), b"x"),
        ];
        let report = clear(&r, &items).unwrap();
        assert_eq!(report.removed, 0);
        assert!(report.skipped.iter().all(|(_, why)| *why == Skip::NotACopy));
        assert_eq!(report.skipped.len(), 4);
        assert!(elsewhere.exists() && in_vault.exists());
    }

    #[test]
    fn clear_reports_a_file_that_is_gone() {
        let tmp = TempDir::new();
        let secret = tmp.path().join("secret");
        let file = put(&secret, b"token-velho");
        let r = [(Class::Secret, secret)];
        let listed = list(&r).unwrap();
        fs::remove_file(&file).unwrap();
        let report = clear(&r, &listed).unwrap();
        assert_eq!(report.removed, 0);
        assert_eq!(report.skipped, vec![(file, Skip::Gone)]);
    }

    #[test]
    fn clear_never_acts_through_a_link_in_the_path() {
        let tmp = TempDir::new();
        let outside = TempDir::new();
        let secret = tmp.path().join("secret");
        fs::create_dir_all(&secret).unwrap();
        let target = put(outside.path(), b"de fora");
        std::os::unix::fs::symlink(outside.path(), secret.join("link")).unwrap();
        let through = secret.join("link").join(kept_name(b"de fora"));
        let report = clear(
            &[(Class::Secret, secret)],
            &[item(Class::Secret, &through, b"de fora")],
        )
        .unwrap();
        assert_eq!(report.removed, 0);
        assert_eq!(report.skipped, vec![(through, Skip::NotACopy)]);
        assert_eq!(fs::read(&target).unwrap(), b"de fora");
    }

    #[test]
    fn clear_refuses_a_name_that_is_a_folder_or_a_link() {
        let tmp = TempDir::new();
        let outside = TempDir::new();
        let secret = tmp.path().join("secret");
        fs::create_dir_all(&secret).unwrap();
        let folder = secret.join(".arca-kept-0000000000000000");
        fs::create_dir_all(&folder).unwrap();
        let target = put(outside.path(), b"de fora");
        let link = secret.join(kept_name(b"de fora"));
        std::os::unix::fs::symlink(&target, &link).unwrap();
        let report = clear(
            &[(Class::Secret, secret)],
            &[
                KeptOriginal {
                    class: Class::Secret,
                    file: folder.clone(),
                    sha_prefix: [0; 8],
                    edited: false,
                },
                item(Class::Secret, &link, b"de fora"),
            ],
        )
        .unwrap();
        assert_eq!(report.removed, 0);
        assert!(report.skipped.iter().all(|(_, why)| *why == Skip::NotACopy));
        assert!(folder.is_dir());
        assert_eq!(fs::read(&target).unwrap(), b"de fora");
    }

    #[test]
    fn clear_goes_on_after_an_invalid_item() {
        let tmp = TempDir::new();
        let secret = tmp.path().join("secret");
        let good = put(&secret, b"token-velho");
        let r = [(Class::Secret, secret.clone())];
        let bad = secret.join("nao-e-nosso.json");
        fs::write(&bad, b"{}").unwrap();
        let report = clear(
            &r,
            &[
                item(Class::Secret, &bad, b"{}"),
                item(Class::Secret, &good, b"token-velho"),
            ],
        )
        .unwrap();
        assert_eq!(report.removed, 1);
        assert_eq!(report.skipped, vec![(bad.clone(), Skip::NotACopy)]);
        assert!(!good.exists() && bad.exists());
    }
}
