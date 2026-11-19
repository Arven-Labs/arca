//! Épocas estrangeiras já reconhecidas (ADR 0018, P-47a): o que a pessoa viu e decidiu ignorar.
//!
//! Esboço do commit de testes: a implementação vem no commit seguinte.
#![allow(dead_code)]

use crate::engine::EngineError;
use crate::manifest::Hash32;
use crate::object::ObjectId;
use std::path::{Path, PathBuf};

/// Quantos reconhecimentos o arquivo guarda; passou disso, sai o mais antigo.
pub(crate) const MAX_ACKS: usize = 256;

/// Os `(época, id)` reconhecidos, do mais antigo para o mais novo.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub(crate) struct ForeignAcks {
    entries: Vec<(u32, Hash32)>,
}

/// O arquivo de reconhecidos: `<nome do estado>.foreign`, ao lado dele.
pub(crate) fn path(state: &Path) -> PathBuf {
    let mut name = state.file_name().unwrap_or_default().to_os_string();
    name.push(".foreign");
    state.with_file_name(name)
}

impl ForeignAcks {
    pub(crate) fn load(_state: &Path) -> Result<ForeignAcks, EngineError> {
        Ok(ForeignAcks::default())
    }

    pub(crate) fn save(&self, _state: &Path) -> Result<(), EngineError> {
        Ok(())
    }

    pub(crate) fn contains(&self, _epoch: u32, _id: &ObjectId) -> bool {
        false
    }

    pub(crate) fn add(&mut self, _epoch: u32, _id: &ObjectId) {}
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::object::ObjectKind;
    use crate::testutil::TempDir;
    use std::fs;

    fn id(n: u8) -> ObjectId {
        ObjectId {
            kind: ObjectKind::Manifest,
            hash: [n; 32],
        }
    }

    fn state_in(tmp: &TempDir) -> PathBuf {
        tmp.path().join("estado.json")
    }

    #[test]
    fn the_file_sits_beside_the_state_file() {
        assert_eq!(
            path(Path::new("/x/estado.json")),
            Path::new("/x/estado.json.foreign")
        );
    }

    #[test]
    fn a_missing_file_is_an_empty_list() {
        let tmp = TempDir::new();
        let acks = ForeignAcks::load(&state_in(&tmp)).unwrap();
        assert_eq!(acks, ForeignAcks::default());
        assert!(!acks.contains(2, &id(1)));
    }

    #[test]
    fn what_was_saved_is_loaded_and_matches_epoch_and_id_together() {
        let tmp = TempDir::new();
        let state = state_in(&tmp);
        let mut acks = ForeignAcks::default();
        acks.add(2, &id(1));
        acks.add(5, &id(2));
        acks.save(&state).unwrap();

        let loaded = ForeignAcks::load(&state).unwrap();
        assert_eq!(loaded, acks);
        assert!(loaded.contains(2, &id(1)));
        assert!(loaded.contains(5, &id(2)));
        // a época sozinha ou o id sozinho não bastam (a próxima época é previsível)
        assert!(!loaded.contains(2, &id(2)));
        assert!(!loaded.contains(3, &id(1)));
    }

    #[cfg(unix)]
    #[test]
    fn the_file_is_private_to_the_owner() {
        use std::os::unix::fs::PermissionsExt;
        let tmp = TempDir::new();
        let state = state_in(&tmp);
        let mut acks = ForeignAcks::default();
        acks.add(2, &id(1));
        acks.save(&state).unwrap();
        let mode = fs::metadata(path(&state)).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o600);
    }

    #[test]
    fn adding_the_same_pair_twice_keeps_one() {
        let mut acks = ForeignAcks::default();
        acks.add(2, &id(1));
        acks.add(2, &id(1));
        assert_eq!(acks.entries.len(), 1);
    }

    /// Passou do teto: sai o mais antigo, nunca vira erro.
    #[test]
    fn past_the_cap_the_oldest_leave_and_the_newest_stay() {
        let mut acks = ForeignAcks::default();
        for n in 0..300u32 {
            let mut hash = [0u8; 32];
            hash[..4].copy_from_slice(&n.to_be_bytes());
            acks.add(
                2,
                &ObjectId {
                    kind: ObjectKind::Manifest,
                    hash,
                },
            );
        }
        assert_eq!(acks.entries.len(), MAX_ACKS);
        let at = |n: u32| {
            let mut hash = [0u8; 32];
            hash[..4].copy_from_slice(&n.to_be_bytes());
            ObjectId {
                kind: ObjectKind::Manifest,
                hash,
            }
        };
        assert!(!acks.contains(2, &at(0)));
        assert!(!acks.contains(2, &at(43)));
        assert!(acks.contains(2, &at(44)));
        assert!(acks.contains(2, &at(299)));
    }

    /// Ilegível, de versão desconhecida, com campo a mais, grande demais, link ou pasta no lugar:
    /// nunca vale como "ninguém reconhecido" (isso desligaria o bloqueio ao contrário).
    #[test]
    fn a_bad_file_is_corrupt_and_never_counts_as_empty() {
        let hex = "ab".repeat(32);
        let good = format!(r#"{{"version":1,"acknowledged":[{{"epoch":2,"id":"{hex}"}}]}}"#);
        let cases: Vec<(&str, Vec<u8>)> = vec![
            ("lixo", b"nao e json".to_vec()),
            ("versao", br#"{"version":2,"acknowledged":[]}"#.to_vec()),
            (
                "campo a mais",
                br#"{"version":1,"acknowledged":[],"extra":1}"#.to_vec(),
            ),
            ("grande demais", vec![b' '; 64 * 1024 + 1]),
        ];
        for (name, bytes) in cases {
            let tmp = TempDir::new();
            let state = state_in(&tmp);
            fs::write(path(&state), bytes).unwrap();
            assert_eq!(
                ForeignAcks::load(&state),
                Err(EngineError::CorruptState),
                "{name}"
            );
        }

        // o arquivo certo ainda carrega (o teste não recusa tudo)
        let tmp = TempDir::new();
        let state = state_in(&tmp);
        fs::write(path(&state), good).unwrap();
        assert_eq!(ForeignAcks::load(&state).unwrap().entries.len(), 1);
    }

    #[cfg(unix)]
    #[test]
    fn a_link_or_a_folder_in_place_of_the_file_is_corrupt() {
        let tmp = TempDir::new();
        let state = state_in(&tmp);
        let elsewhere = tmp.path().join("fora");
        fs::write(&elsewhere, r#"{"version":1,"acknowledged":[]}"#).unwrap();
        std::os::unix::fs::symlink(&elsewhere, path(&state)).unwrap();
        assert_eq!(ForeignAcks::load(&state), Err(EngineError::CorruptState));

        let tmp = TempDir::new();
        let state = state_in(&tmp);
        fs::create_dir(path(&state)).unwrap();
        assert_eq!(ForeignAcks::load(&state), Err(EngineError::CorruptState));
    }
}
