//! Varredura da pasta de uma classe: lista os arquivos que entram no snapshot, sem ler o
//! conteúdo. Nunca segue symlink. O que não pode entrar vai para `skipped`, com motivo fixo,
//! em vez de derrubar o backup inteiro.

use crate::apply::is_reserved_name;
use crate::error::Error;
use crate::manifest::{validate_path, Class};
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

/// Teto de um arquivo, em bytes (o arquivo inteiro é lido e cifrado de uma vez). Palpite a
/// revisar na Fase 3/4.
pub const MAX_FILE_BYTES: u64 = 256 * 1024 * 1024;

/// Arquivo que entra no snapshot.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Candidate {
    pub class: Class,
    /// Relativo à raiz da classe, separador `/`, já válido para o manifest.
    pub path: String,
    /// Caminho no disco, para ler o conteúdo.
    pub abs: PathBuf,
    /// Tamanho visto na varredura.
    pub len: u64,
}

/// Arquivo que ficou de fora do snapshot, e por quê.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Skipped {
    pub class: Class,
    /// Caminho relativo (local, para o usuário ver); não vai para o manifest.
    pub path: String,
    pub reason: &'static str,
}

#[derive(Debug, Default, PartialEq, Eq)]
pub struct Scan {
    /// Em ordem crescente de `path` (bytes).
    pub candidates: Vec<Candidate>,
    pub skipped: Vec<Skipped>,
}

/// Varre `root` (que tem de existir e ser uma pasta: uma raiz ausente é erro, não "vazia",
/// para um disco desmontado não apagar a classe inteira no próximo manifest).
pub fn scan_class(class: Class, root: &Path) -> io::Result<Scan> {
    if !fs::metadata(root)?.is_dir() {
        return Err(io::Error::from(io::ErrorKind::NotADirectory));
    }
    let mut scan = Scan::default();
    let skip = |scan: &mut Scan, path: String, reason: &'static str| {
        scan.skipped.push(Skipped {
            class,
            path,
            reason,
        });
    };
    // Pilha em vez de recursão: a profundidade vem do disco.
    let mut stack = vec![(root.to_path_buf(), String::new())];
    while let Some((dir, prefix)) = stack.pop() {
        for entry in fs::read_dir(&dir)? {
            let entry = entry?;
            let name = entry.file_name();
            let Some(name) = name.to_str() else {
                let lossy = name.to_string_lossy();
                skip(&mut scan, join(&prefix, &lossy), "name is not UTF-8");
                continue;
            };
            let rel = join(&prefix, name);
            // `file_type` e `metadata` da entrada não seguem symlink.
            let kind = entry.file_type()?;
            if kind.is_symlink() {
                skip(&mut scan, rel, "symlink");
            } else if kind.is_dir() {
                stack.push((entry.path(), rel));
            } else if kind.is_file() {
                let len = entry.metadata()?.len();
                if is_reserved_name(name) {
                    // sobra do `apply` ou do `write_atomic`: pode ter a versão antiga de um segredo
                    skip(&mut scan, rel, "reserved name");
                } else if len > MAX_FILE_BYTES {
                    skip(&mut scan, rel, "file too large");
                } else {
                    match validate_path(&rel) {
                        Ok(()) => scan.candidates.push(Candidate {
                            class,
                            path: rel,
                            abs: entry.path(),
                            len,
                        }),
                        Err(Error::InvalidPath(reason)) => skip(&mut scan, rel, reason),
                        Err(_) => skip(&mut scan, rel, "invalid path"),
                    }
                }
            } else {
                skip(&mut scan, rel, "not a regular file");
            }
        }
    }
    scan.candidates.sort_by(|a, b| a.path.cmp(&b.path));
    scan.skipped.sort_by(|a, b| a.path.cmp(&b.path));
    Ok(scan)
}

fn join(prefix: &str, name: &str) -> String {
    if prefix.is_empty() {
        name.to_string()
    } else {
        format!("{prefix}/{name}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::TempDir;
    use std::fs;

    fn write(root: &Path, rel: &str, data: &[u8]) {
        let p = root.join(rel);
        fs::create_dir_all(p.parent().unwrap()).unwrap();
        fs::write(p, data).unwrap();
    }

    /// Sobras do `apply` e do `write_atomic` nunca entram no snapshot: poderiam carregar a
    /// versão antiga de um segredo para o manifest e para os outros dispositivos.
    #[test]
    fn leftover_temporaries_are_skipped_not_backed_up() {
        let tmp = TempDir::new();
        write(tmp.path(), "ok.md", b"1");
        write(tmp.path(), ".arca-old-3", b"segredo antigo");
        write(tmp.path(), "sub/.arca-new-1", b"2");
        write(tmp.path(), ".tmp-0123456789abcdef", b"3");
        // parecidos, mas não são nossos: entram normalmente
        write(tmp.path(), ".arca-new-x", b"4");
        write(tmp.path(), ".tmp-notes", b"5");
        let scan = scan_class(Class::Vault, tmp.path()).unwrap();
        assert_eq!(paths(&scan), [".arca-new-x", ".tmp-notes", "ok.md"]);
        assert_eq!(
            reasons(&scan),
            [
                (".arca-old-3", "reserved name"),
                (".tmp-0123456789abcdef", "reserved name"),
                ("sub/.arca-new-1", "reserved name"),
            ]
        );
    }

    fn paths(scan: &Scan) -> Vec<&str> {
        scan.candidates.iter().map(|c| c.path.as_str()).collect()
    }

    fn reasons(scan: &Scan) -> Vec<(&str, &str)> {
        scan.skipped
            .iter()
            .map(|s| (s.path.as_str(), s.reason))
            .collect()
    }

    #[test]
    fn lists_files_recursively_with_slash_paths_sorted_by_bytes() {
        let tmp = TempDir::new();
        write(tmp.path(), "b.md", b"1");
        write(tmp.path(), "a/z.md", b"22");
        write(tmp.path(), "a/y/deep.md", b"333");
        write(tmp.path(), ".oculto", b"4");
        write(tmp.path(), "B.md", b"5");
        fs::create_dir(tmp.path().join("vazia")).unwrap();

        let scan = scan_class(Class::Vault, tmp.path()).unwrap();
        assert_eq!(
            paths(&scan),
            [".oculto", "B.md", "a/y/deep.md", "a/z.md", "b.md"]
        );
        assert!(scan.skipped.is_empty());
        assert!(scan.candidates.iter().all(|c| c.class == Class::Vault));
        let deep = scan
            .candidates
            .iter()
            .find(|c| c.path == "a/y/deep.md")
            .unwrap();
        assert_eq!(deep.len, 3);
        assert_eq!(deep.abs, tmp.path().join("a/y/deep.md"));
    }

    #[test]
    fn the_class_is_carried_into_every_candidate() {
        let tmp = TempDir::new();
        write(tmp.path(), "login.json", b"{}");
        let scan = scan_class(Class::Secret, tmp.path()).unwrap();
        assert_eq!(scan.candidates[0].class, Class::Secret);
    }

    #[test]
    fn an_empty_root_is_an_empty_scan() {
        let tmp = TempDir::new();
        assert_eq!(
            scan_class(Class::Vault, tmp.path()).unwrap(),
            Scan::default()
        );
    }

    #[test]
    fn a_missing_root_or_a_file_as_root_is_an_error() {
        let tmp = TempDir::new();
        let err = scan_class(Class::Vault, &tmp.path().join("nao-existe")).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::NotFound);
        write(tmp.path(), "arquivo", b"x");
        assert!(scan_class(Class::Vault, &tmp.path().join("arquivo")).is_err());
    }

    #[cfg(unix)]
    #[test]
    fn symlinks_are_never_followed_and_are_reported() {
        let tmp = TempDir::new();
        let outside = TempDir::new();
        write(outside.path(), "segredo.txt", b"nao deve entrar");
        write(outside.path(), "dir/dentro.md", b"nem este");
        write(tmp.path(), "real.md", b"ok");
        std::os::unix::fs::symlink(
            outside.path().join("segredo.txt"),
            tmp.path().join("link.md"),
        )
        .unwrap();
        std::os::unix::fs::symlink(outside.path().join("dir"), tmp.path().join("linkdir")).unwrap();

        let scan = scan_class(Class::Vault, tmp.path()).unwrap();
        assert_eq!(paths(&scan), ["real.md"]);
        assert_eq!(
            reasons(&scan),
            [("link.md", "symlink"), ("linkdir", "symlink")]
        );
    }

    #[test]
    fn names_the_manifest_would_refuse_are_skipped_with_the_reason() {
        let tmp = TempDir::new();
        write(tmp.path(), "ok.md", b"1");
        write(tmp.path(), "CON.md", b"2");
        write(tmp.path(), "cafe\u{301}.md", b"3");
        write(tmp.path(), "termina-com-ponto.", b"4");
        write(tmp.path(), "dir/aux", b"5");

        let scan = scan_class(Class::Vault, tmp.path()).unwrap();
        assert_eq!(paths(&scan), ["ok.md"]);
        let mut skipped = reasons(&scan);
        skipped.sort();
        assert_eq!(skipped.len(), 4);
        assert!(skipped.iter().any(|(p, _)| *p == "CON.md"));
        assert!(skipped.iter().all(|(_, r)| !r.is_empty()));
    }

    #[cfg(unix)]
    #[test]
    fn a_name_that_is_not_utf8_is_skipped_not_fatal() {
        use std::ffi::OsStr;
        use std::os::unix::ffi::OsStrExt;
        let tmp = TempDir::new();
        write(tmp.path(), "ok.md", b"1");
        fs::write(tmp.path().join(OsStr::from_bytes(b"ruim-\xff.md")), b"2").unwrap();

        let scan = scan_class(Class::Vault, tmp.path()).unwrap();
        assert_eq!(paths(&scan), ["ok.md"]);
        assert_eq!(scan.skipped.len(), 1);
        assert_eq!(scan.skipped[0].reason, "name is not UTF-8");
    }

    #[test]
    fn a_file_over_the_limit_is_skipped_and_one_at_the_limit_is_not() {
        let tmp = TempDir::new();
        // arquivos esparsos: não ocupam disco de verdade
        let big = fs::File::create(tmp.path().join("grande.bin")).unwrap();
        big.set_len(MAX_FILE_BYTES + 1).unwrap();
        let edge = fs::File::create(tmp.path().join("no-limite.bin")).unwrap();
        edge.set_len(MAX_FILE_BYTES).unwrap();

        let scan = scan_class(Class::Vault, tmp.path()).unwrap();
        assert_eq!(paths(&scan), ["no-limite.bin"]);
        assert_eq!(reasons(&scan), [("grande.bin", "file too large")]);
    }
}
