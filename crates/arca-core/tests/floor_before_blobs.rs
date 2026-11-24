//! O piso `ClassEmptied` é decidido **antes** de qualquer blob de candidato ir para a loja (P-49,
//! Sessão 129). Antes, `build_snapshot` enviava os blobs dos candidatos e só depois aplicava o
//! piso: a recusa dizia "não gravei nada" (o Warden repassava isso) mas a loja já tinha blobs
//! órfãos, que subiam ao hub no próximo `push`.
//!
//! Cenário comum: o dispositivo `p` publica `vault/x.md` e `config/settings.json` (ponta da
//! época 1). O dispositivo `a`, com as pastas de Vault e de Config ligadas, usa uma loja que
//! **conta os `put`** (`Counting`, em volta de um `DiskStore`) e pode **mexer num arquivo local
//! no primeiro `put`** (a janela entre a leitura e o envio). Cada teste afirma antes a
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
//!   só porque nada é gravado nunca), e o arquivo idêntico à ponta reaproveita o blob existente.
//! - (v) corrida: o arquivo some **depois** da primeira passada, no primeiro `put`. O piso é
//!   reaplicado com a lista final e recusa mesmo assim; os blobs já enviados ficam órfãos (é o
//!   custo, documentado). Passa antes e depois da correção: guarda o 2º piso. Há a versão do
//!   `snapshot` e a da primeira tentativa do `begin_epoch` (onde o `carried` não pode ficar órfão,
//!   porque o 2º piso vem antes do `reseal_carried`).
//! - (vi) o conteúdo muda **entre as passadas**: a entrada do manifest descreve o texto enviado.
//! - (vii) o arquivo idêntico à ponta que muda **depois** da 1ª leitura: a entrada continua
//!   descrevendo o blob existente (hash da 1ª leitura, que é o desse blob), nunca bytes que não
//!   são os dele.

use arca_core::disk::DiskStore;
use arca_core::engine::{Engine, EngineError, Roots, Snapshot};
use arca_core::manifest::{Class, FileEntry, Hash32, Manifest};
use arca_core::scan::MAX_FILE_BYTES;
use arca_core::store::{BlobStore, Capabilities, StoreError};
use arca_core::{open, EpochKeys, ObjectId, ObjectKind, VaultKey};
use sha2::{Digest, Sha256};
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

fn new_keys() -> EpochKeys {
    EpochKeys::derive(&new_key(), 2)
}

fn digest(bytes: &[u8]) -> Hash32 {
    Hash32(Sha256::digest(bytes).into())
}

/// O texto de `config/settings.json` na ponta da época 1.
const SETTINGS_TIP: &[u8] = b"{\"tema\":\"escuro\"}";

/// O que o gancho do primeiro `put` faz com `config/settings.json`.
#[derive(Clone, Copy)]
enum Hook {
    None,
    /// Apaga o arquivo.
    DeleteSettings,
    /// Reescreve o arquivo com este texto.
    RewriteSettings(&'static [u8]),
}

/// Um `DiskStore` que conta os `put` (blob e manifest) e, no **primeiro** `put`, roda o gancho
/// (mexe num arquivo local: a corrida entre a leitura e o envio).
#[derive(Clone)]
struct Counting {
    inner: DiskStore,
    puts: Arc<AtomicUsize>,
    on_first_put: Option<Arc<dyn Fn() + Send + Sync>>,
}

impl BlobStore for Counting {
    async fn put(&self, id: &ObjectId, bytes: &[u8]) -> Result<(), StoreError> {
        if self.puts.fetch_add(1, Ordering::SeqCst) == 0 {
            if let Some(hook) = &self.on_first_put {
                hook();
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

    /// O manifest da única ponta (sem manifest ilegível).
    async fn tip_manifest(&self) -> Manifest {
        let heads = self.a.heads().await.unwrap();
        assert!(heads.rejected.is_empty(), "{:?}", heads.rejected);
        assert_eq!(heads.tips.len(), 1, "{:?}", heads.tips);
        heads.tips.into_iter().next().unwrap().manifest
    }

    /// Os caminhos que a única ponta lista, `classe:caminho`.
    async fn tip_listing(&self) -> Vec<String> {
        listing(&self.tip_manifest().await)
    }

    /// O texto claro do blob que `entry` cita, lido da loja e aberto com as chaves da época 1.
    async fn plain_of(&self, entry: &FileEntry) -> Vec<u8> {
        let store = DiskStore::open(&self.store_dir).unwrap();
        let id = ObjectId {
            kind: ObjectKind::Blob,
            hash: entry.blob.0,
        };
        let bytes = store.get(&id).await.unwrap().expect("blob ausente da loja");
        open(&old_keys(), ObjectKind::Blob, &bytes)
            .unwrap()
            .to_vec()
    }
}

fn listing(manifest: &Manifest) -> Vec<String> {
    manifest
        .files
        .iter()
        .map(|f| format!("{:?}:{}", f.class, f.path))
        .collect()
}

fn entry_of<'m>(manifest: &'m Manifest, path: &str) -> &'m FileEntry {
    manifest
        .files
        .iter()
        .find(|f| f.path == path)
        .unwrap_or_else(|| panic!("{path} não está no manifest"))
}

/// `p` publica `x.md` (Vault) e `settings.json` (Config) na época 1 (e `github.json` em Secret,
/// se `carry_secret`); `a` enxerga a mesma loja com o contador de `put` zerado.
///
/// Sem `carry_secret`, `a` usa as mesmas pastas de `p`. Com ele, `a` tem pastas próprias de Vault
/// e de Config (sem pasta de Secret), com estado, e **aceita** a ponta (`apply`): é o que a
/// primeira tentativa do `begin_epoch` exige para levar a entrada de Secret recifrada (`carried`).
async fn build(hook: Hook, carry_secret: bool) -> Lab {
    let tmp = Tmp::new();
    let store_dir = tmp.0.join("store");
    let (p_vault, p_config) = (tmp.dir("p-vault"), tmp.dir("p-config"));
    fs::write(p_vault.join("x.md"), b"vault").unwrap();
    fs::write(p_config.join("settings.json"), SETTINGS_TIP).unwrap();
    let mut p_roots = Roots {
        vault: Some(p_vault.clone()),
        config: Some(p_config.clone()),
        secret: None,
    };
    if carry_secret {
        let p_secret = tmp.dir("p-secret");
        fs::write(p_secret.join("github.json"), b"{\"token\":\"t\"}").unwrap();
        p_roots.secret = Some(p_secret);
    }

    let publisher = Engine::new(DiskStore::open(&store_dir).unwrap(), old_keys(), "p").unwrap();
    let tip_id = match publisher.snapshot(&p_roots, 1).await.unwrap() {
        Snapshot::Created { id, .. } => id,
        other => panic!("esperava Created, veio {other:?}"),
    };

    let (vault, config) = if carry_secret {
        (tmp.dir("a-vault"), tmp.dir("a-config"))
    } else {
        (p_vault, p_config)
    };
    let settings = config.join("settings.json");
    let on_first_put: Option<Arc<dyn Fn() + Send + Sync>> = match hook {
        Hook::None => None,
        Hook::DeleteSettings => Some(Arc::new(move || fs::remove_file(&settings).unwrap())),
        Hook::RewriteSettings(text) => Some(Arc::new(move || fs::write(&settings, text).unwrap())),
    };
    let puts = Arc::new(AtomicUsize::new(0));
    let store = Counting {
        inner: DiskStore::open(&store_dir).unwrap(),
        puts: puts.clone(),
        on_first_put,
    };
    let mut a = Engine::new(store, old_keys(), "a").unwrap();
    if carry_secret {
        a = a.with_state(tmp.0.join("a-state"));
        let a_roots = Roots {
            vault: Some(vault.clone()),
            config: Some(config.clone()),
            secret: None,
        };
        a.apply(None, &tip_id, &a_roots).await.unwrap();
    }
    let lab = Lab {
        _tmp: tmp,
        store_dir,
        vault,
        config,
        puts,
        a,
    };

    // premissa: a ponta é a de `p`, `a` tem os mesmos arquivos e ainda não gravou nada
    let mut expected = vec!["Vault:x.md", "Config:settings.json"];
    if carry_secret {
        expected.push("Secret:github.json");
    }
    assert_eq!(lab.tip_listing().await, expected);
    assert_eq!(fs::read(lab.vault.join("x.md")).unwrap(), b"vault");
    assert_eq!(
        fs::read(lab.config.join("settings.json")).unwrap(),
        SETTINGS_TIP
    );
    assert_eq!(lab.puts(), 0);
    assert_eq!(
        lab.stored(),
        expected.len() + 1,
        "um blob por arquivo e 1 manifest da época 1"
    );
    lab
}

async fn lab(hook: Hook) -> Lab {
    build(hook, false).await
}

/// Um arquivo novo em Config: um candidato que o piso não impede e que seria enviado.
fn add_new_config_file(lab: &Lab) {
    fs::write(lab.config.join("new.json"), b"{\"novo\":true}").unwrap();
    assert!(lab.config.join("new.json").is_file());
}

/// Um arquivo novo no Vault, que vem **antes** de `x.md` e de `settings.json` na ordem de envio
/// (Vault antes de Config, `a.md` antes de `x.md`): é o primeiro `put`, o que dispara o gancho.
fn add_first_vault_file(lab: &Lab) {
    fs::write(lab.vault.join("a.md"), b"novo no vault").unwrap();
    assert!(lab.vault.join("a.md").is_file());
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
    let lab = lab(Hook::None).await;
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
    let lab = lab(Hook::None).await;
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
/// grava nada. Os arquivos idênticos à ponta (`x.md`, `settings.json`) não geram `put` e a entrada
/// deles é a mesma da ponta (mesmo blob, `sha256` e `size`).
#[tokio::test]
async fn control_snapshot_with_no_class_emptied_still_sends_the_blob_and_the_manifest() {
    let lab = lab(Hook::None).await;
    let tip = lab.tip_manifest().await;
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
    for unchanged in ["x.md", "settings.json"] {
        assert_eq!(entry_of(&manifest, unchanged), entry_of(&tip, unchanged));
    }
    let new = entry_of(&manifest, "new.json");
    assert_eq!(lab.plain_of(new).await, b"{\"novo\":true}");
    assert_eq!(new.sha256, digest(b"{\"novo\":true}"));
}

/// (iii) Controle do `begin_epoch`: Vault intacto: a época 2 sai com os 3 blobs e o manifest.
#[tokio::test]
async fn control_begin_epoch_with_no_class_emptied_still_sends_the_blobs_and_the_manifest() {
    let lab = lab(Hook::None).await;
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
    let lab = lab(Hook::None).await;
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
    let lab = lab(Hook::None).await;
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

fn config_emptied() -> EngineError {
    EngineError::ClassEmptied {
        class: Class::Config,
        had: 1,
    }
}

/// (v) Corrida no `snapshot`: `settings.json` (a única entrada de Config) some no primeiro `put`,
/// depois de a primeira passada já tê-lo lido. O piso reaplicado com a lista final recusa Config
/// esvaziada, e nenhum manifest é publicado. O blob de `a.md`, já enviado, fica órfão: é o custo
/// conhecido.
#[tokio::test]
async fn a_file_that_vanishes_during_the_send_still_cannot_publish_an_emptied_class() {
    let lab = lab(Hook::DeleteSettings).await;
    add_first_vault_file(&lab);
    let stored_before = lab.stored();

    let refused = lab.a.snapshot(&lab.roots(), 5).await;

    assert_eq!(refused, Err(config_emptied()));
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

/// (v) A mesma corrida na primeira tentativa do `begin_epoch` (`old_tip`, sem `prev`: tudo é
/// reenviado), com uma entrada de Secret a levar (`carried`). O 2º piso recusa **antes** do
/// `reseal_carried`: ficam órfãos só os blobs dos candidatos já enviados (`a.md` e `x.md`, nesta
/// ordem, antes de `settings.json` ser lido e achado ausente), e o blob de Secret recifrado não
/// chega a ser gravado. Nenhum manifest da época 2.
#[tokio::test]
async fn a_file_that_vanishes_during_the_rotation_orphans_no_carried_blob_and_publishes_nothing() {
    let lab = build(Hook::DeleteSettings, true).await;
    add_first_vault_file(&lab);
    let before: Vec<_> = {
        let mut names = blob_names(&lab);
        names.sort();
        names
    };
    let stored_before = lab.stored();

    let refused = lab.a.begin_epoch(&new_key(), &lab.roots(), 5).await;

    assert_eq!(refused.map(|_| ()), Err(config_emptied()));
    // premissa: o arquivo sumiu pelo `put` e a rotação chegou a enviar os dois candidatos do Vault
    assert!(!lab.config.join("settings.json").exists());
    assert_eq!(lab.puts(), 2);
    assert_eq!(lab.stored(), stored_before + 2, "só os dois blobs órfãos");
    // os órfãos são da época 2 e são os textos de `a.md` e `x.md`; o `github.json` recifrado
    // (o `carried`) não está entre eles
    let mut plains: Vec<Vec<u8>> = blob_names(&lab)
        .into_iter()
        .filter(|name| !before.contains(name))
        .map(|name| {
            let bytes = fs::read(lab.store_dir.join("blob").join(&name)).unwrap();
            open(&new_keys(), ObjectKind::Blob, &bytes)
                .unwrap()
                .to_vec()
        })
        .collect();
    plains.sort();
    assert_eq!(plains, [b"novo no vault".to_vec(), b"vault".to_vec()]);
    assert_eq!(
        fs::read_dir(lab.store_dir.join("manifest"))
            .unwrap()
            .count(),
        1
    );
}

fn blob_names(lab: &Lab) -> Vec<String> {
    fs::read_dir(lab.store_dir.join("blob"))
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .collect()
}

/// (vi) O conteúdo muda **entre as passadas**. `settings.json` já difere da ponta (precisa de
/// `put`); no primeiro `put` (o de `a.md`) ele é reescrito com um texto de **outro tamanho**. A
/// entrada do manifest tem de descrever o texto enviado (o da 2ª leitura): `sha256` e `size` do
/// texto novo, e o blob gravado abre e é esse texto. Um código que reaproveitasse o hash da 1ª
/// passada para esse arquivo gravaria uma entrada que não descreve o blob.
#[tokio::test]
async fn a_file_that_changes_between_the_passes_is_described_by_the_text_that_was_sent() {
    const READ_FIRST: &[u8] = b"{\"tema\":\"claro\"}";
    const SENT: &[u8] = b"{\"tema\":\"azul-noturno\",\"fonte\":12}";
    let lab = lab(Hook::RewriteSettings(SENT)).await;
    fs::write(lab.config.join("settings.json"), READ_FIRST).unwrap();
    add_first_vault_file(&lab);
    // premissa: o texto lido primeiro difere da ponta e do que será enviado, inclusive no tamanho
    assert_ne!(READ_FIRST, SETTINGS_TIP);
    assert_ne!(READ_FIRST.len(), SENT.len());

    let snapshot = lab.a.snapshot(&lab.roots(), 5).await.unwrap();

    let Snapshot::Created {
        manifest,
        new_blobs,
        ..
    } = snapshot
    else {
        panic!("esperava Created, veio {snapshot:?}");
    };
    // premissa: a corrida aconteceu (o arquivo tem o texto reescrito) e os dois mudados foram enviados
    assert_eq!(fs::read(lab.config.join("settings.json")).unwrap(), SENT);
    assert_eq!(new_blobs, 2, "a.md e settings.json");
    assert_eq!(lab.puts(), 3, "2 blobs e o manifest");

    let settings = entry_of(&manifest, "settings.json");
    assert_eq!(settings.sha256, digest(SENT));
    assert_eq!(settings.size, SENT.len() as u64);
    let plain = lab.plain_of(settings).await;
    assert_eq!(plain, SENT);
    // o invariante em si: a entrada descreve o blob que cita
    assert_eq!(digest(&plain), settings.sha256);
    assert_eq!(plain.len() as u64, settings.size);
}

/// (vii) O arquivo **idêntico à ponta** que muda **depois** da 1ª leitura (no primeiro `put`):
/// não há `put` dele e a entrada é a da ponta (mesmo blob, `sha256` e `size`), que descreve o blob
/// existente. É como se ele tivesse mudado logo depois do snapshot.
#[tokio::test]
async fn an_unchanged_file_that_changes_after_the_first_read_keeps_the_entry_of_its_existing_blob()
{
    const LATER: &[u8] = b"{\"tema\":\"mudou-depois-da-leitura\"}";
    let lab = lab(Hook::RewriteSettings(LATER)).await;
    let tip = lab.tip_manifest().await;
    add_first_vault_file(&lab);

    let snapshot = lab.a.snapshot(&lab.roots(), 5).await.unwrap();

    let Snapshot::Created {
        manifest,
        new_blobs,
        ..
    } = snapshot
    else {
        panic!("esperava Created, veio {snapshot:?}");
    };
    // premissa: a corrida aconteceu, e só `a.md` foi enviado (nenhum `put` de `settings.json`)
    assert_eq!(fs::read(lab.config.join("settings.json")).unwrap(), LATER);
    assert_eq!(new_blobs, 1);
    assert_eq!(lab.puts(), 2, "o blob de a.md e o manifest");

    let settings = entry_of(&manifest, "settings.json");
    assert_eq!(settings, entry_of(&tip, "settings.json"));
    let plain = lab.plain_of(settings).await;
    assert_eq!(plain, SETTINGS_TIP);
    assert_eq!(digest(&plain), settings.sha256);
    assert_eq!(plain.len() as u64, settings.size);
}
