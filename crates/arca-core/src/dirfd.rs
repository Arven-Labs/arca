//! Operações de arquivo relativas a uma pasta já aberta (P-34 item 2): a pasta é um valor
//! (`Folder`), não um caminho que o sistema resolve de novo a cada chamada. Só Unix.
//!
//! RASCUNHO dos testes (Sessão 79): por baixo ainda é caminho, para os testes falharem pelo
//! motivo certo antes da implementação com `openat`.
#![allow(dead_code)]

use crate::manifest::Hash32;
use sha2::{Digest, Sha256};
use std::fs;
use std::io::{self, Read};
use std::path::{Path, PathBuf};

/// Por que `Folder::open_beneath` não abriu.
#[derive(Debug)]
pub(crate) enum OpenError {
    /// Um componente é link ou não é pasta: nunca se segue.
    NotAFolder,
    Io(io::Error),
}

/// Uma pasta aberta. Todas as operações agem nela, não em um caminho refeito do zero.
pub(crate) struct Folder {
    path: PathBuf,
}

impl Folder {
    /// Abre `base` (que pode passar por links: é o que o consumidor escolheu) e desce `relative`
    /// de pasta em pasta sem seguir nenhum link.
    pub(crate) fn open_beneath(base: &Path, relative: &Path) -> Result<Folder, OpenError> {
        let mut walk = base.to_path_buf();
        for part in relative.components() {
            walk.push(part);
            match fs::symlink_metadata(&walk) {
                Ok(meta) if meta.file_type().is_dir() => {}
                Ok(_) => return Err(OpenError::NotAFolder),
                Err(e) => return Err(OpenError::Io(e)),
            }
        }
        Ok(Folder { path: walk })
    }

    pub(crate) fn rename(&self, from: &str, to: &str) -> io::Result<()> {
        fs::rename(self.path.join(from), self.path.join(to))
    }

    pub(crate) fn unlink(&self, name: &str) -> io::Result<()> {
        fs::remove_file(self.path.join(name))
    }

    /// Hard link `to` -> `from`, sem sobrescrever.
    pub(crate) fn link(&self, from: &str, to: &str) -> io::Result<()> {
        fs::hard_link(self.path.join(from), self.path.join(to))
    }

    /// `sha256` de um arquivo regular desta pasta. Link ou outra coisa: `InvalidInput`.
    pub(crate) fn sha256(&self, name: &str) -> io::Result<Hash32> {
        let path = self.path.join(name);
        if !fs::symlink_metadata(&path)?.file_type().is_file() {
            return Err(io::ErrorKind::InvalidInput.into());
        }
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
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::TempDir;
    use std::os::unix::fs::symlink;

    fn sha(bytes: &[u8]) -> Hash32 {
        Hash32(Sha256::digest(bytes).into())
    }

    #[test]
    fn open_beneath_descends_real_folders() {
        let tmp = TempDir::new();
        fs::create_dir_all(tmp.path().join("secret/oauth")).unwrap();
        fs::write(tmp.path().join("secret/oauth/a"), b"x").unwrap();
        let folder = Folder::open_beneath(tmp.path(), Path::new("secret/oauth")).unwrap();
        assert_eq!(folder.sha256("a").unwrap(), sha(b"x"));
    }

    #[test]
    fn open_beneath_refuses_a_link_in_the_middle_and_never_follows_it() {
        let tmp = TempDir::new();
        let outside = TempDir::new();
        fs::create_dir_all(tmp.path().join("secret")).unwrap();
        symlink(outside.path(), tmp.path().join("secret/oauth")).unwrap();
        let r = Folder::open_beneath(tmp.path(), Path::new("secret/oauth"));
        assert!(matches!(r, Err(OpenError::NotAFolder)));
    }

    #[test]
    fn open_beneath_refuses_a_file_where_a_folder_should_be() {
        let tmp = TempDir::new();
        fs::write(tmp.path().join("secret"), b"x").unwrap();
        let r = Folder::open_beneath(tmp.path(), Path::new("secret"));
        assert!(matches!(r, Err(OpenError::NotAFolder)));
    }

    #[test]
    fn open_beneath_reports_a_missing_folder_as_not_found() {
        let tmp = TempDir::new();
        let r = Folder::open_beneath(tmp.path(), Path::new("secret"));
        assert!(matches!(r, Err(OpenError::Io(e)) if e.kind() == io::ErrorKind::NotFound));
    }

    /// A propriedade que fecha o TOCTOU: depois de aberta, a pasta é a mesma mesmo que o caminho
    /// dela passe a apontar para outro lugar.
    #[test]
    fn an_opened_folder_keeps_acting_on_itself_after_its_path_is_swapped_for_a_link() {
        let tmp = TempDir::new();
        let outside = TempDir::new();
        fs::create_dir_all(tmp.path().join("secret/sub")).unwrap();
        fs::write(tmp.path().join("secret/sub/cop"), b"original").unwrap();
        fs::write(outside.path().join("cop"), b"da pessoa").unwrap();

        let folder = Folder::open_beneath(tmp.path(), Path::new("secret/sub")).unwrap();
        // o atacante troca a pasta por um link para fora
        fs::rename(
            tmp.path().join("secret/sub"),
            tmp.path().join("secret/sub.real"),
        )
        .unwrap();
        symlink(outside.path(), tmp.path().join("secret/sub")).unwrap();

        folder.rename("cop", ".tmp-0").unwrap();
        folder.unlink(".tmp-0").unwrap();

        // agiu na pasta original (que agora se chama `sub.real`), nunca no destino do link
        assert!(!tmp.path().join("secret/sub.real/.tmp-0").exists());
        assert!(!tmp.path().join("secret/sub.real/cop").exists());
        assert_eq!(fs::read(outside.path().join("cop")).unwrap(), b"da pessoa");
        assert!(!outside.path().join(".tmp-0").exists());
    }

    #[test]
    fn sha256_refuses_a_link_named_like_a_file() {
        let tmp = TempDir::new();
        let outside = TempDir::new();
        fs::write(outside.path().join("alvo"), b"x").unwrap();
        symlink(outside.path().join("alvo"), tmp.path().join("cop")).unwrap();
        let folder = Folder::open_beneath(tmp.path(), Path::new("")).unwrap();
        let err = folder.sha256("cop").unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::InvalidInput);
    }

    #[test]
    fn link_does_not_overwrite_and_unlink_removes_only_the_name() {
        let tmp = TempDir::new();
        fs::write(tmp.path().join("a"), b"1").unwrap();
        fs::write(tmp.path().join("b"), b"2").unwrap();
        let folder = Folder::open_beneath(tmp.path(), Path::new("")).unwrap();
        let err = folder.link("a", "b").unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::AlreadyExists);
        assert_eq!(fs::read(tmp.path().join("b")).unwrap(), b"2");
        folder.link("a", "c").unwrap();
        folder.unlink("a").unwrap();
        assert_eq!(fs::read(tmp.path().join("c")).unwrap(), b"1");
    }
}
