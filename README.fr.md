# rsboot

> ### Le moyen le plus rapide de construire un service web en Rust.
>
> Plus rapide qu'axum et actix-web sur les charges d'E/S — **9 687 QPS** à 200 clients simultanés / 20 ms, **96× notre propre version synchrone**. Une seule ligne de dépendance dans `Cargo.toml` ; gestion des signaux, documentation OpenAPI et pool de connexions sont intégrés — quelques lignes d'un répertoire vide à un endpoint qui tourne.

Un framework applicatif pour Rust, dans l'esprit de FastAPI : routage déclaratif, extraction typée, documentation OpenAPI générée automatiquement. **La pile HTTP/1.1 et l'ordonnanceur de coroutines sont entièrement écrits maison** : pas de tokio, pas de hyper, pas de axum.

[English](README.md) · [中文](README.zh-CN.md) · [Deutsch](README.de.md) · [Français](README.fr.md) · [العربية](README.ar.md) · [Русский](README.ru.md)

**Statut : alpha (`0.1.0-alpha.1`).** La chaîne principale — HTTP, routage, extracteurs, enveloppe de réponse unifiée, gestion d'erreurs unifiée, OpenAPI, cycle de vie des modules, arrêt gracieux, ordonnancement des coroutines, sockets de base de données — est implémentée et couverte par des tests. SSE / WebSocket / JSON-RPC restent à faire ; voir [Limites connues](#limites-connues).

`rsboot` est le framework applicatif, [rsbatis](https://github.com/pengdeyou/rsbatis) est l'ORM. Crates séparés, dépôts séparés, versions indépendantes.

## Pourquoi ce projet

L'attrait de FastAPI tient à trois choses : des routes déclaratives, les types comme validation, une documentation produite gratuitement. Les frameworks Rust courants en manquent chacun d'une :

| | Handler | Le prix à payer |
|---|---|---|
| axum / actix-web | `async fn` | Contraintes `Send + 'static` ; impossible d'emprunter `&Request` au-delà d'un point d'attente ; un client DB/HTTP asynchrone tire tokio + hyper + sqlx dans l'arbre de dépendances |
| Frameworks purement synchrones | `fn` | Le nombre de workers égale le nombre de cœurs, donc le débit est plafonné à *workers ÷ durée de blocage* — une seule dépendance lente paralyse le serveur |

La deuxième ligne est mesurée, pas supposée : avec 20 ms de latence en aval et 200 clients simultanés, une implémentation purement synchrone servait **99 QPS**. Les deux workers attendaient, simplement.

rsboot sépare le problème en deux couches : **la couche connexion est pilotée par événements, la couche requête tourne sur des coroutines à pile.** Les handlers gardent une signature synchrone ordinaire ; une I/O lente rend le worker disponible au lieu de l'immobiliser.

## Comparaison de performance

Mesuré face à axum et actix-web. Protocole : VM Linux 4 cœurs, **2 threads workers** pour les trois serveurs, routes identiques, les trois sollicités en alternance dans la même fenêtre de temps, valeurs en QPS.

| Scénario | rsboot | axum | actix-web |
|---|---:|---:|---:|
| `/slow` — 20 ms de délai, 5 simultanés | **248** | 236 | 236 |
| `/slow` — 20 ms de délai, 20 simultanés | **990** | 944 | 944 |
| `/slow` — 20 ms de délai, 200 simultanés | **9 687** | 9 237 | 9 292 |
| `/json` — aucune I/O bloquante | **157 730** | 138 926 | **162 098** |

Ce que dit le tableau, y compris ce qui ne nous flatte pas :

- Sur les charges bloquantes, rsboot est le plus rapide des trois, mais de quelques pour cent seulement face à axum/actix — les trois occupent leurs workers et convergent donc. Le chiffre vraiment intéressant est la comparaison avec **notre propre version synchrone** : 99 → 9 687 QPS à 200 simultanés, soit **96×**. Toute la raison d'être de la couche coroutines tient là.
- En débit JSON pur, sans blocage, **actix-web est 2,7 % plus rapide que rsboot** (162 098 contre 157 730), et rsboot devance axum de 13,5 %. Sur la distribution des handlers et le profil d'allocation par requête, actix garde l'avantage — nous ne le masquons pas.
- Appels en aval : 40 clients simultanés × 20 ms de dépendance, mode coopératif **655 QPS / p99 62 ms** contre base bloquante **33 QPS / p99 2 323 ms** — un facteur 20 obtenu par un seul commutateur (`RSBOOT_BLOCKING_IO`).

Ces valeurs varient selon la machine. Comparez toujours dans une même exécution, jamais entre machines ni contre les chiffres absolus d'un article de blog.

## Points forts de la conception

1. **Aucune runtime tierce.** L'analyse HTTP/1.1, la gestion du keep-alive, le regroupement `writev`, la boucle poll et l'ordonnanceur M:N sont tous dans le dépôt. Le niveau de dépendance est celui de serde, pas celui de tokio. Moins de pièces mobiles, et aucune version verrouillée sur les internes d'une runtime asynchrone.

2. **Les handlers restent synchrones sans bloquer le serveur.** Un handler, c'est `fn(&mut Request, &Context) -> Result<Response, ApiError>`. Les coroutines à pile conservent l'état sur une vraie pile : `&mut Request` peut donc être tenu au-delà d'un point de cession, ce qu'une machine à états générée par le compilateur ne peut exprimer sans acrobaties de lifetimes.

3. **Deux couches, séparées volontairement.** Les connexions sont multiplexées par l'event loop ; les requêtes s'exécutent en coroutines. Le nombre de coroutines suit les **requêtes en cours**, pas les connexions ouvertes : une connexion keep-alive coûte quelques octets, pas une pile de 64 Ko.

4. **Les sockets de base de données cèdent le worker.** `mysql+co://…` fait passer le pilote MySQL par des sockets compatibles coroutines : une requête lente parque la requête HTTP au lieu de bloquer un worker. Le hook est au niveau du processus, le mode au niveau du thread — deux moteurs dans le même processus ne peuvent pas se contaminer.

5. **Les appels en aval aussi.** `ctx.service("name")` fournit pool, retry et circuit breaker ; la lecture du socket est compatible coroutines, donc une dépendance lente dégrade un endpoint, pas le serveur entier.

6. **Une seule enveloppe de réponse, un seul type d'erreur.** `Api(...)` emballe succès et échec dans la même forme ; `ApiError` est le seul type d'erreur renvoyé par un handler, et c'est lui qui porte la correspondance des codes de statut. Le texte SQL et les chemins internes vont dans les logs, jamais au client.

7. **La sécurité échoue au démarrage, pas à l'exécution.** Une configuration CORS incorrecte empêche le démarrage au lieu de laisser tout passer en silence. Limitation de débit par jeton, en-têtes de sécurité et masquage des mots de passe dans les chaînes de connexion sont intégrés.

8. **Cycle de vie et arrêt gracieux sont de premier ordre.** Les modules implémentent `init / start / stop / drain` et se ferment dans l'ordre inverse. À SIGTERM, le serveur entre en phase de drainage (`http.drain_secs`) : `/ready` renvoie 503 pour que l'équilibreur retire le trafic *avant* que les connexions ne soient coupées ; les nouvelles requêtes pendant la fermeture reçoivent 503 plutôt qu'un reset.

9. **OpenAPI pousse avec le code, il n'est pas greffé après coup.** `RouteDoc` est attaché à la définition de la route ; `/openapi.json` et `/docs` naissent de la même source de vérité que les handlers.

10. **Configuration en couches, séparation dev/prod, rechargement à chaud.** Couches YAML / ini / env, `app.env` change le comportement, un surveillant de fichiers recharge sans redémarrage.

## Démarrage rapide

```bash
cargo run --example hello
curl -i http://127.0.0.1:8000/api/hello/rsboot
```

Forme minimale :

```rust
use rsboot::{ApiError, App, Config, FromRequest, Path, Response, Router};

fn main() -> rsboot::Result<()> {
    let config = Config::new()
        .set("app.env", "dev")
        .set("http.bind", "127.0.0.1:8000")
        .set("http.docs", "true");

    let mut app = App::builder()
        .config(config)
        .database("default", "sqlite:///hello.db")
        .build()?;

    let router = Router::new()
        .get("/", |_, _| Response::text("rsboot is running\n"))?
        .get("/api/hello/{name}", |req, _| -> Result<String, ApiError> {
            let Path(name) = Path::<String>::from_request(req)?;
            Ok(format!("hello, {name}"))
        })?;

    app.serve(router)
}
```

`serve` installe lui-même la gestion des signaux : arrêt de l'acceptation, fin des workers, fermeture des modules dans l'ordre inverse. Aucun câblage supplémentaire.

## Limites connues

Énoncées clairement, car un framework qui cache ses manques vaut moins qu'un framework qui les liste :

- SSE, WebSocket et JSON-RPC 2.0 ne sont pas encore implémentés (prévus en S11–S13)
- gRPC est explicitement hors périmètre : il exige HTTP/2, donc tirer `h2` et renoncer à l'absence de dépendance de runtime
- L'authentification s'arrête aux extracteurs `Bearer` / `ApiKey` — pas de système complet de rôles et permissions
- Les piles de coroutines font 64 Ko fixes : 10 000 requêtes simultanées coûtent environ 640 Mo de piles
- La **connexion** côté client reste bloquante ; seules les lectures/écritures après connexion cèdent le worker
- La lecture des en-têtes et corps de requête sur le chemin principal peut encore bloquer un worker

## Licence

MIT OR Apache-2.0, au choix.
