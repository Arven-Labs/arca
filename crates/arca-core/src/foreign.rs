//! Épocas estrangeiras já reconhecidas (ADR 0018, P-47a): o que a pessoa viu e decidiu ignorar.
//!
//! Um manifest de época **mais nova** que a das chaves do dispositivo não pode ser aberto e o
//! cabeçalho não é autenticado: pode ser a rotação de verdade ou um objeto forjado por quem tem
//! a chave velha. Enquanto houver um não reconhecido, o `Engine` recusa com `NeedsNewKey`. O
//! reconhecimento é por `(época, id)`, nunca pela época sozinha: a próxima época é previsível, e
//! um objeto forjado já reconhecido não pode esconder a rotação verdadeira, que tem ids novos.
//!
//! O arquivo é local, `0600`, ao lado do arquivo de estado (e não dentro dele: o estado diz até
//! onde o dispositivo aceitou, e o `remember` o regrava a cada `snapshot`). **Corrupção nunca vale
//! como "ninguém reconhecido"**, para não desligar o bloqueio por um arquivo estragado.

use crate::disk::{create_private_dir, write_atomic};
use crate::engine::EngineError;
use crate::manifest::Hash32;
use crate::object::ObjectId;
use serde::{Deserialize, Serialize};
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

const FOREIGN_VERSION: u8 = 1;
/// Quantos reconhecimentos o arquivo guarda; passou disso, sai o mais antigo (nunca é erro).
pub(crate) const MAX_ACKS: usize = 256;
/// Com o teto de 256, o arquivo tem uns 25 KiB; acima disto não é nosso.
const MAX_FOREIGN_BYTES: u64 = 64 * 1024;

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Ack {
    epoch: u32,
    id: Hash32,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct ForeignFile {
    version: u8,
    acknowledged: Vec<Ack>,
    /// Vistos pelo `pull` e ainda não reconhecidos (P-47b). Ausente nos arquivos do 47a.
    #[serde(default)]
    pending: Vec<Ack>,
}

/// Os `(época, id)` reconhecidos, do mais antigo para o mais novo, e os pendentes (vistos pelo
/// `pull`, ainda não reconhecidos). Sem `with_state` vive só na memória do `Engine`.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub(crate) struct ForeignAcks {
    entries: Vec<(u32, Hash32)>,
    pending: Vec<(u32, Hash32)>,
}

/// O arquivo de reconhecidos: `<nome do estado>.foreign`, ao lado dele.
pub(crate) fn path(state: &Path) -> PathBuf {
    let mut name = state.file_name().unwrap_or_default().to_os_string();
    name.push(".foreign");
    state.with_file_name(name)
}

impl ForeignAcks {
    /// Lista vazia se o arquivo não existe. Ilegível, de versão desconhecida, com campo a mais,
    /// grande demais, com mais do que o teto, link ou pasta no lugar: `CorruptState`.
    pub(crate) fn load(state: &Path) -> Result<ForeignAcks, EngineError> {
        let file = path(state);
        // `symlink_metadata`: um link no lugar do arquivo não é seguido, vale como corrupção
        let meta = match fs::symlink_metadata(&file) {
            Ok(meta) => meta,
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(ForeignAcks::default()),
            Err(e) => return Err(EngineError::Io(e.kind())),
        };
        if !meta.is_file() || meta.len() > MAX_FOREIGN_BYTES {
            return Err(EngineError::CorruptState);
        }
        let bytes = fs::read(&file).map_err(|e| EngineError::Io(e.kind()))?;
        let parsed: ForeignFile =
            serde_json::from_slice(&bytes).map_err(|_| EngineError::CorruptState)?;
        if parsed.version != FOREIGN_VERSION
            || parsed.acknowledged.len() > MAX_ACKS
            || parsed.pending.len() > MAX_ACKS
        {
            return Err(EngineError::CorruptState);
        }
        let pairs = |list: &[Ack]| list.iter().map(|a| (a.epoch, a.id)).collect();
        Ok(ForeignAcks {
            entries: pairs(&parsed.acknowledged),
            pending: pairs(&parsed.pending),
        })
    }

    /// Grava de forma atômica (`0600`; a pasta, se faltar, `0700`).
    pub(crate) fn save(&self, state: &Path) -> Result<(), EngineError> {
        let file = path(state);
        let acks = |list: &[(u32, Hash32)]| {
            list.iter()
                .map(|(epoch, id)| Ack {
                    epoch: *epoch,
                    id: *id,
                })
                .collect()
        };
        let body = ForeignFile {
            version: FOREIGN_VERSION,
            acknowledged: acks(&self.entries),
            pending: acks(&self.pending),
        };
        let json = serde_json::to_vec(&body).map_err(|_| EngineError::CorruptState)?;
        if let Some(parent) = file.parent() {
            create_private_dir(parent).map_err(|e| EngineError::Io(e.kind()))?;
        }
        write_atomic(&file, &json).map_err(|e| EngineError::Io(e.kind()))
    }

    /// O par `(época, id)` foi reconhecido? Só o par: nem a época nem o id sozinhos bastam.
    pub(crate) fn contains(&self, epoch: u32, id: &ObjectId) -> bool {
        self.entries
            .iter()
            .any(|(e, hash)| *e == epoch && hash.0 == id.hash)
    }

    /// Reconhece o par (sem repetir). Passou do teto, sai o mais antigo.
    pub(crate) fn add(&mut self, epoch: u32, id: &ObjectId) {
        if self.contains(epoch, id) {
            return;
        }
        self.entries.push((epoch, Hash32(id.hash)));
        if self.entries.len() > MAX_ACKS {
            let extra = self.entries.len() - MAX_ACKS;
            self.entries.drain(..extra);
        }
    }

    /// Reconhece o par e o tira dos pendentes.
    pub(crate) fn acknowledge(&mut self, epoch: u32, id: &ObjectId) {
        self.add(epoch, id);
        self.pending
            .retain(|(e, hash)| !(*e == epoch && hash.0 == id.hash));
    }

    /// Anota um par visto pelo `pull` e ainda não reconhecido. `false` se **não coube** (teto de
    /// `MAX_ACKS`): o chamador não deve dá-lo como visto, para a próxima rodada trazê-lo de
    /// volta (nunca vira erro, e nunca desliga o bloqueio dos que já estão anotados). `true` se
    /// anotou ou se o par já estava anotado ou reconhecido.
    pub(crate) fn add_pending(&mut self, epoch: u32, id: &ObjectId) -> bool {
        if self.contains(epoch, id) || self.pending_contains(epoch, id) {
            return true;
        }
        if self.pending.len() >= MAX_ACKS {
            return false;
        }
        self.pending.push((epoch, Hash32(id.hash)));
        true
    }

    fn pending_contains(&self, epoch: u32, id: &ObjectId) -> bool {
        self.pending
            .iter()
            .any(|(e, hash)| *e == epoch && hash.0 == id.hash)
    }

    /// Tira os pendentes de época `<= mine`: o dispositivo já a alcançou (re-pareou).
    pub(crate) fn drop_pending_up_to(&mut self, mine: u32) {
        self.pending.retain(|(epoch, _)| *epoch > mine);
    }

    /// Os pendentes de época **maior** que `mine`, ainda não reconhecidos.
    pub(crate) fn pending_above(&self, mine: u32) -> Vec<(u32, Hash32)> {
        self.pending
            .iter()
            .filter(|(epoch, _)| *epoch > mine)
            .copied()
            .collect()
    }
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

    #[test]
    fn a_file_written_by_the_47a_without_pending_still_loads_with_none() {
        let tmp = TempDir::new();
        let state = state_in(&tmp);
        fs::write(path(&state), r#"{"version":1,"acknowledged":[]}"#).unwrap();
        assert_eq!(ForeignAcks::load(&state).unwrap(), ForeignAcks::default());
    }

    #[test]
    fn the_pending_round_trips_and_acknowledging_moves_it_to_the_acknowledged() {
        let tmp = TempDir::new();
        let state = state_in(&tmp);
        let mut acks = ForeignAcks::default();
        assert!(acks.add_pending(2, &id(1)));
        assert!(acks.add_pending(3, &id(2)));
        acks.save(&state).unwrap();
        let mut loaded = ForeignAcks::load(&state).unwrap();
        assert_eq!(loaded, acks);
        assert_eq!(loaded.pending_above(1).len(), 2);

        loaded.acknowledge(2, &id(1));
        assert!(loaded.contains(2, &id(1)));
        assert_eq!(loaded.pending_above(1), vec![(3, Hash32([2; 32]))]);
        // época e id juntos: reconhecer (2, id 2) não tira o pendente (3, id 2)
        loaded.acknowledge(2, &id(2));
        assert_eq!(loaded.pending_above(1).len(), 1);
    }

    #[test]
    fn pending_past_the_cap_is_refused_without_error_and_keeps_the_ones_already_noted() {
        let mut acks = ForeignAcks::default();
        for n in 0..MAX_ACKS {
            let mut hash = [0u8; 32];
            hash[..8].copy_from_slice(&(n as u64).to_le_bytes());
            let id = ObjectId {
                kind: ObjectKind::Manifest,
                hash,
            };
            assert!(acks.add_pending(2, &id));
        }
        assert!(!acks.add_pending(2, &id(0xFF)));
        assert_eq!(acks.pending_above(1).len(), MAX_ACKS);
        // já anotado ou reconhecido continua "cabendo"
        assert!(acks.add_pending(
            2,
            &ObjectId {
                kind: ObjectKind::Manifest,
                hash: [0u8; 32]
            }
        ));
    }

    #[test]
    fn pending_at_or_below_the_device_epoch_is_dropped() {
        let mut acks = ForeignAcks::default();
        acks.add_pending(2, &id(1));
        acks.add_pending(3, &id(2));
        acks.drop_pending_up_to(2);
        assert_eq!(acks.pending_above(0), vec![(3, Hash32([2; 32]))]);
    }
}
