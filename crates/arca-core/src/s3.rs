//! Backend S3-compatível (Fase 3, `SPEC-TECNICA.md` §4, ADR 0007 e 0008): o `BlobStore` sobre
//! `object_store`. A chave do objeto é `<tipo>/<hex>`, a mesma do disco.
//!
//! O backend não é confiável para integridade: a leitura segue por `verified_get`, e o `put`
//! recusa bytes que não têm o hash do id antes de ir à rede. Nenhum erro carrega o texto do
//! `object_store` (traz caminho e, às vezes, endpoint), só o tipo (`io::ErrorKind`).
//!
//! `delete` é `Unsupported` como no hub: apagar um backup é irreversível e a v1 não tem coleta
//! de lixo (ADR 0011, retenção P-05).
//!
//! Limites conhecidos: o `object_store` não distingue timeout de recusa de conexão no erro que
//! devolve, então os dois viram `Other`; e o `reqwest` dele honra `HTTP_PROXY`/`HTTPS_PROXY` do
//! ambiente (a requisição é assinada e o corpo já é cifrado, ao contrário do token do hub).

use crate::object::{ObjectId, ObjectKind};
use crate::server::{MAX_BLOB_OBJECT_BYTES, MAX_MANIFEST_OBJECT_BYTES};
use crate::store::{BlobStore, Capabilities, StoreError};
use futures_util::StreamExt;
use object_store::aws::AmazonS3Builder;
use object_store::path::Path;
use object_store::{ClientOptions, ObjectStore, ObjectStoreExt, PutPayload, RetryConfig};
use std::io;
use std::sync::Arc;
use std::time::Duration;
use zeroize::Zeroizing;

/// Como falar com o bucket. `Debug` não mostra endpoint, bucket nem credenciais.
#[derive(Clone)]
pub struct S3Config {
    /// `https://s3.us-west-004.backblazeb2.com`, `http://127.0.0.1:3900` (só com `allow_http`).
    pub endpoint: String,
    pub region: String,
    pub bucket: String,
    pub access_key_id: String,
    pub secret_access_key: Zeroizing<String>,
    /// Aceita `http` em qualquer host. Padrão `false`: a requisição é assinada, mas o objeto
    /// (cifrado) e os nomes andariam em claro.
    pub allow_http: bool,
    /// O que o provedor garante para `list` (ver `Capabilities`). Padrão `false`: só ligue
    /// para um provedor que você sabe que lista o que acabou de gravar.
    pub consistent_list: bool,
}

impl std::fmt::Debug for S3Config {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("S3Config")
            .field("consistent_list", &self.consistent_list)
            .finish_non_exhaustive()
    }
}

/// Configuração recusada antes de qualquer rede. Sem o valor recusado (pode ser um segredo).
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum S3Error {
    #[error("invalid S3 configuration")]
    InvalidConfig,
    #[error("plain http endpoint requires allow_http")]
    InsecureEndpoint,
}

/// O bucket como um `BlobStore`.
#[derive(Clone)]
pub struct S3Store {
    store: Arc<dyn ObjectStore>,
    consistent_list: bool,
    /// Teto de bytes por objeto; `None` = o padrão do tipo (`MAX_*_OBJECT_BYTES`).
    max_object_bytes: Option<u64>,
    max_list_ids: usize,
}

impl std::fmt::Debug for S3Store {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("S3Store").finish_non_exhaustive()
    }
}

/// Ids que uma listagem inteira pode somar antes de o cliente desistir (o mesmo do hub).
pub const MAX_LIST_IDS: usize = 1_000_000;

/// Prazo de uma requisição inteira (um blob grande numa linha lenta) e da conexão.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(120);
const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
/// Retentativas por requisição e o tempo máximo delas (o padrão do `object_store` é mais longo).
const MAX_RETRIES: usize = 3;
const RETRY_TIMEOUT: Duration = Duration::from_secs(60);

/// Um texto de configuração: não vazio e sem caracteres de controle (vira cabeçalho ou caminho).
fn plain(s: &str) -> bool {
    !s.is_empty() && !s.chars().any(char::is_control)
}

impl S3Store {
    pub fn new(config: &S3Config) -> Result<S3Store, S3Error> {
        // só `esquema://host[:porta]`: sem credenciais embutidas, espaço nem controle
        let (rest, plain_http) = if let Some(rest) = config.endpoint.strip_prefix("https://") {
            (rest, false)
        } else if let Some(rest) = config.endpoint.strip_prefix("http://") {
            (rest, true)
        } else {
            return Err(S3Error::InvalidConfig);
        };
        if !plain(rest) || rest.chars().any(|c| c.is_whitespace() || c == '@') {
            return Err(S3Error::InvalidConfig);
        }
        if !plain(&config.region)
            || !plain(&config.bucket)
            || !plain(&config.access_key_id)
            || !plain(&config.secret_access_key)
        {
            return Err(S3Error::InvalidConfig);
        }
        if plain_http && !config.allow_http {
            return Err(S3Error::InsecureEndpoint);
        }
        let retry = RetryConfig {
            max_retries: MAX_RETRIES,
            retry_timeout: RETRY_TIMEOUT,
            ..RetryConfig::default()
        };
        let options = ClientOptions::new()
            .with_allow_http(config.allow_http)
            .with_timeout(REQUEST_TIMEOUT)
            .with_connect_timeout(CONNECT_TIMEOUT);
        let store = AmazonS3Builder::new()
            .with_endpoint(config.endpoint.clone())
            .with_region(config.region.clone())
            .with_bucket_name(config.bucket.clone())
            .with_access_key_id(config.access_key_id.clone())
            .with_secret_access_key(config.secret_access_key.as_str())
            // `endpoint/bucket/chave`: Garage e MinIO não aceitam `bucket.endpoint`
            .with_virtual_hosted_style_request(false)
            .with_retry(retry)
            .with_client_options(options)
            .build()
            .map_err(|_| S3Error::InvalidConfig)?;
        Ok(S3Store::from_object_store(
            Arc::new(store),
            config.consistent_list,
        ))
    }

    /// Sobre qualquer `ObjectStore` (os testes usam `InMemory`).
    pub(crate) fn from_object_store(store: Arc<dyn ObjectStore>, consistent_list: bool) -> S3Store {
        S3Store {
            store,
            consistent_list,
            max_object_bytes: None,
            max_list_ids: MAX_LIST_IDS,
        }
    }

    #[cfg(test)]
    pub(crate) fn with_limits(mut self, max_object_bytes: u64, max_list_ids: usize) -> S3Store {
        self.max_object_bytes = Some(max_object_bytes);
        self.max_list_ids = max_list_ids;
        self
    }

    fn cap(&self, kind: ObjectKind) -> u64 {
        self.max_object_bytes.unwrap_or(match kind {
            ObjectKind::Blob => MAX_BLOB_OBJECT_BYTES,
            ObjectKind::Manifest => MAX_MANIFEST_OBJECT_BYTES,
        })
    }
}

/// A chave do objeto: `<tipo>/<hex>`.
fn path_of(id: &ObjectId) -> Path {
    Path::from(id.to_string().as_str())
}

fn unavailable(kind: io::ErrorKind) -> StoreError {
    StoreError::Unavailable(kind)
}

/// Só o tipo do erro: o texto do `object_store` traz o caminho e, às vezes, o endpoint.
fn map_error(e: &object_store::Error) -> StoreError {
    use object_store::Error as E;
    unavailable(match e {
        E::Unauthenticated { .. } | E::PermissionDenied { .. } => io::ErrorKind::PermissionDenied,
        E::NotSupported { .. } | E::NotImplemented { .. } => io::ErrorKind::Unsupported,
        E::InvalidPath { .. } => io::ErrorKind::InvalidData,
        // `Error` é `non_exhaustive`; o resto (rede, 5xx, quota, ...) é "tente de novo ou use outro"
        _ => io::ErrorKind::Other,
    })
}

impl BlobStore for S3Store {
    async fn put(&self, id: &ObjectId, bytes: &[u8]) -> Result<(), StoreError> {
        if ObjectId::of(id.kind, bytes).hash != id.hash {
            return Err(StoreError::IdMismatch);
        }
        if bytes.len() as u64 > self.cap(id.kind) {
            return Err(unavailable(io::ErrorKind::InvalidInput));
        }
        self.store
            .put(&path_of(id), PutPayload::from(bytes.to_vec()))
            .await
            .map_err(|e| map_error(&e))?;
        Ok(())
    }

    async fn get(&self, id: &ObjectId) -> Result<Option<Vec<u8>>, StoreError> {
        let cap = self.cap(id.kind);
        match self.store.get(&path_of(id)).await {
            Ok(result) => {
                // o tamanho declarado vem do servidor (não confiável): não ler o que passa do teto
                if result.meta.size > cap {
                    return Err(unavailable(io::ErrorKind::InvalidData));
                }
                let bytes = result.bytes().await.map_err(|e| map_error(&e))?;
                if bytes.len() as u64 > cap {
                    return Err(unavailable(io::ErrorKind::InvalidData));
                }
                Ok(Some(bytes.to_vec()))
            }
            Err(object_store::Error::NotFound { .. }) => Ok(None),
            Err(e) => Err(map_error(&e)),
        }
    }

    async fn has(&self, id: &ObjectId) -> Result<bool, StoreError> {
        match self.store.head(&path_of(id)).await {
            Ok(_) => Ok(true),
            Err(object_store::Error::NotFound { .. }) => Ok(false),
            Err(e) => Err(map_error(&e)),
        }
    }

    async fn list(&self, kind: ObjectKind) -> Result<Vec<ObjectId>, StoreError> {
        let prefix = Path::from(kind.as_str());
        let mut stream = self.store.list(Some(&prefix));
        let mut ids = Vec::new();
        let mut seen = 0usize;
        while let Some(item) = stream.next().await {
            let meta = item.map_err(|e| map_error(&e))?;
            // o teto conta tudo o que o bucket devolve, não só o que vira id: um bucket
            // compartilhado com milhões de arquivos de outro uso não pode prender o cliente
            seen += 1;
            if seen >= self.max_list_ids {
                return Err(unavailable(io::ErrorKind::InvalidData));
            }
            // o nome vem do bucket (entrada não confiável): só `<tipo>/<64 hex minúsculos>`
            if let Ok(id) = meta.location.as_ref().parse::<ObjectId>() {
                if id.kind == kind {
                    ids.push(id);
                }
            }
        }
        Ok(ids)
    }

    /// A v1 não apaga (ADR 0011), como o hub: `Unavailable(Unsupported)`, sem ir à rede.
    async fn delete(&self, _id: &ObjectId) -> Result<(), StoreError> {
        Err(unavailable(io::ErrorKind::Unsupported))
    }

    fn capabilities(&self) -> Capabilities {
        Capabilities {
            consistent_list: self.consistent_list,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use object_store::memory::InMemory;
    use object_store::path::Path;
    use object_store::{ObjectStoreExt, PutPayload};
    use std::io;

    fn mem_store(consistent_list: bool) -> (S3Store, Arc<InMemory>) {
        let mem = Arc::new(InMemory::new());
        (
            S3Store::from_object_store(mem.clone(), consistent_list),
            mem,
        )
    }

    fn obj(kind: ObjectKind, data: &[u8]) -> (ObjectId, Vec<u8>) {
        (ObjectId::of(kind, data), data.to_vec())
    }

    /// Grava direto no `InMemory`, sem passar pelo `S3Store` (monta o cenário do teste).
    async fn raw_put(mem: &InMemory, key: &str, data: &[u8]) {
        mem.put(&Path::parse(key).unwrap(), PutPayload::from(data.to_vec()))
            .await
            .unwrap();
    }

    async fn raw_exists(mem: &InMemory, key: &str) -> bool {
        mem.head(&Path::parse(key).unwrap()).await.is_ok()
    }

    async fn sorted_list(s: &S3Store, kind: ObjectKind) -> Vec<String> {
        let mut v: Vec<String> = s
            .list(kind)
            .await
            .unwrap()
            .iter()
            .map(|i| i.to_string())
            .collect();
        v.sort();
        v
    }

    fn config() -> S3Config {
        S3Config {
            endpoint: "https://s3.example.com".to_string(),
            region: "us-east-1".to_string(),
            bucket: "meu-bucket".to_string(),
            access_key_id: "AKIDEXEMPLO".to_string(),
            secret_access_key: Zeroizing::new("segredo-de-teste".to_string()),
            allow_http: false,
            consistent_list: false,
        }
    }

    // ---- contrato do BlobStore (InMemory) ----

    #[tokio::test]
    async fn put_then_get_returns_the_same_bytes_under_kind_slash_hex() {
        let (s, mem) = mem_store(false);
        let (id, bytes) = obj(ObjectKind::Blob, b"cifrado");
        s.put(&id, &bytes).await.unwrap();
        assert_eq!(s.get(&id).await, Ok(Some(bytes.clone())));
        // a chave no bucket é `blob/<64 hex>`, a mesma do disco
        assert!(raw_exists(&mem, &id.to_string()).await);
        assert!(id.to_string().starts_with("blob/"));
    }

    #[tokio::test]
    async fn put_is_idempotent() {
        let (s, _mem) = mem_store(false);
        let (id, bytes) = obj(ObjectKind::Manifest, b"manifest cifrado");
        s.put(&id, &bytes).await.unwrap();
        s.put(&id, &bytes).await.unwrap();
        assert_eq!(s.get(&id).await, Ok(Some(bytes)));
        assert_eq!(sorted_list(&s, ObjectKind::Manifest).await.len(), 1);
    }

    #[tokio::test]
    async fn put_with_bytes_that_do_not_match_the_id_is_refused_and_stores_nothing() {
        let (s, mem) = mem_store(false);
        let (id, _) = obj(ObjectKind::Blob, b"a");
        assert_eq!(s.put(&id, b"b").await, Err(StoreError::IdMismatch));
        assert!(!raw_exists(&mem, &id.to_string()).await);
    }

    #[tokio::test]
    async fn absent_object_is_none_and_false() {
        let (s, _mem) = mem_store(false);
        let (id, _) = obj(ObjectKind::Blob, b"nunca gravado");
        assert_eq!(s.get(&id).await, Ok(None));
        assert_eq!(s.has(&id).await, Ok(false));
    }

    #[tokio::test]
    async fn has_is_true_after_put() {
        let (s, _mem) = mem_store(false);
        let (id, bytes) = obj(ObjectKind::Blob, b"x");
        s.put(&id, &bytes).await.unwrap();
        assert_eq!(s.has(&id).await, Ok(true));
    }

    #[tokio::test]
    async fn empty_and_five_megabyte_objects_round_trip() {
        let (s, _mem) = mem_store(false);
        let big = vec![0xABu8; 5 * 1024 * 1024];
        for data in [Vec::new(), big] {
            let (id, bytes) = obj(ObjectKind::Blob, &data);
            s.put(&id, &bytes).await.unwrap();
            assert_eq!(s.get(&id).await, Ok(Some(data)));
        }
    }

    #[tokio::test]
    async fn the_same_bytes_as_blob_and_as_manifest_are_two_objects() {
        let (s, mem) = mem_store(false);
        let (blob, bytes) = obj(ObjectKind::Blob, b"mesmos bytes");
        let (manifest, _) = obj(ObjectKind::Manifest, b"mesmos bytes");
        s.put(&blob, &bytes).await.unwrap();
        s.put(&manifest, &bytes).await.unwrap();
        assert!(raw_exists(&mem, &blob.to_string()).await);
        assert!(raw_exists(&mem, &manifest.to_string()).await);
        assert_eq!(sorted_list(&s, ObjectKind::Blob).await, [blob.to_string()]);
        assert_eq!(
            sorted_list(&s, ObjectKind::Manifest).await,
            [manifest.to_string()]
        );
    }

    #[tokio::test]
    async fn put_over_the_size_cap_is_refused_before_the_network() {
        let (s, mem) = mem_store(false);
        let s = s.with_limits(10, 100);
        let (id, bytes) = obj(ObjectKind::Blob, b"12345678901"); // 11 bytes
        assert_eq!(
            s.put(&id, &bytes).await,
            Err(StoreError::Unavailable(io::ErrorKind::InvalidInput))
        );
        assert!(!raw_exists(&mem, &id.to_string()).await);
    }

    #[tokio::test]
    async fn get_of_an_object_over_the_cap_is_refused() {
        let (s, mem) = mem_store(false);
        let (id, bytes) = obj(ObjectKind::Blob, b"12345678901"); // 11 bytes
        raw_put(&mem, &id.to_string(), &bytes).await;
        let s = s.with_limits(10, 100);
        assert_eq!(
            s.get(&id).await,
            Err(StoreError::Unavailable(io::ErrorKind::InvalidData))
        );
    }

    // ---- list ----

    #[tokio::test]
    async fn list_returns_every_object_of_the_kind_past_one_thousand() {
        let (s, mem) = mem_store(false);
        let mut expected = Vec::new();
        for i in 0..1500 {
            let id = ObjectId::of(ObjectKind::Blob, format!("objeto {i}").as_bytes());
            raw_put(&mem, &id.to_string(), b"x").await;
            expected.push(id.to_string());
        }
        let (manifest, _) = obj(ObjectKind::Manifest, b"outro tipo");
        raw_put(&mem, &manifest.to_string(), b"x").await;
        expected.sort();
        assert_eq!(sorted_list(&s, ObjectKind::Blob).await, expected);
    }

    #[tokio::test]
    async fn list_ignores_names_that_are_not_kind_slash_lowercase_hex() {
        let (s, mem) = mem_store(false);
        let (good, _) = obj(ObjectKind::Blob, b"bom");
        let hex = good.to_string().split_once('/').unwrap().1.to_string();
        raw_put(&mem, &good.to_string(), b"x").await;
        raw_put(&mem, "blob/README.txt", b"x").await;
        raw_put(&mem, &format!("blob/{}", hex.to_uppercase()), b"x").await;
        raw_put(&mem, &format!("blob/{}", &hex[..63]), b"x").await;
        raw_put(&mem, &format!("blob/sub/{hex}"), b"x").await;
        raw_put(&mem, &format!("outro/{hex}"), b"x").await;
        assert_eq!(sorted_list(&s, ObjectKind::Blob).await, [good.to_string()]);
    }

    #[tokio::test]
    async fn list_over_the_id_cap_is_refused_and_at_the_cap_is_accepted() {
        let (s, mem) = mem_store(false);
        for i in 0..4 {
            let id = ObjectId::of(ObjectKind::Blob, format!("c{i}").as_bytes());
            raw_put(&mem, &id.to_string(), b"x").await;
        }
        let at_cap = s.clone().with_limits(1024, 4);
        assert_eq!(at_cap.list(ObjectKind::Blob).await.unwrap().len(), 4);
        let over = s.with_limits(1024, 3);
        assert_eq!(
            over.list(ObjectKind::Blob).await,
            Err(StoreError::Unavailable(io::ErrorKind::InvalidData))
        );
    }

    // ---- delete, capacidades e erros ----

    #[tokio::test]
    async fn delete_is_unsupported_and_leaves_the_object() {
        let (s, _mem) = mem_store(false);
        let (id, bytes) = obj(ObjectKind::Blob, b"fica");
        s.put(&id, &bytes).await.unwrap();
        assert_eq!(
            s.delete(&id).await,
            Err(StoreError::Unavailable(io::ErrorKind::Unsupported))
        );
        assert_eq!(s.has(&id).await, Ok(true));
    }

    #[test]
    fn consistent_list_follows_the_configuration() {
        let (off, _) = mem_store(false);
        let (on, _) = mem_store(true);
        assert!(!off.capabilities().consistent_list);
        assert!(on.capabilities().consistent_list);
    }

    #[test]
    fn errors_map_to_a_kind_and_never_carry_the_message() {
        let boxed = |m: &str| -> Box<dyn std::error::Error + Send + Sync> { m.to_string().into() };
        let table = [
            (
                object_store::Error::Unauthenticated {
                    path: "blob/x".to_string(),
                    source: boxed("AKIDVAZADO"),
                },
                io::ErrorKind::PermissionDenied,
            ),
            (
                object_store::Error::PermissionDenied {
                    path: "blob/x".to_string(),
                    source: boxed("AKIDVAZADO"),
                },
                io::ErrorKind::PermissionDenied,
            ),
            (
                object_store::Error::Generic {
                    store: "S3",
                    source: boxed("https://segredo.example.com/bucket"),
                },
                io::ErrorKind::Other,
            ),
            (
                object_store::Error::NotImplemented {
                    operation: "x".to_string(),
                    implementer: "y".to_string(),
                },
                io::ErrorKind::Unsupported,
            ),
        ];
        for (err, kind) in table {
            let mapped = map_error(&err);
            assert_eq!(mapped, StoreError::Unavailable(kind));
            let shown = format!("{mapped:?} {mapped}");
            assert!(!shown.contains("AKIDVAZADO") && !shown.contains("segredo.example.com"));
        }
    }

    // ---- configuração ----

    #[test]
    fn debug_of_config_and_store_shows_no_credential_endpoint_or_bucket() {
        let c = config();
        let shown = format!("{c:?}");
        for secret in [
            "segredo-de-teste",
            "AKIDEXEMPLO",
            "s3.example.com",
            "meu-bucket",
        ] {
            assert!(!shown.contains(secret), "{secret} vazou no Debug");
        }
        let store = S3Store::new(&c).unwrap();
        let shown = format!("{store:?}");
        assert!(!shown.contains("segredo-de-teste") && !shown.contains("AKIDEXEMPLO"));
    }

    #[test]
    fn new_accepts_https_and_refuses_plain_http_without_allow_http() {
        assert!(S3Store::new(&config()).is_ok());
        let mut http = config();
        http.endpoint = "http://10.0.0.5:3900".to_string();
        assert_eq!(S3Store::new(&http).err(), Some(S3Error::InsecureEndpoint));
        http.allow_http = true;
        assert!(S3Store::new(&http).is_ok());
    }

    #[test]
    fn new_refuses_empty_or_malformed_fields() {
        let mut c = config();
        c.bucket = String::new();
        assert_eq!(S3Store::new(&c).err(), Some(S3Error::InvalidConfig));
        let mut c = config();
        c.access_key_id = String::new();
        assert_eq!(S3Store::new(&c).err(), Some(S3Error::InvalidConfig));
        let mut c = config();
        c.secret_access_key = Zeroizing::new(String::new());
        assert_eq!(S3Store::new(&c).err(), Some(S3Error::InvalidConfig));
        let mut c = config();
        c.endpoint = "isto não é uma url".to_string();
        assert_eq!(S3Store::new(&c).err(), Some(S3Error::InvalidConfig));
        let mut c = config();
        c.endpoint = "ftp://s3.example.com".to_string();
        assert_eq!(S3Store::new(&c).err(), Some(S3Error::InvalidConfig));
    }

    // ---- integração com um S3 de verdade (Garage no CI) ----
    //
    // Só rodam com as variáveis `ARCA_S3_TEST_*` (o job `s3-garage` as define); sem elas, passam
    // sem fazer nada, para o `cargo test` comum não precisar de servidor. Os testes dividem um
    // bucket e rodam em paralelo: as asserções de `list` usam "contém", nunca igualdade.

    fn config_from_env() -> Option<S3Config> {
        let var = |k: &str| std::env::var(k).ok();
        Some(S3Config {
            endpoint: var("ARCA_S3_TEST_ENDPOINT")?,
            region: var("ARCA_S3_TEST_REGION").unwrap_or_else(|| "garage".to_string()),
            bucket: var("ARCA_S3_TEST_BUCKET")?,
            access_key_id: var("ARCA_S3_TEST_KEY")?,
            secret_access_key: Zeroizing::new(var("ARCA_S3_TEST_SECRET")?),
            allow_http: true,
            consistent_list: false,
        })
    }

    fn integration_config() -> Option<S3Config> {
        let config = config_from_env();
        // o job `s3-garage` exige o servidor: sem as variáveis o teste falha, não passa vazio
        assert!(
            config.is_some() || std::env::var_os("ARCA_S3_TEST_REQUIRED").is_none(),
            "ARCA_S3_TEST_REQUIRED está definida mas faltam variáveis ARCA_S3_TEST_*"
        );
        config
    }

    fn unique(label: &str) -> Vec<u8> {
        let mut random = [0u8; 16];
        getrandom::fill(&mut random).unwrap();
        let hex: String = random.iter().map(|b| format!("{b:02x}")).collect();
        format!("{label}-{hex}").into_bytes()
    }

    #[tokio::test]
    async fn real_s3_round_trip_and_absent_object() {
        let Some(cfg) = integration_config() else {
            return;
        };
        let s = S3Store::new(&cfg).unwrap();
        let data = unique("ida-e-volta");
        let (id, bytes) = obj(ObjectKind::Blob, &data);
        assert_eq!(s.get(&id).await, Ok(None));
        assert_eq!(s.has(&id).await, Ok(false));
        s.put(&id, &bytes).await.unwrap();
        s.put(&id, &bytes).await.unwrap(); // idempotente
        assert_eq!(s.has(&id).await, Ok(true));
        assert_eq!(s.get(&id).await, Ok(Some(bytes)));
        assert_eq!(
            s.delete(&id).await,
            Err(StoreError::Unavailable(io::ErrorKind::Unsupported))
        );
    }

    #[tokio::test]
    async fn real_s3_empty_and_five_megabyte_objects() {
        let Some(cfg) = integration_config() else {
            return;
        };
        let s = S3Store::new(&cfg).unwrap();
        let mut big = unique("grande");
        big.resize(5 * 1024 * 1024, 0x5A);
        for data in [Vec::new(), big] {
            let (id, bytes) = obj(ObjectKind::Blob, &data);
            s.put(&id, &bytes).await.unwrap();
            assert_eq!(s.get(&id).await, Ok(Some(data)));
        }
    }

    #[tokio::test]
    async fn real_s3_list_paginates_past_one_thousand_objects() {
        let Some(cfg) = integration_config() else {
            return;
        };
        let s = S3Store::new(&cfg).unwrap();
        let tag = String::from_utf8(unique("lista")).unwrap();
        let mut mine = Vec::new();
        for i in 0..1100 {
            let (id, bytes) = obj(ObjectKind::Manifest, format!("{tag}-{i}").as_bytes());
            s.put(&id, &bytes).await.unwrap();
            mine.push(id);
        }
        let listed = s.list(ObjectKind::Manifest).await.unwrap();
        assert!(listed.len() >= 1100, "só {} ids listados", listed.len());
        for id in &mine {
            assert!(listed.contains(id), "faltou {id} na listagem");
        }
        assert!(sorted_list(&s, ObjectKind::Blob)
            .await
            .iter()
            .all(|n| n.starts_with("blob/")));
    }

    #[tokio::test]
    async fn real_s3_wrong_credential_is_unavailable_and_does_not_leak_the_key() {
        let Some(mut cfg) = integration_config() else {
            return;
        };
        cfg.secret_access_key = Zeroizing::new("chave-errada-0123456789".to_string());
        let s = S3Store::new(&cfg).unwrap();
        let (id, bytes) = obj(ObjectKind::Blob, &unique("credencial"));
        let err = s.put(&id, &bytes).await.unwrap_err();
        assert!(matches!(err, StoreError::Unavailable(_)));
        let shown = format!("{err:?} {err}");
        assert!(!shown.contains("chave-errada") && !shown.contains(&cfg.access_key_id));
    }

    #[tokio::test]
    async fn real_s3_server_down_is_unavailable_and_does_not_leak_the_key() {
        let Some(mut cfg) = integration_config() else {
            return;
        };
        cfg.endpoint = "http://127.0.0.1:1".to_string(); // nada escuta na porta 1
        let s = S3Store::new(&cfg).unwrap();
        let (id, _) = obj(ObjectKind::Blob, b"x");
        let err = s.get(&id).await.unwrap_err();
        assert!(matches!(err, StoreError::Unavailable(_)));
        let shown = format!("{err:?} {err}");
        assert!(!shown.contains(&cfg.access_key_id) && !shown.contains("127.0.0.1"));
    }
}
