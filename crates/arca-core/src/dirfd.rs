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

pub(crate) use imp::{Folder, FolderLock};

/// Conta os `Folder::sync` desta thread, só nos testes: queda de energia não é testável, mas
/// "o `commit` sincronizou cada pasta" é.
#[cfg(test)]
pub(crate) mod probe {
    use std::cell::Cell;
    thread_local!(static SYNCS: Cell<usize> = const { Cell::new(0) });
    pub(crate) fn count_sync() {
        SYNCS.with(|c| c.set(c.get() + 1));
    }
    pub(crate) fn syncs() -> usize {
        SYNCS.with(Cell::get)
    }
}
#[cfg(not(test))]
mod probe {
    pub(super) fn count_sync() {}
}

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

/// O que `Folder::read_limited` leu.
#[derive(Debug)]
pub(crate) struct FileRead {
    pub bytes: Vec<u8>,
    /// O arquivo é do usuário que roda o processo e grupo e outros não têm nenhum acesso (`0o077`
    /// zerado), conferido no mesmo descritor da leitura (P-33 item 1). Fora do Unix não há como
    /// conferir e vale `true`.
    pub private: bool,
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
    use super::{hash_reader, plain_name, Entry, FileRead, OpenError};
    use crate::manifest::Hash32;
    use rustix::fs::{
        flock, fstat, fsync, linkat, mkdirat, openat, renameat, statat, unlinkat, AtFlags,
        FileType, FlockOperation, Mode, OFlags, CWD,
    };
    use rustix::io::Errno;
    use rustix::process::geteuid;
    use std::ffi::OsStr;
    use std::fs::File;
    use std::io::{self, Read, Write};
    use std::os::fd::OwnedFd;
    use std::path::{Component, Path, PathBuf};

    /// Uma pasta aberta. Todas as operações agem nela, não em um caminho refeito do zero.
    pub(crate) struct Folder {
        fd: OwnedFd,
    }

    /// A trava de uma pasta (`Folder::lock`): vale enquanto este valor existir.
    #[derive(Debug)]
    pub(crate) struct FolderLock {
        _fd: OwnedFd,
    }

    const DIR: OFlags = OFlags::RDONLY
        .union(OFlags::DIRECTORY)
        .union(OFlags::CLOEXEC);

    fn sync_fd(fd: &OwnedFd) -> io::Result<()> {
        super::probe::count_sync();
        match fsync(fd) {
            Ok(()) => Ok(()),
            Err(e) if e == Errno::INVAL => Ok(()),
            Err(e) => Err(e.into()),
        }
    }

    /// A pasta de partida: segue links (é o que o consumidor escolheu).
    fn open_base(base: &Path) -> Result<OwnedFd, OpenError> {
        openat(CWD, base, DIR, Mode::empty()).map_err(|e| OpenError::Io(e.into()))
    }

    /// Uma subpasta de `fd`, sem seguir link.
    fn open_subfolder(fd: &OwnedFd, name: &OsStr) -> Result<OwnedFd, OpenError> {
        match openat(fd, name, DIR.union(OFlags::NOFOLLOW), Mode::empty()) {
            Ok(next) => Ok(next),
            // `ELOOP` (link) ou `ENOTDIR` (não é pasta; algumas plataformas o dão para link)
            Err(e) if e == Errno::LOOP || e == Errno::NOTDIR => Err(OpenError::NotAFolder),
            Err(e) => Err(OpenError::Io(e.into())),
        }
    }

    impl Folder {
        /// Abre `base` (que pode passar por links: é o que o consumidor escolheu) e desce
        /// `relative` de pasta em pasta, cada uma com `O_NOFOLLOW`: um link no meio vira
        /// `NotAFolder`, nunca é seguido. `O_NOFOLLOW` só vale para o último componente de cada
        /// `openat`, por isso não se abre o caminho inteiro de uma vez.
        pub(crate) fn open_beneath(base: &Path, relative: &Path) -> Result<Folder, OpenError> {
            let mut fd = open_base(base)?;
            for part in relative.components() {
                let Component::Normal(name) = part else {
                    return Err(OpenError::NotAFolder);
                };
                fd = open_subfolder(&fd, name)?;
            }
            Ok(Folder { fd })
        }

        /// Como `open_beneath`, mas cria o que falta (`mkdirat`, `0o700`). `base` tem de existir.
        /// Cada pasta criada entra em `created` (caminho completo) **na hora**, mesmo que uma
        /// seguinte falhe, para quem chama poder desfazer. Um link no meio do caminho é recusado
        /// (`NotAFolder`) e nada é criado através dele.
        pub(crate) fn create_beneath(
            base: &Path,
            relative: &Path,
            created: &mut Vec<PathBuf>,
        ) -> Result<Folder, OpenError> {
            let mut fd = open_base(base)?;
            let mut walk = base.to_path_buf();
            for part in relative.components() {
                let Component::Normal(name) = part else {
                    return Err(OpenError::NotAFolder);
                };
                walk.push(name);
                fd = match open_subfolder(&fd, name) {
                    Err(OpenError::Io(e)) if e.kind() == io::ErrorKind::NotFound => {
                        match mkdirat(&fd, name, Mode::RWXU) {
                            Ok(()) => {
                                created.push(walk.clone());
                                // a entrada da pasta nova precisa ser durável no pai (P-33 item 3)
                                sync_fd(&fd).map_err(OpenError::Io)?;
                            }
                            // outro processo criou no meio-tempo: reabre e confere abaixo
                            Err(e) if e == Errno::EXIST => {}
                            Err(e) => return Err(OpenError::Io(e.into())),
                        }
                        open_subfolder(&fd, name)?
                    }
                    other => other?,
                };
            }
            Ok(Folder { fd })
        }

        /// Grava `bytes` em `name` sem nunca deixar um arquivo pela metade: temporário
        /// `.tmp-<hex>` nesta pasta (`O_EXCL`, `O_NOFOLLOW`, `0o600`), `fsync`, `renameat` e
        /// `fsync` da pasta. Se falhar antes do `rename`, o temporário é apagado. Sobrescreve o
        /// destino, como o `rename(2)`.
        pub(crate) fn write_atomic(&self, name: &str, bytes: &[u8]) -> io::Result<()> {
            let name = plain_name(name)?;
            let mut random = [0u8; 8];
            getrandom::fill(&mut random).map_err(|_| io::Error::from(io::ErrorKind::Other))?;
            let suffix: String = random.iter().map(|b| format!("{b:02x}")).collect();
            let tmp = format!(".tmp-{suffix}");
            let result = (|| -> io::Result<()> {
                let flags = OFlags::WRONLY
                    | OFlags::CREATE
                    | OFlags::EXCL
                    | OFlags::NOFOLLOW
                    | OFlags::CLOEXEC;
                let fd = openat(&self.fd, tmp.as_str(), flags, Mode::RUSR | Mode::WUSR)?;
                let mut file = File::from(fd);
                file.write_all(bytes)?;
                file.sync_all()?;
                renameat(&self.fd, tmp.as_str(), &self.fd, name)?;
                Ok(())
            })();
            if let Err(e) = result {
                let _ = unlinkat(&self.fd, tmp.as_str(), AtFlags::empty());
                return Err(e);
            }
            fsync(&self.fd)?;
            Ok(())
        }

        /// Trava a pasta para este processo e devolve a guarda (P-33 item 7): a trava é solta
        /// quando a guarda sai de escopo ou quando o processo morre. `WouldBlock` se outra abertura
        /// da mesma pasta (deste ou de outro processo) já a tem. Não espera.
        pub(crate) fn lock(self) -> io::Result<FolderLock> {
            // `flock` é por descrição de arquivo aberto: esta abertura conflita com qualquer outra
            // da mesma pasta, até no mesmo processo; o kernel a solta no `close` ou se o processo
            // morre. `EWOULDBLOCK` vira `ErrorKind::WouldBlock`.
            flock(&self.fd, FlockOperation::NonBlockingLockExclusive)?;
            Ok(FolderLock { _fd: self.fd })
        }

        /// `fsync` da própria pasta: torna duráveis as entradas criadas, renomeadas ou apagadas
        /// nela (o `fsync` de um arquivo não persiste o nome dele na pasta). `EINVAL` (sistema de
        /// arquivos que não sincroniza pasta) vale como ok; qualquer outro erro sobe.
        pub(crate) fn sync(&self) -> io::Result<()> {
            sync_fd(&self.fd)
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

        /// Lê um arquivo regular desta pasta com teto de `max` bytes. Abre com `O_NOFOLLOW` e
        /// `O_NONBLOCK` e confere tipo e tamanho no descritor aberto, não no nome (P-33 item 6);
        /// a leitura para em `max + 1`, então um arquivo que cresce depois da conferência também
        /// é recusado. Link, FIFO, pasta ou maior que `max`: `InvalidInput`. Também diz se o
        /// arquivo é privado do dono (`FileRead::private`).
        pub(crate) fn read_limited(&self, name: &str, max: u64) -> io::Result<FileRead> {
            let flags = OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::NONBLOCK | OFlags::CLOEXEC;
            let fd = openat(&self.fd, plain_name(name)?, flags, Mode::empty()).map_err(|e| {
                if e == Errno::LOOP {
                    io::ErrorKind::InvalidInput.into()
                } else {
                    io::Error::from(e)
                }
            })?;
            let st = fstat(&fd)?;
            let size = u64::try_from(st.st_size).unwrap_or(u64::MAX);
            if FileType::from_raw_mode(st.st_mode) != FileType::RegularFile || size > max {
                return Err(io::ErrorKind::InvalidInput.into());
            }
            let mut bytes = Vec::new();
            File::from(fd)
                .take(max.saturating_add(1))
                .read_to_end(&mut bytes)?;
            if bytes.len() as u64 > max {
                return Err(io::ErrorKind::InvalidInput.into());
            }
            let private = is_private(
                st.st_uid,
                geteuid().as_raw(),
                Mode::from_raw_mode(st.st_mode),
            );
            Ok(FileRead { bytes, private })
        }
    }

    /// O arquivo é de `euid` e grupo e outros não têm nenhum acesso. Quem não é o dono só cria
    /// arquivo com outro dono, então isso diz que quem gravou foi este usuário.
    pub(super) fn is_private(owner: u32, euid: u32, mode: Mode) -> bool {
        owner == euid && !mode.intersects(Mode::RWXG | Mode::RWXO)
    }
}

#[cfg(not(unix))]
mod imp {
    use super::{hash_reader, plain_name, Entry, FileRead, OpenError};
    use crate::disk::{create_private_dir, write_atomic};
    use crate::manifest::Hash32;
    use std::fs;
    use std::io::{self, Read};
    use std::path::{Path, PathBuf};

    /// Fora do Unix: ainda um caminho (sem a garantia contra a troca por link).
    pub(crate) struct Folder {
        path: PathBuf,
    }

    /// Fora do Unix não há trava de pasta: a guarda existe, mas não impede nada.
    #[derive(Debug)]
    pub(crate) struct FolderLock;

    impl Folder {
        pub(crate) fn lock(self) -> io::Result<FolderLock> {
            Ok(FolderLock)
        }

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

        pub(crate) fn create_beneath(
            base: &Path,
            relative: &Path,
            created: &mut Vec<PathBuf>,
        ) -> Result<Folder, OpenError> {
            let mut walk = base.to_path_buf();
            if let Err(e) = fs::metadata(base) {
                return Err(OpenError::Io(e));
            }
            for part in relative.components() {
                walk.push(part);
                match fs::symlink_metadata(&walk) {
                    Ok(meta) if meta.file_type().is_dir() => {}
                    Ok(_) => return Err(OpenError::NotAFolder),
                    Err(e) if e.kind() == io::ErrorKind::NotFound => {
                        create_private_dir(&walk).map_err(OpenError::Io)?;
                        created.push(walk.clone());
                    }
                    Err(e) => return Err(OpenError::Io(e)),
                }
            }
            Ok(Folder { path: walk })
        }

        pub(crate) fn write_atomic(&self, name: &str, bytes: &[u8]) -> io::Result<()> {
            write_atomic(&self.path.join(plain_name(name)?), bytes)
        }

        /// Fora do Unix não há descritor de pasta para sincronizar: sem efeito.
        pub(crate) fn sync(&self) -> io::Result<()> {
            super::probe::count_sync();
            Ok(())
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

        /// Fora do Unix: confere pelo nome e lê (sem a garantia contra a troca por link).
        pub(crate) fn read_limited(&self, name: &str, max: u64) -> io::Result<FileRead> {
            let path = self.path.join(plain_name(name)?);
            let meta = fs::symlink_metadata(&path)?;
            if !meta.file_type().is_file() || meta.len() > max {
                return Err(io::ErrorKind::InvalidInput.into());
            }
            let mut bytes = Vec::new();
            fs::File::open(path)?
                .take(max.saturating_add(1))
                .read_to_end(&mut bytes)?;
            if bytes.len() as u64 > max {
                return Err(io::ErrorKind::InvalidInput.into());
            }
            Ok(FileRead {
                bytes,
                private: true,
            })
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
    fn create_beneath_makes_the_missing_folders_0700_and_reports_them() {
        use std::os::unix::fs::PermissionsExt;
        let tmp = TempDir::new();
        fs::create_dir_all(tmp.path().join("a")).unwrap();
        let mut created = Vec::new();
        let folder = Folder::create_beneath(tmp.path(), Path::new("a/b/c"), &mut created).unwrap();
        // `a` já existia: só `b` e `c` foram criadas
        assert_eq!(
            created,
            vec![tmp.path().join("a/b"), tmp.path().join("a/b/c")]
        );
        for dir in &created {
            let mode = fs::metadata(dir).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode, 0o700, "{dir:?}");
        }
        folder.write_atomic("x", b"1").unwrap();
        assert_eq!(fs::read(tmp.path().join("a/b/c/x")).unwrap(), b"1");
        // de novo: nada novo a criar
        let mut again = Vec::new();
        Folder::create_beneath(tmp.path(), Path::new("a/b/c"), &mut again).unwrap();
        assert!(again.is_empty());
    }

    /// P-33 item 7: uma pasta só tem uma trava por vez, entre aberturas diferentes (o que vale
    /// também entre processos); soltar a guarda libera.
    #[test]
    fn a_second_lock_on_the_same_folder_would_block_until_the_first_is_dropped() {
        let tmp = TempDir::new();
        let open = || Folder::open_beneath(tmp.path(), Path::new("")).unwrap();
        let first = open().lock().unwrap();
        let err = open().lock().unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::WouldBlock);
        drop(first);
        let again = open().lock();
        assert!(again.is_ok());
    }

    #[test]
    fn sync_works_on_an_opened_folder_and_create_beneath_syncs_each_parent() {
        let tmp = TempDir::new();
        let folder = Folder::open_beneath(tmp.path(), Path::new("")).unwrap();
        folder.sync().unwrap();
        let before = probe::syncs();
        let mut created = Vec::new();
        Folder::create_beneath(tmp.path(), Path::new("a/b"), &mut created).unwrap();
        // duas pastas criadas, o pai de cada uma sincronizado
        assert_eq!(probe::syncs() - before, 2);
    }

    #[test]
    fn create_beneath_refuses_a_link_and_creates_nothing_through_it() {
        let tmp = TempDir::new();
        let outside = TempDir::new();
        symlink(outside.path(), tmp.path().join("sub")).unwrap();
        let mut created = Vec::new();
        let r = Folder::create_beneath(tmp.path(), Path::new("sub/novo/fundo"), &mut created);
        assert!(matches!(r, Err(OpenError::NotAFolder)));
        assert!(created.is_empty());
        assert_eq!(fs::read_dir(outside.path()).unwrap().count(), 0);
    }

    /// A propriedade do TOCTOU para escrever: depois de aberta, a pasta recebe o arquivo mesmo que
    /// o caminho dela passe a apontar para outro lugar.
    #[test]
    fn write_atomic_on_an_opened_folder_ignores_a_path_swapped_for_a_link() {
        let tmp = TempDir::new();
        let outside = TempDir::new();
        fs::create_dir_all(tmp.path().join("sub")).unwrap();
        let mut created = Vec::new();
        let folder = Folder::create_beneath(tmp.path(), Path::new("sub"), &mut created).unwrap();
        fs::rename(tmp.path().join("sub"), tmp.path().join("sub.real")).unwrap();
        symlink(outside.path(), tmp.path().join("sub")).unwrap();

        folder.write_atomic("novo.md", b"do manifest").unwrap();

        assert_eq!(
            fs::read(tmp.path().join("sub.real/novo.md")).unwrap(),
            b"do manifest"
        );
        assert_eq!(fs::read_dir(outside.path()).unwrap().count(), 0);
    }

    #[test]
    fn write_atomic_is_0600_replaces_the_destination_and_leaves_no_temporary() {
        use std::os::unix::fs::PermissionsExt;
        let tmp = TempDir::new();
        let folder = Folder::open_beneath(tmp.path(), Path::new("")).unwrap();
        folder.write_atomic("a", b"um").unwrap();
        folder.write_atomic("a", b"dois").unwrap();
        assert_eq!(fs::read(tmp.path().join("a")).unwrap(), b"dois");
        let mode = fs::metadata(tmp.path().join("a"))
            .unwrap()
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o600);
        assert_eq!(fs::read_dir(tmp.path()).unwrap().count(), 1);
    }

    /// Um FIFO no lugar do arquivo não pode travar a leitura (abrir um FIFO sem `O_NONBLOCK`
    /// espera um escritor para sempre).
    #[test]
    fn sha256_of_a_fifo_is_refused_instead_of_blocking() {
        use rustix::fs::{mknodat, FileType, Mode, CWD};
        let tmp = TempDir::new();
        mknodat(
            CWD,
            tmp.path().join("fifo"),
            FileType::Fifo,
            Mode::RUSR | Mode::WUSR,
            0,
        )
        .unwrap();
        let folder = Folder::open_beneath(tmp.path(), Path::new("")).unwrap();
        let err = folder.sha256("fifo").unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::InvalidInput);
    }

    #[test]
    fn read_limited_reads_up_to_the_limit_and_refuses_one_byte_more() {
        let tmp = TempDir::new();
        fs::write(tmp.path().join("ok"), b"12345").unwrap();
        fs::write(tmp.path().join("grande"), b"123456").unwrap();
        let folder = Folder::open_beneath(tmp.path(), Path::new("")).unwrap();
        assert_eq!(folder.read_limited("ok", 5).unwrap().bytes, b"12345");
        assert_eq!(
            folder.read_limited("grande", 5).unwrap_err().kind(),
            io::ErrorKind::InvalidInput
        );
        assert_eq!(
            folder.read_limited("nao-existe", 5).unwrap_err().kind(),
            io::ErrorKind::NotFound
        );
    }

    /// P-33 item 1: dono e modo vêm do descritor lido. O dono só se testa pela função pura (criar
    /// arquivo de outro usuário pede privilégio).
    #[test]
    fn is_private_needs_our_owner_and_no_access_for_group_or_others() {
        use super::imp::is_private;
        use rustix::fs::Mode;
        let (me, other) = (1000, 1001);
        assert!(is_private(me, me, Mode::RUSR | Mode::WUSR));
        assert!(is_private(me, me, Mode::RUSR));
        assert!(!is_private(other, me, Mode::RUSR | Mode::WUSR));
        for loose in [Mode::RGRP, Mode::WGRP, Mode::ROTH, Mode::WOTH, Mode::XOTH] {
            assert!(!is_private(me, me, Mode::RUSR | Mode::WUSR | loose));
        }
    }

    #[test]
    fn read_limited_reports_whether_the_file_is_private() {
        use std::os::unix::fs::PermissionsExt;
        let tmp = TempDir::new();
        fs::write(tmp.path().join("a"), b"x").unwrap();
        let folder = Folder::open_beneath(tmp.path(), Path::new("")).unwrap();
        for (mode, private) in [(0o600, true), (0o400, true), (0o644, false), (0o660, false)] {
            fs::set_permissions(tmp.path().join("a"), fs::Permissions::from_mode(mode)).unwrap();
            assert_eq!(
                folder.read_limited("a", 8).unwrap().private,
                private,
                "{mode:o}"
            );
        }
    }

    #[test]
    fn read_limited_refuses_a_link_and_never_follows_it() {
        let tmp = TempDir::new();
        let outside = TempDir::new();
        fs::write(outside.path().join("alvo"), b"segredo").unwrap();
        symlink(outside.path().join("alvo"), tmp.path().join("cop")).unwrap();
        let folder = Folder::open_beneath(tmp.path(), Path::new("")).unwrap();
        let err = folder.read_limited("cop", 1024).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::InvalidInput);
    }

    /// Um FIFO no lugar do arquivo não pode travar a leitura.
    #[test]
    fn read_limited_of_a_fifo_or_a_folder_is_refused_instead_of_blocking() {
        use rustix::fs::{mknodat, FileType, Mode, CWD};
        let tmp = TempDir::new();
        mknodat(
            CWD,
            tmp.path().join("fifo"),
            FileType::Fifo,
            Mode::RUSR | Mode::WUSR,
            0,
        )
        .unwrap();
        fs::create_dir(tmp.path().join("pasta")).unwrap();
        let folder = Folder::open_beneath(tmp.path(), Path::new("")).unwrap();
        for name in ["fifo", "pasta"] {
            let err = folder.read_limited(name, 1024).unwrap_err();
            assert_eq!(err.kind(), io::ErrorKind::InvalidInput, "{name}");
        }
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
