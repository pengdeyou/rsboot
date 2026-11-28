<p align="center">
  <img src="docs/rsboot-logo.svg" alt="rsboot" width="200">
</p>

# rsboot

> ### Le moyen le plus rapide de *construire* un service web en Rust.
>
> Plus rapide qu'axum et actix-web sur les charges d'E/S — **9 687 QPS** à 200 clients simultanés / 20 ms, **96× notre propre version synchrone**. Une seule ligne de dépendance dans `Cargo.toml` ; gestion des signaux, documentation OpenAPI et pool de connexions sont intégrés — quelques lignes d'un répertoire vide à un endpoint qui tourne.
>
> Nous ne sommes **pas** le champion du débit brut, et ce README ne fait pas semblant : sur le chemin JSON sans E/S, actix-web mène d'environ 3 % quand les deux tournent avec quatre workers et des clients pipelinés en profondeur 16. Le tableau ci-dessous montre chaque colonne, y compris celles où nous perdons.

Un framework applicatif natif de l'écosystème Rust — routage déclaratif à la FastAPI, extraction typée, documentation OpenAPI générée automatiquement. **Les handlers synchrones s'exécutent sur des coroutines à pile, au-dessus de sockets entièrement non bloquantes** : une E/S lente libère le worker au lieu de l'occuper. La pile HTTP/1.1, l'ordonnanceur et la boucle d'événements sont écrits maison — pas de tokio, pas de hyper, pas de axum.

[English](README.md) · [中文](README.zh-CN.md) · [Deutsch](README.de.md) · [Français](README.fr.md) · [العربية](README.ar.md) · [Русский](README.ru.md)

**Statut : alpha (`0.1.0-alpha.1`).** La chaîne principale — HTTP, routage, extracteurs, enveloppe de réponse unifiée, gestion d'erreurs unifiée, OpenAPI, cycle de vie des modules, arrêt gracieux, ordonnancement des coroutines, sockets de base de données — est implémentée et couverte par des tests. WebSocket / JSON-RPC restent à faire ; voir [Limites connues](#limites-connues).

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

Mesuré face à axum et actix-web. Tous les serveurs tournent dans la même fenêtre de temps, en alternance, avec les mêmes routes ; valeurs en QPS. **Les chiffres de fenêtres différentes ne sont pas comparables** — le protocole a changé, les lignes plus anciennes ne restent que parce qu'elles sont la seule mesure de cette charge.

### Comment les chiffres actuels sont produits

- Machine Linux 4 cœurs, **4 threads workers sur tous les serveurs**, client sur **4 threads**, 1 000 connexions keep-alive, `pipeline 16`.
- Chaque configuration tourne **10 manches × 8 s**, A/B en alternance manche par manche ; puis une seconde série avec l'ordre inversé. Les 20 manches sont regroupées : moyenne plus répartition victoires/défaites.
- Le bruit entre les manches sur cette machine est de **±0,35 %**, avec des splits de 10:10 à 13:7. Tout ce qui est plus petit n'est pas un résultat, et les deux séries doivent aller dans le même sens.
- Position actuelle face à actix-web 4 (même fenêtre, 4 workers des deux côtés) : rsboot **999 411** vs actix **1 033 174** — **−3,3 %**, 1:19.

Le journal complet — chaque changement, son delta et tout ce qui a été mesuré puis abandonné — est dans [docs/perf-log.md](docs/perf-log.md).

Les lignes `/slow` et `/json` ci-dessous viennent d'une **fenêtre antérieure** (VM 4 cœurs, 2 threads workers partout, manches plus courtes).

| Scénario | rsboot | axum | actix-web |
|---|---:|---:|---:|
| `/slow` — 20 ms de délai, 5 simultanés | **248** | 236 | 236 |
| `/slow` — 20 ms de délai, 20 simultanés | **990** | 944 | 944 |
| `/slow` — 20 ms de délai, 200 simultanés | **9 687** | 9 237 | 9 292 |
| `/json` — 4 simultanés, no blocking I/O | **112 267** | 92 280 | 110 919 |
| `/json` — 20 simultanés, no blocking I/O | 131 350 | 133 843 | **155 315** |

Ce que dit le tableau, y compris ce qui ne nous flatte pas :

- Sur les charges bloquantes, rsboot est le plus rapide des trois, mais de quelques pour cent seulement face à axum/actix — les trois occupent leurs workers et convergent donc. Le chiffre vraiment intéressant est la comparaison avec **notre propre version synchrone** : 99 → 9 687 QPS à 200 simultanés, soit **96×**. Toute la raison d'être de la couche coroutines tient là.
- Le débit JSON pur se scinde selon la concurrence, et nous en perdons une moitié. À 4 clients rsboot mène (**112 267** — 1,2 % devant actix, 21,7 % devant axum). À 20 clients **actix-web prend 18 % d'avance** (155 315 contre 131 350) et axum nous dépasse de 1,9 %. Cause mesurée : notre machine à états d'en-têtes (757 ns par tête de requête contre 122 ns pour `httparse`) plus la comptabilité par requête — des coûts qui croissent avec les requêtes en vol. Nous avons aussi vérifié ce que ce n'est *pas* : le blocage `poll` ne représente que 14 % du temps worker, et les petites réponses partent déjà en un seul `write`.
- Le chiffre 157 730 imprimé auparavant est **retiré**. Le générateur de charge coûtait lui-même des microsecondes par requête (deux appels `Instant::now()` à ~1,8 µs chacun sur cette VM), ce qui aplatissait les trois serveurs. Le tableau ci-dessus vient d'un client reconstruit : chronométrage `rdtsc`, zéro allocation et zéro appel système par requête.
- Appels en aval : 40 clients simultanés × 20 ms de dépendance, mode coopératif **655 QPS / p99 62 ms** contre base bloquante **33 QPS / p99 2 323 ms** — un facteur 20 obtenu par un seul commutateur (`RSBOOT_BLOCKING_IO`).

Ces valeurs varient selon la machine. Comparez toujours dans une même exécution, jamais entre machines ni contre les chiffres absolus d'un article de blog.

### Mesuré puis abandonné

Consigné ici pour que personne ne relance la même expérience en espérant un gain. Protocole ci-dessus ; un delta négatif signifie **plus lent**.

| Changement | Résultat | Pourquoi cela n'a rien rapporté |
|---|---|---|
| Champ de nom en une seule passe (deux-points + OWS final + vérification du token) | −0,06 %, 13:7 | Neutre. Le coût du parseur n'est pas « combien de fois on parcourt les octets » |
| `Hdr` 64 → 32 octets (table d'offsets + table latérale) | −0,59 %, 9:11, séries contradictoires | une indirection de plus à chaque accès d'en-tête |
| `get_unchecked` dans les boucles de parsing | **−1,69 %**, 14:6 | les vérifications de bornes étaient déjà gratuites ; `unsafe` bloquait d'autres optimisations |
| Scan CRLF écrit à la main dans la tête de réponse | −1,36 %, 12:8 | `str::contains` passe par un memchr vectorisé |
| `from_utf8_unchecked` sur la tranche d'en-têtes | −0,99 %, 12:8 | même raison — la vérification n'est pas là où part le temps |
| Pool thread-local pour les `Vec` d'en-têtes de réponse | −0,35 %, 12:8 | et `Drop` sur `Headers` casse l'emprunt dans `serve_once` |
| `-C target-cpu=x86-64-v3` / `native` | +0,9 %, mais AB −0,4 % / BA +2,3 % | séries en sens opposés = bruit |
| 12 emplacements d'en-têtes en ligne pour économiser un `malloc` | −8,3 % | le memset de 12 × 64 octets coûte plus que l'allocation |
| Regrouper toutes les réponses dans `out_buf` (suppression du chemin direct) | −4,3 % | un client qui pipeline garde des requêtes en vol, le serveur en voit rarement plus d'une |
| `SO_REUSEPORT`, accept par worker | aucun changement | le thread d'accept se réveille une fois toutes les 5 ms en régime stable |
| `SO_SNDBUF` / `SO_RCVBUF` plus petits | −0,04 % | dans le bruit |
| Tampon de lecture de 4 KiB par défaut | +0,8 %, non significatif | 1 KiB contient déjà ~12 têtes de requête |
| Allocateur tcache intégré (feature `tc-alloc`) | +0,36 % | plafond ~0,5 %, désactivé par défaut |

Pour qui veut attaquer les 3 % restants : sur le profil, deux appels système font 70 % d'une requête (3,5–4 µs sur 5 µs), l'espace utilisateur ~1 µs (`parse` 275 ns, `build` 145, `handle` 555). L'écart est donc de ~150 ns et ne peut se trouver que dans ces trois étages — mais trois tentatives structurelles sur `parse` ont toutes atterri dans le bruit, ce qui dit que ces 275 ns sont du travail fixe par requête (construction des items, comptabilité du `Vec`, insertion des spans, validation finale), pas la largeur du scan. La prochaine étape honnête est d'instrumenter `handle` lui-même avec un échantillonnage 1/64 et de découper ses 555 ns en routage, contexte, handler utilisateur et construction de la réponse.

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

- WebSocket et JSON-RPC 2.0 ne sont pas encore implémentés (prévus en S12–S13)
- gRPC est explicitement hors périmètre : il exige HTTP/2, donc tirer `h2` et renoncer à l'absence de dépendance de runtime
- L'authentification s'arrête aux extracteurs `Bearer` / `ApiKey` — pas de système complet de rôles et permissions
- Les piles de coroutines font 64 Ko fixes : 10 000 requêtes simultanées coûtent environ 640 Mo de piles
- La **connexion** côté client reste bloquante ; seules les lectures/écritures après connexion cèdent le worker
- La lecture des en-têtes et corps de requête sur le chemin principal peut encore bloquer un worker

## Roadmap

Toutes les prochaines étapes — P0 (les 3,3 % restants face à actix-web),
P1 (stabilité), P2 (nouvelles crates), ainsi que la liste des expériences mesurées
et rejetées — sont dans [`docs/ROADMAP.md`](docs/ROADMAP.md).

## Licence

MIT OR Apache-2.0, au choix.
