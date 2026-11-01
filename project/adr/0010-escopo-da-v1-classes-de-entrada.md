# 0010: Escopo da v1: três classes de entrada (vault, config, secret) e conflito fora da pasta viva

**Data:** 2026-10-31

## Contexto
O bundle do `warden-sync` leva três coisas: arquivos do Vault, `config.toml` e os logins OAuth do MCP (`oauth_files`, P54), todos cifrados no mesmo envelope (`crates/warden-sync/src/bundle.rs`, lido em `origin/main`). O P-18 perguntava se a arca sincroniza só o Vault ou também config e logins. Os logins são segredos de terceiros de vida longa. Além disso, o ADR 0005 preserva o perdedor de um conflito como `<nome>.conflito-*.md`, o que para um login `.json` deixaria uma cópia de token na pasta que o Warden lê. Área de segredos, decisão confirmada pelo dono (AGENTS.md §4).

## Decisão
- Cada entrada do manifest leva um campo **`class`**: `vault`, `config` ou `secret`.
- A arca sincroniza as classes que o consumidor entregar. **Quais classes ficam ligadas é configuração do Warden**, com o padrão igual ao comportamento atual do `warden-sync` (as três). A arca não conhece o Warden.
- Todas as classes usam o mesmo envelope e as mesmas subchaves do ADR 0009; `class` fica dentro do manifest cifrado.
- **Conflito nas classes `config` e `secret`** (arquivo que não é `.md` do Vault): vence a entrada de `created_at` mais recente e fica no lugar; a perdedora é guardada em uma **pasta de conflitos da arca** (`.arca/conflitos/`), **fora** do `~/.warden`, da pasta de logins e do Vault, com permissão `0o600` (pasta `0o700`). O conflito é registrado e mostrado ao usuário. Um comando de limpeza apaga as cópias.
- A classe `vault` segue o ADR 0005 (cópia `.conflito-*.md` ao lado).

## Alternativas descartadas
- **Só o Vault**: menor superfície, mas o Warden perde a paridade (dois caminhos de sync por mais tempo) e o usuário relogaria em cada máquina.
- **Logins opt-in**: mais conservador com segredos, mas muda o padrão atual do Warden e exige aviso na interface; a escolha de ligar ou não já cabe na configuração do consumidor.
- **Vence o mais recente sem guardar o perdedor** e **parar e pedir ao usuário**: o primeiro quebra o "nunca perder dado" do ADR 0005 e depende do relógio; o segundo bloqueia o sync por um login que expira de qualquer forma.

## Consequências
- Os logins passam a viver nos backends da arca (disco, hub, S3), sempre cifrados; quem tem a `vault_key` ou a frase de 24 palavras tem também os tokens (já é assim hoje no `warden-sync`).
- Segredo antigo pode ficar em disco na pasta de conflitos até a limpeza. Exige permissão restrita e o comando de limpeza; é a mesma classe de problema do P-11 (`sync_secrets.json` sem `0o600`) e não pode repetir o erro.
- Rotação (ADR 0006): os logins entram no snapshot completo e são recifrados como os demais; um dispositivo perdido continua com os tokens antigos, então a rotação **não** os revoga. Quem perdeu um dispositivo deve também revogar os logins nos serviços de origem.
- O Warden precisa de uma configuração por classe e de um caminho de pasta de conflitos; contrato na Fase 2.
- **Não validado:** como o Warden reage a um login trocado por outro dispositivo com o servidor ainda conectado; se `config.toml` tem valores por máquina que não deveriam sincronizar (hoje o `warden-sync` os envia inteiros); a permissão real dos arquivos aplicados no Windows. Revisão de segurança na Fase 4.
