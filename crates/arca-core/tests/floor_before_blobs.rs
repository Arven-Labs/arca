//! O piso `ClassEmptied` é decidido **antes** de qualquer blob de candidato ir para a loja (P-49,
//! Sessão 129). Antes, `build_snapshot` enviava os blobs dos candidatos e só depois aplicava o
//! piso: a recusa dizia "não gravei nada" (o Warden repassava isso) mas a loja já tinha blobs
//! órfãos, que subiam ao hub no próximo `push`.
//!
//! Cenário comum: o dispositivo `p` publica `vault/x.md` e `config/settings.json` (ponta da
//! época 1). O dispositivo `a`, com as pastas de Vault e de Config ligadas, usa uma loja que
//! **conta os `put`** (`Counting`, em volta de um `DiskStore`). Cada teste afirma antes a
//! **premissa** (o que a ponta lista, que a loja começou com N objetos e zero `put`, e que há
//! candidato que seria enviado) e só depois o resultado.
//!
//! O que cada caso prova, com o código de antes da correção:
//! - (i) `begin_epoch`, Vault esvaziado: antes dava 2 `put` (os dois arquivos de Config) e a loja
//!   crescia; agora zero.
//! - (ii) `snapshot`, Vault esvaziado: antes dava 1 `put` (`new.json`, o único candidato novo).
//! - (iv) o Vault só tem arquivos que o scan pula (grande demais; link): é "esvaziado" do mesmo
//!   jeito, e o blob de Config saía antes.
//! - (iii) controle: classe **não** esvaziada continua gravando blob e manifest (o teste não passa
//!   só porque nada é gravado nunca).
//! - (v) corrida: o arquivo some **depois** da primeira passada, no primeiro `put`. O piso é
//!   reaplicado com a lista final e recusa mesmo assim; o blob já enviado fica órfão (é o custo,
//!   documentado, dessa janela estreita). Passa antes e depois da correção: guarda o 2o piso.

use arca_core::disk::DiskStore;
use arca_core::engine::{Engine, EngineError, Roots, Snapshot};
use arca_core::manifest::Class;
use arca_core::scan::MAX_FILE_BYTES;
use arca_core::store::{BlobStore, Capabilities, StoreError};
use arca_core::{EpochKeys, ObjectId, ObjectKind, VaultKey};
use std::fs;
use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

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

fn old_keys() -> EpochKeys {
    EpochKeys::derive(&VaultKey::from_bytes([3u8; 32]), 1)
}

fn new_key() -> VaultKey {
    VaultKey::from_bytes([4u8; 32])
}

/// Um `DiskStore` que conta os `put` (blob e manifest) e, se pedido, apaga um arquivo local no
/// **primeiro** `put` (a corrida entre a leitura e o envio).
#[derive(Clone)]
struct Counting {
    inner: DiskStore,
    puts: Arc<AtomicUsize>,
    delete_on_first_put: Option<PathBuf>,
}

impl BlobStore for Counting {
    async fn put(&self, id: &ObjectId, bytes: &[u8]) -> Result<(), StoreError> {
        if self.puts.fetch_add(1, Ordering::SeqCst) == 0 {
            if let Some(path) = &self.delete_on_first_put {
                fs::remove_file(path).unwrap();
            }
        }
        self.inner.put(id, bytes).await
    }

    async fn get(&self, id: &ObjectId) -> Result<Option<Vec<u8>>, StoreError> {
        self.inner.get(id).await
    }

    async fn has(&self, id: &ObjectId) -> Result<bool, StoreError> {
        self.inner.has(id).await
    }

    async fn list(&self, kind: ObjectKind) -> Result<Vec<ObjectId>, StoreError> {
        self.inner.list(kind).await
    }

    async fn delete(&self, id: &ObjectId) -> Result<(), StoreError> {
        self.inner.delete(id).await
    }

    fn capabilities(&self) -> Capabilities {
        self.inner.capabilities()
    }
}

struct Lab {
    _tmp: Tmp,
    store_dir: PathBuf,
    vault: PathBuf,
    config: PathBuf,
    puts: Arc<AtomicUsize>,
    a: Engine<Counting>,
}

impl Lab {
    fn roots(&self) -> Roots {
        Roots {
            vault: Some(self.vault.clone()),
            config: Some(self.config.clone()),
            secret: None,
        }
    }

    /// Quantos objetos (blobs e manifests) a loja tem.
    fn stored(&self) -> usize {
        ["blob", "manifest"]
            .iter()
            .map(|kind| fs::read_dir(self.store_dir.join(kind)).unwrap().count())
            .sum()
    }

    fn puts(&self) -> usize {
        self.puts.load(Ordering::SeqCst)
    }

    /// Os caminhos que a única ponta lista, `classe:caminho`.
    async fn tip_listing(&self) -> Vec<String> {
        let heads = self.a.heads().await.unwrap();
        assert!(heads.rejected.is_empty(), "{:?}", heads.rejected);
        assert_eq!(heads.tips.len(), 1, "{:?}", heads.tips);
        heads.tips[0]
            .manifest
            .files
            .iter()
            .map(|f| format!("{:?}:{}", f.class, f.path))
            .collect()
    }
}

/// `p` publica `x.md` (Vault) e `settings.json` (Config) na época 1; `a` enxerga a mesma loja com
/// o contador de `put` zerado. `vanish_settings`: o primeiro `put` de `a` apaga `settings.json`.
async fn lab(vanish_settings: bool) -> Lab {
    let tmp = Tmp::new();
    let store_dir = tmp.0.join("store");
    let (vault, config) = (tmp.dir("vault"), tmp.dir("config"));
    fs::write(vault.join("x.md"), b"vault").unwrap();
    fs::write(config.join("settings.json"), b"{\"tema\":\"escuro\"}").unwrap();

    let publisher = Engine::new(DiskStore::open(&store_dir).unwrap(), old_keys(), "p").unwrap();
    let roots = Roots {
        vault: Some(vault.clone()),
        config: Some(config.clone()),
        secret: None,
    };
    match publisher.snapshot(&roots, 1).await.unwrap() {
        Snapshot::Created { .. } => {}
        other => panic!("esperava Created, veio {other:?}"),
    }

    let puts = Arc::new(AtomicUsize::new(0));
    let store = Counting {
        inner: DiskStore::open(&store_dir).unwrap(),
        puts: puts.clone(),
        delete_on_first_put: vanish_settings.then(|| config.join("settings.json")),
    };
    let a = Engine::new(store, old_keys(), "a").unwrap();
    let lab = Lab {
        _tmp: tmp,
        store_dir,
        vault,
        config,
        puts,
        a,
    };
    // premissa: a ponta é a de `p`, com as duas classes, e `a` ainda não gravou nada
    assert_eq!(
        lab.tip_listing().await,
        ["Vault:x.md", "Config:settings.json"]
    );
    assert_eq!(lab.puts(), 0);
    assert_eq!(lab.stored(), 3, "2 blobs e 1 manifest da época 1");
    lab
}

/// Um arquivo novo em Config: um candidato que o piso não impede e que seria enviado.
fn add_new_config_file(lab: &Lab) {
    fs::write(lab.config.join("new.json"), b"{\"novo\":true}").unwrap();
    assert!(lab.config.join("new.json").is_file());
}

/// O Vault perde o único arquivo que a ponta lista.
fn empty_the_vault(lab: &Lab) {
    fs::remove_file(lab.vault.join("x.md")).unwrap();
    assert!(fs::read_dir(&lab.vault).unwrap().next().is_none());
}

fn vault_emptied() -> EngineError {
    EngineError::ClassEmptied {
        class: Class::Vault,
        had: 1,
    }
}

/// (i) A primeira tentativa do `begin_epoch`, com o Vault esvaziado, recusa sem nenhum `put`,
/// mesmo havendo candidatos em Config para enviar.
#[tokio::test]
async fn begin_epoch_refuses_an_emptied_vault_before_sending_any_blob() {
    let lab = lab(false).await;
    empty_the_vault(&lab);
    add_new_config_file(&lab);
    let stored_before = lab.stored();

    let refused = lab.a.begin_epoch(&new_key(), &lab.roots(), 5).await;

    assert_eq!(refused.map(|_| ()), Err(vault_emptied()));
    assert_eq!(lab.puts(), 0, "enviou blob antes do piso");
    assert_eq!(lab.stored(), stored_before, "a loja cresceu");
}

/// (ii) O mesmo para o `snapshot` comum: `new.json` seria o blob novo, e não pode sair.
#[tokio::test]
async fn snapshot_refuses_an_emptied_vault_before_sending_any_blob() {
    let lab = lab(false).await;
    empty_the_vault(&lab);
    add_new_config_file(&lab);
    let stored_before = lab.stored();

    let refused = lab.a.snapshot(&lab.roots(), 5).await;

    assert_eq!(refused, Err(vault_emptied()));
    assert_eq!(lab.puts(), 0, "enviou blob antes do piso");
    assert_eq!(lab.stored(), stored_before, "a loja cresceu");
}

/// (iii) Controle do `snapshot`: Vault intacto, `new.json` novo em Config: grava o blob e o
/// manifest (2 `put`). Sem este teste, os de cima passariam também com um `snapshot` que nunca
/// grava nada.
#[tokio::test]
async fn control_snapshot_with_no_class_emptied_still_sends_the_blob_and_the_manifest() {
    let lab = lab(false).await;
    add_new_config_file(&lab);

    let snapshot = lab.a.snapshot(&lab.roots(), 5).await.unwrap();

    let Snapshot::Created {
        manifest,
        new_blobs,
        ..
    } = snapshot
    else {
        panic!("esperava Created, veio {snapshot:?}");
    };
    assert_eq!(new_blobs, 1);
    assert_eq!(manifest.files.len(), 3);
    assert_eq!(lab.puts(), 2, "1 blob novo e o manifest");
    assert_eq!(lab.stored(), 5);
}

/// (iii) Controle do `begin_epoch`: Vault intacto: a época 2 sai com os 3 blobs e o manifest.
#[tokio::test]
async fn control_begin_epoch_with_no_class_emptied_still_sends_the_blobs_and_the_manifest() {
    let lab = lab(false).await;
    add_new_config_file(&lab);

    let (_, snapshot) = lab
        .a
        .begin_epoch(&new_key(), &lab.roots(), 5)
        .await
        .unwrap();

    let Snapshot::Created {
        manifest,
        new_blobs,
        ..
    } = snapshot
    else {
        panic!("esperava Created, veio {snapshot:?}");
    };
    assert_eq!(manifest.key_epoch, 2);
    assert_eq!(manifest.base_version, None);
    assert_eq!(new_blobs, 3);
    assert_eq!(
        lab.puts(),
        4,
        "3 blobs (sem base não há atalho) e o manifest"
    );
}

/// (iv) O único arquivo do Vault passou a ser maior que `MAX_FILE_BYTES`: o scan o pula
/// (`skipped`), o Vault fica sem candidato e o piso recusa, sem enviar o blob de Config.
#[tokio::test]
async fn a_vault_left_only_with_a_file_that_is_too_large_is_refused_before_any_blob() {
    let lab = lab(false).await;
    // arquivo esparso: o scan só olha o tamanho do metadata, não lê o conteúdo
    let big = fs::File::create(lab.vault.join("x.md")).unwrap();
    big.set_len(MAX_FILE_BYTES + 1).unwrap();
    drop(big);
    assert_eq!(
        fs::metadata(lab.vault.join("x.md")).unwrap().len(),
        MAX_FILE_BYTES + 1
    );
    add_new_config_file(&lab);
    let stored_before = lab.stored();

    let refused = lab.a.snapshot(&lab.roots(), 5).await;

    assert_eq!(refused, Err(vault_emptied()));
    assert_eq!(lab.puts(), 0, "enviou blob antes do piso");
    assert_eq!(lab.stored(), stored_before, "a loja cresceu");
}

/// (iv) O único item do Vault virou um link simbólico: o scan o pula, e o `begin_epoch` recusa
/// sem enviar os blobs de Config.
#[cfg(unix)]
#[tokio::test]
async fn a_vault_left_only_with_a_symlink_is_refused_before_any_blob() {
    let lab = lab(false).await;
    fs::remove_file(lab.vault.join("x.md")).unwrap();
    std::os::unix::fs::symlink(lab.config.join("settings.json"), lab.vault.join("x.md")).unwrap();
    assert!(fs::symlink_metadata(lab.vault.join("x.md"))
        .unwrap()
        .file_type()
        .is_symlink());
    add_new_config_file(&lab);
    let stored_before = lab.stored();

    let refused = lab.a.begin_epoch(&new_key(), &lab.roots(), 5).await;

    assert_eq!(refused.map(|_| ()), Err(vault_emptied()));
    assert_eq!(lab.puts(), 0, "enviou blob antes do piso");
    assert_eq!(lab.stored(), stored_before, "a loja cresceu");
}

/// (v) Corrida: `settings.json` (a única entrada de Config) some no primeiro `put`, depois de a
/// primeira passada já tê-lo lido. O piso reaplicado com a lista final recusa Config esvaziada, e
/// nenhum manifest é publicado. O blob de `a.md`, já enviado, fica órfão: é o custo conhecido.
#[tokio::test]
async fn a_file_that_vanishes_during_the_send_still_cannot_publish_an_emptied_class() {
    let lab = lab(true).await;
    // `a.md` é novo e vem antes de `settings.json` (Vault antes de Config): o 1o `put` é dele
    fs::write(lab.vault.join("a.md"), b"novo no vault").unwrap();
    assert!(lab.config.join("settings.json").is_file());
    let stored_before = lab.stored();

    let refused = lab.a.snapshot(&lab.roots(), 5).await;

    assert_eq!(
        refused,
        Err(EngineError::ClassEmptied {
            class: Class::Config,
            had: 1
        })
    );
    // premissa: o arquivo sumiu pelo `put` (a corrida aconteceu) e só o blob de `a.md` foi enviado
    assert!(!lab.config.join("settings.json").exists());
    assert_eq!(lab.puts(), 1);
    // nenhum manifest novo: só o blob órfão entrou na loja
    assert_eq!(lab.stored(), stored_before + 1);
    assert_eq!(
        lab.tip_listing().await,
        ["Vault:x.md", "Config:settings.json"]
    );
}
