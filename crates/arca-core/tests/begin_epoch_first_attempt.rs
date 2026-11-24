//! Investigação (P-49, rodada 3): a **primeira** tentativa de `Engine::begin_epoch` leva para a
//! época nova as entradas de classes que este dispositivo não tem pasta?
//!
//! Por que se suspeita: na primeira tentativa não há manifest da época nova, e o `begin_epoch`
//! chama `build_snapshot(prev = None, ..)` (`engine.rs`, ramo `own.is_empty()`). O
//! `build_snapshot` tira de `prev_files` tanto as entradas de classe sem pasta (`carried`, que
//! deveriam ser mantidas como estão) quanto o `had` do piso `ClassEmptied`. Com `prev = None` os
//! dois ficam vazios. O Warden já viu o piso falhar assim com o Vault vazio (#244).
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
use arca_core::manifest::Class;
use arca_core::{EpochKeys, VaultKey};
use std::fs;
use std::path::PathBuf;

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

struct World {
    _tmp: Tmp,
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
    let store = Backend::Disk(DiskStore::open(tmp.0.join("store")).unwrap());
    let keys = EpochKeys::derive(&old_key(), 1);
    let b = Engine::new(store.clone(), keys, "b").unwrap();
    let a = Engine::new(store, EpochKeys::derive(&old_key(), 1), "a")
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

fn listing(head: &Head) -> Vec<(Class, &str)> {
    head.manifest
        .files
        .iter()
        .map(|f| (f.class, f.path.as_str()))
        .collect()
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
