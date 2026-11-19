//! Descobrir a época de uma `VaultKey` olhando os manifests do armazenamento (ADR 0018, item 7).
//!
//! A frase BIP-39 e o pareamento entregam só a `VaultKey`; a época se descobre, não se transporta.

use crate::crypto::{EpochKeys, VaultKey};
use crate::engine::ForeignEpoch;
use crate::envelope::Header;
use crate::manifest::Manifest;
use crate::object::ObjectKind;
use crate::store::{verified_get, BlobStore, StoreError};

/// A época em que a chave abre, e as épocas mais novas que ela não abre.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Discovery {
    /// A maior época em que algum manifest abre com a chave.
    pub epoch: u32,
    /// Manifests de época **maior** que `epoch` e que não abriram, em ordem `(época, hash)`.
    /// É uma dica não autenticada (ADR 0018, item 3): pode ser a rotação de verdade ou um
    /// objeto forjado.
    pub newer: Vec<ForeignEpoch>,
}

/// Por que a época não foi descoberta.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum DiscoverError {
    /// Nenhum manifest no armazenamento: é uma Vault nova, e a época inicial é do consumidor.
    #[error("storage has no manifests")]
    Empty,
    /// Há manifests, mas nenhum abre com esta chave (chave errada, ou só objetos forjados).
    #[error("no manifest opens with this key")]
    NoneOpens,
    /// Backend fora do ar: não é "nenhum abre".
    #[error("storage error: {0}")]
    Store(#[from] StoreError),
}

/// Acha a maior época em que algum manifest de `store` abre com `vault_key`.
///
/// O cabeçalho (em claro, não autenticado) diz a época a tentar; só conta quem **abre**. Por
/// isso um objeto forjado, de qualquer época, não vence nem rebaixa a escolha: o resultado é o
/// mesmo de "da maior época para a menor, tentando todos os manifests de cada uma" (ADR 0018,
/// item 7), mas cada manifest é lido uma vez e um de cada vez. Sem teto de épocas: o custo é o
/// download dos manifests. Objeto ilegível (hash que não bate, ausente, cabeçalho inválido) é
/// pulado; backend fora do ar aborta.
pub async fn discover_epoch<S: BlobStore>(
    store: &S,
    vault_key: &VaultKey,
) -> Result<Discovery, DiscoverError> {
    let ids = store.list(ObjectKind::Manifest).await?;
    if ids.is_empty() {
        return Err(DiscoverError::Empty);
    }
    let mut opened: Option<u32> = None;
    let mut unopened: Vec<ForeignEpoch> = Vec::new();
    for id in ids {
        let bytes = match verified_get(store, &id).await {
            Ok(Some(bytes)) => bytes,
            Err(e @ StoreError::Unavailable(_)) => return Err(e.into()),
            Ok(None) | Err(_) => continue,
        };
        let Ok((header, _)) = Header::decode(&bytes) else {
            continue;
        };
        let epoch = header.key_epoch;
        let keys = EpochKeys::derive(vault_key, epoch);
        if Manifest::open(&keys, &bytes).is_ok() {
            opened = Some(opened.map_or(epoch, |best| best.max(epoch)));
        } else {
            unopened.push(ForeignEpoch { epoch, id });
        }
    }
    let epoch = opened.ok_or(DiscoverError::NoneOpens)?;
    let mut newer: Vec<ForeignEpoch> = unopened.into_iter().filter(|f| f.epoch > epoch).collect();
    newer.sort_by_key(|f| (f.epoch, f.id.hash));
    Ok(Discovery { epoch, newer })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::disk::DiskStore;
    use crate::object::ObjectId;
    use crate::store::Capabilities;
    use crate::testutil::TempDir;
    use crate::FORMAT_VERSION;
    use std::future::{ready, Future};
    use std::io;

    fn key(byte: u8) -> VaultKey {
        VaultKey::from_bytes([byte; 32])
    }

    fn store(tmp: &TempDir) -> DiskStore {
        DiskStore::open(tmp.path().join("store")).unwrap()
    }

    /// Grava um manifest válido de `epoch`, cifrado com `vault_key`; `device` o torna único.
    async fn put_manifest(
        store: &DiskStore,
        vault_key: &VaultKey,
        epoch: u32,
        device: &str,
    ) -> ObjectId {
        let manifest = Manifest {
            format_version: FORMAT_VERSION as u16,
            key_epoch: epoch,
            base_version: None,
            merged_from: vec![],
            counter: 1,
            device: device.to_string(),
            created_at: 1_793_455_331,
            files: vec![],
        };
        let sealed = manifest.seal(&EpochKeys::derive(vault_key, epoch)).unwrap();
        store.put(&sealed.id, &sealed.bytes).await.unwrap();
        sealed.id
    }

    /// Grava lixo como manifest: o id confere (o hash é dos bytes), o conteúdo não é nada.
    async fn put_garbage(store: &DiskStore, bytes: &[u8]) -> ObjectId {
        let id = ObjectId::of(ObjectKind::Manifest, bytes);
        store.put(&id, bytes).await.unwrap();
        id
    }

    fn foreign(epoch: u32, id: ObjectId) -> ForeignEpoch {
        ForeignEpoch { epoch, id }
    }

    #[tokio::test]
    async fn finds_epoch_zero_in_a_vault_that_was_never_rotated() {
        let tmp = TempDir::new();
        let store = store(&tmp);
        put_manifest(&store, &key(1), 0, "pc").await;
        put_manifest(&store, &key(1), 0, "celular").await;
        let found = discover_epoch(&store, &key(1)).await.unwrap();
        assert_eq!(
            found,
            Discovery {
                epoch: 0,
                newer: vec![]
            }
        );
    }

    #[tokio::test]
    async fn takes_the_highest_epoch_that_opens() {
        let tmp = TempDir::new();
        let store = store(&tmp);
        // a época 2 abre com a chave nova; as anteriores são de outra chave (rotação)
        put_manifest(&store, &key(1), 0, "pc").await;
        put_manifest(&store, &key(2), 1, "pc").await;
        put_manifest(&store, &key(3), 2, "pc").await;
        let found = discover_epoch(&store, &key(3)).await.unwrap();
        assert_eq!(found.epoch, 2);
        assert!(found.newer.is_empty());
    }

    #[tokio::test]
    async fn the_new_key_of_a_rotated_vault_finds_the_new_epoch() {
        let tmp = TempDir::new();
        let store = store(&tmp);
        put_manifest(&store, &key(1), 0, "pc").await;
        put_manifest(&store, &key(2), 1, "pc").await;
        let found = discover_epoch(&store, &key(2)).await.unwrap();
        assert_eq!(
            found,
            Discovery {
                epoch: 1,
                newer: vec![]
            }
        );
    }

    #[tokio::test]
    async fn the_old_key_of_a_rotated_vault_finds_the_old_epoch_and_reports_the_newer() {
        let tmp = TempDir::new();
        let store = store(&tmp);
        put_manifest(&store, &key(1), 0, "pc").await;
        let new_a = put_manifest(&store, &key(2), 1, "pc").await;
        let new_b = put_manifest(&store, &key(2), 1, "celular").await;
        let found = discover_epoch(&store, &key(1)).await.unwrap();
        assert_eq!(found.epoch, 0);
        let mut expected = vec![foreign(1, new_a), foreign(1, new_b)];
        expected.sort_by_key(|f| (f.epoch, f.id.hash));
        assert_eq!(found.newer, expected);
    }

    #[tokio::test]
    async fn a_forged_higher_epoch_does_not_win() {
        let tmp = TempDir::new();
        let store = store(&tmp);
        let legit = put_manifest(&store, &key(2), 1, "pc").await;
        // cabeçalho de época 99, mas de outra chave: não abre com a nossa
        let forged = put_manifest(&store, &key(9), 99, "intruso").await;
        let found = discover_epoch(&store, &key(2)).await.unwrap();
        assert_eq!(found.epoch, 1);
        assert_eq!(found.newer, vec![foreign(99, forged)]);
        assert_ne!(legit, forged);
    }

    #[tokio::test]
    async fn a_forged_object_with_the_true_epoch_does_not_hide_the_real_one() {
        let tmp = TempDir::new();
        let store = store(&tmp);
        // vários forjados com a época verdadeira (1); o legítimo pode vir em qualquer ordem
        for i in 0..8 {
            put_manifest(&store, &key(9), 1, &format!("intruso-{i}")).await;
        }
        put_manifest(&store, &key(2), 1, "pc").await;
        put_manifest(&store, &key(1), 0, "pc").await;
        let found = discover_epoch(&store, &key(2)).await.unwrap();
        assert_eq!(found.epoch, 1);
    }

    #[tokio::test]
    async fn a_forged_manifest_of_a_lower_epoch_does_not_lower_the_choice() {
        let tmp = TempDir::new();
        let store = store(&tmp);
        // quem tem a chave velha forja um objeto da época 0 que abre com a chave velha;
        // com a chave nova ele não abre, e a época 1 continua a escolhida
        put_manifest(&store, &key(1), 0, "forjado").await;
        put_manifest(&store, &key(2), 1, "pc").await;
        let found = discover_epoch(&store, &key(2)).await.unwrap();
        assert_eq!(found.epoch, 1);
    }

    #[tokio::test]
    async fn an_empty_storage_is_empty_and_not_none_opens() {
        let tmp = TempDir::new();
        let store = store(&tmp);
        assert_eq!(
            discover_epoch(&store, &key(1)).await,
            Err(DiscoverError::Empty)
        );
    }

    #[tokio::test]
    async fn manifests_that_none_opens_are_none_opens() {
        let tmp = TempDir::new();
        let store = store(&tmp);
        put_manifest(&store, &key(2), 1, "pc").await;
        put_manifest(&store, &key(3), 0, "pc").await;
        assert_eq!(
            discover_epoch(&store, &key(1)).await,
            Err(DiscoverError::NoneOpens)
        );
    }

    #[tokio::test]
    async fn only_garbage_is_none_opens() {
        let tmp = TempDir::new();
        let store = store(&tmp);
        put_garbage(&store, b"isto nao e um manifest").await;
        assert_eq!(
            discover_epoch(&store, &key(1)).await,
            Err(DiscoverError::NoneOpens)
        );
    }

    #[tokio::test]
    async fn a_corrupt_object_is_skipped_and_the_valid_one_is_found() {
        let tmp = TempDir::new();
        let store = store(&tmp);
        put_garbage(&store, b"MAGIC? nao").await;
        put_garbage(&store, b"").await;
        put_manifest(&store, &key(1), 4, "pc").await;
        let found = discover_epoch(&store, &key(1)).await.unwrap();
        assert_eq!(found.epoch, 4);
    }

    #[tokio::test]
    async fn the_largest_possible_epoch_works() {
        let tmp = TempDir::new();
        let store = store(&tmp);
        put_manifest(&store, &key(1), u32::MAX, "pc").await;
        put_manifest(&store, &key(1), 0, "pc").await;
        let found = discover_epoch(&store, &key(1)).await.unwrap();
        assert_eq!(found.epoch, u32::MAX);
    }

    /// Backend que lista um manifest e falha ao lê-lo.
    struct Down;

    impl BlobStore for Down {
        fn put(
            &self,
            _: &ObjectId,
            _: &[u8],
        ) -> impl Future<Output = Result<(), StoreError>> + Send {
            ready(Err(StoreError::Unavailable(io::ErrorKind::TimedOut)))
        }
        fn get(
            &self,
            _: &ObjectId,
        ) -> impl Future<Output = Result<Option<Vec<u8>>, StoreError>> + Send {
            ready(Err(StoreError::Unavailable(io::ErrorKind::TimedOut)))
        }
        fn has(&self, _: &ObjectId) -> impl Future<Output = Result<bool, StoreError>> + Send {
            ready(Err(StoreError::Unavailable(io::ErrorKind::TimedOut)))
        }
        fn list(
            &self,
            _: ObjectKind,
        ) -> impl Future<Output = Result<Vec<ObjectId>, StoreError>> + Send {
            ready(Ok(vec![ObjectId::of(ObjectKind::Manifest, b"x")]))
        }
        fn delete(&self, _: &ObjectId) -> impl Future<Output = Result<(), StoreError>> + Send {
            ready(Err(StoreError::Unavailable(io::ErrorKind::TimedOut)))
        }
        fn capabilities(&self) -> Capabilities {
            Capabilities {
                consistent_list: false,
            }
        }
    }

    #[tokio::test]
    async fn a_backend_that_is_down_is_an_error_and_not_none_opens() {
        assert_eq!(
            discover_epoch(&Down, &key(1)).await,
            Err(DiscoverError::Store(StoreError::Unavailable(
                io::ErrorKind::TimedOut
            )))
        );
    }

    /// Mesmo teste com `list` falhando (não só o `get`).
    struct ListDown;

    impl BlobStore for ListDown {
        fn put(
            &self,
            _: &ObjectId,
            _: &[u8],
        ) -> impl Future<Output = Result<(), StoreError>> + Send {
            ready(Ok(()))
        }
        fn get(
            &self,
            _: &ObjectId,
        ) -> impl Future<Output = Result<Option<Vec<u8>>, StoreError>> + Send {
            ready(Ok(None))
        }
        fn has(&self, _: &ObjectId) -> impl Future<Output = Result<bool, StoreError>> + Send {
            ready(Ok(false))
        }
        fn list(
            &self,
            _: ObjectKind,
        ) -> impl Future<Output = Result<Vec<ObjectId>, StoreError>> + Send {
            ready(Err(StoreError::Unavailable(
                io::ErrorKind::ConnectionRefused,
            )))
        }
        fn delete(&self, _: &ObjectId) -> impl Future<Output = Result<(), StoreError>> + Send {
            ready(Ok(()))
        }
        fn capabilities(&self) -> Capabilities {
            Capabilities {
                consistent_list: false,
            }
        }
    }

    #[tokio::test]
    async fn a_failing_list_is_an_error_and_not_empty() {
        assert_eq!(
            discover_epoch(&ListDown, &key(1)).await,
            Err(DiscoverError::Store(StoreError::Unavailable(
                io::ErrorKind::ConnectionRefused
            )))
        );
    }
}
