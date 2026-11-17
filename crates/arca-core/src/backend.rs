//! Os backends da v1 como um `enum`: `async fn` em trait não é `dyn`-compatível, então o
//! núcleo (PR 6) recebe um `Backend` (ou é genérico sobre `BlobStore`). Hub e S3 entram
//! como novas variantes nas Fases 2 e 3.

use crate::disk::DiskStore;
#[cfg(feature = "hub")]
use crate::hub::HubStore;
use crate::object::{ObjectId, ObjectKind};
#[cfg(feature = "s3")]
use crate::s3::S3Store;
use crate::store::{BlobStore, Capabilities, StoreError};

#[derive(Debug, Clone)]
#[non_exhaustive]
pub enum Backend {
    Disk(DiskStore),
    /// O hub (`warden-server`) como réplica, sobre HTTP (2b-1).
    #[cfg(feature = "hub")]
    Hub(HubStore),
    /// Um bucket S3-compatível, para o backup externo (Fase 3).
    #[cfg(feature = "s3")]
    S3(S3Store),
}

impl BlobStore for Backend {
    async fn put(&self, id: &ObjectId, bytes: &[u8]) -> Result<(), StoreError> {
        match self {
            Backend::Disk(disk) => disk.put(id, bytes).await,
            #[cfg(feature = "hub")]
            Backend::Hub(hub) => hub.put(id, bytes).await,
            #[cfg(feature = "s3")]
            Backend::S3(s3) => s3.put(id, bytes).await,
        }
    }

    async fn get(&self, id: &ObjectId) -> Result<Option<Vec<u8>>, StoreError> {
        match self {
            Backend::Disk(disk) => disk.get(id).await,
            #[cfg(feature = "hub")]
            Backend::Hub(hub) => hub.get(id).await,
            #[cfg(feature = "s3")]
            Backend::S3(s3) => s3.get(id).await,
        }
    }

    async fn has(&self, id: &ObjectId) -> Result<bool, StoreError> {
        match self {
            Backend::Disk(disk) => disk.has(id).await,
            #[cfg(feature = "hub")]
            Backend::Hub(hub) => hub.has(id).await,
            #[cfg(feature = "s3")]
            Backend::S3(s3) => s3.has(id).await,
        }
    }

    async fn list(&self, kind: ObjectKind) -> Result<Vec<ObjectId>, StoreError> {
        match self {
            Backend::Disk(disk) => disk.list(kind).await,
            #[cfg(feature = "hub")]
            Backend::Hub(hub) => hub.list(kind).await,
            #[cfg(feature = "s3")]
            Backend::S3(s3) => s3.list(kind).await,
        }
    }

    async fn delete(&self, id: &ObjectId) -> Result<(), StoreError> {
        match self {
            Backend::Disk(disk) => disk.delete(id).await,
            #[cfg(feature = "hub")]
            Backend::Hub(hub) => hub.delete(id).await,
            #[cfg(feature = "s3")]
            Backend::S3(s3) => s3.delete(id).await,
        }
    }

    fn capabilities(&self) -> Capabilities {
        match self {
            Backend::Disk(disk) => disk.capabilities(),
            #[cfg(feature = "hub")]
            Backend::Hub(hub) => hub.capabilities(),
            #[cfg(feature = "s3")]
            Backend::S3(s3) => s3.capabilities(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::crypto::{EpochKeys, VaultKey};
    use crate::manifest::{Class, FileEntry, Hash32, Manifest};
    use crate::store::verified_get;
    use crate::testutil::TempDir;
    use crate::FORMAT_VERSION;

    fn disk(tmp: &TempDir) -> DiskStore {
        DiskStore::open(tmp.path().join("store")).unwrap()
    }

    #[tokio::test]
    async fn backend_disk_behaves_exactly_like_the_disk_store_it_wraps() {
        let tmp = TempDir::new();
        let d = disk(&tmp);
        let b = Backend::Disk(d.clone());
        let data = b"cifrado".to_vec();
        let id = ObjectId::of(ObjectKind::Blob, &data);

        assert_eq!(b.has(&id).await, Ok(false));
        assert_eq!(b.get(&id).await, Ok(None));
        b.put(&id, &data).await.unwrap();
        // visto pelos dois lados: é o mesmo armazenamento
        assert_eq!(d.get(&id).await, Ok(Some(data.clone())));
        assert_eq!(b.get(&id).await, Ok(Some(data.clone())));
        assert_eq!(b.has(&id).await, Ok(true));
        assert_eq!(b.list(ObjectKind::Blob).await, Ok(vec![id]));
        assert_eq!(b.list(ObjectKind::Manifest).await, Ok(vec![]));
        assert_eq!(b.put(&id, b"outro").await, Err(StoreError::IdMismatch));
        assert_eq!(b.delete(&id).await, Ok(()));
        assert_eq!(d.has(&id).await, Ok(false));
        assert_eq!(b.capabilities(), d.capabilities());
    }

    /// `Backend::S3` só delega ao `S3Store` (sobre `InMemory`, sem rede), com a mesma chave
    /// `<tipo>/<hex>` do disco e a leitura verificada do núcleo.
    #[cfg(feature = "s3")]
    #[tokio::test]
    async fn backend_s3_delegates_to_the_s3_store() {
        use object_store::memory::InMemory;
        use std::sync::Arc;
        let s3 = S3Store::from_object_store(Arc::new(InMemory::new()), true);
        let b = Backend::S3(s3.clone());
        let data = b"cifrado".to_vec();
        let id = ObjectId::of(ObjectKind::Blob, &data);

        assert_eq!(b.has(&id).await, Ok(false));
        assert_eq!(b.get(&id).await, Ok(None));
        b.put(&id, &data).await.unwrap();
        assert_eq!(s3.get(&id).await, Ok(Some(data.clone()))); // o mesmo armazenamento
        assert_eq!(verified_get(&b, &id).await, Ok(Some(data)));
        assert_eq!(b.has(&id).await, Ok(true));
        assert_eq!(b.list(ObjectKind::Blob).await, Ok(vec![id]));
        assert_eq!(b.list(ObjectKind::Manifest).await, Ok(vec![]));
        assert_eq!(b.put(&id, b"outro").await, Err(StoreError::IdMismatch));
        assert_eq!(
            b.delete(&id).await,
            Err(StoreError::Unavailable(std::io::ErrorKind::Unsupported))
        );
        assert_eq!(b.capabilities(), s3.capabilities());
        assert!(b.capabilities().consistent_list);
    }

    #[tokio::test]
    async fn backend_works_with_verified_get() {
        let tmp = TempDir::new();
        let b = Backend::Disk(disk(&tmp));
        let data = b"cifrado".to_vec();
        let id = ObjectId::of(ObjectKind::Blob, &data);
        b.put(&id, &data).await.unwrap();
        assert_eq!(verified_get(&b, &id).await, Ok(Some(data)));
    }

    /// Ponta a ponta do que existe até aqui: manifest -> cifra -> disco -> leitura
    /// verificada -> decifra -> o mesmo manifest.
    #[tokio::test]
    async fn manifest_survives_seal_put_verified_get_open() {
        let tmp = TempDir::new();
        let b = Backend::Disk(disk(&tmp));
        let keys = EpochKeys::derive(&VaultKey::from_bytes([5u8; 32]), 2);
        let manifest = Manifest {
            format_version: FORMAT_VERSION as u16,
            key_epoch: 2,
            base_version: None,
            merged_from: vec![],
            counter: 1,
            device: "portatil-fabio".to_string(),
            created_at: 1_793_455_331,
            files: vec![FileEntry {
                path: "memoria/projeto-x.md".to_string(),
                class: Class::Vault,
                blob: Hash32([1u8; 32]),
                size: 12,
                sha256: Hash32([2u8; 32]),
            }],
        };

        let sealed = manifest.seal(&keys).unwrap();
        b.put(&sealed.id, &sealed.bytes).await.unwrap();
        assert_eq!(b.list(ObjectKind::Manifest).await, Ok(vec![sealed.id]));

        let back = verified_get(&b, &sealed.id).await.unwrap().unwrap();
        assert_eq!(Manifest::open(&keys, &back), Ok(manifest));
    }
}
