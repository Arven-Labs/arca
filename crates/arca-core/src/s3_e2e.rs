//! Ponta a ponta contra um S3 de verdade (Garage no CI, Fase 3, fatia 2): `push`, `pull` e
//! restauração direto do bucket.
//!
//! Só roda com as variáveis `ARCA_S3_TEST_*` (o job `s3-garage` as define); sem elas passa sem
//! fazer nada. Cada cenário usa o **seu bucket** (`ARCA_S3_TEST_BUCKET_E2E_<n>`): o `Engine` lê
//! o bucket inteiro, e um bucket compartilhado com os testes de `s3.rs` (blobs sem envelope,
//! centenas de manifests falsos) viraria `rejected` e travaria o cenário.

use crate::backend::Backend;
use crate::crypto::{EpochKeys, VaultKey};
use crate::disk::DiskStore;
use crate::engine::{Engine, EngineError, MergeOutcome, Roots, Snapshot, VerifyMode};
use crate::envelope::{Alg, Header, NONCE_LEN};
use crate::object::{ObjectId, ObjectKind};
use crate::s3::{S3Config, S3Store};
use crate::store::{BlobStore, StoreError};
use crate::testutil::TempDir;
use crate::FORMAT_VERSION;
use object_store::aws::AmazonS3Builder;
use object_store::path::Path as ObjectPath;
use object_store::{ClientOptions, ObjectStoreExt, PutPayload};
use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use zeroize::Zeroizing;

/// A configuração do bucket do cenário `n`, ou `None` fora do job do Garage.
fn e2e_config(n: u8) -> Option<S3Config> {
    let var = |k: &str| std::env::var(k).ok();
    let config = (|| {
        Some(S3Config {
            endpoint: var("ARCA_S3_TEST_ENDPOINT")?,
            region: var("ARCA_S3_TEST_REGION").unwrap_or_else(|| "garage".to_string()),
            bucket: var(&format!("ARCA_S3_TEST_BUCKET_E2E_{n}"))?,
            access_key_id: var("ARCA_S3_TEST_KEY")?,
            secret_access_key: Zeroizing::new(var("ARCA_S3_TEST_SECRET")?),
            allow_http: true,
            consistent_list: false,
        })
    })();
    // o job `s3-garage` exige o servidor: sem as variáveis o teste falha, não passa vazio
    assert!(
        config.is_some() || std::env::var_os("ARCA_S3_TEST_REQUIRED").is_none(),
        "ARCA_S3_TEST_REQUIRED está definida mas faltam variáveis ARCA_S3_TEST_* do cenário {n}"
    );
    config
}

/// Grava uma chave qualquer no bucket, sem passar pelo `S3Store` (que recusa id que não bate
/// com o hash): monta o objeto corrompido que um provedor ou um atacante poderia deixar lá.
async fn raw_put(config: &S3Config, key: &str, bytes: &[u8]) {
    let raw = AmazonS3Builder::new()
        .with_endpoint(config.endpoint.clone())
        .with_region(config.region.clone())
        .with_bucket_name(config.bucket.clone())
        .with_access_key_id(config.access_key_id.clone())
        .with_secret_access_key(config.secret_access_key.as_str())
        .with_virtual_hosted_style_request(false)
        .with_client_options(ClientOptions::new().with_allow_http(true))
        .build()
        .unwrap();
    raw.put(
        &ObjectPath::parse(key).unwrap(),
        PutPayload::from(bytes.to_vec()),
    )
    .await
    .unwrap();
}

fn keys() -> EpochKeys {
    EpochKeys::derive(&VaultKey::from_bytes([5u8; 32]), 1)
}

fn disk(tmp: &TempDir, name: &str) -> Backend {
    Backend::Disk(DiskStore::open(tmp.path().join(name)).unwrap())
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

/// Pastas de um dispositivo (`vault` e `config`), criadas vazias.
fn roots(tmp: &TempDir, name: &str) -> (Roots, PathBuf, PathBuf) {
    let base = tmp.path().join(name);
    let (vault, config) = (base.join("vault"), base.join("config"));
    fs::create_dir_all(&vault).unwrap();
    fs::create_dir_all(&config).unwrap();
    let r = Roots {
        vault: Some(vault.clone()),
        config: Some(config.clone()),
        secret: None,
    };
    (r, vault, config)
}

fn created(snapshot: Snapshot) -> ObjectId {
    match snapshot {
        Snapshot::Created { id, .. } => id,
        other => panic!("esperava Created, veio {other:?}"),
    }
}

/// Manifest com cabeçalho de envelope válido e corpo que não é cifra nenhuma.
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

/// Escreve o Vault de exemplo: arquivo vazio, um de 5 MB e um `config`.
fn sample_tree(vault: &Path, config: &Path) {
    write(vault, "x.md", b"1");
    write(vault, "notas/y.md", b"2");
    write(vault, "vazio.md", b"");
    write(vault, "grande.md", &vec![0x5A; 5 * 1024 * 1024]);
    write(config, "config.toml", b"modelo = \"x\"");
}

/// Cenário 1: A empurra para o S3, o segundo `push` não copia nada, e B (disco vazio, só a
/// chave) puxa e restaura byte a byte.
#[tokio::test]
async fn real_s3_push_pull_and_restore_round_trip() {
    let Some(cfg) = e2e_config(1) else {
        return;
    };
    let s3 = S3Store::new(&cfg).unwrap();
    let tmp = TempDir::new();

    let (roots_a, vault_a, config_a) = roots(&tmp, "a");
    sample_tree(&vault_a, &config_a);
    let a = Engine::new(disk(&tmp, "a-store"), keys(), "a").unwrap();
    let id = created(a.snapshot(&roots_a, 1).await.unwrap());

    let first = a.push(&s3).await.unwrap();
    assert_eq!(first.copied, 5 + 1); // 5 blobs (x, y, vazio, grande, config) e 1 manifest
    assert!(!first.manifests_withheld && first.forged.is_empty() && first.malformed.is_empty());
    assert_eq!(s3.has(&id).await, Ok(true));

    let second = a.push(&s3).await.unwrap();
    assert_eq!((second.copied, second.already), (0, 6));

    let b_store = disk(&tmp, "b-store");
    let b = Engine::new(b_store.clone(), keys(), "b").unwrap();
    let pulled = b.pull(&s3).await.unwrap();
    assert_eq!(pulled.copied, 6);
    assert!(!pulled.manifests_withheld && pulled.forged.is_empty());
    let (roots_b, vault_b, config_b) = roots(&tmp, "b");
    let tip = b.heads().await.unwrap().tips[0].id;
    assert_eq!(tip, id);
    assert_eq!(b.restore(&tip, &roots_b).await.unwrap().files, 5);
    assert_eq!(tree(&vault_b), tree(&vault_a));
    assert_eq!(tree(&config_b), tree(&config_a));

    // Fase 3, fatia 3: `verify` e `verify_restore` direto no bucket. Por último, porque o
    // blob de 5 MB é sobrescrito (este bucket não é reusado por outro cenário).
    let checker = Engine::new(s3.clone(), keys(), "v").unwrap();
    let presence = checker.verify(VerifyMode::Presence).await.unwrap();
    assert!(presence.clean, "{presence:?}");
    assert_eq!(presence.referenced_blobs, 5);
    let sample = VerifyMode::Sample {
        max_blobs: 100,
        offset: 0,
    };
    let healthy = checker.verify(sample).await.unwrap();
    assert!(healthy.clean && healthy.sampled == 5, "{healthy:?}");
    assert_eq!(checker.verify_restore(&id).await.unwrap().files, 5);

    let mut big = None;
    for blob in s3.list(ObjectKind::Blob).await.unwrap() {
        if s3.get(&blob).await.unwrap().unwrap().len() > 5 * 1024 * 1024 {
            big = Some(blob);
        }
    }
    let big = big.expect("o blob de 5 MB não está no bucket");
    raw_put(&cfg, &big.to_string(), b"adulterado").await;
    let broken = checker.verify(sample).await.unwrap();
    assert_eq!(broken.corrupt, vec![big]);
    assert!(!broken.clean);
    assert!(matches!(
        checker.verify_restore(&id).await,
        Err(EngineError::Store(StoreError::Corrupt))
    ));
}

/// Cenário 2: um `Engine<S3Store>` sem disco local acha a ponta e restaura direto do bucket, e
/// a proteção contra rollback (P-31) convive com o `list` do S3 tratado como atrasado.
#[tokio::test]
async fn real_s3_engine_runs_directly_on_the_bucket() {
    let Some(cfg) = e2e_config(2) else {
        return;
    };
    let s3 = S3Store::new(&cfg).unwrap();
    let tmp = TempDir::new();

    let (roots_a, vault_a, config_a) = roots(&tmp, "a");
    sample_tree(&vault_a, &config_a);
    let a = Engine::new(disk(&tmp, "a-store"), keys(), "a").unwrap();
    let s1 = created(a.snapshot(&roots_a, 1).await.unwrap());
    a.push(&s3).await.unwrap();

    // B não tem armazenamento local: o próprio bucket é o armazenamento
    let b = Engine::new(s3.clone(), keys(), "b")
        .unwrap()
        .with_state(tmp.path().join("b-estado.json"));
    let heads = b.heads().await.unwrap();
    assert!(heads.rejected.is_empty());
    assert_eq!(heads.tips.iter().map(|t| t.id).collect::<Vec<_>>(), [s1]);
    let (roots_b, vault_b, config_b) = roots(&tmp, "b");
    assert_eq!(b.restore(&s1, &roots_b).await.unwrap().files, 5);
    assert_eq!(tree(&vault_b), tree(&vault_a));
    assert_eq!(tree(&config_b), tree(&config_a));

    // B aceita a ponta; A publica a versão seguinte; B avança e publica a sua direto no bucket
    b.apply(None, &s1, &roots_b).await.unwrap();
    write(&vault_a, "x.md", b"1 editado");
    let s2 = created(a.snapshot(&roots_a, 2).await.unwrap());
    a.push(&s3).await.unwrap();
    assert!(matches!(
        b.merge(3).await.unwrap(),
        MergeOutcome::Nothing { head: Some(head) } if head == s2
    ));
    b.apply(Some(&s1), &s2, &roots_b).await.unwrap();
    write(&vault_b, "so-b.md", b"b");
    let s3_id = created(b.snapshot(&roots_b, 4).await.unwrap());
    // com `consistent_list: false` o aceito pode não aparecer na lista: nada de Rollback falso
    write(&vault_b, "so-b.md", b"b2");
    let s4 = created(b.snapshot(&roots_b, 5).await.unwrap());
    assert_ne!(s3_id, s4);
    assert_eq!(s3.has(&s4).await, Ok(true));
}

/// Cenário 3: lixo no bucket não impede o `pull` de trazer o que é legítimo, e um blob
/// corrompido retém os manifests (a rodada não vale como sucesso).
#[tokio::test]
async fn real_s3_pull_survives_forged_and_corrupt_objects() {
    let Some(cfg) = e2e_config(3) else {
        return;
    };
    let s3 = S3Store::new(&cfg).unwrap();
    let tmp = TempDir::new();

    let (roots_a, vault_a, config_a) = roots(&tmp, "a");
    sample_tree(&vault_a, &config_a);
    let a = Engine::new(disk(&tmp, "a-store"), keys(), "a").unwrap();
    let id = created(a.snapshot(&roots_a, 1).await.unwrap());
    a.push(&s3).await.unwrap();

    // manifest forjado: cabeçalho válido, corpo sem tag
    let forged = forged_manifest();
    s3.put(&forged.0, &forged.1).await.unwrap();
    let b = Engine::new(disk(&tmp, "b-store"), keys(), "b").unwrap();
    let report = b.pull(&s3).await.unwrap();
    assert_eq!(report.forged, vec![forged.0]);
    assert!(!report.manifests_withheld);
    let heads = b.heads().await.unwrap();
    assert!(heads.rejected.is_empty(), "o forjado travou o dispositivo");
    assert_eq!(heads.tips.iter().map(|t| t.id).collect::<Vec<_>>(), [id]);
    let (roots_b, vault_b, _) = roots(&tmp, "b");
    b.restore(&id, &roots_b).await.unwrap();
    assert_eq!(tree(&vault_b), tree(&vault_a));

    // blob cujo conteúdo não bate com o id: o `S3Store.put` recusa, então planta-se pelo cliente cru
    let bad = ObjectId::of(ObjectKind::Blob, b"o-que-deveria-estar-aqui");
    raw_put(&cfg, &bad.to_string(), b"adulterado").await;
    let c_store = disk(&tmp, "c-store");
    let c = Engine::new(c_store.clone(), keys(), "c").unwrap();
    let report = c.pull(&s3).await.unwrap();
    assert_eq!(report.corrupt, vec![bad]);
    assert!(report.manifests_withheld);
    assert_eq!(c_store.has(&id).await, Ok(false));
}
