//! Estado local do dispositivo: o último manifest que ele aceitou (`SPEC-TECNICA.md` §6,
//! "Proteção contra manifest antigo"). Com ele, um backend que sirva só manifests velhos (ou um
//! disco restaurado de um backup antigo) é recusado com `Rollback` em vez de aceito em silêncio.
//!
//! O arquivo é local, `0600`, e **corrupção nunca vale como ausência**: tratá-la assim seria um
//! jeito de desligar a proteção.

use crate::disk::{create_private_dir, write_atomic};
use crate::engine::EngineError;
use crate::heads::equivalent_merges;
use crate::manifest::{Hash32, Manifest};
use crate::merge::reachable;
use crate::object::ObjectId;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::fs;
use std::io;
use std::path::Path;

const STATE_VERSION: u8 = 1;
/// O arquivo tem uns 120 bytes; acima disto não é nosso.
const MAX_STATE_BYTES: u64 = 4096;

/// O último manifest aceito e a época das chaves com que foi aceito.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DeviceState {
    pub key_epoch: u32,
    /// Hash do id do manifest (o mesmo valor de `ObjectId::hash`).
    pub last_accepted: Hash32,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct StateFile {
    version: u8,
    key_epoch: u32,
    last_accepted: Hash32,
}

impl DeviceState {
    /// `None` se o arquivo não existe (dispositivo que ainda não aceitou nada). Arquivo
    /// ilegível, de versão desconhecida, grande demais ou com campo a mais é `CorruptState`.
    pub fn load(path: &Path) -> Result<Option<DeviceState>, EngineError> {
        // `symlink_metadata`: um link no lugar do arquivo não é seguido, vale como corrupção
        let meta = match fs::symlink_metadata(path) {
            Ok(meta) => meta,
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(EngineError::Io(e.kind())),
        };
        if !meta.is_file() || meta.len() > MAX_STATE_BYTES {
            return Err(EngineError::CorruptState);
        }
        let bytes = fs::read(path).map_err(|e| EngineError::Io(e.kind()))?;
        let file: StateFile =
            serde_json::from_slice(&bytes).map_err(|_| EngineError::CorruptState)?;
        if file.version != STATE_VERSION {
            return Err(EngineError::CorruptState);
        }
        Ok(Some(DeviceState {
            key_epoch: file.key_epoch,
            last_accepted: file.last_accepted,
        }))
    }

    /// Grava de forma atômica (`0600`; a pasta, se faltar, `0700`).
    pub fn save(&self, path: &Path) -> Result<(), EngineError> {
        let file = StateFile {
            version: STATE_VERSION,
            key_epoch: self.key_epoch,
            last_accepted: self.last_accepted,
        };
        let json = serde_json::to_vec(&file).map_err(|_| EngineError::CorruptState)?;
        if let Some(parent) = path.parent() {
            create_private_dir(parent).map_err(|e| EngineError::Io(e.kind()))?;
        }
        write_atomic(path, &json).map_err(|e| EngineError::Io(e.kind()))
    }
}

/// O estado é satisfeito por alguma das pontas `tips` do grafo `graph`? Sem ponta nenhuma, não:
/// um armazenamento que perdeu tudo é exatamente o que a proteção existe para pegar.
pub(crate) fn satisfied(
    state: &DeviceState,
    graph: &[(ObjectId, Manifest)],
    tips: &[ObjectId],
) -> bool {
    let index = index_of(graph);
    tips.iter().any(|tip| descends_in(state, &index, tip))
}

type Index<'a> = HashMap<[u8; 32], &'a (ObjectId, Manifest)>;

fn index_of(graph: &[(ObjectId, Manifest)]) -> Index<'_> {
    graph.iter().map(|entry| (entry.0.hash, entry)).collect()
}

/// `id` continua a história que o dispositivo já aceitou? Verdadeiro se:
/// - a cadeia `base_version`/`merged_from` de `id` chega ao último aceito (o próprio `id`
///   conta; o contador não decide nada, um backend hostil poderia forjá-lo); ou
/// - `id` é de uma época **maior** que a do estado (rotação: a cadeia nova começa com
///   `base_version = null` e não descende da velha); ou
/// - o último aceito é uma fusão **equivalente** (ADR 0014) a um manifest que `id` alcança:
///   a regra de colapso escolhe a gêmea de menor hash como ponta, e o dispositivo pode ter
///   aceito a de maior hash.
pub(crate) fn descends(state: &DeviceState, graph: &[(ObjectId, Manifest)], id: &ObjectId) -> bool {
    descends_in(state, &index_of(graph), id)
}

fn descends_in(state: &DeviceState, index: &Index<'_>, id: &ObjectId) -> bool {
    let Some((_, manifest)) = index.get(&id.hash) else {
        return false;
    };
    if manifest.key_epoch > state.key_epoch {
        return true;
    }
    let target = state.last_accepted.0;
    let reached = reachable(index, id.hash);
    if reached.contains(&target) {
        return true;
    }
    let Some((_, accepted)) = index.get(&target) else {
        return false;
    };
    reached.iter().any(|hash| {
        index
            .get(hash)
            .is_some_and(|(_, other)| equivalent_merges(accepted, other))
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::TempDir;

    fn state(byte: u8) -> DeviceState {
        DeviceState {
            key_epoch: 3,
            last_accepted: Hash32([byte; 32]),
        }
    }

    #[test]
    fn a_saved_state_loads_back() {
        let dir = TempDir::new();
        let path = dir.path().join("estado/estado.json");
        state(7).save(&path).unwrap();
        assert_eq!(DeviceState::load(&path), Ok(Some(state(7))));
        // regravar troca o conteúdo
        state(8).save(&path).unwrap();
        assert_eq!(DeviceState::load(&path), Ok(Some(state(8))));
    }

    #[test]
    fn a_missing_file_means_no_state() {
        let dir = TempDir::new();
        assert_eq!(
            DeviceState::load(&dir.path().join("nao-existe.json")),
            Ok(None)
        );
    }

    #[cfg(unix)]
    #[test]
    fn the_file_is_0600_and_a_created_folder_0700() {
        use std::os::unix::fs::PermissionsExt;
        let dir = TempDir::new();
        let path = dir.path().join("estado/estado.json");
        state(1).save(&path).unwrap();
        let mode = |p: &Path| fs::metadata(p).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode(&path), 0o600);
        assert_eq!(mode(&dir.path().join("estado")), 0o700);
    }

    /// Corrupção nunca vira "sem estado", e o arquivo não é apagado nem trocado.
    #[test]
    fn anything_that_is_not_a_valid_state_is_corrupt_and_left_alone() {
        let hash = "ab".repeat(32);
        let good = format!(r#"{{"version":1,"key_epoch":1,"last_accepted":"{hash}"}}"#);
        assert!(serde_json::from_str::<serde_json::Value>(&good).is_ok());
        let cases: Vec<(&str, Vec<u8>)> = vec![
            ("vazio", Vec::new()),
            ("lixo", b"\x00\x01\x02 nao e json".to_vec()),
            ("json de outro tipo", b"[1,2,3]".to_vec()),
            (
                "versão desconhecida",
                good.replace("\"version\":1", "\"version\":2").into_bytes(),
            ),
            (
                "campo a mais",
                good.replace('}', r#","extra":1}"#).into_bytes(),
            ),
            ("campo faltando", br#"{"version":1,"key_epoch":1}"#.to_vec()),
            ("hash curto", good.replace(&hash, "abcd").into_bytes()),
            (
                "hash não hex",
                good.replace(&hash, &"zz".repeat(32)).into_bytes(),
            ),
            ("lixo no fim", format!("{good} lixo").into_bytes()),
            ("grande demais", vec![b' '; (MAX_STATE_BYTES + 1) as usize]),
        ];
        for (name, bytes) in cases {
            let dir = TempDir::new();
            let path = dir.path().join("estado.json");
            fs::write(&path, &bytes).unwrap();
            assert_eq!(
                DeviceState::load(&path),
                Err(EngineError::CorruptState),
                "{name}"
            );
            assert_eq!(fs::read(&path).unwrap(), bytes, "{name}: arquivo intacto");
        }
        // e o caso bom, escrito à mão, é aceito (o teste não passa por acaso)
        let dir = TempDir::new();
        let path = dir.path().join("estado.json");
        fs::write(&path, &good).unwrap();
        assert_eq!(
            DeviceState::load(&path),
            Ok(Some(DeviceState {
                key_epoch: 1,
                last_accepted: Hash32([0xab; 32])
            }))
        );
    }

    /// Achado I1: `with_state("estado.json")`, sem pasta. O `fsync` da pasta falhava depois do
    /// `rename`, e toda operação devolvia erro depois de ter valido.
    #[test]
    fn saving_to_a_relative_path_without_a_folder_works() {
        let mut random = [0u8; 6];
        getrandom::fill(&mut random).unwrap();
        let hex: String = random.iter().map(|b| format!("{b:02x}")).collect();
        let path = std::path::PathBuf::from(format!("arca-test-estado-{hex}.json"));
        let saved = state(1).save(&path);
        let loaded = DeviceState::load(&path);
        let _ = fs::remove_file(&path);
        assert_eq!(saved, Ok(()));
        assert_eq!(loaded, Ok(Some(state(1))));
    }

    /// Um link no lugar do arquivo não é seguido (achado S1): vale como corrupção.
    #[cfg(unix)]
    #[test]
    fn a_symlink_where_the_file_should_be_is_corrupt_not_followed() {
        let dir = TempDir::new();
        let real = dir.path().join("real.json");
        state(5).save(&real).unwrap();
        let link = dir.path().join("estado.json");
        std::os::unix::fs::symlink(&real, &link).unwrap();
        assert_eq!(DeviceState::load(&link), Err(EngineError::CorruptState));
    }

    #[test]
    fn a_folder_where_the_file_should_be_is_corrupt_not_absent() {
        let dir = TempDir::new();
        let path = dir.path().join("estado.json");
        fs::create_dir(&path).unwrap();
        assert_eq!(DeviceState::load(&path), Err(EngineError::CorruptState));
    }
}
