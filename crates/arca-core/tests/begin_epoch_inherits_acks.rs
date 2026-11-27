//! O `Engine` novo do `Engine::begin_epoch` herda os **reconhecimentos** do velho (ADR 0018,
//! item 5, "Achado à parte"). Antes, sem `with_state`, os reconhecimentos viviam só na memória do
//! `Engine` e o `next` nascia vazio: **um** manifest forjado de época 3 (uma acima da que a
//! rotação cria), mesmo reconhecido no `Engine` velho, vetava a rotação com `NeedsNewKey { have:
//! 2, found: 3 }`, porque o `next.load()` o via como "época mais nova, não reconhecida". Com
//! `with_state` o arquivo `<estado>.foreign` é compartilhado e a rotação já passava.
//!
//! O que continua valendo, e é o que os testes de recusa fixam:
//! - o reconhecimento é por `(época, id)`: um **segundo** forjado de época 3, de id diferente e
//!   não reconhecido, continua vetando (a próxima época é previsível, e a rotação verdadeira, ou
//!   outro forjado, tem id novo);
//! - o que o `pull` apenas **anotou** como pendente (visto, não reconhecido) é herdado como
//!   pendente, nunca como reconhecido: a pendência continua vetando, já no `next.load()` (`have:
//!   2`), com ou sem `with_state`. Na **retomada** (já existe manifest da época nova) só o
//!   `next.load()` roda, e o veto do pendente só existe porque o `next` o herdou.
//!
//! Cada teste afirma antes a **premissa** (o que `foreign_epochs` mostra e o que o armazenamento
//! lista) e só depois o **resultado**.

use arca_core::backend::Backend;
use arca_core::disk::DiskStore;
use arca_core::engine::{Engine, EngineError, ForeignEpoch, Roots, Snapshot};
use arca_core::manifest::{Hash32, Manifest};
use arca_core::state::DeviceState;
use arca_core::store::BlobStore;
use arca_core::{EpochKeys, ObjectId, ObjectKind, VaultKey};
use std::fs;
use std::path::PathBuf;

/// Uma acima da época que a rotação cria (2): é a que o `next.load()` vê como "mais nova".
const FOREIGN_EPOCH: u32 = 3;

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
}

impl Drop for Tmp {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn old_keys() -> EpochKeys {
    EpochKeys::derive(&VaultKey::from_bytes([3u8; 32]), 1)
}

/// A chave que a rotação usa (`begin_epoch`).
fn next_vault_key() -> VaultKey {
    VaultKey::from_bytes([4u8; 32])
}

/// As chaves de quem forja: de outra `VaultKey`, na época pedida. `a` nunca as tem.
fn attacker_keys(epoch: u32) -> EpochKeys {
    EpochKeys::derive(&VaultKey::from_bytes([0xAA; 32]), epoch)
}

struct Lab {
    tmp: Tmp,
    /// O armazenamento de `a`: ele **é** o backend hostil (sem réplica local).
    store: Backend,
    roots: Roots,
    /// O manifest legítimo de `a`, da época 1.
    own: ObjectId,
}

impl Lab {
    fn state_path(&self) -> PathBuf {
        self.tmp.0.join("a-state")
    }

    async fn manifest_count(&self) -> usize {
        self.store.list(ObjectKind::Manifest).await.unwrap().len()
    }
}

/// `a` (época 1) publica o manifest legítimo, com um arquivo no Vault, com ou sem `with_state`.
async fn setup(with_state: bool) -> (Lab, Engine<Backend>) {
    let tmp = Tmp::new();
    let store = Backend::Disk(DiskStore::open(tmp.0.join("store")).unwrap());
    let vault = tmp.0.join("a-vault");
    fs::create_dir_all(vault.join("notas")).unwrap();
    fs::write(
        vault.join("notas/diario.md"),
        b"diario de a: 27 de novembro",
    )
    .unwrap();
    let roots = Roots {
        vault: Some(vault),
        config: None,
        secret: None,
    };
    let mut lab = Lab {
        tmp,
        store,
        roots,
        own: ObjectId::of(ObjectKind::Manifest, b""),
    };
    let mut engine = Engine::new(lab.store.clone(), old_keys(), "a").unwrap();
    if with_state {
        engine = engine.with_state(lab.state_path());
    }
    let Snapshot::Created { id, .. } = engine.snapshot(&lab.roots, 1).await.unwrap() else {
        panic!("esperava Created");
    };
    lab.own = id;
    // controle: antes de qualquer forjado, nada bloqueia
    assert_eq!(lab.manifest_count().await, 1);
    assert!(engine.foreign_epochs().await.unwrap().is_empty());
    (lab, engine)
}

/// Um manifest da `epoch`, de `device` "forjador", gravado em `store`. O `counter` distingue os
/// corpos (`n`), então cada `n` dá um id diferente.
async fn forge(store: &Backend, epoch: u32, n: u64) -> ObjectId {
    let manifest = Manifest {
        format_version: 1,
        key_epoch: epoch,
        base_version: None,
        merged_from: Vec::new(),
        counter: n,
        device: "forjador".to_string(),
        created_at: n,
        files: Vec::new(),
    };
    let sealed = manifest.seal(&attacker_keys(epoch)).unwrap();
    store.put(&sealed.id, &sealed.bytes).await.unwrap();
    sealed.id
}

/// Como `foreign_epochs` mostra `ids` de época 3: ordem `(época, hash)`.
fn foreign(ids: &[ObjectId]) -> Vec<ForeignEpoch> {
    let mut list: Vec<ForeignEpoch> = ids
        .iter()
        .map(|id| ForeignEpoch {
            epoch: FOREIGN_EPOCH,
            id: *id,
        })
        .collect();
    list.sort_by_key(|f| f.id.hash);
    list
}

/// O erro que o `next.load()` dá quando vê um forjado de época 3 não reconhecido.
fn blocked_by_next() -> EngineError {
    EngineError::NeedsNewKey {
        have: 2,
        found: FOREIGN_EPOCH,
    }
}

/// `begin_epoch` tem de publicar a época 2; devolve o `Engine` novo e o id do manifest novo.
async fn rotate_ok(lab: &Lab, engine: &Engine<Backend>) -> (Engine<Backend>, ObjectId) {
    let outcome = engine.begin_epoch(&next_vault_key(), &lab.roots, 30).await;
    assert_eq!(outcome.as_ref().err(), None, "a rotação devia passar");
    let (next, snapshot) = outcome.unwrap();
    let Snapshot::Created { id, manifest, .. } = snapshot else {
        panic!("esperava Created, veio {snapshot:?}");
    };
    assert_eq!(manifest.key_epoch, 2);
    (next, id)
}

/// O `Engine` novo enxerga só a ponta nova: o forjado de época 3 reconhecido não vira ponta nem
/// rejeitado, e nada bloqueia.
async fn assert_next_is_healthy(next: &Engine<Backend>, id: ObjectId) {
    let heads = next.heads().await.unwrap();
    assert_eq!(heads.tips.len(), 1);
    assert_eq!(heads.tips[0].id, id);
    assert!(heads.rejected.is_empty());
    assert!(next.foreign_epochs().await.unwrap().is_empty());
}

/// Sem estado: **um** forjado de época 3, reconhecido no `Engine` velho, não veta mais a rotação.
/// Antes: `NeedsNewKey { have: 2, found: 3 }`.
#[tokio::test]
async fn without_state_one_acknowledged_forged_manifest_of_epoch_3_lets_begin_epoch_through() {
    let (lab, engine) = setup(false).await;
    let forged = forge(&lab.store, FOREIGN_EPOCH, 1).await;
    // premissa: o `Engine` velho vê o forjado como estrangeiro não reconhecido e o reconhece
    assert_eq!(lab.manifest_count().await, 2);
    assert_eq!(engine.foreign_epochs().await.unwrap(), foreign(&[forged]));
    engine
        .acknowledge_foreign(&foreign(&[forged]))
        .await
        .unwrap();
    assert!(engine.foreign_epochs().await.unwrap().is_empty());
    assert_eq!(engine.heads().await.unwrap().tips[0].id, lab.own);

    let (next, id) = rotate_ok(&lab, &engine).await;
    assert_eq!(lab.manifest_count().await, 3);
    assert_next_is_healthy(&next, id).await;
}

/// Com estado: o arquivo `<estado>.foreign` já era compartilhado, e a rotação segue passando; o
/// estado vai para a época nova.
#[tokio::test]
async fn with_state_one_acknowledged_forged_manifest_of_epoch_3_still_lets_begin_epoch_through() {
    let (lab, engine) = setup(true).await;
    let forged = forge(&lab.store, FOREIGN_EPOCH, 1).await;
    assert_eq!(engine.foreign_epochs().await.unwrap(), foreign(&[forged]));
    engine
        .acknowledge_foreign(&foreign(&[forged]))
        .await
        .unwrap();
    assert!(engine.foreign_epochs().await.unwrap().is_empty());

    let (next, id) = rotate_ok(&lab, &engine).await;
    assert_next_is_healthy(&next, id).await;
    assert_eq!(
        DeviceState::load(&lab.state_path()).unwrap(),
        Some(DeviceState {
            key_epoch: 2,
            last_accepted: Hash32(id.hash),
        })
    );
}

/// O reconhecimento é por `(época, id)`: com **dois** forjados de época 3 e só um reconhecido, o
/// outro continua vetando (`have: 2`, o veto do `next.load()`); reconhecer o segundo também
/// destrava. Se herdar valesse por época (ou o `next` ignorasse o id), a rotação passaria com o
/// segundo ainda sem reconhecer.
#[tokio::test]
async fn a_second_unacknowledged_forged_manifest_of_epoch_3_still_vetoes_begin_epoch() {
    let (lab, engine) = setup(false).await;
    let first = forge(&lab.store, FOREIGN_EPOCH, 1).await;
    let second = forge(&lab.store, FOREIGN_EPOCH, 2).await;
    assert_ne!(first, second);
    // premissa: dois forjados listados, os dois estrangeiros e não reconhecidos
    assert_eq!(lab.manifest_count().await, 3);
    assert_eq!(
        engine.foreign_epochs().await.unwrap(),
        foreign(&[first, second])
    );
    // reconhece só o `first`; `second` é o que sobra
    engine
        .acknowledge_foreign(&foreign(&[first]))
        .await
        .unwrap();
    assert_eq!(engine.foreign_epochs().await.unwrap(), foreign(&[second]));

    let outcome = engine
        .begin_epoch(&next_vault_key(), &lab.roots, 30)
        .await
        .map(|_| ());
    assert_eq!(outcome, Err(blocked_by_next()));
    // nada foi publicado
    assert_eq!(lab.manifest_count().await, 3);

    // reconhecido o segundo, os dois estão herdados e a rotação passa
    engine
        .acknowledge_foreign(&foreign(&[second]))
        .await
        .unwrap();
    let (next, id) = rotate_ok(&lab, &engine).await;
    assert_eq!(lab.manifest_count().await, 4);
    assert_next_is_healthy(&next, id).await;
}

/// Depois da rotação, um forjado **novo** de época 3 (id que ninguém reconheceu) veta o `Engine`
/// novo: herdar não vira "época 3 está liberada".
#[tokio::test]
async fn a_new_forged_id_of_epoch_3_after_the_rotation_still_vetoes_the_new_engine() {
    let (lab, engine) = setup(false).await;
    let first = forge(&lab.store, FOREIGN_EPOCH, 1).await;
    engine
        .acknowledge_foreign(&foreign(&[first]))
        .await
        .unwrap();
    let (next, id) = rotate_ok(&lab, &engine).await;
    assert_next_is_healthy(&next, id).await;

    let fresh = forge(&lab.store, FOREIGN_EPOCH, 2).await;
    assert_ne!(first, fresh);
    // premissa: só o `fresh` aparece como não reconhecido para o `Engine` novo
    assert_eq!(next.foreign_epochs().await.unwrap(), foreign(&[fresh]));
    assert_eq!(
        next.heads().await.map(|_| ()),
        Err(blocked_by_next()),
        "o id novo de época 3 tem de vetar o Engine novo"
    );
}

/// O que o `pull` só **anotou** (pendente: visto, não reconhecido) é herdado **como pendente**,
/// nunca como reconhecido: a rotação continua vetada, e o veto sai do `next.load()` com as chaves
/// novas (`have: 2`), como acontece com `with_state`, em que a pendência está no arquivo
/// `<estado>.foreign` compartilhado. Se o `next` a descartasse, o veto viria do `self.load()` das
/// chaves velhas, com `have: 1`. Depois de reconhecer, a pendência vira reconhecimento e a
/// rotação passa.
#[tokio::test]
async fn a_pending_epoch_3_noted_by_pull_is_inherited_and_still_vetoes_the_new_engine() {
    let (lab, engine) = setup(false).await;
    let remote = Backend::Disk(DiskStore::open(lab.tmp.0.join("remote")).unwrap());
    let forged = forge(&remote, FOREIGN_EPOCH, 1).await;

    let report = engine.pull(&remote).await.unwrap();
    // premissa: o `pull` anotou o forjado como pendente e **não** o gravou no armazenamento de `a`
    assert_eq!(report.foreign_epochs, foreign(&[forged]));
    assert!(!lab.store.has(&forged).await.unwrap());
    assert_eq!(lab.manifest_count().await, 1);
    assert_eq!(engine.foreign_epochs().await.unwrap(), foreign(&[forged]));

    let outcome = engine
        .begin_epoch(&next_vault_key(), &lab.roots, 30)
        .await
        .map(|_| ());
    assert_eq!(outcome, Err(blocked_by_next()));
    assert_eq!(lab.manifest_count().await, 1);

    engine
        .acknowledge_foreign(&foreign(&[forged]))
        .await
        .unwrap();
    assert!(engine.foreign_epochs().await.unwrap().is_empty());
    let (next, id) = rotate_ok(&lab, &engine).await;
    assert_next_is_healthy(&next, id).await;
}

/// **Retomada** sem estado: uma tentativa anterior já publicou o manifest da época 2 com a chave
/// nova (`own` não vazio), então só o `next.load()` roda e o `self.load()` das chaves velhas não.
/// Um pendente de época 3, anotado pelo `pull` depois, só veta se o `next` o herdou: com o
/// descarte, nada vetaria e a rotação passaria. É o caso que distingue herdar de descartar sem
/// depender da ordem dos dois `load()`. Reconhecido o pendente, a retomada passa.
#[tokio::test]
async fn without_state_a_resumed_begin_epoch_is_vetoed_by_a_pending_epoch_3_noted_by_pull() {
    let (lab, engine) = setup(false).await;
    // a tentativa anterior: publicou a época 2 (o `Engine` novo dela é descartado)
    let (_previous_next, published) = rotate_ok(&lab, &engine).await;
    // premissa: o armazenamento tem o manifest da época 1 e o da época 2 (`own` da retomada)
    assert_eq!(lab.manifest_count().await, 2);
    assert!(lab.store.has(&published).await.unwrap());

    let remote = Backend::Disk(DiskStore::open(lab.tmp.0.join("remote")).unwrap());
    let forged = forge(&remote, FOREIGN_EPOCH, 1).await;
    let report = engine.pull(&remote).await.unwrap();
    // premissa: o `pull` anotou o forjado de época 3 como pendente, sem gravá-lo
    assert_eq!(report.foreign_epochs, foreign(&[forged]));
    assert!(!lab.store.has(&forged).await.unwrap());
    assert_eq!(lab.manifest_count().await, 2);
    let pending = ForeignEpoch {
        epoch: FOREIGN_EPOCH,
        id: forged,
    };
    assert!(engine.foreign_epochs().await.unwrap().contains(&pending));

    let outcome = engine
        .begin_epoch(&next_vault_key(), &lab.roots, 31)
        .await
        .map(|_| ());
    assert_eq!(outcome, Err(blocked_by_next()));
    assert_eq!(lab.manifest_count().await, 2);

    // reconhecido o pendente, a retomada passa (adota o manifest da época 2)
    engine.acknowledge_foreign(&[pending]).await.unwrap();
    let resumed = engine.begin_epoch(&next_vault_key(), &lab.roots, 32).await;
    assert_eq!(resumed.as_ref().err(), None, "a retomada devia passar");
    assert_eq!(lab.manifest_count().await, 2);
}
