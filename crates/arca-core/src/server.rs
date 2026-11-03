//! O lado servidor do contrato `/arca/v1/` (`SPEC-TECNICA.md` §4.2), **sem rede e sem
//! autenticação**: recebe método, alvo e corpo já lidos e devolve a resposta. Quem hospeda
//! (o `warden-server`) cuida do transporte HTTP e de decidir *quem* pode chamar; aqui só se
//! decide *o que* o contrato permite.
//!
//! O hub é uma réplica, não a autoridade (ADR 0007): ele não tem a chave do Vault, então só
//! barra lixo trivial (hash que não bate, envelope inválido ou de outro tipo). Um cabeçalho
//! válido com corpo aleatório passa; quem o recusa é o `Engine::pull` do cliente (P-32).

use crate::envelope::Header;
use crate::object::{ObjectId, ObjectKind};
use crate::store::{verified_get, BlobStore, StoreError};

/// Teto de um blob no hub: o maior arquivo da arca mais o envelope e a margem.
pub const MAX_BLOB_OBJECT_BYTES: u64 = crate::scan::MAX_FILE_BYTES + 1024;
/// Teto de um manifest no hub.
pub const MAX_MANIFEST_OBJECT_BYTES: u64 = crate::manifest::MAX_MANIFEST_JSON_BYTES as u64 + 1024;
/// Ids por página de listagem, no máximo.
pub const MAX_PAGE_IDS: usize = 1000;

/// A resposta de um pedido, já em termos de HTTP mas sem transporte.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Reply {
    pub status: u16,
    pub content_type: &'static str,
    pub body: Vec<u8>,
}

/// Quantos bytes de corpo vale a pena ler para este pedido: o teto do `kind` num `PUT` de
/// objeto, e `0` para todo o resto (inclusive alvo inválido). O chamador recusa com `413`
/// antes de ler o corpo.
pub fn body_limit(method: &str, target: &str) -> u64 {
    match (method, route(target)) {
        ("PUT", Route::Object(Ok(id))) => max_object_bytes(id.kind),
        _ => 0,
    }
}

fn max_object_bytes(kind: ObjectKind) -> u64 {
    match kind {
        ObjectKind::Blob => MAX_BLOB_OBJECT_BYTES,
        ObjectKind::Manifest => MAX_MANIFEST_OBJECT_BYTES,
    }
}

/// Para onde um alvo aponta, sem olhar o método.
enum Route<'a> {
    /// `/arca/v1/objects/{kind}/{hex}`; `Err` quando o id não é `<tipo>/<64 hex minúsculos>`.
    Object(Result<ObjectId, ()>),
    /// `/arca/v1/objects?...`, com a consulta (sem o `?`).
    List(&'a str),
    NotFound,
}

fn route(target: &str) -> Route<'_> {
    let (path, query) = match target.split_once('?') {
        Some((path, query)) => (path, Some(query)),
        None => (target, None),
    };
    let Some(rest) = path.strip_prefix("/arca/v1/objects") else {
        return Route::NotFound;
    };
    if rest.is_empty() {
        return Route::List(query.unwrap_or(""));
    }
    match (rest.strip_prefix('/'), query) {
        // Só a listagem aceita consulta; o `kind/hex` tem de ser exatamente dois segmentos.
        (Some(id), None) if id.matches('/').count() == 1 => {
            Route::Object(id.parse().map_err(|_| ()))
        }
        _ => Route::NotFound,
    }
}

fn reply(status: u16, content_type: &'static str, body: Vec<u8>) -> Reply {
    Reply {
        status,
        content_type,
        body,
    }
}

/// Resposta de erro curta, em texto: o cliente olha o status, o texto serve a quem depura.
fn fail(status: u16, why: &'static str) -> Reply {
    reply(status, "text/plain; charset=utf-8", why.as_bytes().to_vec())
}

fn from_store(error: StoreError) -> Reply {
    match error {
        StoreError::Unavailable(_) => fail(503, "storage unavailable"),
        // `put` só vê `IdMismatch` se o chamador errou (conferimos antes); `Corrupt` é arquivo
        // que não bate com o id. Nos dois casos o hub não serve nem confia.
        StoreError::Corrupt | StoreError::IdMismatch => fail(500, "storage inconsistent"),
    }
}

/// Atende um pedido do contrato sobre `store`. `target` é caminho mais consulta, como na
/// linha de requisição (`/arca/v1/objects?kind=blob`).
pub async fn handle<S: BlobStore>(store: &S, method: &str, target: &str, body: &[u8]) -> Reply {
    match route(target) {
        Route::NotFound => fail(404, "not found"),
        Route::List(query) => match method {
            "GET" => list(store, query).await,
            _ => fail(405, "method not allowed"),
        },
        Route::Object(id) => {
            if !matches!(method, "PUT" | "GET" | "HEAD") {
                return fail(405, "method not allowed");
            }
            let Ok(id) = id else {
                return fail(400, "bad object id");
            };
            match method {
                "PUT" => put(store, &id, body).await,
                "GET" => get(store, &id).await,
                _ => head(store, &id).await,
            }
        }
    }
}

async fn put<S: BlobStore>(store: &S, id: &ObjectId, body: &[u8]) -> Reply {
    if body.len() as u64 > max_object_bytes(id.kind) {
        return fail(413, "object too large");
    }
    // O mesmo filtro do `replicate`, sem chave: envelope legível do tipo do `kind`. Vem antes
    // do hash porque é O(1) e o hash de um corpo grande não.
    if !matches!(Header::decode(body), Ok((header, _)) if header.kind == id.kind) {
        return fail(400, "not a valid envelope of this kind");
    }
    if ObjectId::of(id.kind, body) != *id {
        return fail(400, "hash does not match the id");
    }
    match store.put(id, body).await {
        Ok(()) => reply(204, "text/plain; charset=utf-8", Vec::new()),
        Err(e) => from_store(e),
    }
}

async fn get<S: BlobStore>(store: &S, id: &ObjectId) -> Reply {
    match verified_get(store, id).await {
        Ok(Some(bytes)) => reply(200, "application/octet-stream", bytes),
        Ok(None) => fail(404, "not found"),
        Err(e) => from_store(e),
    }
}

async fn head<S: BlobStore>(store: &S, id: &ObjectId) -> Reply {
    match store.has(id).await {
        Ok(true) => reply(200, "application/octet-stream", Vec::new()),
        Ok(false) => reply(404, "text/plain; charset=utf-8", Vec::new()),
        Err(e) => from_store(e),
    }
}

/// Os parâmetros da listagem já validados.
struct ListQuery {
    kind: ObjectKind,
    after: Option<[u8; 32]>,
    limit: usize,
}

fn parse_list_query(query: &str) -> Result<ListQuery, ()> {
    let (mut kind, mut after, mut limit) = (None, None, None);
    for pair in query.split('&').filter(|p| !p.is_empty()) {
        let (key, value) = pair.split_once('=').ok_or(())?;
        let slot = match key {
            "kind" => &mut kind,
            "after" => &mut after,
            "limit" => &mut limit,
            _ => continue,
        };
        if slot.replace(value).is_some() {
            return Err(());
        }
    }
    let kind = match kind.ok_or(())? {
        "blob" => ObjectKind::Blob,
        "manifest" => ObjectKind::Manifest,
        _ => return Err(()),
    };
    // Mesma regra do id de objeto: 64 hex minúsculos, nada mais.
    let after = match after {
        None => None,
        Some(hex) => Some(
            format!("blob/{hex}")
                .parse::<ObjectId>()
                .map_err(|_| ())?
                .hash,
        ),
    };
    let limit = match limit {
        None => MAX_PAGE_IDS,
        // `parse` aceita `+5`; só dígitos ASCII valem (zeros à esquerda, sim).
        Some(text) if text.bytes().all(|b| b.is_ascii_digit()) => match text.parse::<usize>() {
            Ok(0) | Err(_) => return Err(()),
            Ok(n) => n.min(MAX_PAGE_IDS),
        },
        Some(_) => return Err(()),
    };
    Ok(ListQuery { kind, after, limit })
}

async fn list<S: BlobStore>(store: &S, query: &str) -> Reply {
    let Ok(query) = parse_list_query(query) else {
        return fail(400, "bad list query");
    };
    let mut all = match store.list(query.kind).await {
        Ok(all) => all,
        Err(e) => return from_store(e),
    };
    // O `BlobStore::list` não promete ordem: ordenar e cortar é conosco. Custa O(n log n) por
    // página, mas sem alocar texto por id: o hex só é gerado para os ids da página (P-35).
    // A ordem dos bytes do hash é a ordem do hex minúsculo.
    all.sort_unstable_by_key(|id| id.hash);
    let start = query
        .after
        .map_or(0, |after| all.partition_point(|id| id.hash <= after));
    let page = &all[start..];
    let ids: Vec<String> = page.iter().take(query.limit).map(hex_of).collect();
    let next = if page.len() > query.limit {
        ids.last().cloned()
    } else {
        None
    };
    let json = serde_json::json!({ "ids": ids, "next": next });
    reply(200, "application/json", json.to_string().into_bytes())
}

fn hex_of(id: &ObjectId) -> String {
    id.hash.iter().map(|b| format!("{b:02x}")).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::disk::DiskStore;
    use crate::envelope::{Alg, Header, NONCE_LEN};
    use crate::object::{ObjectId, ObjectKind};
    use crate::store::{Capabilities, StoreError};
    use crate::testutil::TempDir;
    use crate::FORMAT_VERSION;
    use std::future::{ready, Future};

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

    fn setup() -> (TempDir, DiskStore) {
        let dir = TempDir::new();
        let store = DiskStore::open(dir.path().join("hub")).unwrap();
        (dir, store)
    }

    fn url(id: &ObjectId) -> String {
        format!("/arca/v1/objects/{id}")
    }

    async fn put(store: &DiskStore, id: &ObjectId, bytes: &[u8]) -> Reply {
        handle(store, "PUT", &url(id), bytes).await
    }

    /// Decodifica a resposta de uma listagem: (ids, next).
    fn page(reply: &Reply) -> (Vec<String>, Option<String>) {
        assert_eq!(
            reply.status,
            200,
            "{:?}",
            String::from_utf8_lossy(&reply.body)
        );
        assert_eq!(reply.content_type, "application/json");
        let json: serde_json::Value = serde_json::from_slice(&reply.body).unwrap();
        let ids = json["ids"]
            .as_array()
            .unwrap()
            .iter()
            .map(|v| v.as_str().unwrap().to_string())
            .collect();
        let next = json["next"].as_str().map(str::to_string);
        (ids, next)
    }

    fn hex(id: &ObjectId) -> String {
        id.to_string().split_once('/').unwrap().1.to_string()
    }

    // ---- PUT ----

    #[tokio::test]
    async fn put_de_blob_valido_grava_e_responde_204() {
        let (_dir, store) = setup();
        let (id, bytes) = obj(ObjectKind::Blob, b"conteudo");
        let reply = put(&store, &id, &bytes).await;
        assert_eq!(reply.status, 204);
        assert!(reply.body.is_empty());
        assert_eq!(store.get(&id).await.unwrap(), Some(bytes));
    }

    #[tokio::test]
    async fn put_de_manifest_valido_responde_204() {
        let (_dir, store) = setup();
        let (id, bytes) = obj(ObjectKind::Manifest, b"m");
        assert_eq!(put(&store, &id, &bytes).await.status, 204);
        assert!(store.has(&id).await.unwrap());
    }

    #[tokio::test]
    async fn put_repetido_continua_204_e_nao_duplica() {
        let (_dir, store) = setup();
        let (id, bytes) = obj(ObjectKind::Blob, b"x");
        assert_eq!(put(&store, &id, &bytes).await.status, 204);
        assert_eq!(put(&store, &id, &bytes).await.status, 204);
        assert_eq!(store.list(ObjectKind::Blob).await.unwrap().len(), 1);
    }

    #[tokio::test]
    async fn put_com_corpo_que_nao_tem_o_hash_do_id_e_400_e_nao_grava() {
        let (_dir, store) = setup();
        let (id, _) = obj(ObjectKind::Blob, b"um");
        let (_, outro) = obj(ObjectKind::Blob, b"outro");
        assert_eq!(put(&store, &id, &outro).await.status, 400);
        assert!(!store.has(&id).await.unwrap());
        assert!(store.list(ObjectKind::Blob).await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn put_de_bytes_sem_envelope_e_400_mesmo_com_hash_certo() {
        let (_dir, store) = setup();
        let lixo = b"isto nao e um envelope da arca, mas o hash bate".to_vec();
        let id = ObjectId::of(ObjectKind::Blob, &lixo);
        assert_eq!(put(&store, &id, &lixo).await.status, 400);
        assert!(!store.has(&id).await.unwrap());
    }

    #[tokio::test]
    async fn put_de_corpo_vazio_e_400() {
        let (_dir, store) = setup();
        let id = ObjectId::of(ObjectKind::Blob, b"");
        assert_eq!(put(&store, &id, b"").await.status, 400);
    }

    #[tokio::test]
    async fn put_de_envelope_de_manifest_na_rota_de_blob_e_400() {
        let (_dir, store) = setup();
        let (manifest_id, bytes) = obj(ObjectKind::Manifest, b"m");
        // Mesmo hash, mas dito blob: o cabeçalho diz manifest.
        let as_blob = ObjectId {
            kind: ObjectKind::Blob,
            hash: manifest_id.hash,
        };
        assert_eq!(put(&store, &as_blob, &bytes).await.status, 400);
        assert!(!store.has(&as_blob).await.unwrap());
    }

    #[tokio::test]
    async fn put_com_id_mal_formado_e_400() {
        let (_dir, store) = setup();
        let (id, bytes) = obj(ObjectKind::Blob, b"x");
        let good = hex(&id);
        let ruins = [
            format!("/arca/v1/objects/blob/{}", good.to_uppercase()),
            format!("/arca/v1/objects/blob/{}", &good[..63]),
            format!("/arca/v1/objects/blob/{good}0"),
            format!("/arca/v1/objects/tipo/{good}"),
            "/arca/v1/objects/blob/..".to_string(),
            format!("/arca/v1/objects/blob/%2e%2e%2f{}", &good[..58]),
        ];
        for target in ruins {
            let reply = handle(&store, "PUT", &target, &bytes).await;
            assert_eq!(reply.status, 400, "{target}");
        }
        assert!(store.list(ObjectKind::Blob).await.unwrap().is_empty());
    }

    // ---- GET e HEAD ----

    #[tokio::test]
    async fn get_devolve_os_bytes_gravados() {
        let (_dir, store) = setup();
        let (id, bytes) = obj(ObjectKind::Blob, b"conteudo");
        put(&store, &id, &bytes).await;
        let reply = handle(&store, "GET", &url(&id), b"").await;
        assert_eq!(reply.status, 200);
        assert_eq!(reply.body, bytes);
    }

    #[tokio::test]
    async fn get_de_objeto_ausente_e_404() {
        let (_dir, store) = setup();
        let (id, _) = obj(ObjectKind::Blob, b"nunca enviado");
        assert_eq!(handle(&store, "GET", &url(&id), b"").await.status, 404);
    }

    #[tokio::test]
    async fn head_diz_se_existe_sem_corpo() {
        let (_dir, store) = setup();
        let (id, bytes) = obj(ObjectKind::Manifest, b"m");
        let (ausente, _) = obj(ObjectKind::Manifest, b"outro");
        put(&store, &id, &bytes).await;
        let achou = handle(&store, "HEAD", &url(&id), b"").await;
        assert_eq!((achou.status, achou.body.len()), (200, 0));
        let nao = handle(&store, "HEAD", &url(&ausente), b"").await;
        assert_eq!((nao.status, nao.body.len()), (404, 0));
    }

    #[tokio::test]
    async fn get_de_arquivo_corrompido_no_disco_e_500_e_nao_serve_os_bytes() {
        let (dir, store) = setup();
        let (id, bytes) = obj(ObjectKind::Blob, b"conteudo");
        put(&store, &id, &bytes).await;
        std::fs::write(dir.path().join("hub").join(id.to_string()), b"corrompido").unwrap();
        let reply = handle(&store, "GET", &url(&id), b"").await;
        assert_eq!(reply.status, 500);
        assert!(!reply.body.windows(10).any(|w| w == b"corrompido"));
    }

    // ---- métodos e rotas ----

    #[tokio::test]
    async fn delete_e_outros_metodos_sao_405_e_nada_e_apagado() {
        let (_dir, store) = setup();
        let (id, bytes) = obj(ObjectKind::Blob, b"x");
        put(&store, &id, &bytes).await;
        for method in ["DELETE", "POST", "PATCH", "OPTIONS"] {
            let reply = handle(&store, method, &url(&id), b"").await;
            assert_eq!(reply.status, 405, "{method}");
        }
        assert!(store.has(&id).await.unwrap());
    }

    #[tokio::test]
    async fn rota_desconhecida_e_404() {
        let (_dir, store) = setup();
        let (id, _) = obj(ObjectKind::Blob, b"x");
        let alvos = [
            "/arca/v1/".to_string(),
            "/arca/v1/nada".to_string(),
            "/arca/v2/objects".to_string(),
            format!("{}/extra", url(&id)),
            format!("{}?x=1", url(&id)),
            "/v1/models".to_string(),
        ];
        for target in alvos {
            assert_eq!(
                handle(&store, "GET", &target, b"").await.status,
                404,
                "{target}"
            );
        }
    }

    // ---- listagem ----

    #[tokio::test]
    async fn lista_so_do_tipo_pedido_em_ordem_crescente() {
        let (_dir, store) = setup();
        let mut blobs = Vec::new();
        for i in 0..4u8 {
            let (id, bytes) = obj(ObjectKind::Blob, &[i]);
            put(&store, &id, &bytes).await;
            blobs.push(hex(&id));
        }
        let (mid, mbytes) = obj(ObjectKind::Manifest, b"m");
        put(&store, &mid, &mbytes).await;
        blobs.sort();

        let reply = handle(&store, "GET", "/arca/v1/objects?kind=blob", b"").await;
        assert_eq!(page(&reply), (blobs, None));
        let reply = handle(&store, "GET", "/arca/v1/objects?kind=manifest", b"").await;
        assert_eq!(page(&reply), (vec![hex(&mid)], None));
    }

    #[tokio::test]
    async fn lista_vazia_tem_ids_vazio_e_next_nulo() {
        let (_dir, store) = setup();
        let reply = handle(&store, "GET", "/arca/v1/objects?kind=blob", b"").await;
        assert_eq!(page(&reply), (vec![], None));
    }

    #[tokio::test]
    async fn paginacao_percorre_tudo_sem_repetir_nem_perder() {
        let (_dir, store) = setup();
        let mut todos = Vec::new();
        for i in 0..5u8 {
            let (id, bytes) = obj(ObjectKind::Blob, &[i]);
            put(&store, &id, &bytes).await;
            todos.push(hex(&id));
        }
        todos.sort();

        let mut vistos = Vec::new();
        let mut after: Option<String> = None;
        let mut paginas = 0;
        loop {
            let mut target = "/arca/v1/objects?kind=blob&limit=2".to_string();
            if let Some(a) = &after {
                target.push_str(&format!("&after={a}"));
            }
            let (ids, next) = page(&handle(&store, "GET", &target, b"").await);
            assert!(ids.len() <= 2);
            vistos.extend(ids);
            paginas += 1;
            match next {
                Some(n) => after = Some(n),
                None => break,
            }
            assert!(paginas < 10, "laço");
        }
        assert_eq!(vistos, todos);
        assert_eq!(paginas, 3); // 2 + 2 + 1
    }

    #[tokio::test]
    async fn pagina_cheia_que_acaba_exatamente_nao_deixa_next() {
        let (_dir, store) = setup();
        for i in 0..2u8 {
            let (id, bytes) = obj(ObjectKind::Blob, &[i]);
            put(&store, &id, &bytes).await;
        }
        let reply = handle(&store, "GET", "/arca/v1/objects?kind=blob&limit=2", b"").await;
        let (ids, next) = page(&reply);
        assert_eq!((ids.len(), next), (2, None));
    }

    #[tokio::test]
    async fn after_que_nao_existe_devolve_so_os_ids_maiores() {
        let (_dir, store) = setup();
        let mut todos = Vec::new();
        for i in 0..4u8 {
            let (id, bytes) = obj(ObjectKind::Blob, &[i]);
            put(&store, &id, &bytes).await;
            todos.push(hex(&id));
        }
        todos.sort();
        // Um id fictício entre o segundo e o terceiro.
        let fake = format!("{}{}", &todos[1][..63], "f");
        let target = format!("/arca/v1/objects?kind=blob&after={fake}");
        let (ids, _) = page(&handle(&store, "GET", &target, b"").await);
        let esperado: Vec<String> = todos.iter().filter(|h| **h > fake).cloned().collect();
        assert_eq!(ids, esperado);
    }

    #[tokio::test]
    async fn limit_acima_do_maximo_e_cortado_em_1000() {
        let (_dir, store) = setup();
        for i in 0..1001u32 {
            let (id, bytes) = obj(ObjectKind::Blob, &i.to_be_bytes());
            put(&store, &id, &bytes).await;
        }
        let reply = handle(&store, "GET", "/arca/v1/objects?kind=blob&limit=5000", b"").await;
        let (ids, next) = page(&reply);
        assert_eq!(ids.len(), MAX_PAGE_IDS);
        assert_eq!(next.as_deref(), ids.last().map(String::as_str));
        let sem_limit = handle(&store, "GET", "/arca/v1/objects?kind=blob", b"").await;
        assert_eq!(page(&sem_limit).0.len(), MAX_PAGE_IDS);
    }

    #[tokio::test]
    async fn listagem_com_parametro_invalido_e_400() {
        let (_dir, store) = setup();
        let ok = "0".repeat(64);
        let ruins = [
            "/arca/v1/objects".to_string(),
            "/arca/v1/objects?kind=".to_string(),
            "/arca/v1/objects?kind=outro".to_string(),
            "/arca/v1/objects?kind=blob&limit=0".to_string(),
            "/arca/v1/objects?kind=blob&limit=-1".to_string(),
            "/arca/v1/objects?kind=blob&limit=abc".to_string(),
            "/arca/v1/objects?kind=blob&limit=+5".to_string(),
            "/arca/v1/objects?kind=blob&limit=%35".to_string(),
            "/arca/v1/objects?kind=blob&limit=".to_string(),
            "/arca/v1/objects?kind=blob&limit=99999999999999999999999999".to_string(),
            "/arca/v1/objects?kind=blob&limit=1&limit=2".to_string(),
            format!(
                "/arca/v1/objects?kind=blob&after={0}&after={0}",
                "0".repeat(64)
            ),
            "/arca/v1/objects?kind=blob&after=xyz".to_string(),
            format!(
                "/arca/v1/objects?kind=blob&after={}",
                ok.to_uppercase().replace('0', "A")
            ),
            format!("/arca/v1/objects?kind=blob&after={}", &ok[..63]),
            "/arca/v1/objects?kind=blob&kind=manifest".to_string(),
        ];
        for target in ruins {
            assert_eq!(
                handle(&store, "GET", &target, b"").await.status,
                400,
                "{target}"
            );
        }
    }

    #[tokio::test]
    async fn limit_com_zeros_a_esquerda_e_numero_normal() {
        let (_dir, store) = setup();
        for i in 0..3u8 {
            let (id, bytes) = obj(ObjectKind::Blob, &[i]);
            put(&store, &id, &bytes).await;
        }
        let reply = handle(&store, "GET", "/arca/v1/objects?kind=blob&limit=0002", b"").await;
        let (ids, next) = page(&reply);
        assert_eq!(ids.len(), 2);
        assert_eq!(next.as_deref(), ids.last().map(String::as_str));
    }

    #[tokio::test]
    async fn put_na_rota_de_listagem_e_405() {
        let (_dir, store) = setup();
        let reply = handle(&store, "PUT", "/arca/v1/objects?kind=blob", b"x").await;
        assert_eq!(reply.status, 405);
    }

    #[tokio::test]
    async fn get_e_head_com_corpo_ignoram_o_corpo() {
        let (_dir, store) = setup();
        let (id, bytes) = obj(ObjectKind::Blob, b"conteudo");
        put(&store, &id, &bytes).await;
        let get = handle(&store, "GET", &url(&id), b"lixo").await;
        assert_eq!((get.status, get.body), (200, bytes));
        let head = handle(&store, "HEAD", &url(&id), b"lixo").await;
        assert_eq!((head.status, head.body.len()), (200, 0));
    }

    /// O limite que o chamador usa para recusar com 413 só é positivo onde o `handle` de fato
    /// trata o corpo como objeto: o chamador e o `handle` não podem divergir.
    #[tokio::test]
    async fn body_limit_so_e_positivo_onde_o_put_chega_na_validacao() {
        let (_dir, store) = setup();
        let (id, bytes) = obj(ObjectKind::Blob, b"x");
        let good = hex(&id);
        let alvos = [
            (url(&id), true),
            (format!("/arca/v1/objects/manifest/{good}"), true),
            (
                format!("/arca/v1/objects/blob/{}", good.to_uppercase()),
                false,
            ),
            (format!("/arca/v1/objects/blob/{good}/"), false),
            (format!("/arca/v1/objects/blob/{good}?x=1"), false),
            (format!("/arca/v1/objects/blob/{good}#f"), false),
            (format!("//arca/v1/objects/blob/{good}"), false),
            (format!("/ARCA/v1/objects/blob/{good}"), false),
            ("/arca/v1/objects?kind=blob".to_string(), false),
            ("/arca/v1/objects/blob".to_string(), false),
            ("/arca/v1/objects/".to_string(), false),
        ];
        for (target, valido) in alvos {
            let limit = body_limit("PUT", &target);
            assert_eq!(limit > 0, valido, "{target}");
            let status = handle(&store, "PUT", &target, &bytes).await.status;
            if valido {
                // Chegou na validação do objeto: o corpo do blob não serve de manifest (400).
                assert!(matches!(status, 204 | 400), "{target}: {status}");
            } else {
                assert!(matches!(status, 400 | 404 | 405), "{target}: {status}");
            }
        }
    }

    // ---- tetos ----

    #[test]
    fn body_limit_e_o_teto_do_tipo_so_para_put_de_objeto() {
        let (blob, _) = obj(ObjectKind::Blob, b"x");
        let (manifest, _) = obj(ObjectKind::Manifest, b"x");
        assert_eq!(body_limit("PUT", &url(&blob)), MAX_BLOB_OBJECT_BYTES);
        assert_eq!(
            body_limit("PUT", &url(&manifest)),
            MAX_MANIFEST_OBJECT_BYTES
        );
        assert_eq!(body_limit("GET", &url(&blob)), 0);
        assert_eq!(body_limit("DELETE", &url(&blob)), 0);
        assert_eq!(body_limit("PUT", "/arca/v1/objects/blob/xyz"), 0);
        assert_eq!(body_limit("PUT", "/arca/v1/objects?kind=blob"), 0);
        assert_eq!(body_limit("PUT", "/outra/rota"), 0);
    }

    #[tokio::test]
    async fn put_de_manifest_acima_do_teto_e_413_e_nao_grava() {
        let (_dir, store) = setup();
        let grande = vec![0u8; MAX_MANIFEST_OBJECT_BYTES as usize + 1];
        let id = ObjectId::of(ObjectKind::Manifest, &grande);
        assert_eq!(put(&store, &id, &grande).await.status, 413);
        assert!(!store.has(&id).await.unwrap());
    }

    #[tokio::test]
    async fn put_de_manifest_exatamente_no_teto_passa_do_teto_e_chega_na_validacao() {
        let (_dir, store) = setup();
        let no_teto = vec![0u8; MAX_MANIFEST_OBJECT_BYTES as usize];
        let id = ObjectId::of(ObjectKind::Manifest, &no_teto);
        // Não é 413; é recusado por não ter envelope.
        assert_eq!(put(&store, &id, &no_teto).await.status, 400);
    }

    // ---- backend com defeito ----

    /// Backend que falha sempre.
    struct Down;

    impl BlobStore for Down {
        fn put(
            &self,
            _: &ObjectId,
            _: &[u8],
        ) -> impl Future<Output = Result<(), StoreError>> + Send {
            ready(Err(StoreError::Unavailable(std::io::ErrorKind::Other)))
        }
        fn get(
            &self,
            _: &ObjectId,
        ) -> impl Future<Output = Result<Option<Vec<u8>>, StoreError>> + Send {
            ready(Err(StoreError::Unavailable(std::io::ErrorKind::Other)))
        }
        fn has(&self, _: &ObjectId) -> impl Future<Output = Result<bool, StoreError>> + Send {
            ready(Err(StoreError::Unavailable(std::io::ErrorKind::Other)))
        }
        fn list(
            &self,
            _: ObjectKind,
        ) -> impl Future<Output = Result<Vec<ObjectId>, StoreError>> + Send {
            ready(Err(StoreError::Unavailable(std::io::ErrorKind::Other)))
        }
        fn delete(&self, _: &ObjectId) -> impl Future<Output = Result<(), StoreError>> + Send {
            ready(Err(StoreError::Unavailable(std::io::ErrorKind::Other)))
        }
        fn capabilities(&self) -> Capabilities {
            Capabilities {
                consistent_list: true,
            }
        }
    }

    #[tokio::test]
    async fn backend_indisponivel_vira_503_em_todas_as_rotas() {
        let (id, bytes) = obj(ObjectKind::Blob, b"x");
        let casos = [
            ("PUT", url(&id), bytes.clone()),
            ("GET", url(&id), vec![]),
            ("HEAD", url(&id), vec![]),
            ("GET", "/arca/v1/objects?kind=blob".to_string(), vec![]),
        ];
        for (method, target, body) in casos {
            let reply = handle(&Down, method, &target, &body).await;
            assert_eq!(reply.status, 503, "{method} {target}");
        }
    }
}
