# 0016: Entrega da `vault_key` a um dispositivo novo por relay no hub, com ECIES e duas conferências humanas

**Data:** 2026-11-03

## Contexto
O ADR 0015 deixou para a 2b-3 o pareamento: como um dispositivo novo recebe a `vault_key`. A frase BIP-39 (ADR 0004) já serve de recuperação e entrega manual, mas o objetivo é o dispositivo receber a chave de outro, sem digitar 24 palavras. O ADR 0015 previa reaproveitar o pareamento do `warden-sync` (código de 8 caracteres, na LAN).

Área de criptografia, chaves e autenticação (AGENTS.md §4). Decisões confirmadas pelo dono em 2026-11-03. Duas auditorias do `security-auditor` sobre o código no Warden (warden#79). O código vive no Warden; a arca só ganhou `VaultKey::export_bytes` (arca#51).

## Decisão
- **Relay pelo hub, cifrado com ECIES** (secp256k1 + AES-256-GCM, `warden_truthid::crypto`, o mesmo do `warden-sync`). O dispositivo novo (B) publica uma chave pública (oferta); outro dispositivo **aprovado do mesmo dono** (A) cifra a `vault_key` para ela e deposita (concessão); B lê **uma vez**. O hub só vê chave pública e texto cifrado. Rotas em `SPEC-TECNICA.md` §4.2.
- **Mesmo token do `Hello`** (ADR 0015); só dispositivo `Approved` entra. Duas barreiras: o operador aprova o dispositivo, e uma pessoa confere os códigos abaixo.
- **Duas conferências por uma pessoa, nos dois dispositivos, cada código calculado localmente** (o hub nunca o calcula nem o transmite), de 64 bits em 4 grupos de 4 hex:
  1. **`verification_code(device_id, chave pública)`**, antes de A conceder: o hub escolhe qual chave pública A recebe e poderia trocar a de B pela dele. Código de 64 bits e não de 6 dígitos: o hub vê a chave de B antes de o código aparecer e poderia procurar uma chave colidente.
  2. **`key_fingerprint(vault_key)`**, depois de B receber: o ECIES não diz quem cifrou, então o hub, ou um dispositivo aprovado que ainda não tem a chave, pode entregar a B uma `vault_key` própria.
- **Concessão ligada à chave que A viu** (`?key=`): se a oferta mudou entre a listagem e a concessão, `409` e a oferta nova fica. Chave pública canônica (comprimida, minúscula) para o código não depender da grafia.
- **Limites no hub:** oferta e concessão expiram em 10 min; 8 ofertas por dono; corpo até 4 KiB; um lock por dono; arquivos `0600` fora da pasta de objetos.
- **A arca só expõe `VaultKey::export_bytes()`** (`Zeroizing<[u8; 32]>`, `pub`). Cripto e rótulos do pareamento ficam no Warden; o `Debug` da cópia mostra os bytes, risco aceito e documentado.

## Alternativas descartadas
- **Pareamento LAN do `warden-sync`** (o que o ADR 0015 previa): só funciona na mesma rede, o código de 8 caracteres não tem limite de tentativas no host (um atacante na LAN tenta adivinhar dentro dos 5 min) e não liga a chave ao token do hub.
- **Só a frase BIP-39:** zero criptografia nova, mas a pessoa digita 24 palavras em cada dispositivo. Continua como recuperação.
- **Token separado só para a arca:** permitiria revogar a arca sem tirar o dispositivo do Warden, mas é um segredo a mais por dispositivo, sem ganho agora.
- **Código curto de 6 dígitos:** forjável por busca (cerca de 1 milhão de tentativas).
- **Aceitar a concessão não autenticada e deixar para a Fase 4:** descartado pelo dono depois da auditoria; a segunda conferência custa uma comparação.
- **Validar a chave tentando abrir um objeto já no hub:** não protege contra um hub que esconde os objetos reais, e não serve para Vault vazio.

## Consequências
- **Sem a interface que mostra e confere os dois códigos, o desenho não protege contra um hub hostil.** O servidor não consegue impor a conferência. Nenhum cliente deve chamar `PUT grant` antes disso, e o cliente deve exigir os códigos confirmados e só então gravar a chave em B (Warden P182 item 1).
- O fingerprint sem sal só é seguro com `vault_key` de entropia plena (BIP-39). Se um dia a chave vier de senha, ele vira um oráculo de força bruta.
- O hub passa a guardar chaves públicas e blobs de 93 bytes por até 10 min; um dispositivo aprovado do dono pode conceder a qualquer oferta (é o modelo de confiança: aprovado do dono é confiável, e o fingerprint cobre o resto).
- Limites registrados no Warden P182: lock por processo (dois hubs no mesmo diretório não se protegem), `device_id` com `/` ou `?` não recebe concessão, leitura do `devices.json` por oferta, corpo da concessão sem o tamanho exato.
- **Não resolvido:** o primeiro acesso de um dispositivo novo e a rotação de época aceitam qualquer cabeça uma vez (P-31(1), Fase 4); o pareamento seria o canal natural para confirmar o hash da cabeça, mas ficou de fora. Dispositivo perdido e entrega de chave de época nova (P-32) seguem para a Fase 4. Sem revisão independente além das duas auditorias.
