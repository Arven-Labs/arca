//! Replicação entre armazenamentos (`SPEC-TECNICA.md` §4 e §6, Fase 2a, PR 5).
//!
//! `replicate(origem, destino)` copia bytes opacos: não tem chave e não abre manifest. Não é
//! público (P-32.b): o consumidor usa `Engine::push` (local para o remoto) e `Engine::pull`
//! (remoto para o local, que recusa o manifest forjado). `replicate(remoto, local)` reabriria o
//! P-32. Quem agenda (segundo plano, retry) é o consumidor; o núcleo não cria tarefas.

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
    /// **Não detecta** cabeçalho forjado com corpo aleatório: isso é do `Engine::pull` (P-32).
    pub malformed: Vec<ObjectId>,
    /// Manifests com envelope válido que o filtro da chamada recusou (em `Engine::pull`: não
    /// abrem com a chave da época do dispositivo, ou seja, forjados ou de outro Vault): nunca
    /// gravados, e não retêm os demais manifests. O `replicate` cru não filtra e deixa isto vazio.
    pub forged: Vec<ObjectId>,
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
pub(crate) async fn replicate<A: BlobStore, B: BlobStore>(
    from: &A,
    to: &B,
) -> Result<ReplicateReport, StoreError> {
    replicate_filtered(from, to, |_| true).await
}

/// Como `replicate`, mas só grava um manifest se `accept_manifest(bytes)` aceitar (os bytes já
/// são um envelope de manifest com o hash certo). Um recusado vai para `forged`, não é gravado
/// e não retém os demais. `Engine::pull` usa isto com a chave do dispositivo (P-32).
pub(crate) async fn replicate_filtered<A: BlobStore, B: BlobStore>(
    from: &A,
    to: &B,
    accept_manifest: impl Fn(&[u8]) -> bool,
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
                    // o cabeçalho é em claro: dá para barrar lixo sem chave. Um cabeçalho
                    // forjado com corpo aleatório só o `accept_manifest` (com chave) barra.
                    if !is_envelope_of(kind, &bytes) {
                        report.malformed.push(id);
                        continue;
                    }
                    if kind == ObjectKind::Manifest && !accept_manifest(&bytes) {
                        report.forged.push(id);
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
                forged: vec![],
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
                forged: vec![],
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
        converge_through(&tmp, remote).await;
    }

    /// O mesmo cenário, com o remoto sendo um hub HTTP de verdade (2b-1): o hub de referência
    /// do contrato `/arca/v1/`, autenticado pelo token de cada dispositivo.
    #[cfg(feature = "hub")]
    #[tokio::test]
    async fn the_same_two_devices_converge_through_an_http_hub() {
        use crate::hub::HubStore;
        use crate::hub_testserver::TestHub;
        let tmp = TempDir::new();
        let hub = TestHub::start().await;
        hub.approve("token-de-teste");
        let remote = Backend::Hub(HubStore::new(&hub.url, "token-de-teste").unwrap());
        converge_through(&tmp, remote).await;
        // o hub só guardou objetos, e cada um com envelope válido
        assert!(hub.object_count() > 0);
    }

    /// Dois dispositivos divergem offline, trocam objetos por `remote`, fundem e convergem.
    async fn converge_through(tmp: &TempDir, remote: Backend) {
        let (a, b) = (Device::new(tmp, "a", false), Device::new(tmp, "b", false));

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

    // ---------- pull filtrado com chave (P-32, item 1) ----------

    fn sample_manifest(epoch: u32) -> crate::manifest::Manifest {
        crate::manifest::Manifest {
            format_version: FORMAT_VERSION as u16,
            key_epoch: epoch,
            base_version: None,
            merged_from: vec![],
            counter: 1,
            device: "x".to_string(),
            created_at: 0,
            files: vec![],
        }
    }

    /// Cabeçalho perfeito (época 1, tipo manifest) e corpo sem tag válida: o que o `replicate`
    /// cru deixa passar.
    fn forged_manifest() -> (ObjectId, Vec<u8>) {
        object(ObjectKind::Manifest, b"corpo aleatorio sem tag valida")
    }

    /// Manifest legítimo de **outro Vault**, na mesma época: envelope e tag perfeitos, mas a
    /// tag não confere com a chave deste dispositivo.
    fn foreign_manifest() -> (ObjectId, Vec<u8>) {
        let other = EpochKeys::derive(&VaultKey::from_bytes([9u8; 32]), 1);
        let sealed = sample_manifest(1).seal(&other).unwrap();
        (sealed.id, sealed.bytes)
    }

    /// `a` cria o Vault e publica em `remote`; devolve o id do manifest.
    async fn publish_base(a: &Device, remote: &Backend) -> ObjectId {
        write(&a.vault, "x.md", b"1");
        let base = a.snapshot(1).await;
        replicate(&a.local, remote).await.unwrap();
        base
    }

    #[tokio::test]
    async fn pull_skips_a_forged_manifest_keeps_the_real_ones_and_leaves_the_device_unblocked() {
        let tmp = TempDir::new();
        let remote = disk(&tmp, "remoto");
        let (a, b) = (Device::new(&tmp, "a", false), Device::new(&tmp, "b", false));
        let base = publish_base(&a, &remote).await;
        let forged = forged_manifest();
        remote.put(&forged.0, &forged.1).await.unwrap();

        let report = b.engine.pull(&remote).await.unwrap();
        assert_eq!(report.forged, vec![forged.0]);
        assert!(report.malformed.is_empty() && report.corrupt.is_empty());
        assert!(!report.manifests_withheld);
        assert_eq!(b.local.has(&forged.0).await, Ok(false));
        assert_eq!(b.local.has(&base).await, Ok(true));

        // `tips()` falha se houver `rejected`: o armazenamento local está limpo
        assert_eq!(b.tips().await, vec![base]);
        b.engine.apply(None, &base, &b.roots).await.unwrap();
        write(&b.vault, "y.md", b"2");
        b.snapshot(2).await; // não dá Diverged
    }

    /// Contraste: é por isso que o pull existe. O `replicate` cru copia o forjado e o
    /// armazenamento local passa a recusar o `snapshot`.
    #[tokio::test]
    async fn the_raw_replicate_still_copies_a_forged_manifest_and_that_blocks_the_snapshot() {
        let tmp = TempDir::new();
        let remote = disk(&tmp, "remoto");
        let (a, b) = (Device::new(&tmp, "a", false), Device::new(&tmp, "b", false));
        publish_base(&a, &remote).await;
        let forged = forged_manifest();
        remote.put(&forged.0, &forged.1).await.unwrap();

        let report = replicate(&remote, &b.local).await.unwrap();
        assert!(report.forged.is_empty());
        assert_eq!(b.local.has(&forged.0).await, Ok(true));
        write(&b.vault, "y.md", b"2");
        assert_eq!(
            b.engine.snapshot(&b.roots, 2).await.map(|_| ()),
            Err(EngineError::Diverged {
                tips: 1,
                rejected: 1
            })
        );
    }

    #[tokio::test]
    async fn pull_skips_a_manifest_sealed_with_another_vault_key_in_the_same_epoch() {
        let tmp = TempDir::new();
        let remote = disk(&tmp, "remoto");
        let (a, b) = (Device::new(&tmp, "a", false), Device::new(&tmp, "b", false));
        let base = publish_base(&a, &remote).await;
        let foreign = foreign_manifest();
        remote.put(&foreign.0, &foreign.1).await.unwrap();

        let report = b.engine.pull(&remote).await.unwrap();
        assert_eq!(report.forged, vec![foreign.0]);
        assert_eq!(b.local.has(&foreign.0).await, Ok(false));
        assert_eq!(b.tips().await, vec![base]);
    }

    #[tokio::test]
    async fn a_second_pull_copies_nothing_and_the_forged_one_is_tested_again() {
        let tmp = TempDir::new();
        let remote = disk(&tmp, "remoto");
        let (a, b) = (Device::new(&tmp, "a", false), Device::new(&tmp, "b", false));
        publish_base(&a, &remote).await;
        let forged = forged_manifest();
        remote.put(&forged.0, &forged.1).await.unwrap();

        let first = b.engine.pull(&remote).await.unwrap();
        assert!(first.copied > 0);
        let second = b.engine.pull(&remote).await.unwrap();
        assert_eq!(second.copied, 0);
        assert_eq!(second.already, first.copied);
        assert_eq!(second.forged, vec![forged.0]);
    }

    /// Sem a chave da outra época não há como autenticar: o manifest passa como no
    /// `replicate`. Documenta o limite (um cabeçalho forjado de época maior ainda dá
    /// `NeedsNewKey`; o de época menor vira histórico, salvo num dispositivo sem manifest da
    /// própria época, onde vira `rejected`: limite herdado do ADR 0013).
    #[tokio::test]
    async fn a_manifest_of_another_epoch_cannot_be_verified_and_is_copied() {
        let tmp = TempDir::new();
        let remote = disk(&tmp, "remoto");
        let local = disk(&tmp, "novo");
        let keys2 = EpochKeys::derive(&VaultKey::from_bytes([3u8; 32]), 2);
        let engine = Engine::new(local.clone(), keys2, "pc").unwrap();

        let old = sample_manifest(1).seal(&keys()).unwrap();
        remote.put(&old.id, &old.bytes).await.unwrap();
        let header = Header {
            format_version: FORMAT_VERSION as u16,
            alg: Alg::XChaCha20Poly1305,
            kind: ObjectKind::Manifest,
            key_epoch: 3,
            nonce: [7u8; NONCE_LEN],
        };
        let mut newer = header.encode().to_vec();
        newer.extend_from_slice(b"nao da para verificar");
        let newer_id = ObjectId::of(ObjectKind::Manifest, &newer);
        remote.put(&newer_id, &newer).await.unwrap();

        let report = engine.pull(&remote).await.unwrap();
        assert!(report.forged.is_empty());
        assert_eq!(local.has(&old.id).await, Ok(true));
        assert_eq!(local.has(&newer_id).await, Ok(true));
        assert!(matches!(
            engine.heads().await,
            Err(EngineError::NeedsNewKey { have: 2, found: 3 })
        ));
    }

    #[tokio::test]
    async fn a_corrupt_blob_still_withholds_the_manifests_in_a_pull() {
        let tmp = TempDir::new();
        let remote = disk(&tmp, "remoto");
        let (a, b) = (Device::new(&tmp, "a", false), Device::new(&tmp, "b", false));
        let base = publish_base(&a, &remote).await;
        let bad = object(ObjectKind::Blob, b"ruim");
        remote.put(&bad.0, &bad.1).await.unwrap();
        corrupt_on_disk(&tmp, "remoto", &bad.0);

        let report = b.engine.pull(&remote).await.unwrap();
        assert_eq!(report.corrupt, vec![bad.0]);
        assert!(report.manifests_withheld);
        assert_eq!(b.local.has(&base).await, Ok(false));
    }

    /// Fuzz: corpos aleatórios sob cabeçalho válido, manifests legítimos com um bit trocado
    /// no corpo e truncados em qualquer ponto. Nenhum entra, nenhum causa pânico.
    #[tokio::test]
    async fn pull_never_copies_random_mutated_or_truncated_manifests() {
        struct Rng(u64);
        impl Rng {
            fn next(&mut self) -> u64 {
                self.0 ^= self.0 << 13;
                self.0 ^= self.0 >> 7;
                self.0 ^= self.0 << 17;
                self.0
            }
        }
        let tmp = TempDir::new();
        let remote = disk(&tmp, "remoto");
        let b = Device::new(&tmp, "b", false);
        let real = sample_manifest(1).seal(&keys()).unwrap();
        let mut rng = Rng(0x9E37_79B9_7F4A_7C15);
        let mut bad: Vec<(ObjectId, Vec<u8>)> = Vec::new();

        for _ in 0..100 {
            let mut header = Header {
                format_version: FORMAT_VERSION as u16,
                alg: Alg::XChaCha20Poly1305,
                kind: ObjectKind::Manifest,
                key_epoch: 1,
                nonce: [0u8; NONCE_LEN],
            };
            header
                .nonce
                .iter_mut()
                .for_each(|byte| *byte = rng.next() as u8);
            let mut bytes = header.encode().to_vec();
            (0..(rng.next() % 200)).for_each(|_| bytes.push(rng.next() as u8));
            bad.push((ObjectId::of(ObjectKind::Manifest, &bytes), bytes));
        }
        for _ in 0..100 {
            let mut bytes = real.bytes.clone();
            let at = crate::HEADER_LEN + (rng.next() as usize) % (bytes.len() - crate::HEADER_LEN);
            bytes[at] ^= 1 << (rng.next() % 8);
            bad.push((ObjectId::of(ObjectKind::Manifest, &bytes), bytes));
        }
        for cut in crate::HEADER_LEN..real.bytes.len() {
            let bytes = real.bytes[..cut].to_vec();
            bad.push((ObjectId::of(ObjectKind::Manifest, &bytes), bytes));
        }
        bad.sort_by_key(|(id, _)| id.hash);
        bad.dedup_by_key(|(id, _)| id.hash);
        put_all(&remote, &bad).await;
        // o legítimo no meio do lixo é o único que entra
        remote.put(&real.id, &real.bytes).await.unwrap();

        let report = b.engine.pull(&remote).await.unwrap();
        let mut forged = report.forged.clone();
        forged.sort_by_key(|id| id.hash);
        let expected: Vec<ObjectId> = bad.iter().map(|(id, _)| *id).collect();
        assert_eq!(forged, expected);
        assert_eq!(report.copied, 1);
        assert_eq!(count(&b.local, ObjectKind::Manifest).await, 1);
        assert_eq!(b.local.has(&real.id).await, Ok(true));
    }

    // ---------- Engine::push (P-32.b) ----------

    #[tokio::test]
    async fn push_copies_what_the_remote_lacks_and_a_second_push_copies_nothing() {
        let tmp = TempDir::new();
        let remote = disk(&tmp, "remoto");
        let a = Device::new(&tmp, "a", false);
        write(&a.vault, "x.md", b"1");
        write(&a.vault, "y.md", b"2");
        let base = a.snapshot(1).await;

        let first = a.engine.push(&remote).await.unwrap();
        assert_eq!(first.copied, 3); // 2 blobs e 1 manifest
        assert!(!first.manifests_withheld && first.forged.is_empty());
        assert_eq!(remote.has(&base).await, Ok(true));

        let second = a.engine.push(&remote).await.unwrap();
        assert_eq!((second.copied, second.already), (0, 3));
    }

    #[tokio::test]
    async fn push_never_deletes_what_only_the_remote_has() {
        let tmp = TempDir::new();
        let remote = Probe::new(disk(&tmp, "remoto"));
        let a = Device::new(&tmp, "a", false);
        let only_there = object(ObjectKind::Blob, b"so-no-remoto");
        remote.put(&only_there.0, &only_there.1).await.unwrap();
        write(&a.vault, "x.md", b"1");
        a.snapshot(1).await;

        a.engine.push(&remote).await.unwrap();
        assert_eq!(remote.has(&only_there.0).await, Ok(true));
        assert_eq!(*remote.deletes.lock().unwrap(), 0);
    }

    /// Mesma semântica do `replicate`: blob local corrompido retém os manifests, e `Ok` com
    /// `manifests_withheld` não é sucesso.
    #[tokio::test]
    async fn push_withholds_the_manifests_when_a_local_blob_is_corrupt() {
        let tmp = TempDir::new();
        let remote = disk(&tmp, "remoto");
        let a = Device::new(&tmp, "a", false);
        write(&a.vault, "x.md", b"1");
        let base = a.snapshot(1).await;
        let blob = a.local.list(ObjectKind::Blob).await.unwrap()[0];
        corrupt_on_disk(&tmp, "a-store", &blob);

        let report = a.engine.push(&remote).await.unwrap();
        assert_eq!(report.corrupt, vec![blob]);
        assert!(report.manifests_withheld);
        assert_eq!(remote.has(&base).await, Ok(false));
    }

    /// Auditoria do P-32: um manifest cifrado com a chave certa (tag válida) mas cujo conteúdo o
    /// `Manifest::open` recusa também não pode entrar. O filtro tem de ser o mesmo critério do
    /// `classify_manifest`; um filtro que só conferisse a tag deixaria isto virar `rejected`.
    #[tokio::test]
    async fn pull_refuses_authentic_ciphertext_whose_manifest_content_is_invalid() {
        let tmp = TempDir::new();
        let remote = disk(&tmp, "remoto");
        let b = Device::new(&tmp, "b", false);
        let json = String::from_utf8(sample_manifest(1).to_vec().unwrap()).unwrap();
        let bodies = [
            json.replace("\"key_epoch\":1", "\"key_epoch\":5"),
            json.replace("\"device\":\"x\"", "\"device\":\"\""),
            json.replacen('}', ",\"extra\":1}", 1),
        ];
        let mut planted = Vec::new();
        for body in &bodies {
            assert_ne!(body, &json, "a troca não aconteceu");
            let sealed = crate::crypto::seal(&keys(), ObjectKind::Manifest, body.as_bytes());
            let sealed = sealed.unwrap();
            remote.put(&sealed.id, &sealed.bytes).await.unwrap();
            planted.push(sealed.id);
        }

        let report = b.engine.pull(&remote).await.unwrap();
        let mut forged = report.forged.clone();
        forged.sort_by_key(|id| id.hash);
        planted.sort_by_key(|id| id.hash);
        assert_eq!(forged, planted);
        assert_eq!(report.copied, 0);
        // invariante: depois de um pull, o armazenamento local não tem nada ilegível
        assert!(b.engine.heads().await.unwrap().rejected.is_empty());
    }

    /// Dois dispositivos divergem, trocam objetos por um remoto que tem manifests forjados e
    /// convergem: nenhum dos dois fica travado.
    #[tokio::test]
    async fn two_devices_converge_through_a_remote_that_holds_forged_manifests() {
        let tmp = TempDir::new();
        let remote = disk(&tmp, "remoto");
        let (a, b) = (Device::new(&tmp, "a", false), Device::new(&tmp, "b", false));
        let base = publish_base(&a, &remote).await;
        b.engine.pull(&remote).await.unwrap();
        b.engine.apply(None, &base, &b.roots).await.unwrap();

        for planted in [forged_manifest(), foreign_manifest()] {
            remote.put(&planted.0, &planted.1).await.unwrap();
        }
        write(&a.vault, "so-a.md", b"a");
        a.snapshot(10).await;
        write(&b.vault, "so-b.md", b"b");
        b.snapshot(20).await;
        replicate(&a.local, &remote).await.unwrap();
        replicate(&b.local, &remote).await.unwrap();
        a.engine.pull(&remote).await.unwrap();
        b.engine.pull(&remote).await.unwrap();
        assert_eq!(a.tips().await.len(), 2);
        assert_eq!(b.tips().await.len(), 2);

        merge_locally(&a, 30).await;
        merge_locally(&b, 31).await;
        replicate(&a.local, &remote).await.unwrap();
        replicate(&b.local, &remote).await.unwrap();
        a.engine.pull(&remote).await.unwrap();
        b.engine.pull(&remote).await.unwrap();
        let (tips_a, tips_b) = (a.tips().await, b.tips().await);
        assert_eq!(tips_a.len(), 1, "{tips_a:?}");
        assert_eq!(tips_a, tips_b);
    }
}
