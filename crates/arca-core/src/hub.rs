//! Cliente do hub (`warden-server`) como réplica: `HubStore` implementa o `BlobStore` sobre o
//! contrato HTTP `/arca/v1/` (`SPEC-TECNICA.md` §4.2, 2b-1). O hub não é confiável para
//! integridade (tudo que se lê passa por `verified_get`) nem enxerga texto claro.
//!
//! O token do dispositivo vai em `Authorization: Bearer`. Nunca aparece em `Debug`, erro ou log.
//! Redirecionamento não é seguido (o token iria para outro host), e `http` só vale em loopback.

use crate::object::{ObjectId, ObjectKind};
use crate::store::{BlobStore, Capabilities, StoreError};
use std::io;
use std::time::Duration;
use zeroize::Zeroizing;

/// Por que um `HubStore` não pôde ser criado. Nunca carrega a URL nem o token.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum HubError {
    /// A URL não é um endereço `http(s)` de hub (sem credenciais, consulta nem caminho).
    #[error("invalid hub url")]
    InvalidUrl,
    /// `http` fora de loopback: o token do dispositivo andaria em claro.
    #[error("plain http is only allowed for loopback hubs")]
    InsecureUrl,
    /// Token vazio ou com caracteres que não cabem num cabeçalho.
    #[error("invalid device token")]
    InvalidToken,
    /// O cliente HTTP não pôde ser montado.
    #[error("http client could not be built")]
    Client,
}

/// Teto de um blob no hub: o maior arquivo da arca mais o envelope e a margem.
pub const MAX_BLOB_OBJECT_BYTES: u64 = crate::scan::MAX_FILE_BYTES + 1024;
/// Teto de um manifest no hub.
pub const MAX_MANIFEST_OBJECT_BYTES: u64 = crate::manifest::MAX_MANIFEST_JSON_BYTES as u64 + 1024;
/// Ids que uma listagem inteira pode somar antes de o cliente desistir.
pub const MAX_LIST_IDS: usize = 1_000_000;

/// Ids por página pedidos ao hub.
const PAGE_LIMIT: usize = 1000;
/// Uma página de ids tem uns 70 KB; acima disto o hub está mentindo.
const MAX_LIST_PAGE_BYTES: u64 = 1 << 20;
/// Páginas que uma listagem pode somar (cada uma com `PAGE_LIMIT` ids no máximo).
const MAX_LIST_PAGES: usize = 10_000;

/// O hub como um `BlobStore`.
#[derive(Clone)]
pub struct HubStore {
    client: reqwest::Client,
    /// URL base sem a barra final (`https://hub.example.com:8443`).
    base: String,
    /// O token zera ao sair da memória, mas o `reqwest` monta uma cópia (`String` e
    /// `HeaderValue`) por requisição, sem zerar: o token é revogável, esse resíduo é aceito.
    token: Zeroizing<String>,
    /// Teto de bytes por objeto; `None` = o padrão do tipo (`MAX_*_OBJECT_BYTES`).
    max_object_bytes: Option<u64>,
    max_list_ids: usize,
    timing: Timing,
}

/// Prazo total de uma operação: `base` mais o tempo de mover os bytes a `min_bytes_per_sec`.
/// Quem entrega mais devagar que isso (um hub em gotas) é cortado; quem entrega mais rápido
/// nunca chega perto do prazo.
#[derive(Clone, Copy)]
struct Timing {
    base: Duration,
    min_bytes_per_sec: u64,
}

const DEFAULT_TIMING: Timing = Timing {
    base: Duration::from_secs(30),
    min_bytes_per_sec: 256 * 1024,
};

impl Timing {
    fn budget(self, bytes: u64) -> Duration {
        self.base + Duration::from_secs_f64(bytes as f64 / self.min_bytes_per_sec as f64)
    }
}

// Sem URL nem token: nenhum dos dois entra em log por acidente.
impl std::fmt::Debug for HubStore {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("HubStore").finish_non_exhaustive()
    }
}

impl HubStore {
    /// `https` em qualquer host, ou `http` só em loopback (`127.0.0.1`, `::1`, `localhost`).
    pub fn new(base_url: &str, token: &str) -> Result<HubStore, HubError> {
        HubStore::build(base_url, token, false)
    }

    /// Como `new`, mas aceita `http` em qualquer host (uma LAN sem TLS). O token anda em claro:
    /// o nome é de propósito.
    pub fn allow_insecure_http(base_url: &str, token: &str) -> Result<HubStore, HubError> {
        HubStore::build(base_url, token, true)
    }

    fn build(base_url: &str, token: &str, insecure_ok: bool) -> Result<HubStore, HubError> {
        let url = reqwest::Url::parse(base_url).map_err(|_| HubError::InvalidUrl)?;
        let plain = match url.scheme() {
            "https" => false,
            "http" => true,
            _ => return Err(HubError::InvalidUrl),
        };
        // só `esquema://host[:porta]/`: credenciais, caminho, consulta e fragmento não têm lugar
        if url.host().is_none()
            || !url.username().is_empty()
            || url.password().is_some()
            || url.path() != "/"
            || url.query().is_some()
            || url.fragment().is_some()
        {
            return Err(HubError::InvalidUrl);
        }
        if plain && !insecure_ok && !is_loopback(&url) {
            return Err(HubError::InsecureUrl);
        }
        // o token vira valor de cabeçalho: ASCII visível, sem espaço nem quebra de linha
        if token.is_empty() || !token.bytes().all(|b| (0x21..=0x7e).contains(&b)) {
            return Err(HubError::InvalidToken);
        }
        let client = reqwest::Client::builder()
            // o token iria para o host do redirecionamento
            .redirect(reqwest::redirect::Policy::none())
            // e para o proxy do ambiente (`HTTP_PROXY`, `ALL_PROXY`), que o `reqwest` honra mesmo
            // sem a feature de proxy do sistema: o token em `http` andaria até ele
            .no_proxy()
            .connect_timeout(Duration::from_secs(10))
            // sem `read_timeout`: ele reinicia a cada pedaço (uma resposta em gotas nunca
            // estoura) e inclui o envio do corpo. O prazo é por operação (`within`).
            .user_agent(concat!("arca-core/", env!("CARGO_PKG_VERSION")))
            .build()
            .map_err(|_| HubError::Client)?;
        Ok(HubStore {
            client,
            base: url.as_str().trim_end_matches('/').to_string(),
            token: Zeroizing::new(token.to_string()),
            max_object_bytes: None,
            max_list_ids: MAX_LIST_IDS,
            timing: DEFAULT_TIMING,
        })
    }

    /// Só os testes encurtam os prazos.
    #[cfg(test)]
    pub(crate) fn with_timing(mut self, base: Duration, min_bytes_per_sec: u64) -> HubStore {
        self.timing = Timing {
            base,
            min_bytes_per_sec,
        };
        self
    }

    /// Roda `work` com o prazo total de `bytes` bytes. Estourou: `TimedOut`.
    async fn within<T>(
        &self,
        bytes: u64,
        work: impl std::future::Future<Output = Result<T, StoreError>>,
    ) -> Result<T, StoreError> {
        tokio::time::timeout(self.timing.budget(bytes), work)
            .await
            .unwrap_or_else(|_| Err(unavailable(io::ErrorKind::TimedOut)))
    }

    /// Manda a requisição e espera o cabeçalho da resposta. `upload_bytes` é o tamanho do corpo
    /// enviado (0 se não há): o prazo cobre o envio inteiro, a `min_bytes_per_sec`.
    async fn send(
        &self,
        request: reqwest::RequestBuilder,
        upload_bytes: u64,
    ) -> Result<reqwest::Response, StoreError> {
        self.within(upload_bytes, async {
            request.send().await.map_err(|e| transport_error(&e))
        })
        .await
    }

    /// Tetos de bytes por objeto e de ids por listagem (os padrões são as constantes acima).
    /// Só os testes baixam os tetos; quem usa a crate fica com os padrões.
    #[cfg(test)]
    pub(crate) fn with_limits(mut self, max_object_bytes: u64, max_list_ids: usize) -> HubStore {
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

    fn object_url(&self, id: &ObjectId) -> String {
        // `ObjectId` em texto é `blob/<hex>` ou `manifest/<hex>`
        format!("{}/arca/v1/objects/{id}", self.base)
    }

    fn request(&self, method: reqwest::Method, url: String) -> reqwest::RequestBuilder {
        self.client
            .request(method, url)
            .bearer_auth(self.token.as_str())
    }
}

fn is_loopback(url: &reqwest::Url) -> bool {
    let Some(host) = url.host_str() else {
        return false;
    };
    // IPv6 vem entre colchetes (`[::1]`); `127.0.0.1.evil.com` não é IP, é nome
    let bare = host.trim_start_matches('[').trim_end_matches(']');
    match bare.parse::<std::net::IpAddr>() {
        Ok(ip) => ip.is_loopback(),
        Err(_) => host == "localhost",
    }
}

fn unavailable(kind: io::ErrorKind) -> StoreError {
    StoreError::Unavailable(kind)
}

/// Falha de transporte, sem a URL nem o texto do erro (que pode trazê-la).
fn transport_error(e: &reqwest::Error) -> StoreError {
    unavailable(if e.is_timeout() {
        io::ErrorKind::TimedOut
    } else if e.is_connect() && is_tls_failure(e) {
        // certificado inválido, emissor desconhecido, handshake quebrado: não é "hub fora do ar"
        // (e a interface não deve empurrar a pessoa para `allow_insecure_http`)
        io::ErrorKind::ConnectionAborted
    } else if e.is_connect() {
        io::ErrorKind::ConnectionRefused
    } else {
        io::ErrorKind::Other
    })
}

/// A cadeia de causas do erro de conexão passa por TLS? Só se olha o texto das causas para
/// classificar; ele não é devolvido a ninguém (pode trazer a URL).
fn is_tls_failure(e: &reqwest::Error) -> bool {
    let mut cause: Option<&(dyn std::error::Error + 'static)> = std::error::Error::source(e);
    while let Some(err) = cause {
        let text = format!("{err:?}").to_ascii_lowercase();
        if ["tls", "certificate", "handshake", "invalidmessage"]
            .iter()
            .any(|word| text.contains(word))
        {
            return true;
        }
        cause = err.source();
    }
    false
}

/// Estado HTTP -> erro. `Ok(true)` = 2xx, `Ok(false)` = 404.
fn check_status(status: reqwest::StatusCode) -> Result<bool, StoreError> {
    use reqwest::StatusCode;
    match status {
        s if s.is_success() => Ok(true),
        StatusCode::NOT_FOUND => Ok(false),
        // sem token, token errado, dispositivo pendente ou revogado
        StatusCode::UNAUTHORIZED | StatusCode::FORBIDDEN => {
            Err(unavailable(io::ErrorKind::PermissionDenied))
        }
        // o hub recusou o objeto (hash ou envelope)
        StatusCode::BAD_REQUEST => Err(unavailable(io::ErrorKind::InvalidData)),
        StatusCode::PAYLOAD_TOO_LARGE => Err(unavailable(io::ErrorKind::InvalidInput)),
        // 3xx (nunca seguido), 5xx e o resto
        _ => Err(unavailable(io::ErrorKind::Other)),
    }
}

/// Lê o corpo até `cap` bytes; acima disso é erro, sem terminar de ler (memória).
async fn read_capped(mut response: reqwest::Response, cap: u64) -> Result<Vec<u8>, StoreError> {
    if response.content_length().is_some_and(|n| n > cap) {
        return Err(unavailable(io::ErrorKind::InvalidData));
    }
    let mut body = Vec::new();
    while let Some(chunk) = response.chunk().await.map_err(|e| transport_error(&e))? {
        if (body.len() + chunk.len()) as u64 > cap {
            return Err(unavailable(io::ErrorKind::InvalidData));
        }
        body.extend_from_slice(&chunk);
    }
    Ok(body)
}

#[derive(serde::Deserialize)]
struct ListPage {
    ids: Vec<String>,
    next: Option<String>,
}

impl BlobStore for HubStore {
    async fn put(&self, id: &ObjectId, bytes: &[u8]) -> Result<(), StoreError> {
        if ObjectId::of(id.kind, bytes).hash != id.hash {
            return Err(StoreError::IdMismatch);
        }
        if bytes.len() as u64 > self.cap(id.kind) {
            return Err(unavailable(io::ErrorKind::InvalidInput));
        }
        let request = self
            .request(reqwest::Method::PUT, self.object_url(id))
            .body(bytes.to_vec());
        let response = self.send(request, bytes.len() as u64).await?;
        check_status(response.status())?;
        Ok(())
    }

    async fn get(&self, id: &ObjectId) -> Result<Option<Vec<u8>>, StoreError> {
        let request = self.request(reqwest::Method::GET, self.object_url(id));
        let response = self.send(request, 0).await?;
        if !check_status(response.status())? {
            return Ok(None);
        }
        // o prazo da leitura vem do tamanho declarado (ou do teto, se não há `Content-Length`)
        let cap = self.cap(id.kind);
        let expected = response.content_length().unwrap_or(cap).min(cap);
        self.within(expected, read_capped(response, cap))
            .await
            .map(Some)
    }

    async fn has(&self, id: &ObjectId) -> Result<bool, StoreError> {
        let request = self.request(reqwest::Method::HEAD, self.object_url(id));
        let response = self.send(request, 0).await?;
        check_status(response.status())
    }

    async fn list(&self, kind: ObjectKind) -> Result<Vec<ObjectId>, StoreError> {
        let invalid = || unavailable(io::ErrorKind::InvalidData);
        let mut out: Vec<ObjectId> = Vec::new();
        let mut after: Option<String> = None;
        for _ in 0..MAX_LIST_PAGES {
            let mut url = format!(
                "{}/arca/v1/objects?kind={}&limit={PAGE_LIMIT}",
                self.base,
                kind.as_str()
            );
            if let Some(after) = &after {
                url.push_str(&format!("&after={after}"));
            }
            let response = self
                .send(self.request(reqwest::Method::GET, url), 0)
                .await?;
            check_status(response.status())?;
            let body = self
                .within(
                    MAX_LIST_PAGE_BYTES,
                    read_capped(response, MAX_LIST_PAGE_BYTES),
                )
                .await?;
            let page: ListPage = serde_json::from_slice(&body).map_err(|_| invalid())?;

            // o hub é entrada não confiável: ids em hex, estritamente crescentes e depois de
            // `after`; `next` tem de avançar (senão o laço nunca acabaria)
            let mut last = after.clone();
            for hex in &page.ids {
                let id: ObjectId = format!("{}/{hex}", kind.as_str())
                    .parse()
                    .map_err(|_| invalid())?;
                if last.as_ref().is_some_and(|l| hex <= l) {
                    return Err(invalid());
                }
                last = Some(hex.clone());
                out.push(id);
                if out.len() > self.max_list_ids {
                    return Err(invalid());
                }
            }
            let Some(next) = page.next else {
                return Ok(out);
            };
            // uma página vazia com `next` não avança nada: só serviria para fazer o cliente
            // pedir 10 000 páginas. Com ids, `next` não pode ficar antes do último (e o último
            // já é depois de `after`, então `next` sempre avança).
            let next_ok = format!("{}/{next}", kind.as_str())
                .parse::<ObjectId>()
                .is_ok()
                && !page.ids.is_empty()
                && last.as_ref().is_none_or(|l| next >= *l);
            if !next_ok {
                return Err(invalid());
            }
            after = Some(next);
        }
        Err(invalid())
    }

    /// A v1 não apaga (ADR 0011): sempre `Unavailable(Unsupported)`, sem ir à rede.
    async fn delete(&self, _id: &ObjectId) -> Result<(), StoreError> {
        Err(unavailable(io::ErrorKind::Unsupported))
    }

    /// `consistent_list` descreve o contrato (o hub lista tudo que aceitou), não uma prova: um
    /// hub hostil pode mentir em `put`, `has` e `list`. Presença no hub é **alegada**; a
    /// integridade do que se lê é provada por `verified_get`.
    fn capabilities(&self) -> Capabilities {
        Capabilities {
            consistent_list: true,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::backend::Backend;
    use crate::envelope::{Alg, Header, NONCE_LEN};
    use crate::hub_testserver::{Access, ListMode, TestHub};
    use crate::replicate::replica_status;
    use crate::store::verified_get;
    use crate::FORMAT_VERSION;
    use std::io;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;
    use std::time::Instant;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    const TOKEN: &str = "token-do-dispositivo-A";

    /// Objeto com envelope válido do tipo dado (o corpo é opaco).
    fn obj(kind: ObjectKind, content: &[u8]) -> (ObjectId, Vec<u8>) {
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

    async fn setup() -> (TestHub, HubStore) {
        let hub = TestHub::start().await;
        hub.approve(TOKEN);
        let store = HubStore::new(&hub.url, TOKEN).unwrap();
        (hub, store)
    }

    // ---------- ida e volta ----------

    #[tokio::test]
    async fn put_get_has_list_round_trip_and_keep_the_kinds_apart() {
        let (hub, store) = setup().await;
        let blob = obj(ObjectKind::Blob, b"conteudo");
        let manifest = obj(ObjectKind::Manifest, b"estado");
        assert_eq!(store.has(&blob.0).await, Ok(false));
        assert_eq!(store.get(&blob.0).await, Ok(None));
        store.put(&blob.0, &blob.1).await.unwrap();
        store.put(&manifest.0, &manifest.1).await.unwrap();

        assert_eq!(store.has(&blob.0).await, Ok(true));
        assert_eq!(store.get(&blob.0).await, Ok(Some(blob.1.clone())));
        assert_eq!(
            verified_get(&store, &manifest.0).await,
            Ok(Some(manifest.1))
        );
        assert_eq!(store.list(ObjectKind::Blob).await, Ok(vec![blob.0]));
        assert_eq!(store.list(ObjectKind::Manifest).await, Ok(vec![manifest.0]));
        assert_eq!(hub.object_count(), 2);
    }

    #[tokio::test]
    async fn put_is_idempotent() {
        let (hub, store) = setup().await;
        let blob = obj(ObjectKind::Blob, b"x");
        store.put(&blob.0, &blob.1).await.unwrap();
        store.put(&blob.0, &blob.1).await.unwrap();
        assert_eq!(hub.object_count(), 1);
    }

    #[tokio::test]
    async fn list_follows_every_page() {
        let (hub, store) = setup().await;
        hub.state.lock().unwrap().page_max = Some(7);
        let mut expected = Vec::new();
        for i in 0..60u8 {
            let (id, bytes) = obj(ObjectKind::Blob, &[i]);
            store.put(&id, &bytes).await.unwrap();
            expected.push(id);
        }
        let mut got = store.list(ObjectKind::Blob).await.unwrap();
        got.sort_by_key(|id| id.hash);
        expected.sort_by_key(|id| id.hash);
        assert_eq!(got, expected);
        assert!(
            hub.seen()
                .iter()
                .filter(|r| r.path == "/arca/v1/objects")
                .count()
                >= 9
        );
    }

    // ---------- integridade: o hub não é confiável ----------

    #[tokio::test]
    async fn bytes_altered_by_the_hub_are_caught_by_verified_get() {
        let (hub, store) = setup().await;
        let blob = obj(ObjectKind::Blob, b"original");
        store.put(&blob.0, &blob.1).await.unwrap();
        hub.state.lock().unwrap().lie_on_get = true;
        assert_eq!(
            verified_get(&store, &blob.0).await,
            Err(StoreError::Corrupt)
        );
    }

    #[tokio::test]
    async fn a_put_whose_bytes_do_not_match_the_id_never_reaches_the_network() {
        let (hub, store) = setup().await;
        let (id, _) = obj(ObjectKind::Blob, b"a");
        let (_, other) = obj(ObjectKind::Blob, b"b");
        assert_eq!(store.put(&id, &other).await, Err(StoreError::IdMismatch));
        assert!(hub.seen().is_empty());
    }

    #[tokio::test]
    async fn the_hub_refuses_junk_and_an_envelope_of_the_wrong_kind() {
        let (hub, store) = setup().await;
        let junk_bytes = b"lixo sem envelope".to_vec();
        let junk = (ObjectId::of(ObjectKind::Manifest, &junk_bytes), junk_bytes);
        let (_, blob_bytes) = obj(ObjectKind::Blob, b"b");
        let wrong_kind = (ObjectId::of(ObjectKind::Manifest, &blob_bytes), blob_bytes);
        for (id, bytes) in [junk, wrong_kind] {
            assert_eq!(
                store.put(&id, &bytes).await,
                Err(StoreError::Unavailable(io::ErrorKind::InvalidData))
            );
        }
        assert_eq!(hub.object_count(), 0);
    }

    #[tokio::test]
    async fn each_operation_uses_the_http_method_of_the_contract() {
        let (hub, store) = setup().await;
        let blob = obj(ObjectKind::Blob, b"x");
        store.put(&blob.0, &blob.1).await.unwrap();
        store.has(&blob.0).await.unwrap();
        store.get(&blob.0).await.unwrap();
        store.list(ObjectKind::Blob).await.unwrap();
        let used: Vec<(String, String)> =
            hub.seen().into_iter().map(|r| (r.method, r.path)).collect();
        let object = format!("/arca/v1/objects/{}", blob.0);
        assert_eq!(
            used,
            vec![
                ("PUT".to_string(), object.clone()),
                ("HEAD".to_string(), object.clone()),
                ("GET".to_string(), object),
                ("GET".to_string(), "/arca/v1/objects".to_string()),
            ]
        );
    }

    // ---------- autenticação ----------

    #[tokio::test]
    async fn an_unknown_pending_or_revoked_token_is_permission_denied_on_every_operation() {
        let hub = TestHub::start().await;
        hub.set_access("pendente", Access::Pending);
        hub.set_access("revogado", Access::Revoked);
        let blob = obj(ObjectKind::Blob, b"x");
        for token in ["desconhecido", "pendente", "revogado"] {
            let store = HubStore::new(&hub.url, token).unwrap();
            let denied = StoreError::Unavailable(io::ErrorKind::PermissionDenied);
            assert_eq!(
                store.put(&blob.0, &blob.1).await,
                Err(denied.clone()),
                "{token}"
            );
            assert_eq!(store.get(&blob.0).await, Err(denied.clone()), "{token}");
            assert_eq!(store.has(&blob.0).await, Err(denied.clone()), "{token}");
            assert_eq!(store.list(ObjectKind::Blob).await, Err(denied), "{token}");
        }
        assert_eq!(hub.object_count(), 0);
    }

    #[tokio::test]
    async fn a_device_revoked_in_the_middle_loses_access_at_once() {
        let (hub, store) = setup().await;
        let blob = obj(ObjectKind::Blob, b"x");
        store.put(&blob.0, &blob.1).await.unwrap();
        hub.set_access(TOKEN, Access::Revoked);
        assert_eq!(
            store.get(&blob.0).await,
            Err(StoreError::Unavailable(io::ErrorKind::PermissionDenied))
        );
    }

    #[tokio::test]
    async fn the_token_goes_in_the_bearer_header_and_nowhere_else() {
        let (hub, store) = setup().await;
        let blob = obj(ObjectKind::Blob, b"x");
        store.put(&blob.0, &blob.1).await.unwrap();
        store.list(ObjectKind::Blob).await.unwrap();
        let seen = hub.seen();
        assert!(!seen.is_empty());
        for request in seen {
            assert_eq!(
                request.authorization.as_deref(),
                Some(&*format!("Bearer {TOKEN}"))
            );
            assert!(!request.path.contains(TOKEN));
        }
    }

    // ---------- rede ----------

    #[tokio::test]
    async fn a_hub_that_is_down_or_failing_is_unavailable() {
        // porta fechada: abre, anota a porta e fecha
        let closed = {
            let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            l.local_addr().unwrap()
        };
        let store = HubStore::new(&format!("http://{closed}"), TOKEN).unwrap();
        let id = obj(ObjectKind::Blob, b"x").0;
        assert_eq!(
            store.has(&id).await,
            Err(StoreError::Unavailable(io::ErrorKind::ConnectionRefused))
        );

        let (hub, store) = setup().await;
        hub.state.lock().unwrap().force_status = Some(503);
        assert!(matches!(
            store.get(&id).await,
            Err(StoreError::Unavailable(_))
        ));
        assert!(matches!(
            store.list(ObjectKind::Blob).await,
            Err(StoreError::Unavailable(_))
        ));
    }

    #[tokio::test]
    async fn a_redirect_is_never_followed_and_the_token_is_not_sent_to_the_target() {
        let target = TestHub::start().await;
        target.approve(TOKEN);
        let (hub, store) = setup().await;
        hub.state.lock().unwrap().redirect_to = Some(target.url.clone());
        let id = obj(ObjectKind::Blob, b"x").0;
        assert!(matches!(
            store.get(&id).await,
            Err(StoreError::Unavailable(_))
        ));
        assert!(matches!(
            store.has(&id).await,
            Err(StoreError::Unavailable(_))
        ));
        // o envio (que carrega o corpo) e a listagem também não seguem o redirecionamento
        let blob = obj(ObjectKind::Blob, b"x");
        assert!(matches!(
            store.put(&blob.0, &blob.1).await,
            Err(StoreError::Unavailable(_))
        ));
        assert!(matches!(
            store.list(ObjectKind::Blob).await,
            Err(StoreError::Unavailable(_))
        ));
        assert!(target.seen().is_empty(), "o token chegou ao segundo host");
    }

    // ---------- tetos ----------

    #[tokio::test]
    async fn a_response_above_the_cap_is_refused() {
        let (hub, store) = setup().await;
        let big = obj(ObjectKind::Blob, &vec![9u8; 10 * 1024]);
        store.put(&big.0, &big.1).await.unwrap();
        let small_cap = store.clone().with_limits(1024, MAX_LIST_IDS);
        assert_eq!(
            small_cap.get(&big.0).await,
            Err(StoreError::Unavailable(io::ErrorKind::InvalidData))
        );
        drop(hub);
    }

    #[tokio::test]
    async fn a_chunked_response_with_no_length_is_cut_at_the_cap() {
        // servidor cru: responde em pedaços, sem `Content-Length`, bem acima do teto
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut request = [0u8; 2048];
            let _ = socket.read(&mut request).await;
            let _ = socket
                .write_all(b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n")
                .await;
            for _ in 0..64 {
                let chunk = [b"400\r\n".as_slice(), &[7u8; 1024], b"\r\n"].concat();
                if socket.write_all(&chunk).await.is_err() {
                    return;
                }
            }
            let _ = socket.write_all(b"0\r\n\r\n").await;
        });
        let store = HubStore::new(&url, TOKEN)
            .unwrap()
            .with_limits(4096, MAX_LIST_IDS);
        let id = obj(ObjectKind::Blob, b"x").0;
        assert_eq!(
            store.get(&id).await,
            Err(StoreError::Unavailable(io::ErrorKind::InvalidData))
        );
    }

    #[tokio::test]
    async fn a_put_above_the_cap_is_refused_locally() {
        let (hub, store) = setup().await;
        let store = store.with_limits(1024, MAX_LIST_IDS);
        let big = obj(ObjectKind::Blob, &vec![1u8; 4096]);
        assert_eq!(
            store.put(&big.0, &big.1).await,
            Err(StoreError::Unavailable(io::ErrorKind::InvalidInput))
        );
        assert!(hub.seen().is_empty());
    }

    // ---------- listagem hostil ----------

    #[tokio::test]
    async fn a_hostile_listing_is_an_error_not_a_loop_or_a_bad_id() {
        for mode in [ListMode::NotHex, ListMode::Unsorted, ListMode::StuckNext] {
            let (hub, store) = setup().await;
            for i in 0..5u8 {
                let (id, bytes) = obj(ObjectKind::Blob, &[i]);
                store.put(&id, &bytes).await.unwrap();
            }
            hub.state.lock().unwrap().list_mode = mode;
            assert_eq!(
                store.list(ObjectKind::Blob).await,
                Err(StoreError::Unavailable(io::ErrorKind::InvalidData)),
                "{mode:?}"
            );
        }
    }

    #[tokio::test]
    async fn an_endless_listing_stops_at_the_id_cap() {
        let (hub, store) = setup().await;
        {
            let mut state = hub.state.lock().unwrap();
            state.list_mode = ListMode::Endless;
            state.page_max = Some(10);
        }
        let store = store.with_limits(MAX_BLOB_OBJECT_BYTES, 50);
        assert_eq!(
            store.list(ObjectKind::Blob).await,
            Err(StoreError::Unavailable(io::ErrorKind::InvalidData))
        );
        // cortou no meio de várias páginas, e não na primeira
        let pages = hub
            .seen()
            .iter()
            .filter(|r| r.path == "/arca/v1/objects")
            .count();
        assert!((5..=7).contains(&pages), "{pages} páginas");
    }

    // ---------- auditoria da 2b-1: servidores crus ----------

    /// Lê o pedido inteiro (cabeçalho e o corpo declarado) e o descarta.
    async fn read_request(socket: &mut tokio::net::TcpStream) {
        let mut head = Vec::new();
        let mut byte = [0u8; 1];
        while !head.ends_with(b"\r\n\r\n") {
            if socket.read(&mut byte).await.unwrap_or(0) == 0 {
                return;
            }
            head.push(byte[0]);
        }
        let text = String::from_utf8_lossy(&head).to_ascii_lowercase();
        let length = text
            .lines()
            .find_map(|l| l.strip_prefix("content-length:"))
            .and_then(|v| v.trim().parse::<usize>().ok())
            .unwrap_or(0);
        let mut sink = vec![0u8; length.clamp(1, 64 * 1024)];
        let mut left = length;
        while left > 0 {
            let want = left.min(sink.len());
            match socket.read(&mut sink[..want]).await {
                Ok(0) | Err(_) => return,
                Ok(n) => left -= n,
            }
        }
    }

    /// Servidor cru: a cada conexão lê o pedido, espera `delay` e devolve `response`. Devolve a
    /// URL e quantos pedidos recebeu.
    async fn raw_server(delay: Duration, response: Vec<u8>) -> (String, Arc<AtomicUsize>) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let count = Arc::new(AtomicUsize::new(0));
        let counter = count.clone();
        tokio::spawn(async move {
            while let Ok((mut socket, _)) = listener.accept().await {
                let (response, counter) = (response.clone(), counter.clone());
                tokio::spawn(async move {
                    read_request(&mut socket).await;
                    counter.fetch_add(1, Ordering::SeqCst);
                    tokio::time::sleep(delay).await;
                    let _ = socket.write_all(&response).await;
                    let _ = socket.shutdown().await;
                });
            }
        });
        (url, count)
    }

    fn json_response(body: &str) -> Vec<u8> {
        format!(
            "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len()
        )
        .into_bytes()
    }

    /// M1: o `reqwest` lê `HTTP_PROXY` e `ALL_PROXY` do ambiente mesmo sem a feature de proxy do
    /// sistema; o token em `http` iria parar no proxy. O `HubStore` ignora o ambiente.
    #[tokio::test]
    async fn the_environment_proxy_is_ignored_and_the_token_never_reaches_it() {
        let proxy = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let proxy_url = format!("http://{}", proxy.local_addr().unwrap());
        let (hub, _) = setup().await;
        let names = ["HTTP_PROXY", "http_proxy", "ALL_PROXY", "all_proxy"];
        for name in names {
            std::env::set_var(name, &proxy_url);
        }
        // o cliente lê o ambiente ao ser montado
        let store = HubStore::new(&hub.url, TOKEN).unwrap();
        for name in names {
            std::env::remove_var(name);
        }
        let id = obj(ObjectKind::Blob, b"x").0;
        assert_eq!(store.has(&id).await, Ok(false));
        assert_eq!(hub.seen().len(), 1, "o pedido foi direto ao hub");
        let reached = tokio::time::timeout(Duration::from_millis(200), proxy.accept()).await;
        assert!(reached.is_err(), "alguém falou com o proxy");
    }

    /// M2: o prazo vale para a operação inteira. Uma resposta em gotas (um byte por vez, com
    /// `Content-Length` válido) não pode prender a chamada para sempre.
    #[tokio::test]
    async fn a_response_that_drips_is_cut_by_the_total_deadline() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        tokio::spawn(async move {
            while let Ok((mut socket, _)) = listener.accept().await {
                tokio::spawn(async move {
                    read_request(&mut socket).await;
                    let head = b"HTTP/1.1 200 OK\r\nContent-Length: 20\r\n\r\n";
                    let _ = socket.write_all(head).await;
                    for _ in 0..20 {
                        tokio::time::sleep(Duration::from_millis(300)).await;
                        if socket.write_all(b"x").await.is_err() {
                            return;
                        }
                    }
                });
            }
        });
        // prazo = 300 ms + 20 bytes a 1000 B/s = 320 ms; a resposta levaria 6 s
        let store = HubStore::new(&url, TOKEN)
            .unwrap()
            .with_timing(Duration::from_millis(300), 1000);
        let id = obj(ObjectKind::Blob, b"x").0;
        let started = Instant::now();
        assert_eq!(
            store.get(&id).await,
            Err(StoreError::Unavailable(io::ErrorKind::TimedOut))
        );
        assert!(started.elapsed() < Duration::from_secs(3));
    }

    /// M3: o prazo de um envio cresce com o tamanho do corpo (a uma vazão mínima), em vez de ser
    /// um teto fixo que um blob grande numa linha lenta nunca cumpriria.
    #[tokio::test]
    async fn the_upload_deadline_grows_with_the_size_of_the_body() {
        let reply = b"HTTP/1.1 204 No Content\r\nConnection: close\r\n\r\n".to_vec();
        let (url, _) = raw_server(Duration::from_millis(1500), reply).await;
        // 3 MB a 1 MB/s dão 3 s de folga sobre os 200 ms de base; o hub demora 1,5 s
        let store = HubStore::new(&url, TOKEN)
            .unwrap()
            .with_timing(Duration::from_millis(200), 1_000_000);
        let big = obj(ObjectKind::Blob, &vec![1u8; 3_000_000]);
        assert_eq!(store.put(&big.0, &big.1).await, Ok(()));
        // o mesmo hub lento, com um corpo minúsculo: o prazo é só a base e estoura
        let small = obj(ObjectKind::Blob, b"pequeno");
        assert_eq!(
            store.put(&small.0, &small.1).await,
            Err(StoreError::Unavailable(io::ErrorKind::TimedOut))
        );
    }

    /// B1: página vazia com `next` não avança nada; sem esta regra o cliente pediria 10 000.
    #[tokio::test]
    async fn an_empty_page_with_a_next_is_refused_after_one_request() {
        let next = "ab".repeat(32);
        let body = format!("{{\"ids\":[],\"next\":\"{next}\"}}");
        let (url, count) = raw_server(Duration::ZERO, json_response(&body)).await;
        let store = HubStore::new(&url, TOKEN).unwrap();
        assert_eq!(
            store.list(ObjectKind::Blob).await,
            Err(StoreError::Unavailable(io::ErrorKind::InvalidData))
        );
        assert_eq!(count.load(Ordering::SeqCst), 1);
    }

    /// B2: falha de TLS não é "hub fora do ar": a interface precisa distinguir.
    #[tokio::test]
    async fn a_tls_failure_is_not_reported_as_a_refused_connection() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("https://{}", listener.local_addr().unwrap());
        tokio::spawn(async move {
            while let Ok((mut socket, _)) = listener.accept().await {
                // quem responde ao ClientHello com texto puro não fala TLS
                let _ = socket.write_all(b"HTTP/1.1 400 Bad Request\r\n\r\n").await;
                let _ = socket.shutdown().await;
            }
        });
        let store = HubStore::new(&url, TOKEN).unwrap();
        let id = obj(ObjectKind::Blob, b"x").0;
        assert_eq!(
            store.has(&id).await,
            Err(StoreError::Unavailable(io::ErrorKind::ConnectionAborted))
        );
    }

    /// B3: o teto de uma página de ids (1 MiB) também vale na listagem.
    #[tokio::test]
    async fn a_listing_page_above_the_page_cap_is_refused() {
        let reply = b"HTTP/1.1 200 OK\r\nContent-Length: 2097152\r\nConnection: close\r\n\r\n";
        let (url, _) = raw_server(Duration::ZERO, reply.to_vec()).await;
        let store = HubStore::new(&url, TOKEN).unwrap();
        assert_eq!(
            store.list(ObjectKind::Blob).await,
            Err(StoreError::Unavailable(io::ErrorKind::InvalidData))
        );
    }

    // ---------- construção ----------

    #[test]
    fn https_anywhere_http_only_on_loopback() {
        for ok in [
            "https://hub.example.com",
            "https://hub.example.com:8443/",
            "https://192.168.1.2:8443",
            "http://127.0.0.1:8080",
            "http://localhost:3000",
            "http://[::1]:9",
        ] {
            assert!(HubStore::new(ok, TOKEN).is_ok(), "{ok}");
        }
        for insecure in [
            "http://hub.example.com",
            "http://192.168.1.10:8080",
            "http://127.0.0.1.evil.com",
            "http://localhost.evil.com",
        ] {
            assert_eq!(
                HubStore::new(insecure, TOKEN).err(),
                Some(HubError::InsecureUrl),
                "{insecure}"
            );
            assert!(
                HubStore::allow_insecure_http(insecure, TOKEN).is_ok(),
                "{insecure}"
            );
        }
        for invalid in [
            "",
            "not a url",
            "ftp://hub.example.com",
            "https://",
            "https://user:senha@hub.example.com",
            "https://hub.example.com/prefixo",
            "https://hub.example.com/?q=1",
            "https://hub.example.com/#frag",
        ] {
            assert_eq!(
                HubStore::new(invalid, TOKEN).err(),
                Some(HubError::InvalidUrl),
                "{invalid}"
            );
            assert_eq!(
                HubStore::allow_insecure_http(invalid, TOKEN).err(),
                Some(HubError::InvalidUrl),
                "{invalid}"
            );
        }
    }

    #[test]
    fn a_token_that_cannot_be_a_header_value_is_refused() {
        for bad in [
            "",
            "   ",
            "com\nquebra",
            "com\rquebra",
            "acentuação",
            "com espaço",
        ] {
            assert_eq!(
                HubStore::new("https://hub.example.com", bad).err(),
                Some(HubError::InvalidToken),
                "{bad:?}"
            );
        }
    }

    #[test]
    fn the_token_never_shows_in_debug_or_in_errors() {
        let secret = "SEGREDO-DO-TOKEN-123";
        let store = HubStore::new("https://hub.example.com", secret).unwrap();
        assert!(!format!("{store:?}").contains(secret));
        for e in [
            HubStore::new("not a url", secret).unwrap_err(),
            HubStore::new("http://hub.example.com", secret).unwrap_err(),
        ] {
            assert!(!format!("{e} {e:?}").contains(secret));
            assert!(!format!("{e} {e:?}").contains("hub.example.com"));
        }
    }

    // ---------- contrato do trait ----------

    #[tokio::test]
    async fn delete_is_not_supported_and_never_reaches_the_network() {
        let (hub, store) = setup().await;
        let id = obj(ObjectKind::Blob, b"x").0;
        assert_eq!(
            store.delete(&id).await,
            Err(StoreError::Unavailable(io::ErrorKind::Unsupported))
        );
        assert!(hub.seen().is_empty());
    }

    #[test]
    fn the_hub_list_is_consistent() {
        let store = HubStore::new("https://hub.example.com", TOKEN).unwrap();
        assert!(store.capabilities().consistent_list);
    }

    #[tokio::test]
    async fn backend_hub_delegates_and_counts_in_the_replica_status() {
        let (hub, store) = setup().await;
        let backend = Backend::Hub(store);
        let blob = obj(ObjectKind::Blob, b"x");
        backend.put(&blob.0, &blob.1).await.unwrap();
        assert_eq!(backend.get(&blob.0).await, Ok(Some(blob.1)));
        assert_eq!(backend.list(ObjectKind::Blob).await, Ok(vec![blob.0]));
        assert_eq!(
            backend.capabilities(),
            HubStore::new(&hub.url, TOKEN).unwrap().capabilities()
        );
        let status = replica_status(&blob.0, std::slice::from_ref(&backend))
            .await
            .unwrap();
        assert_eq!((status.present, status.total), (1, 1));
        assert_eq!(
            backend.delete(&blob.0).await,
            Err(StoreError::Unavailable(io::ErrorKind::Unsupported))
        );
    }
}
