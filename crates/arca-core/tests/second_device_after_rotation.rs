//! O **outro lado** da rotação (arca#198, P-49 rodada 5): o que um segundo dispositivo `c`
//! recebe do manifest da época 2 que o dispositivo `a` publicou na primeira tentativa de
//! `Engine::begin_epoch`. Os testes de `begin_epoch_first_attempt.rs` só olham o manifest que
//! sai de `a`; aqui o manifest é puxado (`pull`) e aplicado (`apply`) por quem não o criou.
//!
//! Cenário: o dispositivo `b` tem as três pastas e publica na época 1 arquivos de Vault, Config
//! e Secret (dois por classe, um deles em subpasta). O dispositivo `a` (Warden sem pasta de
//! Config nem de Secret, com estado) adota essa ponta no Vault, cria `diario.md` e rotaciona a
//! chave: o manifest da época 2 leva o Vault varrido e as entradas de Config e Secret
//! **recifradas**. `a` e `b` usam o mesmo armazenamento; `c` tem o dele e puxa o de `a`. No caso 1
//! esse armazenamento começa vazio; nos casos 2 e 3, `c` já puxou a época 1 e a aplicou só no Vault;
//! o caso 3b é um `c` de chave velha sem arquivo de estado.
//!
//! Cada teste afirma antes a **premissa** (a rotação foi a primeira tentativa e recifrou as
//! entradas; de onde `c` parte; o que o `pull` fez) e só depois o **resultado**. É uma
//! investigação: os textos claros usados aqui não coincidem com os de outros testes, e `c` parte
//! sem nenhum arquivo de Config ou de Secret, para que um verde não possa vir de uma pasta que já
//! tinha o conteúdo.

use arca_core::backend::Backend;
use arca_core::disk::DiskStore;
use arca_core::engine::{
    Applied, Engine, EngineError, ForeignEpoch, MergeOutcome, Roots, Snapshot,
};
use arca_core::heads::Head;
use arca_core::manifest::{Class, Hash32, Manifest};
use arca_core::state::DeviceState;
use arca_core::store::BlobStore;
use arca_core::{EpochKeys, Error, ObjectKind, VaultKey};
use sha2::{Digest, Sha256};
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

/// Tudo o que o cenário publica na época 2, `(classe, caminho, texto claro)`, na ordem do
/// manifest. `diario.md` é de `a`; o resto é de `b` (época 1).
fn expected() -> Vec<(Class, &'static str, &'static [u8])> {
    vec![
        (Class::Vault, "diario.md", b"diario de a, 2026".as_slice()),
        (
            Class::Vault,
            "notas/ideia.md",
            b"ideia de b: tres pastas, uma chave".as_slice(),
        ),
        (
            Class::Config,
            "editor/settings.json",
            b"{\"fonte\":\"mono\",\"tamanho\":14}".as_slice(),
        ),
        (Class::Config, "tema.toml", b"tema = \"solar\"\n".as_slice()),
        (
            Class::Secret,
            "contas/banco.json",
            b"\x00\xffagencia 0042\x80\nconta 7781-3".as_slice(),
        ),
        (
            Class::Secret,
            "github.json",
            b"{\"token\":\"ghp_Zk3_exemplo\"}".as_slice(),
        ),
    ]
}

fn write_file(root: &Path, relative: &str, bytes: &[u8]) {
    let path = root.join(relative);
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, bytes).unwrap();
}

fn is_empty_dir(path: &Path) -> bool {
    fs::read_dir(path).unwrap().next().is_none()
}

fn sha256_of(bytes: &[u8]) -> Hash32 {
    Hash32(Sha256::digest(bytes).into())
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

fn expected_names() -> Vec<(Class, &'static str)> {
    expected()
        .into_iter()
        .map(|(class, path, _)| (class, path))
        .collect()
}

/// Três pastas novas e vazias, `<prefix>-vault`, `<prefix>-config` e `<prefix>-secret`.
fn empty_roots(tmp: &Tmp, prefix: &str) -> Roots {
    Roots {
        vault: Some(tmp.dir(&format!("{prefix}-vault"))),
        config: Some(tmp.dir(&format!("{prefix}-config"))),
        secret: Some(tmp.dir(&format!("{prefix}-secret"))),
    }
}

/// O mesmo que `roots`, mas só com o Vault ligado.
fn vault_only(roots: &Roots) -> Roots {
    Roots {
        vault: roots.vault.clone(),
        config: None,
        secret: None,
    }
}

/// O cenário até **antes** da rotação: `b` publicou a época 1, `a` a adotou no Vault e criou
/// `diario.md` (ainda sem snapshot).
struct Rig {
    tmp: Tmp,
    /// O armazenamento de `a` e de `b`.
    store: Backend,
    a: Engine<Backend>,
    roots_a: Roots,
    /// A ponta da época 1, de `b`.
    old_tip: Head,
}

async fn rig() -> Rig {
    let tmp = Tmp::new();
    let store = Backend::Disk(DiskStore::open(tmp.0.join("store")).unwrap());
    let b = Engine::new(store.clone(), old_keys(), "b").unwrap();
    let a = Engine::new(store.clone(), old_keys(), "a")
        .unwrap()
        .with_state(tmp.0.join("a-state"));

    let roots_b = empty_roots(&tmp, "b");
    let roots_a = Roots {
        vault: Some(tmp.dir("a-vault")),
        config: None,
        secret: None,
    };
    for (class, path, bytes) in expected() {
        let root = match class {
            Class::Vault if path == "diario.md" => continue,
            Class::Vault => &roots_b.vault,
            Class::Config => &roots_b.config,
            Class::Secret => &roots_b.secret,
        };
        write_file(root.as_ref().unwrap(), path, bytes);
    }
    let published = match b.snapshot(&roots_b, 1).await.unwrap() {
        Snapshot::Created { id, .. } => id,
        other => panic!("esperava Created, veio {other:?}"),
    };
    a.apply(None, &published, &roots_a).await.unwrap();
    write_file(
        roots_a.vault.as_ref().unwrap(),
        "diario.md",
        b"diario de a, 2026",
    );
    let old_tip = tip(&a).await;
    assert_eq!(old_tip.id, published);
    Rig {
        tmp,
        store,
        a,
        roots_a,
        old_tip,
    }
}

/// `a` rotaciona para a época 2 e o manifest publicado é devolvido. Afirma a premissa de que foi a
/// **primeira tentativa** e de que Config e Secret foram recifrados (mesmo conteúdo, blob novo).
async fn rotate(rig: &Rig) -> Head {
    // premissa: o armazenamento só tem o manifest da época 1
    assert_eq!(
        rig.store.list(ObjectKind::Manifest).await.unwrap(),
        vec![rig.old_tip.id]
    );
    assert_eq!(rig.old_tip.manifest.key_epoch, 1);

    let (next, snapshot) = rig
        .a
        .begin_epoch(&new_key(), &rig.roots_a, 5)
        .await
        .unwrap();
    let Snapshot::Created { id, manifest, .. } = snapshot else {
        panic!("esperava Created, veio {snapshot:?}");
    };
    // premissa: primeira tentativa (sem base, contador 1) e época 2
    assert_eq!(manifest.key_epoch, 2);
    assert_eq!(manifest.base_version, None);
    assert_eq!(manifest.counter, 1);
    let published = tip(&next).await;
    assert_eq!(published.id, id);
    assert_eq!(names(&published.manifest), expected_names());
    // premissa: as entradas das classes sem pasta em `a` foram recifradas (blob novo, mesmo
    // conteúdo: `sha256` e `size` iguais aos da época 1)
    for class in [Class::Config, Class::Secret] {
        let news: Vec<_> = published
            .manifest
            .files
            .iter()
            .filter(|f| f.class == class)
            .collect();
        assert_eq!(news.len(), 2, "{class:?}");
        for new in news {
            let old = rig
                .old_tip
                .manifest
                .files
                .iter()
                .find(|f| f.class == class && f.path == new.path)
                .unwrap_or_else(|| panic!("{class:?} {} não estava na época 1", new.path));
            assert_ne!(
                new.blob, old.blob,
                "{class:?} {} não foi recifrado",
                new.path
            );
            assert_eq!(new.sha256, old.sha256, "{class:?} {}", new.path);
            assert_eq!(new.size, old.size, "{class:?} {}", new.path);
        }
    }
    published
}

/// `c` com a chave **velha** (época 1), as três pastas, estado próprio e armazenamento próprio:
/// puxou a época 1 de `a` e a aplicou **só no Vault** (Config e Secret ficam vazios). Devolve o
/// engine velho, o armazenamento, o arquivo de estado e as pastas.
async fn c_on_epoch_one(rig: &Rig) -> (Engine<Backend>, Backend, PathBuf, Roots) {
    let store_c = Backend::Disk(DiskStore::open(rig.tmp.0.join("c-store")).unwrap());
    let state_c = rig.tmp.0.join("c-state");
    let roots_c = empty_roots(&rig.tmp, "c");
    let old_c = Engine::new(store_c.clone(), old_keys(), "c")
        .unwrap()
        .with_state(&state_c);
    let report = old_c.pull(&rig.store).await.unwrap();
    assert!(report.foreign_epochs.is_empty(), "{report:?}");
    assert_eq!(tip(&old_c).await.id, rig.old_tip.id);
    old_c
        .apply(None, &rig.old_tip.id, &vault_only(&roots_c))
        .await
        .unwrap();
    // premissa: `c` aceitou a época 1 e só tem arquivo no Vault
    assert_eq!(
        DeviceState::load(&state_c).unwrap(),
        Some(DeviceState {
            key_epoch: 1,
            last_accepted: Hash32(rig.old_tip.id.hash),
        })
    );
    assert!(roots_c
        .vault
        .as_ref()
        .unwrap()
        .join("notas/ideia.md")
        .is_file());
    assert!(is_empty_dir(roots_c.config.as_ref().unwrap()));
    assert!(is_empty_dir(roots_c.secret.as_ref().unwrap()));
    (old_c, store_c, state_c, roots_c)
}

/// Cada arquivo esperado existe em `roots`, com os bytes originais, e a entrada do manifest
/// publicado confere com ele (`size` e `sha256`).
fn assert_files_are_the_originals(roots: &Roots, published: &Head) {
    for (class, path, bytes) in expected() {
        let root = match class {
            Class::Vault => &roots.vault,
            Class::Config => &roots.config,
            Class::Secret => &roots.secret,
        };
        let who = format!("{class:?} {path}");
        let on_disk =
            fs::read(root.as_ref().unwrap().join(path)).unwrap_or_else(|e| panic!("{who}: {e}"));
        assert_eq!(on_disk, bytes, "{who}");
        let entry = published
            .manifest
            .files
            .iter()
            .find(|f| f.class == class && f.path == path)
            .expect(&who);
        assert_eq!(entry.size, bytes.len() as u64, "{who}");
        assert_eq!(entry.sha256, sha256_of(bytes), "{who}");
    }
}

/// Caso 1: `c` entra **depois** da rotação, com a chave nova e sem estado nem arquivo algum. Puxa,
/// aplica e fica com tudo, inclusive Config e Secret recifrados, e o estado dele é coerente.
#[tokio::test]
async fn a_fresh_second_device_with_the_new_key_receives_the_resealed_entries() {
    let rig = rig().await;
    let published = rotate(&rig).await;

    // premissa: `c` parte do zero (chave da época 2, armazenamento vazio, pastas vazias, sem estado)
    let store_c = Backend::Disk(DiskStore::open(rig.tmp.0.join("c-store")).unwrap());
    assert!(store_c.list(ObjectKind::Manifest).await.unwrap().is_empty());
    assert!(store_c.list(ObjectKind::Blob).await.unwrap().is_empty());
    let roots_c = empty_roots(&rig.tmp, "c");
    for root in [&roots_c.vault, &roots_c.config, &roots_c.secret] {
        assert!(is_empty_dir(root.as_ref().unwrap()));
    }
    let state_c = rig.tmp.0.join("c-state");
    assert_eq!(DeviceState::load(&state_c).unwrap(), None);
    let c = Engine::new(store_c.clone(), new_keys(), "c")
        .unwrap()
        .with_state(&state_c);

    // o `pull`: o manifest da época 2 é aceito; o da época 1 é de outra época e não é gravado
    let report = c.pull(&rig.store).await.unwrap();
    assert!(
        report.forged.is_empty() && report.corrupt.is_empty(),
        "{report:?}"
    );
    assert!(
        report.missing.is_empty() && report.unreadable.is_empty(),
        "{report:?}"
    );
    assert!(
        report.malformed.is_empty() && !report.manifests_withheld,
        "{report:?}"
    );
    assert_eq!(
        report.foreign_epochs,
        vec![ForeignEpoch {
            epoch: 1,
            id: rig.old_tip.id
        }]
    );
    assert!(store_c.has(&published.id).await.unwrap());
    assert!(!store_c.has(&rig.old_tip.id).await.unwrap());
    // sem `NeedsNewKey` (a época 1 é menor, não bloqueia) e uma ponta só: o `merge` não tem o que fazer
    assert!(c.foreign_epochs().await.unwrap().is_empty());
    assert_eq!(tip(&c).await.id, published.id);
    assert_eq!(
        c.merge(8).await,
        Ok(MergeOutcome::Nothing {
            head: Some(published.id)
        })
    );

    // resultado: o `apply` cria os seis arquivos, com os bytes originais
    let applied = c.apply(None, &published.id, &roots_c).await;
    assert_eq!(
        applied,
        Ok(Applied {
            created: 6,
            updated: 0,
            removed: 0,
            unchanged: 0
        })
    );
    assert_files_are_the_originals(&roots_c, &published);

    // o estado de `c` é a ponta da época 2
    assert_eq!(
        DeviceState::load(&state_c).unwrap(),
        Some(DeviceState {
            key_epoch: 2,
            last_accepted: Hash32(published.id.hash),
        })
    );
    // e `c` aceita o que vem depois: reaplicar não muda nada, o `snapshot` da pasta é `Unchanged`
    assert_eq!(
        c.apply(Some(&published.id), &published.id, &roots_c).await,
        Ok(Applied {
            created: 0,
            updated: 0,
            removed: 0,
            unchanged: 6
        })
    );
    match c.snapshot(&roots_c, 9).await.unwrap() {
        Snapshot::Unchanged { head, .. } => assert_eq!(head, published.id),
        other => panic!("esperava Unchanged, veio {other:?}"),
    }
    // uma edição de `c` num Secret vira o manifest seguinte da época 2, sobre o publicado por `a`
    write_file(
        roots_c.secret.as_ref().unwrap(),
        "github.json",
        b"{\"token\":\"ghp_trocado_em_c\"}",
    );
    let Snapshot::Created { manifest, .. } = c.snapshot(&roots_c, 11).await.unwrap() else {
        panic!("esperava Created");
    };
    assert_eq!(manifest.key_epoch, 2);
    assert_eq!(manifest.counter, 2);
    assert_eq!(manifest.device, "c");
    assert_eq!(manifest.base_version, Some(Hash32(published.id.hash)));
    assert_eq!(names(&manifest), expected_names());
}

/// Caso 2: `c` já estava na época 1 (estado de época 1, Vault aplicado) e passa a usar a chave nova.
/// O estado de época 1 não é `Rollback` para o manifest da época 2, e Config e Secret chegam.
#[tokio::test]
async fn a_second_device_that_moves_from_epoch_one_to_the_new_key_receives_the_resealed_entries() {
    let rig = rig().await;
    // o engine de chave velha não é usado de novo: `c` passa a abrir o armazenamento com a nova
    let (_old_c, store_c, state_c, roots_c) = c_on_epoch_one(&rig).await;
    let published = rotate(&rig).await;

    let c = Engine::new(store_c.clone(), new_keys(), "c")
        .unwrap()
        .with_state(&state_c);
    // premissa: o manifest da época 1 já estava no armazenamento de `c`, e o da época 2 não
    assert!(store_c.has(&rig.old_tip.id).await.unwrap());
    assert!(!store_c.has(&published.id).await.unwrap());

    let report = c.pull(&rig.store).await.unwrap();
    assert!(report.foreign_epochs.is_empty(), "{report:?}");
    assert!(
        report.forged.is_empty() && !report.manifests_withheld,
        "{report:?}"
    );
    assert!(store_c.has(&published.id).await.unwrap());
    assert!(c.foreign_epochs().await.unwrap().is_empty());
    assert_eq!(tip(&c).await.id, published.id);

    // ACHADO: o chamador real passaria o `from` que a pasta reflete (a ponta da época 1), mas
    // hoje o `apply` abre o `from` com as chaves do engine (`read_manifest`, antes do `load()`)
    // e o manifest da época 1 não abre com a chave nova: `WrongKeyEpoch`, nada é escrito. Quem
    // troca de chave tem de passar `from = None` (e então nada é removido: sem `from`, o que a
    // época 1 tinha e a 2 não tem não vira `Remove`).
    assert_eq!(
        c.apply(Some(&rig.old_tip.id), &published.id, &roots_c)
            .await,
        Err(EngineError::Object(Error::WrongKeyEpoch {
            found: 1,
            expected: 2
        }))
    );
    assert!(is_empty_dir(roots_c.config.as_ref().unwrap()));
    assert!(is_empty_dir(roots_c.secret.as_ref().unwrap()));
    assert_eq!(
        DeviceState::load(&state_c).unwrap().map(|s| s.key_epoch),
        Some(1)
    );

    // `from = None`: o manifest da época 1 não abre com a chave nova. O arquivo do Vault que `c`
    // já tinha (`notas/ideia.md`) conta como já aplicado; os outros cinco são criados.
    let applied = c.apply(None, &published.id, &roots_c).await;
    assert_eq!(
        applied,
        Ok(Applied {
            created: 5,
            updated: 0,
            removed: 0,
            unchanged: 1
        })
    );
    assert_files_are_the_originals(&roots_c, &published);
    assert_eq!(
        DeviceState::load(&state_c).unwrap(),
        Some(DeviceState {
            key_epoch: 2,
            last_accepted: Hash32(published.id.hash),
        })
    );
    match c.snapshot(&roots_c, 9).await.unwrap() {
        Snapshot::Unchanged { head, .. } => assert_eq!(head, published.id),
        other => panic!("esperava Unchanged, veio {other:?}"),
    }
}

/// Caso 3: `c` ficou com a chave **velha** (época 1) e puxa depois da rotação. O manifest da época 2
/// não é gravado, vira pendência (`foreign_epochs`, ADR 0018 itens 2 e 3) e bloqueia `heads`,
/// `merge` e `snapshot` com `NeedsNewKey`; o `apply` também (com ou sem estado: ver o caso 3b).
/// O manifest da época 2 nunca é aplicado
/// com a chave velha (`MissingObject`: não está no armazenamento de `c`). A pendência sobrevive a
/// reiniciar o engine (arquivo `<estado>.foreign`) e `acknowledge_foreign` a tira.
#[tokio::test]
async fn a_second_device_on_the_old_key_is_blocked_by_the_unknown_epoch_and_applies_nothing() {
    let rig = rig().await;
    let (old_c, store_c, state_c, roots_c) = c_on_epoch_one(&rig).await;
    let published = rotate(&rig).await;
    // premissa: o armazenamento de `a` já tem o manifest da época 2 (e `c` ainda não)
    assert!(rig.store.has(&published.id).await.unwrap());
    assert!(!store_c.has(&published.id).await.unwrap());
    assert!(old_c.foreign_epochs().await.unwrap().is_empty());

    let report = old_c.pull(&rig.store).await.unwrap();
    let pending = ForeignEpoch {
        epoch: 2,
        id: published.id,
    };
    assert_eq!(report.foreign_epochs, vec![pending]);
    // o manifest de outra época nunca é gravado e a pendência aparece
    assert!(!store_c.has(&published.id).await.unwrap());
    assert_eq!(old_c.foreign_epochs().await.unwrap(), vec![pending]);

    let blocked = EngineError::NeedsNewKey { have: 1, found: 2 };
    assert_eq!(old_c.heads().await.map(|_| ()), Err(blocked.clone()));
    assert_eq!(old_c.merge(8).await.map(|_| ()), Err(blocked.clone()));
    assert_eq!(
        old_c.snapshot(&roots_c, 9).await.map(|_| ()),
        Err(blocked.clone())
    );
    assert_eq!(
        old_c
            .apply(Some(&rig.old_tip.id), &rig.old_tip.id, &roots_c)
            .await
            .map(|_| ()),
        Err(blocked.clone())
    );
    // o manifest da época 2 em si nunca é aplicado: `apply` lê o `to` antes de olhar a pendência
    // (`load()`), e `read_manifest` não o acha no armazenamento de `c`: `MissingObject`
    assert_eq!(
        old_c.apply(None, &published.id, &roots_c).await,
        Err(EngineError::MissingObject)
    );

    // nada foi aplicado: Vault como estava, Config e Secret vazios, estado na época 1
    let vault = roots_c.vault.as_ref().unwrap();
    assert!(vault.join("notas/ideia.md").is_file());
    assert!(!vault.join("diario.md").exists());
    assert!(is_empty_dir(roots_c.config.as_ref().unwrap()));
    assert!(is_empty_dir(roots_c.secret.as_ref().unwrap()));
    assert_eq!(
        DeviceState::load(&state_c).unwrap(),
        Some(DeviceState {
            key_epoch: 1,
            last_accepted: Hash32(rig.old_tip.id.hash),
        })
    );

    // reiniciar o app: engine novo, mesmo armazenamento e mesmo arquivo de estado. A pendência
    // está em disco (`<estado>.foreign`) e continua bloqueando
    let restarted = Engine::new(store_c.clone(), old_keys(), "c")
        .unwrap()
        .with_state(&state_c);
    assert_eq!(restarted.foreign_epochs().await.unwrap(), vec![pending]);
    assert_eq!(restarted.heads().await.map(|_| ()), Err(blocked));

    // reconhecer o par (época, id) tira o bloqueio: `c` volta a enxergar a época 1 como ponta
    restarted.acknowledge_foreign(&[pending]).await.unwrap();
    assert!(restarted.foreign_epochs().await.unwrap().is_empty());
    assert_eq!(tip(&restarted).await.id, rig.old_tip.id);
    assert_eq!(
        restarted.merge(10).await,
        Ok(MergeOutcome::Nothing {
            head: Some(rig.old_tip.id)
        })
    );
    // (só o Vault: Config e Secret estão ligados e vazios em `roots_c`, o que seria `ClassEmptied`)
    match restarted.snapshot(&vault_only(&roots_c), 11).await {
        Ok(Snapshot::Unchanged { head, .. }) => assert_eq!(head, rig.old_tip.id),
        other => panic!("esperava Unchanged, veio {other:?}"),
    }
    // um `pull` novo (engine sem o cache negativo) volta a ver o manifest da época 2 e o reporta,
    // mas o par reconhecido não vira pendência de novo
    let again = Engine::new(store_c.clone(), old_keys(), "c")
        .unwrap()
        .with_state(&state_c);
    let report = again.pull(&rig.store).await.unwrap();
    assert_eq!(report.foreign_epochs, vec![pending]);
    assert!(again.foreign_epochs().await.unwrap().is_empty());
    assert_eq!(tip(&again).await.id, rig.old_tip.id);
}

/// Caso 3b: `c` com a chave velha **sem arquivo de estado** (`Engine::new` sem `with_state`), depois
/// da rotação. `heads`, `merge`, `snapshot` **e `apply`** dão `NeedsNewKey` enquanto a pendência
/// (que vive só na memória do `Engine`) não é reconhecida, como manda o ADR 0018 item 3. Nada é
/// escrito. Depois de `acknowledge_foreign` (em memória) o `apply` da ponta da época 1 aplica; o
/// manifest da época 2 continua inaplicável (`MissingObject`: não está no armazenamento de `d`).
#[tokio::test]
async fn a_second_device_on_the_old_key_without_a_state_file_is_blocked_for_apply_too() {
    let rig = rig().await;
    let published = rotate(&rig).await;
    let store_d = Backend::Disk(DiskStore::open(rig.tmp.0.join("d-store")).unwrap());
    let roots_d = empty_roots(&rig.tmp, "d");
    let d = Engine::new(store_d.clone(), old_keys(), "d").unwrap();

    let report = d.pull(&rig.store).await.unwrap();
    let pending = ForeignEpoch {
        epoch: 2,
        id: published.id,
    };
    // premissa: o `pull` gravou a época 1, não gravou a 2 e anotou a pendência (só na memória)
    assert_eq!(report.foreign_epochs, vec![pending]);
    assert!(store_d.has(&rig.old_tip.id).await.unwrap());
    assert!(!store_d.has(&published.id).await.unwrap());
    assert_eq!(d.foreign_epochs().await.unwrap(), vec![pending]);
    for root in [&roots_d.vault, &roots_d.config, &roots_d.secret] {
        assert!(is_empty_dir(root.as_ref().unwrap()));
    }

    let blocked = EngineError::NeedsNewKey { have: 1, found: 2 };
    assert_eq!(d.heads().await.map(|_| ()), Err(blocked.clone()));
    assert_eq!(d.merge(8).await.map(|_| ()), Err(blocked.clone()));
    assert_eq!(
        d.snapshot(&roots_d, 9).await.map(|_| ()),
        Err(blocked.clone())
    );

    // o `apply` também recusa, mesmo sem estado: a ponta da época 1 está no armazenamento e abre
    // (não é `MissingObject`), então o `NeedsNewKey` vem da pendência. Nada é escrito
    assert_eq!(
        d.apply(None, &rig.old_tip.id, &roots_d).await.map(|_| ()),
        Err(blocked)
    );
    for root in [&roots_d.vault, &roots_d.config, &roots_d.secret] {
        assert!(is_empty_dir(root.as_ref().unwrap()));
    }

    // reconhecer o par (época, id) só na memória tira o bloqueio, e o `apply` da época 1 aplica
    // as 5 entradas de `b`
    d.acknowledge_foreign(&[pending]).await.unwrap();
    assert!(d.foreign_epochs().await.unwrap().is_empty());
    assert_eq!(
        d.apply(None, &rig.old_tip.id, &roots_d).await,
        Ok(Applied {
            created: 5,
            updated: 0,
            removed: 0,
            unchanged: 0
        })
    );
    for (class, path, bytes) in expected() {
        if path == "diario.md" {
            continue;
        }
        let root = match class {
            Class::Vault => &roots_d.vault,
            Class::Config => &roots_d.config,
            Class::Secret => &roots_d.secret,
        };
        let who = format!("{class:?} {path}");
        let on_disk =
            fs::read(root.as_ref().unwrap().join(path)).unwrap_or_else(|e| panic!("{who}: {e}"));
        assert_eq!(on_disk, bytes, "{who}");
    }
    // a época 2 não: o manifest dela não está no armazenamento de `d`
    assert_eq!(
        d.apply(None, &published.id, &roots_d).await,
        Err(EngineError::MissingObject)
    );
}
