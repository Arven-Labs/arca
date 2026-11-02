//! Replicação entre armazenamentos (`SPEC-TECNICA.md` §4 e §6, Fase 2a, PR 5).
//!
//! `replicate(origem, destino)` copia bytes opacos: não tem chave e não abre manifest. Pull e
//! push são a mesma função com os lados trocados. Quem agenda (segundo plano, retry) é o
//! consumidor; o núcleo não cria tarefas.

use crate::backend::Backend;
use crate::envelope::Header;
use crate::object::{ObjectId, ObjectKind};
use crate::store::{verified_get, BlobStore, StoreError};
use std::collections::HashSet;

/// O que uma chamada de `replicate` fez.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ReplicateReport {
    /// Objetos (blobs e manifests) gravados no destino agora.
    pub copied: usize,
    /// Objetos que o destino já tinha.
    pub already: usize,
    /// Objetos da origem cujo hash não bate com o id: pulados, nunca gravados.
    pub corrupt: Vec<ObjectId>,
    /// Blobs que a origem listou e não entregou: pulados. Retêm os manifests como `corrupt`.
    pub missing: Vec<ObjectId>,
    /// Objetos que a origem listou e falharam no `get` (erro de E/S): pulados. Um blob assim
    /// retém os manifests; um manifest assim não.
    pub unreadable: Vec<ObjectId>,
    /// Objetos que não começam com um envelope válido do tipo do id (lixo, truncado, tipo
    /// trocado): nunca gravados, e não retêm os manifests (nenhum manifest legítimo cita lixo).
    /// **Não detecta** cabeçalho forjado com corpo aleatório (P-32).
    pub malformed: Vec<ObjectId>,
    /// `true` se os manifests foram retidos porque algum blob foi pulado (corrompido ou não
    /// entregue; sem a chave não dá para saber qual manifest o cita). **`Ok` com isto ligado
    /// não é sucesso**: a rodada ficou incompleta e o consumidor não deve marcar "replicado".
    pub manifests_withheld: bool,
}

/// Em quantos locais um objeto está (ADR 0007: aviso abaixo de 2).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ReplicaStatus {
    pub present: usize,
    pub total: usize,
}

impl ReplicaStatus {
    /// Menos de 2 cópias: um único disco ou provedor perdido leva o dado.
    pub fn warn(&self) -> bool {
        self.present < 2
    }
}

/// Copia para `to` os blobs e depois os manifests que `from` tem e `to` não tem.
/// Idempotente e retomável.
pub async fn replicate<A: BlobStore, B: BlobStore>(
    from: &A,
    to: &B,
) -> Result<ReplicateReport, StoreError> {
    let mut report = ReplicateReport::default();
    // Os manifests são listados ANTES de copiar os blobs: um manifest que a origem publicar
    // durante a rodada fica para a próxima, em vez de chegar sem os blobs que a lista de
    // blobs (já tirada) não viu.
    let manifests = from.list(ObjectKind::Manifest).await?;
    // blobs primeiro: o destino nunca recebe manifest antes dos blobs que ele pode citar
    for (kind, ids) in [
        (ObjectKind::Blob, from.list(ObjectKind::Blob).await?),
        (ObjectKind::Manifest, manifests),
    ] {
        let blob_gap = !(report.corrupt.is_empty()
            && report.missing.is_empty()
            && report.unreadable.is_empty());
        if kind == ObjectKind::Manifest && blob_gap {
            report.manifests_withheld = true;
            break;
        }
        let have: HashSet<ObjectId> = to.list(kind).await?.into_iter().collect();
        let mut seen = HashSet::new();
        for id in ids {
            // a origem não é confiável: id repetido na lista não multiplica leitura nem escrita
            if !seen.insert(id) {
                continue;
            }
            if have.contains(&id) {
                report.already += 1;
                continue;
            }
            match verified_get(from, &id).await {
                Ok(Some(bytes)) => {
                    // o cabeçalho é em claro: dá para barrar lixo sem chave. Não barra um
                    // cabeçalho forjado com corpo aleatório (P-32).
                    if !is_envelope_of(kind, &bytes) {
                        report.malformed.push(id);
                        continue;
                    }
                    to.put(&id, &bytes).await?;
                    report.copied += 1;
                }
                // listado e não entregue: um blob assim pode ser citado por algum manifest
                Ok(None) if kind == ObjectKind::Blob => report.missing.push(id),
                // manifest que sumiu entre o `list` e o `get`: a próxima rodada pega
                Ok(None) => {}
                Err(StoreError::Corrupt) => report.corrupt.push(id),
                // falha ao ler UM objeto da origem não trava o lote; erro do destino aborta
                Err(StoreError::Unavailable(_)) => report.unreadable.push(id),
                Err(e) => return Err(e),
            }
        }
    }
    Ok(report)
}

/// O objeto começa com um cabeçalho de envelope válido e do tipo esperado.
fn is_envelope_of(kind: ObjectKind, bytes: &[u8]) -> bool {
    matches!(Header::decode(bytes), Ok((header, _)) if header.kind == kind)
}

/// Quantos dos `locations` têm o objeto `id`. Erro de qualquer local aborta.
pub async fn replica_status(
    id: &ObjectId,
    locations: &[Backend],
) -> Result<ReplicaStatus, StoreError> {
    let mut present = 0;
    for location in locations {
        if location.has(id).await? {
            present += 1;
        }
    }
    Ok(ReplicaStatus {
        present,
        total: locations.len(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::crypto::{EpochKeys, VaultKey};
    use crate::disk::DiskStore;
    use crate::engine::{Engine, EngineError, MergeOutcome, Roots, Snapshot};
    use crate::envelope::{Alg, Header, NONCE_LEN};
    use crate::store::Capabilities;
    use crate::testutil::TempDir;
    use crate::FORMAT_VERSION;
    use std::collections::BTreeMap;
    use std::fs;
    use std::io;
    use std::path::{Path, PathBuf};
    use std::sync::{Arc, Mutex};

    // ---------- ajudantes ----------

    fn disk(tmp: &TempDir, name: &str) -> Backend {
        Backend::Disk(DiskStore::open(tmp.path().join(name)).unwrap())
    }

    /// Objeto com cabeçalho de envelope válido (o corpo é opaco: o `replicate` não decifra).
    fn object(kind: ObjectKind, content: &[u8]) -> (ObjectId, Vec<u8>) {
        let header = Header {
            format_version: FORMAT_VERSION as u16,
            alg: Alg::XChaCha20Poly1305,
            kind,
            key_epoch: 1,
            nonce: [7u8; NONCE_LEN],
        };
        let mut bytes = header.encode().to_vec();
        bytes.extend_from_slice(content);
        (ObjectId::of(kind, &bytes), bytes)
    }

    /// Bytes que não são um envelope, sob o id certo (o hash bate; só o formato é lixo).
    fn junk(kind: ObjectKind, content: &[u8]) -> (ObjectId, Vec<u8>) {
        (ObjectId::of(kind, content), content.to_vec())
    }

    async fn put_all(store: &Backend, objects: &[(ObjectId, Vec<u8>)]) {
        for (id, bytes) in objects {
            store.put(id, bytes).await.unwrap();
        }
    }

    async fn count(store: &Backend, kind: ObjectKind) -> usize {
        store.list(kind).await.unwrap().len()
    }

    /// Backend de teste: registra a ordem dos `put`s e pode falhar de propósito.
    #[derive(Clone)]
    struct Probe {
        inner: Backend,
        puts: Arc<Mutex<Vec<ObjectKind>>>,
        fail_put: Option<ObjectKind>,
        /// Falha o n-ésimo `put` (a partir de 0) que seria feito.
        fail_put_nth: Option<usize>,
        fail_get: Option<ObjectId>,
        fail_list: bool,
        deletes: Arc<Mutex<usize>>,
    }

    impl Probe {
        fn new(inner: Backend) -> Probe {
            Probe {
                inner,
                puts: Arc::new(Mutex::new(Vec::new())),
                fail_put: None,
                fail_put_nth: None,
                fail_get: None,
                fail_list: false,
                deletes: Arc::new(Mutex::new(0)),
            }
        }
    }

    impl BlobStore for Probe {
        async fn put(&self, id: &ObjectId, bytes: &[u8]) -> Result<(), StoreError> {
            let done = self.puts.lock().unwrap().len();
            if self.fail_put == Some(id.kind) || self.fail_put_nth == Some(done) {
                return Err(StoreError::Unavailable(io::ErrorKind::TimedOut));
            }
            self.puts.lock().unwrap().push(id.kind);
            self.inner.put(id, bytes).await
        }
        async fn get(&self, id: &ObjectId) -> Result<Option<Vec<u8>>, StoreError> {
            if self.fail_get == Some(*id) {
                return Err(StoreError::Unavailable(io::ErrorKind::PermissionDenied));
            }
            self.inner.get(id).await
        }
        async fn has(&self, id: &ObjectId) -> Result<bool, StoreError> {
            self.inner.has(id).await
        }
        async fn list(&self, kind: ObjectKind) -> Result<Vec<ObjectId>, StoreError> {
            if self.fail_list {
                return Err(StoreError::Unavailable(io::ErrorKind::ConnectionRefused));
            }
            self.inner.list(kind).await
        }
        async fn delete(&self, id: &ObjectId) -> Result<(), StoreError> {
            *self.deletes.lock().unwrap() += 1;
            self.inner.delete(id).await
        }
        fn capabilities(&self) -> Capabilities {
            self.inner.capabilities()
        }
    }

    // ---------- replicate ----------

    #[tokio::test]
    async fn copies_only_what_is_missing_and_a_second_call_copies_nothing() {
        let tmp = TempDir::new();
        let (from, to) = (disk(&tmp, "origem"), disk(&tmp, "destino"));
        let blobs = [
            object(ObjectKind::Blob, b"b1"),
            object(ObjectKind::Blob, b"b2"),
            object(ObjectKind::Blob, b"b3"),
        ];
        let manifests = [
            object(ObjectKind::Manifest, b"m1"),
            object(ObjectKind::Manifest, b"m2"),
        ];
        put_all(&from, &blobs).await;
        put_all(&from, &manifests).await;
        put_all(&to, &blobs[..1]).await; // o destino já tem b1

        let first = replicate(&from, &to).await.unwrap();
        assert_eq!(
            first,
            ReplicateReport {
                copied: 4,
                already: 1,
                corrupt: vec![],
                missing: vec![],
                unreadable: vec![],
                malformed: vec![],
                manifests_withheld: false
            }
        );
        assert_eq!(count(&to, ObjectKind::Blob).await, 3);
        assert_eq!(count(&to, ObjectKind::Manifest).await, 2);

        let second = replicate(&from, &to).await.unwrap();
        assert_eq!(
            second,
            ReplicateReport {
                copied: 0,
                already: 5,
                corrupt: vec![],
                missing: vec![],
                unreadable: vec![],
                malformed: vec![],
                manifests_withheld: false
            }
        );
    }

    #[tokio::test]
    async fn never_deletes_and_never_touches_what_only_the_destination_has() {
        let tmp = TempDir::new();
        let (from, to) = (
            Probe::new(disk(&tmp, "origem")),
            Probe::new(disk(&tmp, "destino")),
        );
        let only_there = object(ObjectKind::Manifest, b"so-no-destino");
        let shared = object(ObjectKind::Blob, b"comum");
        to.put(&only_there.0, &only_there.1).await.unwrap();
        from.put(&shared.0, &shared.1).await.unwrap();
        let report = replicate(&from, &to).await.unwrap();
        assert_eq!(report.copied, 1);
        assert_eq!(to.has(&only_there.0).await, Ok(true));
        assert_eq!(
            (*from.deletes.lock().unwrap(), *to.deletes.lock().unwrap()),
            (0, 0)
        );
    }

    #[tokio::test]
    async fn a_destination_that_drops_in_the_middle_of_the_blobs_keeps_what_was_written_and_a_retry_completes(
    ) {
        let tmp = TempDir::new();
        let from = disk(&tmp, "origem");
        let blobs: Vec<_> = (0..5u8).map(|i| object(ObjectKind::Blob, &[i])).collect();
        let manifest = object(ObjectKind::Manifest, b"m");
        put_all(&from, &blobs).await;
        from.put(&manifest.0, &manifest.1).await.unwrap();
        let target = disk(&tmp, "destino");

        let mut flaky = Probe::new(target.clone());
        flaky.fail_put_nth = Some(2); // o terceiro `put` cai
        assert_eq!(
            replicate(&from, &flaky).await,
            Err(StoreError::Unavailable(io::ErrorKind::TimedOut))
        );
        assert_eq!(count(&target, ObjectKind::Blob).await, 2);
        assert_eq!(count(&target, ObjectKind::Manifest).await, 0);

        let retry = replicate(&from, &target).await.unwrap();
        assert_eq!((retry.copied, retry.already), (4, 2));
        assert_eq!(count(&target, ObjectKind::Blob).await, 5);
        assert_eq!(count(&target, ObjectKind::Manifest).await, 1);
    }

    #[tokio::test]
    async fn an_unreadable_blob_is_reported_and_withholds_the_manifests_but_the_rest_is_copied() {
        let tmp = TempDir::new();
        let from = disk(&tmp, "origem");
        let (b1, b2, b3) = (
            object(ObjectKind::Blob, b"1"),
            object(ObjectKind::Blob, b"2"),
            object(ObjectKind::Blob, b"3"),
        );
        let manifest = object(ObjectKind::Manifest, b"m");
        put_all(
            &from,
            &[b1.clone(), b2.clone(), b3.clone(), manifest.clone()],
        )
        .await;
        let mut source = Probe::new(from);
        source.fail_get = Some(b2.0);
        let to = disk(&tmp, "destino");

        let report = replicate(&source, &to).await.unwrap();
        assert_eq!(report.unreadable, vec![b2.0]);
        assert!(report.manifests_withheld);
        assert_eq!(report.copied, 2); // o resto do lote não fica refém de um objeto
        assert_eq!(to.has(&b1.0).await, Ok(true));
        assert_eq!(to.has(&b3.0).await, Ok(true));
        assert_eq!(to.has(&b2.0).await, Ok(false));
        assert_eq!(to.has(&manifest.0).await, Ok(false));
    }

    #[tokio::test]
    async fn an_unreadable_manifest_is_reported_but_does_not_stop_the_other_manifests() {
        let tmp = TempDir::new();
        let from = disk(&tmp, "origem");
        let (m1, m2) = (
            object(ObjectKind::Manifest, b"1"),
            object(ObjectKind::Manifest, b"2"),
        );
        put_all(&from, &[m1.clone(), m2.clone()]).await;
        let mut source = Probe::new(from);
        source.fail_get = Some(m1.0);
        let to = disk(&tmp, "destino");

        let report = replicate(&source, &to).await.unwrap();
        assert_eq!(report.unreadable, vec![m1.0]);
        assert!(!report.manifests_withheld);
        assert_eq!(to.has(&m2.0).await, Ok(true));
    }

    #[tokio::test]
    async fn objects_that_are_not_an_envelope_of_their_kind_are_never_written() {
        let tmp = TempDir::new();
        let from = disk(&tmp, "origem");
        let good_blob = object(ObjectKind::Blob, b"bom");
        let good_manifest = object(ObjectKind::Manifest, b"bom");
        // lixo puro, curto demais, e um blob que se passa por manifest (o hash cobre só os bytes)
        let garbage = junk(ObjectKind::Manifest, b"lixo");
        let short = junk(ObjectKind::Blob, b"ARCA");
        let (_, blob_bytes) = object(ObjectKind::Blob, b"outro");
        let wrong_kind = (ObjectId::of(ObjectKind::Manifest, &blob_bytes), blob_bytes);
        put_all(
            &from,
            &[
                good_blob.clone(),
                good_manifest.clone(),
                garbage.clone(),
                short.clone(),
                wrong_kind.clone(),
            ],
        )
        .await;
        let to = disk(&tmp, "destino");

        let report = replicate(&from, &to).await.unwrap();
        let mut malformed = report.malformed.clone();
        malformed.sort_by_key(|id| id.to_string());
        let mut expected = vec![garbage.0, short.0, wrong_kind.0];
        expected.sort_by_key(|id| id.to_string());
        assert_eq!(malformed, expected);
        assert_eq!(report.copied, 2);
        // lixo não é blob corrompido: não retém os manifests legítimos
        assert!(!report.manifests_withheld);
        for (id, _) in [&garbage, &short, &wrong_kind] {
            assert_eq!(to.has(id).await, Ok(false));
        }
        assert_eq!(to.has(&good_manifest.0).await, Ok(true));
    }

    #[tokio::test]
    async fn blobs_are_written_before_any_manifest() {
        let tmp = TempDir::new();
        let from = disk(&tmp, "origem");
        for i in 0..4u8 {
            let (id, bytes) = object(ObjectKind::Blob, &[i]);
            from.put(&id, &bytes).await.unwrap();
            let (id, bytes) = object(ObjectKind::Manifest, &[100 + i]);
            from.put(&id, &bytes).await.unwrap();
        }
        let to = Probe::new(disk(&tmp, "destino"));
        replicate(&from, &to).await.unwrap();
        let order = to.puts.lock().unwrap().clone();
        assert_eq!(order.len(), 8);
        let first_manifest = order
            .iter()
            .position(|k| *k == ObjectKind::Manifest)
            .unwrap();
        assert!(
            order[first_manifest..]
                .iter()
                .all(|k| *k == ObjectKind::Manifest),
            "{order:?}"
        );
    }

    #[tokio::test]
    async fn a_failed_manifest_write_leaves_blobs_only_and_a_retry_finishes_the_job() {
        let tmp = TempDir::new();
        let from = disk(&tmp, "origem");
        let blob = object(ObjectKind::Blob, b"b");
        let manifest = object(ObjectKind::Manifest, b"m");
        put_all(&from, &[blob.clone(), manifest.clone()]).await;
        let target = disk(&tmp, "destino");

        let mut flaky = Probe::new(target.clone());
        flaky.fail_put = Some(ObjectKind::Manifest);
        assert_eq!(
            replicate(&from, &flaky).await,
            Err(StoreError::Unavailable(io::ErrorKind::TimedOut))
        );
        // o destino nunca tem manifest sem os blobs
        assert_eq!(target.has(&blob.0).await, Ok(true));
        assert_eq!(target.has(&manifest.0).await, Ok(false));

        let retry = replicate(&from, &target).await.unwrap();
        assert_eq!((retry.copied, retry.already), (1, 1));
        assert_eq!(target.has(&manifest.0).await, Ok(true));
    }

    #[tokio::test]
    async fn an_unavailable_source_or_destination_is_an_error_and_writes_nothing() {
        let tmp = TempDir::new();
        let from = disk(&tmp, "origem");
        let blob = object(ObjectKind::Blob, b"b");
        from.put(&blob.0, &blob.1).await.unwrap();
        let target = disk(&tmp, "destino");

        let mut down_source = Probe::new(from.clone());
        down_source.fail_list = true;
        assert!(matches!(
            replicate(&down_source, &target).await,
            Err(StoreError::Unavailable(_))
        ));
        let mut down_target = Probe::new(target.clone());
        down_target.fail_list = true;
        assert!(matches!(
            replicate(&from, &down_target).await,
            Err(StoreError::Unavailable(_))
        ));
        assert_eq!(count(&target, ObjectKind::Blob).await, 0);
    }

    fn corrupt_on_disk(tmp: &TempDir, store: &str, id: &ObjectId) {
        fs::write(tmp.path().join(store).join(id.to_string()), b"adulterado").unwrap();
    }

    #[tokio::test]
    async fn a_corrupt_manifest_is_skipped_and_reported_but_the_rest_is_copied() {
        let tmp = TempDir::new();
        let from = disk(&tmp, "origem");
        let blob = object(ObjectKind::Blob, b"b");
        let good = object(ObjectKind::Manifest, b"bom");
        let bad = object(ObjectKind::Manifest, b"ruim");
        put_all(&from, &[blob.clone(), good.clone(), bad.clone()]).await;
        corrupt_on_disk(&tmp, "origem", &bad.0);
        let to = disk(&tmp, "destino");

        let report = replicate(&from, &to).await.unwrap();
        assert_eq!(report.corrupt, vec![bad.0]);
        assert_eq!(report.copied, 2);
        assert!(!report.manifests_withheld);
        assert_eq!(to.has(&bad.0).await, Ok(false)); // nunca gravado
        assert_eq!(to.has(&good.0).await, Ok(true));
    }

    #[tokio::test]
    async fn a_corrupt_blob_withholds_every_manifest_so_none_can_cite_a_missing_blob() {
        let tmp = TempDir::new();
        let from = disk(&tmp, "origem");
        let good_blob = object(ObjectKind::Blob, b"bom");
        let bad_blob = object(ObjectKind::Blob, b"ruim");
        let manifest = object(ObjectKind::Manifest, b"m");
        put_all(
            &from,
            &[good_blob.clone(), bad_blob.clone(), manifest.clone()],
        )
        .await;
        corrupt_on_disk(&tmp, "origem", &bad_blob.0);
        let to = disk(&tmp, "destino");

        let report = replicate(&from, &to).await.unwrap();
        assert_eq!(report.corrupt, vec![bad_blob.0]);
        assert!(report.manifests_withheld);
        assert_eq!(to.has(&good_blob.0).await, Ok(true));
        assert_eq!(to.has(&bad_blob.0).await, Ok(false));
        assert_eq!(to.has(&manifest.0).await, Ok(false));
    }

    /// Origem que mente na lista: anuncia blobs que não entrega e repete ids.
    struct Liar {
        inner: Backend,
        ghosts: Vec<ObjectId>,
        repeat: usize,
        gets: Arc<Mutex<usize>>,
    }

    impl BlobStore for Liar {
        async fn put(&self, id: &ObjectId, bytes: &[u8]) -> Result<(), StoreError> {
            self.inner.put(id, bytes).await
        }
        async fn get(&self, id: &ObjectId) -> Result<Option<Vec<u8>>, StoreError> {
            *self.gets.lock().unwrap() += 1;
            if self.ghosts.contains(id) {
                return Ok(None);
            }
            self.inner.get(id).await
        }
        async fn has(&self, id: &ObjectId) -> Result<bool, StoreError> {
            self.inner.has(id).await
        }
        async fn list(&self, kind: ObjectKind) -> Result<Vec<ObjectId>, StoreError> {
            let mut ids = self.inner.list(kind).await?;
            if kind == ObjectKind::Blob {
                ids.extend(self.ghosts.iter().copied());
            }
            let once = ids.clone();
            for _ in 1..self.repeat {
                ids.extend(once.iter().copied());
            }
            Ok(ids)
        }
        async fn delete(&self, id: &ObjectId) -> Result<(), StoreError> {
            self.inner.delete(id).await
        }
        fn capabilities(&self) -> Capabilities {
            self.inner.capabilities()
        }
    }

    #[tokio::test]
    async fn a_listed_blob_that_cannot_be_read_withholds_the_manifests() {
        let tmp = TempDir::new();
        let inner = disk(&tmp, "origem");
        let manifest = object(ObjectKind::Manifest, b"cita-o-fantasma");
        inner.put(&manifest.0, &manifest.1).await.unwrap();
        let ghost = ObjectId::of(ObjectKind::Blob, b"fantasma");
        let from = Liar {
            inner,
            ghosts: vec![ghost],
            repeat: 1,
            gets: Arc::default(),
        };
        let to = disk(&tmp, "destino");

        let report = replicate(&from, &to).await.unwrap();
        assert_eq!(report.missing, vec![ghost]);
        assert!(report.manifests_withheld);
        assert_eq!(to.has(&manifest.0).await, Ok(false));
    }

    #[tokio::test]
    async fn ids_repeated_by_the_source_are_read_and_written_once() {
        let tmp = TempDir::new();
        let inner = disk(&tmp, "origem");
        let blob = object(ObjectKind::Blob, b"b");
        let bad = object(ObjectKind::Blob, b"ruim");
        put_all(&inner, &[blob.clone(), bad.clone()]).await;
        corrupt_on_disk(&tmp, "origem", &bad.0);
        let gets = Arc::new(Mutex::new(0));
        let from = Liar {
            inner,
            ghosts: vec![],
            repeat: 50,
            gets: gets.clone(),
        };
        let to = Probe::new(disk(&tmp, "destino"));

        let report = replicate(&from, &to).await.unwrap();
        assert_eq!(report.copied, 1);
        assert_eq!(report.corrupt, vec![bad.0]);
        assert_eq!(*gets.lock().unwrap(), 2); // um por objeto distinto
        assert_eq!(to.puts.lock().unwrap().len(), 1);
    }

    // ---------- replica_status ----------

    #[tokio::test]
    async fn status_warns_below_two_locations_and_stops_at_two() {
        let tmp = TempDir::new();
        let (a, b, c) = (disk(&tmp, "a"), disk(&tmp, "b"), disk(&tmp, "c"));
        let m = object(ObjectKind::Manifest, b"ponta");
        a.put(&m.0, &m.1).await.unwrap();
        let all = [a.clone(), b.clone(), c.clone()];

        let one = replica_status(&m.0, &all).await.unwrap();
        assert_eq!(
            one,
            ReplicaStatus {
                present: 1,
                total: 3
            }
        );
        assert!(one.warn());

        b.put(&m.0, &m.1).await.unwrap();
        let two = replica_status(&m.0, &all).await.unwrap();
        assert_eq!(
            two,
            ReplicaStatus {
                present: 2,
                total: 3
            }
        );
        assert!(!two.warn());

        let none = replica_status(&m.0, &[c]).await.unwrap();
        assert_eq!(
            none,
            ReplicaStatus {
                present: 0,
                total: 1
            }
        );
        assert!(none.warn());
    }

    // ---------- ponta a ponta: dois dispositivos e um remoto ----------

    fn keys() -> EpochKeys {
        EpochKeys::derive(&VaultKey::from_bytes([3u8; 32]), 1)
    }

    struct Device {
        name: &'static str,
        local: Backend,
        engine: Engine<Backend>,
        roots: Roots,
        vault: PathBuf,
        config: PathBuf,
    }

    impl Device {
        fn new(tmp: &TempDir, name: &'static str, with_state: bool) -> Device {
            let local = disk(tmp, &format!("{name}-store"));
            let base = tmp.path().join(format!("{name}-pastas"));
            let (vault, config) = (base.join("vault"), base.join("config"));
            fs::create_dir_all(&vault).unwrap();
            fs::create_dir_all(&config).unwrap();
            let mut engine = Engine::new(local.clone(), keys(), name).unwrap();
            if with_state {
                engine = engine.with_state(tmp.path().join(format!("{name}-estado.json")));
            }
            Device {
                name,
                local,
                engine,
                roots: Roots {
                    vault: Some(vault.clone()),
                    config: Some(config.clone()),
                    secret: None,
                },
                vault,
                config,
            }
        }

        async fn snapshot(&self, now: u64) -> crate::object::ObjectId {
            match self.engine.snapshot(&self.roots, now).await.unwrap() {
                Snapshot::Created { id, .. } => id,
                other => panic!("{}: esperava Created, veio {other:?}", self.name),
            }
        }

        async fn tips(&self) -> Vec<ObjectId> {
            let heads = self.engine.heads().await.unwrap();
            assert!(
                heads.rejected.is_empty(),
                "{}: {:?}",
                self.name,
                heads.rejected
            );
            heads.tips.iter().map(|t| t.id).collect()
        }
    }

    fn write(root: &Path, rel: &str, data: &[u8]) {
        let p = root.join(rel);
        fs::create_dir_all(p.parent().unwrap()).unwrap();
        fs::write(p, data).unwrap();
    }

    fn tree(root: &Path) -> BTreeMap<String, Vec<u8>> {
        fn walk(dir: &Path, prefix: &str, out: &mut BTreeMap<String, Vec<u8>>) {
            for entry in fs::read_dir(dir).unwrap() {
                let entry = entry.unwrap();
                let name = entry.file_name().into_string().unwrap();
                let rel = if prefix.is_empty() {
                    name
                } else {
                    format!("{prefix}/{name}")
                };
                if entry.file_type().unwrap().is_dir() {
                    walk(&entry.path(), &rel, out);
                } else {
                    out.insert(rel, fs::read(entry.path()).unwrap());
                }
            }
        }
        let mut out = BTreeMap::new();
        walk(root, "", &mut out);
        out
    }

    /// Funde, só com o que o dispositivo já tem, até sobrar uma ponta. Devolve os conflitos
    /// vistos. Não aplica na pasta (o teste aplica de propósito).
    async fn merge_locally(device: &Device, now: u64) -> Vec<crate::merge::Conflict> {
        let mut conflicts = Vec::new();
        loop {
            match device.engine.merge(now).await.unwrap() {
                MergeOutcome::Nothing { .. } => break,
                MergeOutcome::Merged {
                    conflicts: found,
                    tips_after,
                    ..
                } => {
                    conflicts.extend(found);
                    if tips_after <= 1 {
                        break;
                    }
                }
            }
        }
        conflicts
    }

    #[tokio::test]
    async fn two_devices_diverge_offline_exchange_through_a_remote_and_converge() {
        let tmp = TempDir::new();
        let remote = disk(&tmp, "remoto");
        let (a, b) = (Device::new(&tmp, "a", false), Device::new(&tmp, "b", false));

        // A cria o Vault e publica; B baixa e aplica o mesmo estado
        write(&a.vault, "notas/comum.md", b"comum");
        write(&a.vault, "notas/partilhada.md", b"base");
        write(&a.config, "app.toml", b"tema = 'base'");
        let base = a.snapshot(1).await;
        replicate(&a.local, &remote).await.unwrap();
        replicate(&remote, &b.local).await.unwrap();
        b.engine.apply(None, &base, &b.roots).await.unwrap();
        assert_eq!(tree(&a.vault), tree(&b.vault));

        // offline: os dois editam o mesmo arquivo (vault e config) e cada um cria um novo
        write(&a.vault, "notas/partilhada.md", b"do A");
        write(&a.vault, "notas/so-a.md", b"so a");
        write(&a.config, "app.toml", b"tema = 'a'");
        let tip_a = a.snapshot(10).await;
        write(&b.vault, "notas/partilhada.md", b"do B");
        write(&b.vault, "notas/so-b.md", b"so b");
        write(&b.config, "app.toml", b"tema = 'b'");
        let tip_b = b.snapshot(20).await;

        // troca pelo remoto: cada um fica com duas pontas
        replicate(&a.local, &remote).await.unwrap();
        replicate(&b.local, &remote).await.unwrap();
        replicate(&remote, &a.local).await.unwrap();
        replicate(&remote, &b.local).await.unwrap();
        assert_eq!(a.tips().await.len(), 2);
        assert_eq!(b.tips().await.len(), 2);

        // cada um funde sozinho (ids diferentes, nonce aleatório) e troca de novo
        let conflicts_a = merge_locally(&a, 30).await;
        let conflicts_b = merge_locally(&b, 31).await;
        assert_ne!(a.tips().await, b.tips().await); // duas fusões gêmeas, ids diferentes
        replicate(&a.local, &remote).await.unwrap();
        replicate(&b.local, &remote).await.unwrap();
        replicate(&remote, &a.local).await.unwrap();
        replicate(&remote, &b.local).await.unwrap();

        // as duas fusões gêmeas contam como uma ponta só, a mesma nos dois
        let (tips_a, tips_b) = (a.tips().await, b.tips().await);
        assert_eq!(tips_a.len(), 1, "{tips_a:?}");
        assert_eq!(tips_a, tips_b);
        assert!(!conflicts_a.is_empty() && !conflicts_b.is_empty());
        let merged = tips_a[0];

        // aplicar leva as duas pastas ao mesmo conteúdo
        a.engine
            .apply(Some(&tip_a), &merged, &a.roots)
            .await
            .unwrap();
        b.engine
            .apply(Some(&tip_b), &merged, &b.roots)
            .await
            .unwrap();
        assert_eq!(tree(&a.vault), tree(&b.vault));
        assert_eq!(tree(&a.config), tree(&b.config));

        let vault = tree(&a.vault);
        // o mais novo (B, 20) fica no lugar; o do A é preservado numa cópia .conflito-
        assert_eq!(vault["notas/partilhada.md"], b"do B");
        assert_eq!(vault["notas/so-a.md"], b"so a");
        assert_eq!(vault["notas/so-b.md"], b"so b");
        assert_eq!(vault["notas/comum.md"], b"comum");
        // nome da spec: <path>.conflito-<dispositivo>-<created_at do perdedor, UTC>.md
        assert_eq!(
            vault["notas/partilhada.md.conflito-a-19700101T000010Z.md"],
            b"do A"
        );
        assert_eq!(vault.len(), 5, "{:?}", vault.keys().collect::<Vec<_>>());

        // config conflitado: o perdedor (A) vai para fora das pastas das classes
        assert_eq!(tree(&a.config)["app.toml"], b"tema = 'b'");
        let losers = tmp.path().join("perdedores");
        a.engine
            .save_conflict_losers(&conflicts_a, &losers, &a.roots)
            .await
            .unwrap();
        let saved = tree(&losers);
        assert_eq!(saved.len(), 1, "{:?}", saved.keys().collect::<Vec<_>>());
        let (path, content) = saved.iter().next().unwrap();
        assert!(path.starts_with("config/app.toml.conflito-a-"), "{path}");
        assert_eq!(content, b"tema = 'a'");
    }

    #[tokio::test]
    async fn replication_never_makes_a_device_forget_and_a_rolled_back_store_is_refused() {
        let tmp = TempDir::new();
        let remote = disk(&tmp, "remoto");
        let stale = disk(&tmp, "remoto-velho");
        let (a, b) = (Device::new(&tmp, "a", false), Device::new(&tmp, "b", true));

        write(&a.vault, "x.md", b"v1");
        let v1 = a.snapshot(1).await;
        replicate(&a.local, &remote).await.unwrap();
        replicate(&remote, &stale).await.unwrap(); // foto do remoto no tempo de v1
        replicate(&remote, &b.local).await.unwrap();
        b.engine.apply(None, &v1, &b.roots).await.unwrap();

        write(&a.vault, "x.md", b"v2");
        let v2 = a.snapshot(2).await;
        replicate(&a.local, &remote).await.unwrap();
        replicate(&remote, &b.local).await.unwrap();
        b.engine.apply(Some(&v1), &v2, &b.roots).await.unwrap(); // B aceitou v2

        // o remoto volta no tempo e B puxa dele: replicar só soma, B continua em v2
        replicate(&stale, &b.local).await.unwrap();
        assert_eq!(b.tips().await, vec![v2]);

        // mas se o armazenamento do próprio B for trocado por uma cópia velha, é Rollback
        let reset = disk(&tmp, "b-resetado");
        replicate(&stale, &reset).await.unwrap();
        let again = Engine::new(reset, keys(), "b")
            .unwrap()
            .with_state(tmp.path().join("b-estado.json"));
        write(&b.vault, "y.md", b"nova");
        assert_eq!(
            again.snapshot(&b.roots, 3).await.map(|_| ()),
            Err(EngineError::Rollback)
        );
    }
}
