//! Hub de referência, **só para testes**: implementa o contrato `/arca/v1/` em memória
//! (`SPEC-TECNICA.md` §4.2) e sabe se comportar mal de propósito (mentir, redirecionar,
//! devolver listas hostis) para provar as defesas do `HubStore`.

use crate::envelope::Header;
use crate::object::{ObjectId, ObjectKind};
use axum::body::Bytes;
use axum::extract::{DefaultBodyLimit, Path, State};
use axum::http::{header, HeaderMap, Method, StatusCode, Uri};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::Router;
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, HashMap};
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

fn parse_id(kind: &str, hex: &str) -> Option<ObjectId> {
    format!("{kind}/{hex}").parse().ok()
}

async fn put_object(
    State(state): State<Shared>,
    method: Method,
    uri: Uri,
    headers: HeaderMap,
    Path((kind, hex)): Path<(String, String)>,
    body: Bytes,
) -> Response {
    if let Some(stop) = gate(&state, &method, &uri, &headers) {
        return stop;
    }
    let Some(id) = parse_id(&kind, &hex) else {
        return (StatusCode::BAD_REQUEST, "bad_id").into_response();
    };
    if Sha256::digest(&body)[..] != id.hash {
        return (StatusCode::BAD_REQUEST, "hash_mismatch").into_response();
    }
    let envelope_ok = matches!(Header::decode(&body), Ok((h, _)) if h.kind == id.kind);
    if !envelope_ok {
        return (StatusCode::BAD_REQUEST, "bad_envelope").into_response();
    }
    state
        .lock()
        .unwrap()
        .objects
        .insert(id.to_string(), body.to_vec());
    StatusCode::NO_CONTENT.into_response()
}

async fn get_object(
    State(state): State<Shared>,
    method: Method,
    uri: Uri,
    headers: HeaderMap,
    Path((kind, hex)): Path<(String, String)>,
) -> Response {
    if let Some(stop) = gate(&state, &method, &uri, &headers) {
        return stop;
    }
    let Some(id) = parse_id(&kind, &hex) else {
        return (StatusCode::BAD_REQUEST, "bad_id").into_response();
    };
    let st = state.lock().unwrap();
    let Some(bytes) = st.objects.get(&id.to_string()) else {
        return StatusCode::NOT_FOUND.into_response();
    };
    if st.lie_on_get {
        return (StatusCode::OK, b"outros bytes".to_vec()).into_response();
    }
    (StatusCode::OK, bytes.clone()).into_response()
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
    StatusCode::METHOD_NOT_ALLOWED.into_response()
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
    let kind = match query.get("kind").map(String::as_str) {
        Some("blob") => ObjectKind::Blob,
        Some("manifest") => ObjectKind::Manifest,
        _ => return (StatusCode::BAD_REQUEST, "bad_kind").into_response(),
    };
    let after = query.get("after").cloned();
    let asked = query
        .get("limit")
        .and_then(|l| l.parse::<usize>().ok())
        .unwrap_or(1000);
    let st = state.lock().unwrap();
    let limit = st.page_max.map_or(asked, |max| max.min(asked));
    let mut ids: Vec<String> = st
        .objects
        .keys()
        .filter_map(|k| {
            k.strip_prefix(&format!("{}/", kind.as_str()))
                .map(str::to_string)
        })
        .filter(|hex| after.as_ref().is_none_or(|a| hex > a))
        .collect();
    ids.sort();
    let more = ids.len() > limit;
    ids.truncate(limit);
    let mut next = if more { ids.last().cloned() } else { None };

    match st.list_mode {
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
