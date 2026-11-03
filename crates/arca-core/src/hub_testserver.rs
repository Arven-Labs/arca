//! Hub de referência, **só para testes**: implementa o contrato `/arca/v1/` em memória
//! (`SPEC-TECNICA.md` §4.2) e sabe se comportar mal de propósito (mentir, redirecionar,
//! devolver listas hostis) para provar as defesas do `HubStore`.

use crate::object::{ObjectId, ObjectKind};
use crate::server;
use crate::store::{BlobStore, Capabilities, StoreError};
use axum::body::Bytes;
use axum::extract::{DefaultBodyLimit, Path, State};
use axum::http::{header, HeaderMap, Method, StatusCode, Uri};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::Router;
use std::collections::{BTreeMap, HashMap};
use std::future::{ready, Future};
use std::sync::{Arc, Mutex};

/// O que um token pode.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Access {
    Approved,
    Pending,
    Revoked,
}

/// Como a listagem se comporta.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub(crate) enum ListMode {
    #[default]
    Honest,
    /// Um id que não é hex de 64 caracteres no meio da página.
    NotHex,
    /// Ids fora de ordem.
    Unsorted,
    /// `next` sempre igual ao `after` recebido (laço infinito).
    StuckNext,
    /// Páginas sem fim, cada uma com ids novos.
    Endless,
}

/// Uma requisição como o hub a viu.
#[derive(Debug, Clone)]
pub(crate) struct Seen {
    pub method: String,
    pub path: String,
    pub authorization: Option<String>,
}

#[derive(Default)]
pub(crate) struct HubState {
    /// Chave = `ObjectId` em texto (`blob/<hex>`).
    pub objects: BTreeMap<String, Vec<u8>>,
    pub tokens: HashMap<String, Access>,
    pub log: Vec<Seen>,
    /// Máximo de ids por página, não importa o `limit` pedido.
    pub page_max: Option<usize>,
    pub list_mode: ListMode,
    /// Responde isto a tudo (depois de logar).
    pub force_status: Option<u16>,
    /// Responde `307` para `<isto><caminho>`.
    pub redirect_to: Option<String>,
    /// No `GET`, devolve bytes diferentes dos guardados.
    pub lie_on_get: bool,
}

type Shared = Arc<Mutex<HubState>>;

pub(crate) struct TestHub {
    pub url: String,
    pub state: Shared,
    task: tokio::task::JoinHandle<()>,
}

impl Drop for TestHub {
    fn drop(&mut self) {
        self.task.abort();
    }
}

impl TestHub {
    pub(crate) async fn start() -> TestHub {
        let state: Shared = Arc::default();
        let app = Router::new()
            .route("/arca/v1/objects", get(list_objects))
            .route(
                "/arca/v1/objects/{kind}/{hex}",
                get(get_object).put(put_object).delete(refuse_delete),
            )
            .layer(DefaultBodyLimit::disable())
            .with_state(state.clone());
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let task = tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        TestHub {
            url: format!("http://{addr}"),
            state,
            task,
        }
    }

    pub(crate) fn approve(&self, token: &str) {
        self.set_access(token, Access::Approved);
    }

    pub(crate) fn set_access(&self, token: &str, access: Access) {
        self.state
            .lock()
            .unwrap()
            .tokens
            .insert(token.to_string(), access);
    }

    pub(crate) fn object_count(&self) -> usize {
        self.state.lock().unwrap().objects.len()
    }

    pub(crate) fn seen(&self) -> Vec<Seen> {
        self.state.lock().unwrap().log.clone()
    }
}

/// Registra a requisição e decide se ela passa: redirecionamento ou status forçado de teste,
/// depois `401` (sem token ou token desconhecido) e `403` (pendente ou revogado).
fn gate(state: &Shared, method: &Method, uri: &Uri, headers: &HeaderMap) -> Option<Response> {
    let authorization = headers
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .map(str::to_string);
    let mut st = state.lock().unwrap();
    st.log.push(Seen {
        method: method.to_string(),
        path: uri.path().to_string(),
        authorization: authorization.clone(),
    });
    if let Some(to) = &st.redirect_to {
        let location = format!("{to}{}", uri.path());
        return Some(
            (
                StatusCode::TEMPORARY_REDIRECT,
                [(header::LOCATION, location)],
            )
                .into_response(),
        );
    }
    if let Some(code) = st.force_status {
        return Some(StatusCode::from_u16(code).unwrap().into_response());
    }
    let token = authorization
        .as_deref()
        .and_then(|a| a.strip_prefix("Bearer "));
    match token.and_then(|t| st.tokens.get(t)) {
        None => Some(StatusCode::UNAUTHORIZED.into_response()),
        Some(Access::Approved) => None,
        Some(_) => Some(StatusCode::FORBIDDEN.into_response()),
    }
}

/// Os objetos do `HubState` como um `BlobStore`, para o hub de teste usar o mesmo
/// `server::handle` que o hub real. O `Mutex` nunca fica preso através de um `await`.
struct StateStore(Shared);

impl BlobStore for StateStore {
    fn put(
        &self,
        id: &ObjectId,
        bytes: &[u8],
    ) -> impl Future<Output = Result<(), StoreError>> + Send {
        self.0
            .lock()
            .unwrap()
            .objects
            .insert(id.to_string(), bytes.to_vec());
        ready(Ok(()))
    }

    fn get(
        &self,
        id: &ObjectId,
    ) -> impl Future<Output = Result<Option<Vec<u8>>, StoreError>> + Send {
        ready(Ok(self
            .0
            .lock()
            .unwrap()
            .objects
            .get(&id.to_string())
            .cloned()))
    }

    fn has(&self, id: &ObjectId) -> impl Future<Output = Result<bool, StoreError>> + Send {
        ready(Ok(self
            .0
            .lock()
            .unwrap()
            .objects
            .contains_key(&id.to_string())))
    }

    fn list(
        &self,
        kind: ObjectKind,
    ) -> impl Future<Output = Result<Vec<ObjectId>, StoreError>> + Send {
        let prefix = format!("{}/", kind.as_str());
        let ids = self
            .0
            .lock()
            .unwrap()
            .objects
            .keys()
            .filter(|k| k.starts_with(&prefix))
            .filter_map(|k| k.parse().ok())
            .collect();
        ready(Ok(ids))
    }

    fn delete(&self, _: &ObjectId) -> impl Future<Output = Result<(), StoreError>> + Send {
        ready(Ok(()))
    }

    fn capabilities(&self) -> Capabilities {
        Capabilities {
            consistent_list: true,
        }
    }
}

/// Passa o pedido pelo `server::handle` e devolve a resposta HTTP. O alvo é a linha de
/// requisição crua (caminho e consulta), como o `warden-server` fará; nada de `Path<>`, que
/// decodifica `%xx`.
async fn serve(state: &Shared, method: &Method, uri: &Uri, body: &[u8]) -> Response {
    let target = uri.path_and_query().map_or("", |p| p.as_str());
    let reply = server::handle(&StateStore(state.clone()), method.as_str(), target, body).await;
    (
        StatusCode::from_u16(reply.status).unwrap(),
        [(header::CONTENT_TYPE, reply.content_type)],
        reply.body,
    )
        .into_response()
}

async fn put_object(
    State(state): State<Shared>,
    method: Method,
    uri: Uri,
    headers: HeaderMap,
    Path(_): Path<(String, String)>,
    body: Bytes,
) -> Response {
    if let Some(stop) = gate(&state, &method, &uri, &headers) {
        return stop;
    }
    serve(&state, &method, &uri, &body).await
}

async fn get_object(
    State(state): State<Shared>,
    method: Method,
    uri: Uri,
    headers: HeaderMap,
    Path(_): Path<(String, String)>,
) -> Response {
    if let Some(stop) = gate(&state, &method, &uri, &headers) {
        return stop;
    }
    let lie = state.lock().unwrap().lie_on_get;
    let response = serve(&state, &method, &uri, b"").await;
    if lie && response.status() == StatusCode::OK {
        return (StatusCode::OK, b"outros bytes".to_vec()).into_response();
    }
    response
}

async fn refuse_delete(
    State(state): State<Shared>,
    method: Method,
    uri: Uri,
    headers: HeaderMap,
) -> Response {
    if let Some(stop) = gate(&state, &method, &uri, &headers) {
        return stop;
    }
    serve(&state, &method, &uri, b"").await
}

async fn list_objects(
    State(state): State<Shared>,
    method: Method,
    uri: Uri,
    headers: HeaderMap,
) -> Response {
    if let Some(stop) = gate(&state, &method, &uri, &headers) {
        return stop;
    }
    let query: HashMap<String, String> = uri
        .query()
        .unwrap_or("")
        .split('&')
        .filter_map(|pair| pair.split_once('='))
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect();
    let after = query.get("after").cloned();
    let asked = query
        .get("limit")
        .and_then(|l| l.parse::<usize>().ok())
        .unwrap_or(1000);
    let (page_max, list_mode) = {
        let st = state.lock().unwrap();
        (st.page_max, st.list_mode)
    };
    let limit = page_max.map_or(asked, |max| max.min(asked));
    // A página honesta vem do `server::handle`; os modos hostis a estragam depois.
    let mut target = format!(
        "/arca/v1/objects?kind={}&limit={limit}",
        query.get("kind").map_or("", String::as_str)
    );
    if let Some(after) = &after {
        target.push_str(&format!("&after={after}"));
    }
    let honest = server::handle(&StateStore(state.clone()), "GET", &target, b"").await;
    if honest.status != 200 {
        return (StatusCode::from_u16(honest.status).unwrap(), honest.body).into_response();
    }
    let page: serde_json::Value = serde_json::from_slice(&honest.body).unwrap();
    let mut ids: Vec<String> = page["ids"]
        .as_array()
        .unwrap()
        .iter()
        .map(|v| v.as_str().unwrap().to_string())
        .collect();
    let mut next = page["next"].as_str().map(str::to_string);

    match list_mode {
        ListMode::Honest => {}
        ListMode::NotHex => ids.insert(ids.len() / 2, "../../etc/passwd".to_string()),
        ListMode::Unsorted => ids.reverse(),
        ListMode::StuckNext => next = after.clone().or_else(|| Some("0".repeat(64))),
        ListMode::Endless => {
            let base = after.as_deref().unwrap_or(&"0".repeat(64)).to_string();
            let n = u128::from_str_radix(&base[base.len() - 32..], 16).unwrap_or(0) + 1;
            ids = (0..limit.max(1) as u128)
                .map(|i| format!("{:064x}", n + i))
                .collect();
            next = ids.last().cloned();
        }
    }
    let json = serde_json::json!({ "ids": ids, "next": next });
    (
        StatusCode::OK,
        [(header::CONTENT_TYPE, "application/json")],
        json.to_string(),
    )
        .into_response()
}
