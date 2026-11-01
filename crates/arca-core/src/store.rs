//! Fronteira de backend: o `BlobStore` guarda **bytes opacos** sob um `ObjectId`
//! (`SPEC-TECNICA.md` §4). Não conhece chaves; quem confia nos bytes é o núcleo, via
//! `verified_get`.

use crate::object::{ObjectId, ObjectKind};
use std::future::Future;
use std::io;

/// Erros de um backend. Carregam só o tipo do erro de E/S, nunca caminho nem bytes.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum StoreError {
    /// Backend inacessível (disco, rede, quota, credencial): tentar de novo ou usar outro.
    #[error("backend unavailable: {0}")]
    Unavailable(io::ErrorKind),
    /// Os bytes devolvidos não têm o hash do id pedido.
    #[error("stored object does not match its id")]
    Corrupt,
    /// `put` com bytes que não têm o hash do id informado (bug do chamador).
    #[error("bytes do not match the object id")]
    IdMismatch,
}

/// O que um backend garante. A v1 não depende de nada aqui (SPEC-TECNICA §4).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub struct Capabilities {
    /// `list` enxerga todo objeto já gravado com `put`.
    pub consistent_list: bool,
}

/// Armazenamento de objetos imutáveis endereçados pelo hash dos bytes cifrados.
///
/// Os métodos devolvem `impl Future + Send` (e não `async fn`) para que o contrato `Send`
/// fique explícito; as implementações podem escrevê-los como `async fn`.
pub trait BlobStore {
    /// Grava o objeto. Idempotente. Recusa (`IdMismatch`) bytes que não têm o hash de `id`.
    fn put(
        &self,
        id: &ObjectId,
        bytes: &[u8],
    ) -> impl Future<Output = Result<(), StoreError>> + Send;

    /// Lê o objeto, ou `None` se não existe. **Não verifica o hash**: use `verified_get`.
    fn get(
        &self,
        id: &ObjectId,
    ) -> impl Future<Output = Result<Option<Vec<u8>>, StoreError>> + Send;

    fn has(&self, id: &ObjectId) -> impl Future<Output = Result<bool, StoreError>> + Send;

    /// Ids dos objetos de um tipo, em ordem não especificada.
    fn list(
        &self,
        kind: ObjectKind,
    ) -> impl Future<Output = Result<Vec<ObjectId>, StoreError>> + Send;

    /// Apaga o objeto; apagar um id que não existe é sucesso. A v1 não chama (ADR 0011).
    fn delete(&self, id: &ObjectId) -> impl Future<Output = Result<(), StoreError>> + Send;

    fn capabilities(&self) -> Capabilities;
}

/// `get` mais a regra "o backend não é confiável para integridade": recalcula o SHA-256 e
/// recusa (`Corrupt`) o que não bate com o id. É a única forma de ler que o núcleo usa.
pub async fn verified_get<S: BlobStore>(
    store: &S,
    id: &ObjectId,
) -> Result<Option<Vec<u8>>, StoreError> {
    match store.get(id).await? {
        Some(bytes) if ObjectId::of(id.kind, &bytes).hash != id.hash => Err(StoreError::Corrupt),
        other => Ok(other),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::disk::DiskStore;
    use crate::testutil::TempDir;
    use std::future::ready;

    /// Backend que devolve o que lhe mandaram devolver, sem olhar o id.
    struct Liar(Option<Vec<u8>>);

    impl BlobStore for Liar {
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
            ready(Ok(self.0.clone()))
        }
        fn has(&self, _: &ObjectId) -> impl Future<Output = Result<bool, StoreError>> + Send {
            ready(Ok(self.0.is_some()))
        }
        fn list(
            &self,
            _: ObjectKind,
        ) -> impl Future<Output = Result<Vec<ObjectId>, StoreError>> + Send {
            ready(Ok(vec![]))
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

    /// Backend que falha sempre.
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
            ready(Err(StoreError::Unavailable(io::ErrorKind::TimedOut)))
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
    async fn verified_get_returns_bytes_that_match_the_id() {
        let bytes = b"cifrado".to_vec();
        let id = ObjectId::of(ObjectKind::Blob, &bytes);
        assert_eq!(
            verified_get(&Liar(Some(bytes.clone())), &id).await,
            Ok(Some(bytes))
        );
    }

    #[tokio::test]
    async fn verified_get_refuses_bytes_with_another_hash() {
        let id = ObjectId::of(ObjectKind::Blob, b"cifrado");
        for lie in [&b"outro"[..], b"cifrad", b"cifrado\0", b""] {
            assert_eq!(
                verified_get(&Liar(Some(lie.to_vec())), &id).await,
                Err(StoreError::Corrupt),
                "{lie:?}"
            );
        }
    }

    #[tokio::test]
    async fn verified_get_hash_depends_only_on_the_bytes_not_on_the_kind() {
        // O id é (tipo, hash): bytes certos pedidos como outro tipo ainda têm o mesmo hash.
        // O tipo é conferido na abertura do envelope (`open`), não aqui.
        let bytes = b"cifrado".to_vec();
        let id = ObjectId::of(ObjectKind::Manifest, &bytes);
        assert_eq!(
            verified_get(&Liar(Some(bytes.clone())), &id).await,
            Ok(Some(bytes))
        );
    }

    #[tokio::test]
    async fn verified_get_passes_through_absence_and_backend_errors() {
        let id = ObjectId::of(ObjectKind::Blob, b"x");
        assert_eq!(verified_get(&Liar(None), &id).await, Ok(None));
        assert_eq!(
            verified_get(&Down, &id).await,
            Err(StoreError::Unavailable(io::ErrorKind::TimedOut))
        );
    }

    #[tokio::test]
    async fn a_tampered_file_on_disk_is_caught_by_verified_get_but_not_by_get() {
        let tmp = TempDir::new();
        let store = DiskStore::open(tmp.path().join("store")).unwrap();
        let bytes = b"cifrado".to_vec();
        let id = ObjectId::of(ObjectKind::Blob, &bytes);
        store.put(&id, &bytes).await.unwrap();
        assert_eq!(verified_get(&store, &id).await, Ok(Some(bytes)));

        std::fs::write(tmp.path().join("store").join(id.to_string()), b"adulterado").unwrap();
        assert_eq!(store.get(&id).await, Ok(Some(b"adulterado".to_vec())));
        assert_eq!(verified_get(&store, &id).await, Err(StoreError::Corrupt));
    }

    #[test]
    fn store_errors_carry_no_path_or_bytes() {
        for e in [
            StoreError::Unavailable(io::ErrorKind::PermissionDenied),
            StoreError::Corrupt,
            StoreError::IdMismatch,
        ] {
            let text = format!("{e} {e:?}");
            assert!(!text.contains('/'), "{text}");
        }
    }
}
