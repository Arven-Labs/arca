//! Verificação de integridade do armazenamento e teste de restauração (Fase 3, fatia 3).
//!
//! Duas funções puras, só de leitura, que servem a qualquer `BlobStore` (disco, hub, S3):
//! `Engine::verify` (todo blob que um manifest legível cita existe e, na amostra, bate com o
//! hash) e `Engine::verify_restore` (a ponta baixa, decifra e confere o texto claro de cada
//! entrada, **sem gravar nada em disco**). Quem agenda é o consumidor: o núcleo não cria tarefas.
//!
//! Três degraus de custo e de prova: `has` (o nome existe; um provedor hostil pode mentir),
//! `verified_get` (os bytes cifrados têm o hash do id) e `plain_blob` (tag de autenticação e
//! hash do texto claro). Relatórios e erros só trazem ids, nunca caminho ou nome de arquivo.

use super::{Engine, EngineError, Loaded};
use crate::object::{ObjectId, ObjectKind};
use crate::store::{verified_get, BlobStore, StoreError};
use std::collections::{BTreeSet, HashSet};

/// Quanto custa e o que prova uma rodada de `verify`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VerifyMode {
    /// Só `list` e `has`: confere que cada blob citado existe. Não baixa blob, então não vê
    /// objeto com os bytes trocados.
    Presence,
    /// Presença, mais `verified_get` de uma janela de até `max_blobs` blobs, a partir de
    /// `offset` na lista ordenada por hash, **sem dar a volta dentro da rodada**. O relatório
    /// devolve `next_offset`: encadeando, o Vault parado é lido inteiro em ⌈N/`max_blobs`⌉
    /// rodadas. `offset` maior que N dá a volta (módulo N).
    Sample { max_blobs: usize, offset: usize },
}

/// O que uma chamada de `verify` achou. `clean == false` não é sucesso: há algo errado ou algo
/// que ficou sem conferir.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct VerifyReport {
    /// Manifests que abriram com a chave.
    pub manifests_ok: usize,
    /// Manifests que não abriram (forjados, corrompidos, de outro Vault): não travam a rodada.
    pub rejected: Vec<ObjectId>,
    /// Blobs distintos citados por todos os manifests legíveis (o histórico inteiro).
    pub referenced_blobs: usize,
    /// Blobs citados que o armazenamento não tem.
    pub missing: Vec<ObjectId>,
    /// Blobs lidos na amostra cujos bytes não têm o hash do id.
    pub corrupt: Vec<ObjectId>,
    /// Blobs cuja checagem falhou por erro de armazenamento (rede, quota): tentar de novo.
    pub unreadable: Vec<ObjectId>,
    /// Quantos blobs a janela da amostra tentou ler (0 em `Presence`).
    pub sampled: usize,
    /// Onde a próxima rodada de `Sample` deve começar (0 em `Presence` ou ao fechar a volta).
    pub next_offset: usize,
    /// Nada errado e nada sem conferir: sem `rejected`, `missing`, `corrupt` nem `unreadable`.
    pub clean: bool,
}

/// O que `verify_restore` conferiu.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RestoreCheck {
    /// Entradas decifradas e conferidas.
    pub files: usize,
    /// Soma dos tamanhos do texto claro.
    pub bytes: u64,
}

impl<S: BlobStore> Engine<S> {
    /// Ver `VerifyMode`. `Unavailable` ao listar ou abrir os manifests é `Err` (tentar de
    /// novo); numa checagem de blob vira `unreadable` e a rodada segue.
    pub async fn verify(&self, mode: VerifyMode) -> Result<VerifyReport, EngineError> {
        let Loaded {
            readable, rejected, ..
        } = self.load().await?;
        let mut report = VerifyReport {
            manifests_ok: readable.len(),
            rejected: rejected.iter().map(|r| r.id).collect(),
            ..VerifyReport::default()
        };

        // os blobs distintos de todo o histórico, em ordem por hash (a ordem da janela)
        let referenced: BTreeSet<[u8; 32]> = readable
            .iter()
            .flat_map(|(_, manifest)| manifest.files.iter().map(|entry| entry.blob.0))
            .collect();
        let blobs: Vec<ObjectId> = referenced
            .into_iter()
            .map(|hash| ObjectId {
                kind: ObjectKind::Blob,
                hash,
            })
            .collect();
        report.referenced_blobs = blobs.len();

        let mut present = HashSet::with_capacity(blobs.len());
        for id in &blobs {
            match self.store.has(id).await {
                Ok(true) => {
                    present.insert(*id);
                }
                Ok(false) => report.missing.push(*id),
                Err(StoreError::Unavailable(_)) => report.unreadable.push(*id),
                Err(e) => return Err(e.into()),
            }
        }

        if let VerifyMode::Sample { max_blobs, offset } = mode {
            if !blobs.is_empty() {
                let start = offset % blobs.len();
                let end = start.saturating_add(max_blobs).min(blobs.len());
                report.sampled = end - start;
                report.next_offset = end % blobs.len();
                for id in &blobs[start..end] {
                    // o que já falhou na presença não é lido de novo
                    if !present.contains(id) {
                        continue;
                    }
                    match verified_get(&self.store, id).await {
                        Ok(Some(_)) => {}
                        Ok(None) => report.missing.push(*id),
                        Err(StoreError::Corrupt) => report.corrupt.push(*id),
                        Err(StoreError::Unavailable(_)) => report.unreadable.push(*id),
                        Err(e) => return Err(e.into()),
                    }
                }
            }
        }

        report.clean = report.rejected.is_empty()
            && report.missing.is_empty()
            && report.corrupt.is_empty()
            && report.unreadable.is_empty();
        Ok(report)
    }

    /// Baixa, decifra e confere o texto claro de cada entrada de `id`, um blob por vez e sem
    /// escrever em disco. Baixa o Vault inteiro (egress do provedor): uso raro. O primeiro erro
    /// aborta (`MissingObject`, `Store(Corrupt)`, `Integrity`).
    pub async fn verify_restore(&self, id: &ObjectId) -> Result<RestoreCheck, EngineError> {
        let manifest = self.read_manifest(id).await?;
        let mut check = RestoreCheck { files: 0, bytes: 0 };
        for entry in &manifest.files {
            if !self.accepts(entry) {
                continue;
            }
            // o texto claro vive só nesta volta do laço e é zerado ao sair (`Zeroizing`)
            let blob = ObjectId {
                kind: ObjectKind::Blob,
                hash: entry.blob.0,
            };
            let bytes = verified_get(&self.store, &blob)
                .await?
                .ok_or(EngineError::MissingObject)?;
            check.files += 1;
            check.bytes += bytes.len() as u64;
        }
        Ok(check)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::backend::Backend;
    use crate::crypto::{EpochKeys, VaultKey};
    use crate::disk::DiskStore;
    use crate::engine::{Roots, Snapshot};
    use crate::envelope::{Alg, Header, NONCE_LEN};
    use crate::store::Capabilities;
    use crate::testutil::TempDir;
    use crate::FORMAT_VERSION;
    use std::fs;
    use std::io;
    use std::path::{Path, PathBuf};
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::{Arc, Mutex};

    fn keys() -> EpochKeys {
        EpochKeys::derive(&VaultKey::from_bytes([4u8; 32]), 1)
    }

    /// Armazenamento de disco com espiões: registra cada `get` de blob e falha de propósito.
    #[derive(Clone)]
    struct Spy {
        inner: Backend,
        blob_reads: Arc<Mutex<Vec<ObjectId>>>,
        has_calls: Arc<Mutex<usize>>,
        fail_get: Arc<Mutex<Option<ObjectId>>>,
        fail_list: Arc<AtomicBool>,
    }

    impl BlobStore for Spy {
        async fn put(&self, id: &ObjectId, bytes: &[u8]) -> Result<(), StoreError> {
            self.inner.put(id, bytes).await
        }
        async fn get(&self, id: &ObjectId) -> Result<Option<Vec<u8>>, StoreError> {
            if id.kind == ObjectKind::Blob {
                self.blob_reads.lock().unwrap().push(*id);
            }
            if *self.fail_get.lock().unwrap() == Some(*id) {
                return Err(StoreError::Unavailable(io::ErrorKind::Other));
            }
            self.inner.get(id).await
        }
        async fn has(&self, id: &ObjectId) -> Result<bool, StoreError> {
            *self.has_calls.lock().unwrap() += 1;
            self.inner.has(id).await
        }
        async fn list(&self, kind: ObjectKind) -> Result<Vec<ObjectId>, StoreError> {
            if self.fail_list.load(Ordering::SeqCst) {
                return Err(StoreError::Unavailable(io::ErrorKind::Other));
            }
            self.inner.list(kind).await
        }
        async fn delete(&self, id: &ObjectId) -> Result<(), StoreError> {
            self.inner.delete(id).await
        }
        fn capabilities(&self) -> Capabilities {
            self.inner.capabilities()
        }
    }

    /// Dispositivo com o Vault de exemplo já em um snapshot: `x.md`, `notas/y.md` e `config.toml`
    /// (3 blobs, 1 manifest).
    struct Fx {
        tmp: TempDir,
        spy: Spy,
        engine: Engine<Spy>,
        roots: Roots,
        vault: PathBuf,
        base: ObjectId,
    }

    fn write(root: &Path, rel: &str, data: &[u8]) {
        let p = root.join(rel);
        fs::create_dir_all(p.parent().unwrap()).unwrap();
        fs::write(p, data).unwrap();
    }

    fn created(s: Snapshot) -> ObjectId {
        match s {
            Snapshot::Created { id, .. } => id,
            other => panic!("esperava Created, veio {other:?}"),
        }
    }

    impl Fx {
        async fn new() -> Fx {
            let tmp = TempDir::new();
            let store = Backend::Disk(DiskStore::open(tmp.path().join("store")).unwrap());
            let spy = Spy {
                inner: store,
                blob_reads: Arc::default(),
                has_calls: Arc::default(),
                fail_get: Arc::default(),
                fail_list: Arc::default(),
            };
            let vault = tmp.path().join("vault");
            let config = tmp.path().join("config");
            fs::create_dir_all(&vault).unwrap();
            fs::create_dir_all(&config).unwrap();
            write(&vault, "x.md", b"uno");
            write(&vault, "notas/y.md", b"dos");
            write(&config, "config.toml", b"modelo = \"x\"");
            let roots = Roots {
                vault: Some(vault.clone()),
                config: Some(config),
                secret: None,
            };
            let engine = Engine::new(spy.clone(), keys(), "pc").unwrap();
            let base = created(engine.snapshot(&roots, 1).await.unwrap());
            Fx {
                tmp,
                spy,
                engine,
                roots,
                vault,
                base,
            }
        }

        async fn blobs(&self) -> Vec<ObjectId> {
            let mut ids = self.spy.inner.list(ObjectKind::Blob).await.unwrap();
            ids.sort_by_key(|id| id.hash);
            ids
        }

        fn corrupt(&self, id: &ObjectId) {
            fs::write(
                self.tmp.path().join("store").join(id.to_string()),
                b"adulterado",
            )
            .unwrap();
        }

        fn reads(&self) -> Vec<ObjectId> {
            self.spy.blob_reads.lock().unwrap().clone()
        }

        fn clear_reads(&self) {
            self.spy.blob_reads.lock().unwrap().clear();
            *self.spy.has_calls.lock().unwrap() = 0;
        }

        /// Uma segunda versão: só `x.md` muda (4 blobs e 2 manifests no total).
        async fn second_version(&self) -> ObjectId {
            write(&self.vault, "x.md", b"uno editado");
            created(self.engine.snapshot(&self.roots, 2).await.unwrap())
        }
    }

    /// Manifest com cabeçalho de envelope válido e corpo sem tag: o que o `list` mostra e o
    /// `Manifest::open` recusa.
    fn forged_manifest() -> (ObjectId, Vec<u8>) {
        let header = Header {
            format_version: FORMAT_VERSION as u16,
            alg: Alg::XChaCha20Poly1305,
            kind: ObjectKind::Manifest,
            key_epoch: 1,
            nonce: [7u8; NONCE_LEN],
        };
        let mut bytes = header.encode().to_vec();
        bytes.extend_from_slice(b"corpo aleatorio sem tag valida");
        (ObjectId::of(ObjectKind::Manifest, &bytes), bytes)
    }

    // 1
    #[tokio::test]
    async fn presence_on_a_healthy_vault_is_clean_and_reads_no_blob() {
        let fx = Fx::new().await;
        fx.clear_reads();
        let report = fx.engine.verify(VerifyMode::Presence).await.unwrap();
        assert!(report.clean, "{report:?}");
        assert_eq!(report.manifests_ok, 1);
        assert_eq!(report.referenced_blobs, 3);
        assert!(report.missing.is_empty() && report.corrupt.is_empty());
        assert_eq!((report.sampled, report.next_offset), (0, 0));
        assert!(fx.reads().is_empty(), "Presence baixou blob");
        assert_eq!(*fx.spy.has_calls.lock().unwrap(), 3);
    }

    // 2
    #[tokio::test]
    async fn presence_reports_a_deleted_blob_as_missing() {
        let fx = Fx::new().await;
        let gone = fx.blobs().await[1];
        fx.spy.inner.delete(&gone).await.unwrap();
        let report = fx.engine.verify(VerifyMode::Presence).await.unwrap();
        assert_eq!(report.missing, vec![gone]);
        assert!(!report.clean);
        assert_eq!(report.referenced_blobs, 3);
    }

    // 3
    #[tokio::test]
    async fn presence_does_not_see_a_corrupt_blob_but_sample_does() {
        let fx = Fx::new().await;
        let bad = fx.blobs().await[0];
        fx.corrupt(&bad);

        let presence = fx.engine.verify(VerifyMode::Presence).await.unwrap();
        assert!(presence.clean && presence.corrupt.is_empty());

        let sample = fx
            .engine
            .verify(VerifyMode::Sample {
                max_blobs: 3,
                offset: 0,
            })
            .await
            .unwrap();
        assert_eq!(sample.corrupt, vec![bad]);
        assert_eq!(sample.sampled, 3);
        assert!(!sample.clean);
    }

    // 4
    #[tokio::test]
    async fn a_forged_manifest_is_rejected_and_makes_the_round_unclean_but_the_rest_is_checked() {
        let fx = Fx::new().await;
        let forged = forged_manifest();
        fx.spy.put(&forged.0, &forged.1).await.unwrap();
        let report = fx.engine.verify(VerifyMode::Presence).await.unwrap();
        assert_eq!(report.rejected, vec![forged.0]);
        assert!(!report.clean);
        assert_eq!((report.manifests_ok, report.referenced_blobs), (1, 3));
        assert!(report.missing.is_empty());
    }

    // 5
    #[tokio::test]
    async fn sample_windows_chained_by_next_offset_read_every_blob_once() {
        let fx = Fx::new().await;
        fx.second_version().await;
        let all = fx.blobs().await;
        assert_eq!(all.len(), 4);
        fx.clear_reads();

        let first = fx
            .engine
            .verify(VerifyMode::Sample {
                max_blobs: 3,
                offset: 0,
            })
            .await
            .unwrap();
        assert_eq!((first.sampled, first.next_offset), (3, 3));
        assert_eq!(fx.reads(), all[0..3].to_vec());

        fx.clear_reads();
        let second = fx
            .engine
            .verify(VerifyMode::Sample {
                max_blobs: 3,
                offset: first.next_offset,
            })
            .await
            .unwrap();
        assert_eq!((second.sampled, second.next_offset), (1, 0));
        assert_eq!(fx.reads(), all[3..4].to_vec());
        assert!(first.clean && second.clean);
    }

    #[tokio::test]
    async fn sample_with_zero_blobs_reads_nothing_and_a_big_offset_wraps() {
        let fx = Fx::new().await;
        fx.second_version().await;
        let all = fx.blobs().await;
        fx.clear_reads();

        let none = fx
            .engine
            .verify(VerifyMode::Sample {
                max_blobs: 0,
                offset: 0,
            })
            .await
            .unwrap();
        assert_eq!(none.sampled, 0);
        assert!(fx.reads().is_empty());

        // 10 % 4 = 2: lê os blobs 2 e 3 e fecha a volta
        let wrapped = fx
            .engine
            .verify(VerifyMode::Sample {
                max_blobs: 3,
                offset: 10,
            })
            .await
            .unwrap();
        assert_eq!((wrapped.sampled, wrapped.next_offset), (2, 0));
        assert_eq!(fx.reads(), all[2..4].to_vec());
    }

    // 6
    #[tokio::test]
    async fn a_blob_cited_by_two_manifests_is_counted_and_checked_once() {
        let fx = Fx::new().await;
        fx.second_version().await;
        fx.clear_reads();
        let report = fx
            .engine
            .verify(VerifyMode::Sample {
                max_blobs: 100,
                offset: 0,
            })
            .await
            .unwrap();
        assert_eq!(report.manifests_ok, 2);
        assert_eq!(report.referenced_blobs, 4); // 6 citações, 4 blobs distintos
        assert_eq!(fx.reads().len(), 4);
        assert_eq!(*fx.spy.has_calls.lock().unwrap(), 4);
        assert!(report.clean);
    }

    // 7
    #[tokio::test]
    async fn a_blob_whose_read_fails_is_unreadable_and_the_others_are_still_read() {
        let fx = Fx::new().await;
        let all = fx.blobs().await;
        *fx.spy.fail_get.lock().unwrap() = Some(all[1]);
        fx.clear_reads();
        let report = fx
            .engine
            .verify(VerifyMode::Sample {
                max_blobs: 3,
                offset: 0,
            })
            .await
            .unwrap();
        assert_eq!(report.unreadable, vec![all[1]]);
        assert_eq!(report.sampled, 3);
        assert_eq!(fx.reads().len(), 3, "a falha de um blob parou a rodada");
        assert!(!report.clean);
    }

    #[tokio::test]
    async fn a_failing_list_is_an_error_not_a_report() {
        let fx = Fx::new().await;
        fx.spy.fail_list.store(true, Ordering::SeqCst);
        assert!(matches!(
            fx.engine.verify(VerifyMode::Presence).await,
            Err(EngineError::Store(StoreError::Unavailable(_)))
        ));
    }

    // 8
    #[tokio::test]
    async fn verify_restore_checks_every_entry_and_writes_nothing_to_disk() {
        let fx = Fx::new().await;
        let before = tree_names(fx.tmp.path());
        let check = fx.engine.verify_restore(&fx.base).await.unwrap();
        assert_eq!(check.files, 3);
        assert_eq!(check.bytes, 3 + 3 + 12); // "uno" + "dos" + `modelo = "x"` (12)
        assert_eq!(tree_names(fx.tmp.path()), before, "criou ou apagou arquivo");
    }

    fn tree_names(root: &Path) -> Vec<(String, u64)> {
        fn walk(dir: &Path, out: &mut Vec<(String, u64)>) {
            for entry in fs::read_dir(dir).unwrap() {
                let entry = entry.unwrap();
                let meta = entry.metadata().unwrap();
                out.push((entry.path().display().to_string(), meta.len()));
                if meta.is_dir() {
                    walk(&entry.path(), out);
                }
            }
        }
        let mut out = Vec::new();
        walk(root, &mut out);
        out.sort();
        out
    }

    // 9
    #[tokio::test]
    async fn verify_restore_with_a_missing_blob_is_missing_object() {
        let fx = Fx::new().await;
        let gone = fx.blobs().await[1];
        fx.spy.inner.delete(&gone).await.unwrap();
        assert!(matches!(
            fx.engine.verify_restore(&fx.base).await,
            Err(EngineError::MissingObject)
        ));
    }

    #[tokio::test]
    async fn verify_restore_with_a_corrupt_blob_is_a_storage_corrupt_error() {
        let fx = Fx::new().await;
        let bad = fx.blobs().await[0];
        fx.corrupt(&bad);
        assert!(matches!(
            fx.engine.verify_restore(&fx.base).await,
            Err(EngineError::Store(StoreError::Corrupt))
        ));
    }

    // 10
    #[tokio::test]
    async fn verify_restore_with_a_blob_swapped_for_another_valid_one_is_integrity() {
        let fx = Fx::new().await;
        // um manifest autêntico cuja 1ª entrada aponta para o blob de OUTRA entrada: o hash do
        // blob confere com o id e a tag abre, mas o texto claro não é o que o manifest diz
        let manifest = {
            let Loaded { readable, .. } = fx.engine.load().await.unwrap();
            readable[0].1.clone()
        };
        let mut swapped = manifest.clone();
        swapped.files[0].blob = manifest.files[1].blob;
        swapped.counter += 1;
        let sealed = swapped.seal(&keys()).unwrap();
        fx.spy.put(&sealed.id, &sealed.bytes).await.unwrap();
        assert!(matches!(
            fx.engine.verify_restore(&sealed.id).await,
            Err(EngineError::Integrity)
        ));
    }

    // 11
    #[tokio::test]
    async fn verify_restore_accepts_an_empty_file_and_a_five_megabyte_file() {
        let fx = Fx::new().await;
        write(&fx.vault, "vazio.md", b"");
        write(&fx.vault, "grande.md", &vec![0x5A; 5 * 1024 * 1024]);
        let id = created(fx.engine.snapshot(&fx.roots, 3).await.unwrap());
        let check = fx.engine.verify_restore(&id).await.unwrap();
        assert_eq!(check.files, 5);
        assert_eq!(check.bytes, 3 + 3 + 12 + 5 * 1024 * 1024);
    }

    // 12
    #[tokio::test]
    async fn reports_and_errors_never_leak_file_names_or_paths() {
        let fx = Fx::new().await;
        write(&fx.vault, "nome-secreto.md", b"x");
        let id = created(fx.engine.snapshot(&fx.roots, 2).await.unwrap());
        for blob in fx.blobs().await {
            fx.corrupt(&blob);
        }
        let report = fx
            .engine
            .verify(VerifyMode::Sample {
                max_blobs: 100,
                offset: 0,
            })
            .await
            .unwrap();
        let err = fx.engine.verify_restore(&id).await.unwrap_err();
        let shown = format!("{report:?} {err:?} {err}");
        let tmp = fx.tmp.path().display().to_string();
        assert!(!report.corrupt.is_empty());
        assert!(
            !shown.contains("nome-secreto") && !shown.contains(&tmp),
            "{shown}"
        );
    }
}
