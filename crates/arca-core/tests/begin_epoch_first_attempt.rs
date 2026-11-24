//! A **primeira** tentativa de `Engine::begin_epoch` (arca#198, P-49 rodada 3): sem manifest da
//! época nova, o manifest novo não tem base (`base_version = null`), mas a ponta da época velha
//! tem de servir de base do piso `ClassEmptied` e das entradas das classes que este dispositivo
//! não tem pasta (`carried`), que vão para a época nova **recifradas** (a época nova não cita
//! blob da anterior, ADR 0018 item 5). Antes da correção os dois olhavam uma lista vazia: as
//! entradas de Config e Secret de outros dispositivos sumiam do manifest da época 2 e uma pasta
//! vazia era publicada como "esvaziei" (o caso do Warden #244, que o Vault vazio já mostrava).
//!
//! Cenário: o dispositivo `b` tem as três classes e publica `vault/x.md`, `config/settings.json`
//! e `secret/github.json` na época 1. O dispositivo `a` (Warden que não aceitou logins nem
//! config) só tem pasta do Vault e a de Secret vazia, e rotaciona a chave. Os dois dispositivos
//! usam o mesmo armazenamento (como o `Rig` dos testes de `src/engine.rs`).
//!
//! Cada teste afirma antes a **premissa** (o que a ponta da época 1 lista, de qual dispositivo,
//! e que a chamada foi mesmo a primeira tentativa) e só depois o **resultado**. As asserções
//! descrevem o comportamento correto: entradas de classe sem pasta não podem sumir, e uma pasta
//! de Secret vazia não pode ser publicada como "esvaziei".

use arca_core::backend::Backend;
use arca_core::disk::DiskStore;
use arca_core::engine::{Engine, EngineError, Roots, Snapshot};
use arca_core::heads::Head;
use arca_core::manifest::{Class, FileEntry, Hash32, Manifest};
use arca_core::store::{BlobStore, StoreError};
use arca_core::{open, seal, EpochKeys, ObjectId, ObjectKind, VaultKey, FORMAT_VERSION};
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

/// Pasta única em `temp_dir()`, removida no `Drop` (o `testutil` do crate não é visível aqui).
struct Tmp(PathBuf);

impl Tmp {
    fn new() -> Tmp {
        let mut random = [0u8; 8];
        getrandom::fill(&mut random).unwrap();
        let hex: String = random.iter().map(|b| format!("{b:02x}")).collect();
        let path = std::env::temp_dir().join(format!("arca-it-{hex}"));
        fs::create_dir_all(&path).unwrap();
        Tmp(path)
    }

    fn dir(&self, name: &str) -> PathBuf {
        let path = self.0.join(name);
        fs::create_dir_all(&path).unwrap();
        path
    }
}

impl Drop for Tmp {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn old_key() -> VaultKey {
    VaultKey::from_bytes([3u8; 32])
}

fn new_key() -> VaultKey {
    VaultKey::from_bytes([4u8; 32])
}

fn old_keys() -> EpochKeys {
    EpochKeys::derive(&old_key(), 1)
}

fn new_keys() -> EpochKeys {
    EpochKeys::derive(&new_key(), 2)
}

/// Tudo o que o armazenamento de disco guarda, por `<tipo>/<hex>`, com os bytes.
fn store_bytes(dir: &Path) -> BTreeMap<String, Vec<u8>> {
    let mut all = BTreeMap::new();
    for kind in ["blob", "manifest"] {
        for entry in fs::read_dir(dir.join(kind)).unwrap() {
            let entry = entry.unwrap();
            let name = format!("{kind}/{}", entry.file_name().to_string_lossy());
            all.insert(name, fs::read(entry.path()).unwrap());
        }
    }
    all
}

struct World {
    _tmp: Tmp,
    /// O armazenamento compartilhado (o mesmo de `a` e `b`) e a pasta dele no disco.
    store: Backend,
    store_dir: PathBuf,
    /// O dispositivo que rotaciona, na época 1, com estado (como o Warden).
    a: Engine<Backend>,
    /// O dispositivo que tem as três classes e publicou a ponta da época 1.
    b: Engine<Backend>,
    vault_a: PathBuf,
    secret_a_empty: PathBuf,
    vault_b: PathBuf,
    config_b: PathBuf,
    secret_b: PathBuf,
}

impl World {
    /// `a` só com o Vault (sem pasta de Config nem de Secret).
    fn roots_a(&self) -> Roots {
        Roots {
            vault: Some(self.vault_a.clone()),
            config: None,
            secret: None,
        }
    }

    /// `a` com a pasta de Secret ligada, mas vazia.
    fn roots_a_with_empty_secret(&self) -> Roots {
        Roots {
            vault: Some(self.vault_a.clone()),
            config: None,
            secret: Some(self.secret_a_empty.clone()),
        }
    }

    fn roots_b(&self) -> Roots {
        Roots {
            vault: Some(self.vault_b.clone()),
            config: Some(self.config_b.clone()),
            secret: Some(self.secret_b.clone()),
        }
    }
}

/// `b` publica as três classes na época 1; `a` adota essa ponta no Vault (`apply` só com a raiz
/// do Vault), fica com um arquivo novo `y.md` ainda sem snapshot e guarda o estado.
async fn world() -> World {
    let tmp = Tmp::new();
    let store_dir = tmp.0.join("store");
    let store = Backend::Disk(DiskStore::open(&store_dir).unwrap());
    let b = Engine::new(store.clone(), old_keys(), "b").unwrap();
    let a = Engine::new(store.clone(), old_keys(), "a")
        .unwrap()
        .with_state(tmp.0.join("a-state"));

    let (vault_b, config_b, secret_b) =
        (tmp.dir("b-vault"), tmp.dir("b-config"), tmp.dir("b-secret"));
    fs::write(vault_b.join("x.md"), b"vault de b").unwrap();
    fs::write(config_b.join("settings.json"), b"{\"tema\":\"escuro\"}").unwrap();
    fs::write(secret_b.join("github.json"), b"{\"token\":\"t\"}").unwrap();
    let (vault_a, secret_a_empty) = (tmp.dir("a-vault"), tmp.dir("a-secret"));

    let world = World {
        _tmp: tmp,
        store,
        store_dir,
        a,
        b,
        vault_a,
        secret_a_empty,
        vault_b,
        config_b,
        secret_b,
    };
    let published = match world.b.snapshot(&world.roots_b(), 1).await.unwrap() {
        Snapshot::Created { id, .. } => id,
        other => panic!("esperava Created, veio {other:?}"),
    };
    world
        .a
        .apply(None, &published, &world.roots_a())
        .await
        .unwrap();
    fs::write(world.vault_a.join("y.md"), b"vault de a").unwrap();
    world
}

/// A única ponta que `engine` enxerga (sem manifest ilegível).
async fn tip(engine: &Engine<Backend>) -> Head {
    let heads = engine.heads().await.unwrap();
    assert!(heads.rejected.is_empty(), "{:?}", heads.rejected);
    assert_eq!(heads.tips.len(), 1, "{:?}", heads.tips);
    heads.tips.into_iter().next().unwrap()
}

fn names(manifest: &Manifest) -> Vec<(Class, &str)> {
    manifest
        .files
        .iter()
        .map(|f| (f.class, f.path.as_str()))
        .collect()
}

fn listing(head: &Head) -> Vec<(Class, &str)> {
    names(&head.manifest)
}

/// O que `b` publicou na época 1.
fn what_b_published() -> Vec<(Class, &'static str)> {
    vec![
        (Class::Vault, "x.md"),
        (Class::Config, "settings.json"),
        (Class::Secret, "github.json"),
    ]
}

/// O que o manifest novo de `a` deve listar: o Vault de `a` (`x.md` adotado e `y.md` novo) e as
/// entradas de Config e Secret de `b`, que `a` não tem pasta para varrer e portanto mantém.
fn what_a_must_list() -> Vec<(Class, &'static str)> {
    vec![
        (Class::Vault, "x.md"),
        (Class::Vault, "y.md"),
        (Class::Config, "settings.json"),
        (Class::Secret, "github.json"),
    ]
}

/// Caso (a): a primeira tentativa, sem manifest da época 2, com Config e Secret sem pasta.
#[tokio::test]
async fn first_attempt_keeps_the_entries_of_the_classes_without_a_folder() {
    let w = world().await;

    // premissa: a ponta da época 1 é de `b` e lista as três classes
    let before = tip(&w.a).await;
    assert_eq!(before.manifest.key_epoch, 1);
    assert_eq!(before.manifest.device, "b");
    assert_eq!(listing(&before), what_b_published());
    // premissa: `a` não tem pasta de Config nem de Secret
    let roots = w.roots_a();
    assert!(roots.config.is_none() && roots.secret.is_none());

    let (next, snapshot) = w.a.begin_epoch(&new_key(), &roots, 5).await.unwrap();
    let Snapshot::Created { id, manifest, .. } = snapshot else {
        panic!("esperava Created, veio {snapshot:?}");
    };
    // premissa: foi a primeira tentativa (nada da época 2 foi adotado)
    assert_eq!(manifest.key_epoch, 2);
    assert_eq!(manifest.base_version, None);
    assert_eq!(manifest.counter, 1);

    // resultado: o manifest publicado, lido de volta do armazenamento
    let published = tip(&next).await;
    assert_eq!(published.id, id);
    assert_eq!(
        listing(&published),
        what_a_must_list(),
        "o manifest da época 2 perdeu entradas de classe sem pasta"
    );
}

/// Caso (b1), controle: o `snapshot` comum da época 1, com as mesmas pastas e a mesma ponta,
/// mantém as entradas de Config e Secret (`prev` presente, `carried` preenchido).
#[tokio::test]
async fn control_a_plain_snapshot_keeps_the_entries_of_the_classes_without_a_folder() {
    let w = world().await;
    assert_eq!(listing(&tip(&w.a).await), what_b_published());

    let snapshot = w.a.snapshot(&w.roots_a(), 5).await.unwrap();
    let Snapshot::Created { manifest, .. } = snapshot else {
        panic!("esperava Created, veio {snapshot:?}");
    };
    // premissa: construiu sobre a ponta de `b` (`prev` presente)
    assert_eq!(manifest.key_epoch, 1);
    assert_eq!(manifest.counter, 2);
    assert!(manifest.base_version.is_some());

    let published = tip(&w.a).await;
    assert_eq!(listing(&published), what_a_must_list());
}

/// Caso (b2), controle: quando a época 2 já existe (outro dispositivo rotacionou com a mesma
/// chave), o `begin_epoch` de `a` constrói sobre ela (`prev` presente) e mantém as entradas.
#[tokio::test]
async fn control_a_retry_on_an_existing_new_epoch_keeps_the_entries_without_a_folder() {
    let w = world().await;
    let (_, by_b) = w.b.begin_epoch(&new_key(), &w.roots_b(), 4).await.unwrap();
    let Snapshot::Created { id: b_epoch2, .. } = by_b else {
        panic!("esperava Created, veio {by_b:?}");
    };

    let (next, snapshot) = w.a.begin_epoch(&new_key(), &w.roots_a(), 5).await.unwrap();
    let Snapshot::Created { manifest, .. } = snapshot else {
        panic!("esperava Created, veio {snapshot:?}");
    };
    // premissa: não foi a primeira tentativa (construiu sobre o manifest da época 2 de `b`)
    assert_eq!(manifest.key_epoch, 2);
    assert_eq!(manifest.counter, 2);
    assert_eq!(manifest.base_version.map(|h| h.0), Some(b_epoch2.hash));

    let published = tip(&next).await;
    assert_eq!(listing(&published), what_a_must_list());
}

/// Caso (c): a pasta de Secret está ligada e vazia, e a ponta tem uma entrada de Secret. O
/// piso `ClassEmptied` tem de disparar também na primeira tentativa, sem publicar nada.
#[tokio::test]
async fn first_attempt_refuses_to_publish_an_emptied_secret_folder() {
    let w = world().await;
    assert_eq!(listing(&tip(&w.a).await), what_b_published());
    let roots = w.roots_a_with_empty_secret();
    assert!(fs::read_dir(roots.secret.as_ref().unwrap())
        .unwrap()
        .next()
        .is_none());

    match w.a.begin_epoch(&new_key(), &roots, 5).await {
        Err(error) => assert_eq!(
            error,
            EngineError::ClassEmptied {
                class: Class::Secret,
                had: 1
            }
        ),
        Ok((next, snapshot)) => panic!(
            "publicou a época 2 com a pasta de Secret vazia: {:?} (manifest lido de volta: {:?})",
            snapshot,
            listing(&tip(&next).await)
        ),
    }
    // nada foi gravado: `a` ainda enxerga só a ponta da época 1 (uma época 2 daria `NeedsNewKey`)
    assert_eq!(listing(&tip(&w.a).await), what_b_published());
}

/// Caso (c), controle: o `snapshot` comum da época 1 recusa a mesma pasta de Secret vazia.
#[tokio::test]
async fn control_a_plain_snapshot_refuses_an_emptied_secret_folder() {
    let w = world().await;
    assert_eq!(listing(&tip(&w.a).await), what_b_published());

    assert_eq!(
        w.a.snapshot(&w.roots_a_with_empty_secret(), 5).await,
        Err(EngineError::ClassEmptied {
            class: Class::Secret,
            had: 1
        })
    );
}

/// O texto claro que cada entrada do cenário tem de ter.
fn plain_of(file: &FileEntry) -> &'static [u8] {
    match file.path.as_str() {
        "x.md" => b"vault de b",
        "y.md" => b"vault de a",
        "settings.json" => b"{\"tema\":\"escuro\"}",
        "github.json" => b"{\"token\":\"t\"}",
        other => panic!("entrada inesperada: {other}"),
    }
}

/// Caso (d): o manifest da época 2 abre com a chave nova (e não com a velha), e **cada** blob que
/// ele lista, inclusive os das classes sem pasta, abre só com as chaves novas, com o texto e o
/// sha256 originais; nenhum blob da época 1 é citado.
#[tokio::test]
async fn every_blob_of_the_new_epoch_opens_only_with_the_new_keys_and_none_is_from_the_old_one() {
    let w = world().await;
    let old_tip = tip(&w.a).await;
    assert_eq!(listing(&old_tip), what_b_published());
    let old_blobs: Vec<[u8; 32]> = old_tip.manifest.files.iter().map(|f| f.blob.0).collect();
    assert_eq!(old_blobs.len(), 3);

    let (next, _) = w.a.begin_epoch(&new_key(), &w.roots_a(), 5).await.unwrap();
    let published = tip(&next).await;
    assert_eq!(listing(&published), what_a_must_list());

    let bytes = w.store.get(&published.id).await.unwrap().unwrap();
    assert!(Manifest::open(&old_keys(), &bytes).is_err());
    assert_eq!(
        Manifest::open(&new_keys(), &bytes),
        Ok(published.manifest.clone())
    );
    for file in &published.manifest.files {
        let who = format!("{:?} {}", file.class, file.path);
        assert!(
            !old_blobs.contains(&file.blob.0),
            "{who} cita blob da época 1"
        );
        let id = ObjectId {
            kind: ObjectKind::Blob,
            hash: file.blob.0,
        };
        let blob = w.store.get(&id).await.unwrap().expect(&who);
        assert!(open(&old_keys(), ObjectKind::Blob, &blob).is_err(), "{who}");
        let plain = open(&new_keys(), ObjectKind::Blob, &blob).expect(&who);
        assert_eq!(plain.as_slice(), plain_of(file), "{who}");
        assert_eq!(file.size, plain.len() as u64, "{who}");
        assert_eq!(
            file.sha256,
            Hash32(Sha256::digest(plain.as_slice()).into()),
            "{who}"
        );
    }
}

/// Caso (e1): um blob carried ausente da loja. O **último** (Secret) é o que falta, para que o de
/// Config, que vem antes, fosse gravado se a conferência não fosse feita antes de gravar.
#[tokio::test]
async fn a_missing_carried_blob_refuses_the_rotation_and_writes_nothing() {
    let w = world().await;
    let secret = secret_entry(&w).await;
    w.store.delete(&blob_id(&secret)).await.unwrap();
    let before = store_bytes(&w.store_dir);

    let refused = w.a.begin_epoch(&new_key(), &w.roots_a(), 5).await;
    assert_eq!(refused.map(|_| ()), Err(EngineError::MissingObject));
    assert_eq!(store_bytes(&w.store_dir), before);
}

/// Caso (e2): um blob carried corrompido na loja (o hash dos bytes não bate com o id).
#[tokio::test]
async fn a_corrupted_carried_blob_refuses_the_rotation_and_writes_nothing() {
    let w = world().await;
    let secret = secret_entry(&w).await;
    fs::write(w.store_dir.join(blob_id(&secret).to_string()), b"lixo").unwrap();
    let before = store_bytes(&w.store_dir);

    let refused = w.a.begin_epoch(&new_key(), &w.roots_a(), 5).await;
    assert_eq!(
        refused.map(|_| ()),
        Err(EngineError::Store(StoreError::Corrupt))
    );
    assert_eq!(store_bytes(&w.store_dir), before);
}

async fn secret_entry(w: &World) -> FileEntry {
    let entry = tip(&w.a).await.manifest.files.into_iter().last().unwrap();
    assert_eq!(entry.class, Class::Secret);
    entry
}

fn blob_id(entry: &FileEntry) -> ObjectId {
    ObjectId {
        kind: ObjectKind::Blob,
        hash: entry.blob.0,
    }
}

/// Uma ponta da época 1 (de `b`) com uma só entrada de Secret cujo blob tem `plain`, `size` e o
/// sha256 de `claimed` declarados; `a` (só com a pasta do Vault) tenta rotacionar. Devolve o
/// resultado e se a loja ficou byte a byte igual.
async fn rotate_over_entry(
    plain: &[u8],
    size: u64,
    claimed: &[u8],
) -> (Result<(), EngineError>, bool) {
    let tmp = Tmp::new();
    let store_dir = tmp.0.join("store");
    let store = Backend::Disk(DiskStore::open(&store_dir).unwrap());
    let blob = seal(&old_keys(), ObjectKind::Blob, plain).unwrap();
    store.put(&blob.id, &blob.bytes).await.unwrap();
    let manifest = Manifest {
        format_version: FORMAT_VERSION as u16,
        key_epoch: 1,
        base_version: None,
        merged_from: vec![],
        counter: 1,
        device: "b".to_string(),
        created_at: 1,
        files: vec![FileEntry {
            path: "github.json".to_string(),
            class: Class::Secret,
            blob: Hash32(blob.id.hash),
            size,
            sha256: Hash32(Sha256::digest(claimed).into()),
        }],
    };
    let sealed = manifest.seal(&old_keys()).unwrap();
    store.put(&sealed.id, &sealed.bytes).await.unwrap();
    let vault = tmp.dir("vault");
    fs::write(vault.join("x.md"), b"x").unwrap();
    let a = Engine::new(store, old_keys(), "a").unwrap();
    // premissa: a ponta é a que montei, com a entrada de Secret que `a` não tem pasta para varrer
    let head = tip(&a).await;
    assert_eq!(listing(&head), [(Class::Secret, "github.json")]);
    let roots = Roots {
        vault: Some(vault),
        config: None,
        secret: None,
    };
    let before = store_bytes(&store_dir);

    let result = a.begin_epoch(&new_key(), &roots, 5).await.map(|_| ());
    (result, store_bytes(&store_dir) == before)
}

/// Caso (f), controle: a entrada coerente com o blob rotaciona (o setup não é a causa dos erros).
#[tokio::test]
async fn control_a_carried_entry_that_matches_its_blob_rotates() {
    let (result, _) = rotate_over_entry(b"aaaa", 4, b"aaaa").await;
    assert_eq!(result, Ok(()));
}

/// Caso (f1): o `sha256` da entrada não é o do texto do blob (o tamanho confere).
#[tokio::test]
async fn a_carried_entry_whose_sha256_does_not_match_its_blob_refuses_and_writes_nothing() {
    let (result, untouched) = rotate_over_entry(b"aaaa", 4, b"bbbb").await;
    assert_eq!(result, Err(EngineError::Integrity));
    assert!(untouched, "a loja mudou");
}

/// Caso (f2): o `size` da entrada não é o do texto do blob (o sha256 confere).
#[tokio::test]
async fn a_carried_entry_whose_size_does_not_match_its_blob_refuses_and_writes_nothing() {
    let (result, untouched) = rotate_over_entry(b"aaaa", 5, b"aaaa").await;
    assert_eq!(result, Err(EngineError::Integrity));
    assert!(untouched, "a loja mudou");
}

/// Caso (g): o Vault vazio na primeira tentativa (o achado do Warden #244) dá `ClassEmptied` no
/// núcleo, com a ponta da época 1 como base do piso.
#[tokio::test]
async fn first_attempt_refuses_an_emptied_vault() {
    let w = world().await;
    let old_tip = tip(&w.a).await;
    assert_eq!(listing(&old_tip), what_b_published());
    fs::remove_file(w.vault_a.join("x.md")).unwrap();
    fs::remove_file(w.vault_a.join("y.md")).unwrap();
    assert!(fs::read_dir(&w.vault_a).unwrap().next().is_none());

    let refused = w.a.begin_epoch(&new_key(), &w.roots_a(), 5).await;
    assert_eq!(
        refused.map(|_| ()),
        Err(EngineError::ClassEmptied {
            class: Class::Vault,
            had: 1
        })
    );
    // nenhum manifest da época 2: `a` ainda enxerga só a ponta da época 1
    assert_eq!(tip(&w.a).await.id, old_tip.id);
}

/// Caso (h): um cofre que nunca teve arquivo (nenhuma ponta) ainda rotaciona.
#[tokio::test]
async fn a_vault_that_never_had_a_file_still_rotates() {
    let tmp = Tmp::new();
    let store = Backend::Disk(DiskStore::open(tmp.0.join("store")).unwrap());
    let a = Engine::new(store, old_keys(), "a").unwrap();
    let vault = tmp.dir("vault");
    let roots = Roots {
        vault: Some(vault),
        config: None,
        secret: None,
    };
    // premissa: não há ponta nenhuma
    assert!(a.heads().await.unwrap().tips.is_empty());

    let (next, snapshot) = a.begin_epoch(&new_key(), &roots, 5).await.unwrap();
    let Snapshot::Created { manifest, .. } = snapshot else {
        panic!("esperava Created, veio {snapshot:?}");
    };
    assert_eq!(manifest.key_epoch, 2);
    assert_eq!(manifest.base_version, None);
    assert!(manifest.files.is_empty());
    assert_eq!(tip(&next).await.manifest, manifest);
}

/// Caso (i): a retomada com a mesma chave adota o manifest já publicado (`Unchanged`), não grava
/// nada (os blobs carried não são recifrados de novo) e mantém as entradas das classes sem pasta.
#[tokio::test]
async fn the_retry_with_the_same_key_adopts_the_published_manifest_and_writes_nothing() {
    let w = world().await;
    let (_, first) = w.a.begin_epoch(&new_key(), &w.roots_a(), 5).await.unwrap();
    let Snapshot::Created { id: first_id, .. } = first else {
        panic!("esperava Created, veio {first:?}");
    };
    let before = store_bytes(&w.store_dir);

    let (next, retried) = w.a.begin_epoch(&new_key(), &w.roots_a(), 6).await.unwrap();
    match retried {
        Snapshot::Unchanged { head, .. } => assert_eq!(head, first_id),
        other => panic!("esperava Unchanged, veio {other:?}"),
    }
    assert_eq!(store_bytes(&w.store_dir), before);
    assert_eq!(listing(&tip(&next).await), what_a_must_list());
}

/// Caso (j): `snapshot_allowing_empty` segue esvaziando uma classe de propósito, e depois disso a
/// primeira tentativa do `begin_epoch` rotaciona com a pasta vazia (a ponta já não tem Secret).
/// O `begin_epoch` não tem `allow_empty`: esvaziar e rotacionar na mesma chamada não existe.
#[tokio::test]
async fn allow_empty_still_empties_a_class_and_the_rotation_after_it_goes_through() {
    let w = world().await;
    let roots = w.roots_a_with_empty_secret();

    let emptied =
        w.a.snapshot_allowing_empty(&roots, 5, &[Class::Secret])
            .await;
    let Ok(Snapshot::Created { manifest, .. }) = emptied else {
        panic!("esperava Created, veio {emptied:?}");
    };
    assert_eq!(
        names(&manifest),
        [
            (Class::Vault, "x.md"),
            (Class::Vault, "y.md"),
            (Class::Config, "settings.json")
        ]
    );

    let (next, _) = w.a.begin_epoch(&new_key(), &roots, 6).await.unwrap();
    assert_eq!(
        listing(&tip(&next).await),
        [
            (Class::Vault, "x.md"),
            (Class::Vault, "y.md"),
            (Class::Config, "settings.json")
        ]
    );
}
