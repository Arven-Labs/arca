//! Operações de arquivo relativas a uma pasta já aberta (P-34 item 2): a pasta é um valor
//! (`Folder`), não um caminho que o sistema resolve de novo a cada chamada.
//!
//! Em Unix a pasta é um descritor aberto sem seguir links (`openat` com `O_NOFOLLOW`, componente
//! por componente) e as operações são `renameat`, `unlinkat`, `linkat` e `statat` relativas a ele:
//! depois de aberta, trocar o caminho por um link não muda onde as operações agem. Fora do Unix
//! o `Folder` ainda guarda um caminho (a janela do TOCTOU continua; criar link no Windows exige
//! privilégio).

use crate::manifest::Hash32;
use sha2::{Digest, Sha256};
use std::io::{self, Read};

pub(crate) use imp::Folder;

/// Por que `Folder::open_beneath` não abriu.
#[derive(Debug)]
pub(crate) enum OpenError {
    /// Um componente é link ou não é pasta: nunca se segue.
    NotAFolder,
    Io(io::Error),
}

/// O que `Folder::stat` sabe de um nome, sem seguir link.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Entry {
    /// Arquivo regular (não link, não pasta, não FIFO).
    pub is_file: bool,
    pub len: u64,
}

/// `sha256` do que `reader` entrega, lido em blocos.
fn hash_reader(mut reader: impl Read) -> io::Result<Hash32> {
    let mut hasher = Sha256::new();
    let mut buffer = [0u8; 64 * 1024];
    loop {
        let read = reader.read(&mut buffer)?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
    }
    Ok(Hash32(hasher.finalize().into()))
}

/// Só um nome de arquivo desta pasta: nada de `/`, `.` ou `..` (as operações são relativas a ela).
fn plain_name(name: &str) -> io::Result<&str> {
    if name.is_empty() || name == "." || name == ".." || name.contains(['/', '\0']) {
        return Err(io::ErrorKind::InvalidInput.into());
    }
    Ok(name)
}

#[cfg(unix)]
mod imp {
    use super::{hash_reader, plain_name, Entry, OpenError};
    use crate::manifest::Hash32;
    use rustix::fs::{
        fstat, linkat, openat, renameat, statat, unlinkat, AtFlags, FileType, Mode, OFlags, CWD,
    };
    use rustix::io::Errno;
    use std::fs::File;
    use std::io;
    use std::os::fd::OwnedFd;
    use std::path::{Component, Path};

    /// Uma pasta aberta. Todas as operações agem nela, não em um caminho refeito do zero.
    pub(crate) struct Folder {
        fd: OwnedFd,
    }

    impl Folder {
        /// Abre `base` (que pode passar por links: é o que o consumidor escolheu) e desce
        /// `relative` de pasta em pasta, cada uma com `O_NOFOLLOW`: um link no meio vira
        /// `NotAFolder`, nunca é seguido. `O_NOFOLLOW` só vale para o último componente de cada
        /// `openat`, por isso não se abre o caminho inteiro de uma vez.
        pub(crate) fn open_beneath(base: &Path, relative: &Path) -> Result<Folder, OpenError> {
            let dir = OFlags::RDONLY | OFlags::DIRECTORY | OFlags::CLOEXEC;
            let mut fd =
                openat(CWD, base, dir, Mode::empty()).map_err(|e| OpenError::Io(e.into()))?;
            for part in relative.components() {
                let Component::Normal(name) = part else {
                    return Err(OpenError::NotAFolder);
                };
                let next = match openat(&fd, name, dir | OFlags::NOFOLLOW, Mode::empty()) {
                    Ok(next) => next,
                    // `ELOOP` (link) ou `ENOTDIR` (não é pasta; algumas plataformas o dão para link)
                    Err(e) if e == Errno::LOOP || e == Errno::NOTDIR => {
                        return Err(OpenError::NotAFolder)
                    }
                    Err(e) => return Err(OpenError::Io(e.into())),
                };
                fd = next;
            }
            Ok(Folder { fd })
        }

        /// `rename` dentro desta pasta. Sobrescreve o destino, como o `rename(2)`.
        pub(crate) fn rename(&self, from: &str, to: &str) -> io::Result<()> {
            renameat(&self.fd, plain_name(from)?, &self.fd, plain_name(to)?).map_err(Into::into)
        }

        pub(crate) fn unlink(&self, name: &str) -> io::Result<()> {
            unlinkat(&self.fd, plain_name(name)?, AtFlags::empty()).map_err(Into::into)
        }

        /// Remove a subpasta `name` desta pasta, se estiver vazia.
        pub(crate) fn rmdir(&self, name: &str) -> io::Result<()> {
            unlinkat(&self.fd, plain_name(name)?, AtFlags::REMOVEDIR).map_err(Into::into)
        }

        /// Hard link `to` -> `from`, sem sobrescrever (`AlreadyExists`).
        pub(crate) fn link(&self, from: &str, to: &str) -> io::Result<()> {
            linkat(
                &self.fd,
                plain_name(from)?,
                &self.fd,
                plain_name(to)?,
                AtFlags::empty(),
            )
            .map_err(Into::into)
        }

        /// O que `name` é, sem seguir link. `NotFound` se não existe.
        pub(crate) fn stat(&self, name: &str) -> io::Result<Entry> {
            let st = statat(&self.fd, plain_name(name)?, AtFlags::SYMLINK_NOFOLLOW)?;
            Ok(Entry {
                is_file: FileType::from_raw_mode(st.st_mode) == FileType::RegularFile,
                len: u64::try_from(st.st_size).unwrap_or(0),
            })
        }

        /// `sha256` de um arquivo regular desta pasta. Link ou outra coisa: `InvalidInput`.
        /// Abre com `O_NOFOLLOW` e `O_NONBLOCK` (um FIFO não trava a abertura) e confere o tipo
        /// no descritor aberto, não no nome.
        pub(crate) fn sha256(&self, name: &str) -> io::Result<Hash32> {
            let flags = OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::NONBLOCK | OFlags::CLOEXEC;
            let fd = openat(&self.fd, plain_name(name)?, flags, Mode::empty()).map_err(|e| {
                if e == Errno::LOOP {
                    io::ErrorKind::InvalidInput.into()
                } else {
                    io::Error::from(e)
                }
            })?;
            if FileType::from_raw_mode(fstat(&fd)?.st_mode) != FileType::RegularFile {
                return Err(io::ErrorKind::InvalidInput.into());
            }
            hash_reader(File::from(fd))
        }
    }
}

#[cfg(not(unix))]
mod imp {
    use super::{hash_reader, plain_name, Entry, OpenError};
    use crate::manifest::Hash32;
    use std::fs;
    use std::io;
    use std::path::{Path, PathBuf};

    /// Fora do Unix: ainda um caminho (sem a garantia contra a troca por link).
    pub(crate) struct Folder {
        path: PathBuf,
    }

    impl Folder {
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
            fs::rename(
                self.path.join(plain_name(from)?),
                self.path.join(plain_name(to)?),
            )
        }

        pub(crate) fn unlink(&self, name: &str) -> io::Result<()> {
            fs::remove_file(self.path.join(plain_name(name)?))
        }

        pub(crate) fn rmdir(&self, name: &str) -> io::Result<()> {
            fs::remove_dir(self.path.join(plain_name(name)?))
        }

        pub(crate) fn link(&self, from: &str, to: &str) -> io::Result<()> {
            fs::hard_link(
                self.path.join(plain_name(from)?),
                self.path.join(plain_name(to)?),
            )
        }

        pub(crate) fn stat(&self, name: &str) -> io::Result<Entry> {
            let meta = fs::symlink_metadata(self.path.join(plain_name(name)?))?;
            Ok(Entry {
                is_file: meta.file_type().is_file(),
                len: meta.len(),
            })
        }

        pub(crate) fn sha256(&self, name: &str) -> io::Result<Hash32> {
            let path = self.path.join(plain_name(name)?);
            if !fs::symlink_metadata(&path)?.file_type().is_file() {
                return Err(io::ErrorKind::InvalidInput.into());
            }
            hash_reader(fs::File::open(path)?)
        }
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use crate::testutil::TempDir;
    use std::fs;
    use std::os::unix::fs::symlink;
    use std::path::Path;

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

    #[test]
    fn a_name_that_leaves_the_folder_is_refused() {
        let tmp = TempDir::new();
        fs::create_dir_all(tmp.path().join("sub")).unwrap();
        fs::write(tmp.path().join("fora"), b"x").unwrap();
        let folder = Folder::open_beneath(tmp.path(), Path::new("sub")).unwrap();
        for name in ["../fora", "a/b", "..", ".", ""] {
            assert_eq!(
                folder.unlink(name).unwrap_err().kind(),
                io::ErrorKind::InvalidInput,
                "{name:?}"
            );
        }
        assert!(tmp.path().join("fora").exists());
    }
}
