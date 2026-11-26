# rsboot

> ### Der schnellste Weg, einen Rust-Webdienst zu bauen.
>
> Bei I/O-lastiger Last schneller als axum und actix-web — **9 687 QPS** bei 200 gleichzeitigen Clients / 20 ms, **96-fach gegenüber der eigenen synchronen Vorgängerversion**. Eine Abhängigkeitszeile in `Cargo.toml`; Signalbehandlung, OpenAPI-Dokumentation und Connection-Pooling sind eingebaut — in wenigen Zeilen vom leeren Verzeichnis zum laufenden Endpunkt.

Ein Anwendungsframework für Rust im Stil von FastAPI — deklaratives Routing, typisierte Extraktion, automatisch erzeugte OpenAPI-Dokumentation. **Der HTTP/1.1-Stack und der Coroutine-Scheduler sind vollständig selbst geschrieben**: kein tokio, kein hyper, kein axum.

[English](README.md) · [中文](README.zh-CN.md) · [Deutsch](README.de.md) · [Français](README.fr.md) · [العربية](README.ar.md) · [Русский](README.ru.md)

**Status: Alpha (`0.1.0-alpha.1`).** Die Kernkette — HTTP, Routing, Extraktoren, einheitliche Antwort-Hülle, einheitliche Fehlerbehandlung, OpenAPI, Modul-Lebenszyklus, kontrolliertes Herunterfahren, Coroutine-Scheduling, Datenbank-Sockets — ist implementiert und durch Tests abgedeckt. SSE / WebSocket / JSON-RPC fehlen noch; siehe [Bekannte Einschränkungen](#bekannte-einschränkungen).

`rsboot` ist das Anwendungsframework, [rsbatis](https://github.com/pengdeyou/rsbatis) ist das ORM. Getrennte Crates, getrennte Repositories, unabhängige Versionen.

## Warum es das gibt

Der Reiz von FastAPI lässt sich auf drei Punkte reduzieren: deklarative Routen, Typen als Validierung, kostenlos erzeugte Dokumentation. Den gängigen Rust-Frameworks fehlt jeweils einer davon:

| | Handler | Preis |
|---|---|---|
| axum / actix-web | `async fn` | `Send + 'static`-Bounds; `&Request` lässt sich nicht über einen await-Punkt hinweg ausleihen; asynchrone DB-/HTTP-Clients ziehen tokio + hyper + sqlx in den Abhängigkeitsbaum |
| Rein synchrone Frameworks | `fn` | Die Worker-Anzahl entspricht der Kernanzahl, der Durchsatz ist also auf *Worker ÷ Blockierdauer* begrenzt — eine langsame Abhängigkeit legt den ganzen Server lahm |

Die zweite Zeile ist gemessen, nicht behauptet: bei 20 ms Downstream-Latenz und 200 gleichzeitigen Clients lieferte eine rein synchrone Implementierung **99 QPS**. Beide Worker warteten schlicht.

rsboot teilt das Problem in zwei Schichten: **die Verbindungsschicht ist ereignisgetrieben, die Anfrageschicht läuft auf stackbasierten Coroutinen.** Handler behalten eine gewöhnliche synchrone Signatur; langsame I/O gibt den Worker frei, statt ihn zu belegen.

## Leistungsvergleich

Gemessen gegen axum und actix-web. Aufbau: Linux-VM mit 4 Kernen, **2 Worker-Threads** für alle drei Server, identische Routen, alle drei im selben Zeitfenster abwechselnd belastet, angegeben sind QPS.

| Szenario | rsboot | axum | actix-web |
|---|---:|---:|---:|
| `/slow` — 20 ms Verzögerung, 5 gleichzeitig | **248** | 236 | 236 |
| `/slow` — 20 ms Verzögerung, 20 gleichzeitig | **990** | 944 | 944 |
| `/slow` — 20 ms Verzögerung, 200 gleichzeitig | **9 687** | 9 237 | 9 292 |
| `/json` — keine blockierende I/O | **157 730** | 138 926 | **162 098** |

Was die Tabelle aussagt — einschließlich dessen, was uns nicht schmeichelt:

- Bei blockierenden Lasten ist rsboot der schnellste der drei, aber nur um wenige Prozent gegenüber axum/actix — alle drei halten die Worker beschäftigt und konvergieren daher. Die eigentlich interessante Zahl ist der Vergleich mit **unserer eigenen synchronen Version**: 99 → 9 687 QPS bei 200 gleichzeitigen Clients, **96-fach**. Genau dafür existiert die Coroutine-Schicht.
- Bei reinem, nicht blockierendem JSON-Durchsatz ist **actix-web 2,7 % schneller als rsboot** (162 098 gegenüber 157 730), rsboot liegt 13,5 % vor axum. Bei Handler-Dispatch und Allokationsprofil pro Anfrage gewinnt actix weiterhin — das wird hier nicht beschönigt.
- Downstream-Aufrufe: 40 gleichzeitige Clients × 20 ms Abhängigkeit, kooperativer Modus **655 QPS / p99 62 ms** gegenüber blockierender Basis **33 QPS / p99 2 323 ms** — 20-facher Unterschied, ausgelöst durch einen einzigen Schalter (`RSBOOT_BLOCKING_IO`).

Diese Zahlen schwanken je nach Maschine. Vergleiche nur innerhalb eines Laufs, nie über Maschinen hinweg und nie gegen absolute Werte aus fremden Blogposts.

## Entwurfsmerkmale

1. **Keine Laufzeitumgebung von Drittanbietern.** HTTP/1.1-Parsing, Keep-Alive-Verwaltung, `writev`-Batching, Poll-Schleife und M:N-Scheduler liegen alle im Repository. Die Abhängigkeitsebene ist serde, nicht tokio. Weniger bewegliche Teile — und keine Versionsbindung an Interna einer Async-Runtime.

2. **Handler bleiben synchron und blockieren den Server trotzdem nicht.** Ein Handler ist `fn(&mut Request, &Context) -> Result<Response, ApiError>`. Stackbasierte Coroutinen halten den Zustand auf einem echten Stack, deshalb lässt sich `&mut Request` über einen Yield-Punkt hinweg halten — etwas, das eine compilererzeugte Zustandsmaschine ohne akrobatische Lifetimes nicht ausdrücken kann.

3. **Zwei Schichten, bewusst getrennt.** Verbindungen werden vom Event-Loop gemultiplext, Anfragen laufen als Coroutinen. Die Coroutine-Anzahl folgt den **laufenden Anfragen**, nicht den offenen Verbindungen — eine Keep-Alive-Verbindung kostet daher wenige Bytes statt eines 64-KB-Stacks.

4. **Datenbank-Sockets geben den Worker frei.** `mysql+co://…` leitet den MySQL-Treiber über coroutine-fähige Sockets: Eine langsame Abfrage parkt die Anfrage, statt einen Worker zu blockieren. Der Hook ist prozessweit, der Modus thread-lokal — zwei Engines im selben Prozess können sich nicht gegenseitig verunreinigen.

5. **Auch Downstream-Aufrufe geben frei.** `ctx.service("name")` liefert Pool, Retry und Circuit Breaker; das Socket-Lesen ist coroutine-fähig, eine langsame Abhängigkeit beeinträchtigt also einen Endpunkt statt des ganzen Servers.

6. **Eine Antwort-Hülle, ein Fehlertyp.** `Api(...)` verpackt Erfolg und Fehler in dieselbe Form; `ApiError` ist der einzige Fehlertyp, den ein Handler zurückgibt, und er besitzt die Statuscode-Zuordnung. SQL-Text und interne Pfade gehen ins Log, nie an den Client.

7. **Sicherheit scheitert beim Start, nicht im Betrieb.** Eine fehlerhafte CORS-Konfiguration verhindert den Start, statt stillschweigend alles zu erlauben. Token-Bucket-Ratenbegrenzung, Security-Header und Maskierung von Passwörtern in Verbindungsstrings sind eingebaut.

8. **Lebenszyklus und kontrolliertes Herunterfahren sind erstklassig.** Module implementieren `init / start / stop / drain` und werden in umgekehrter Reihenfolge geschlossen. Bei SIGTERM geht der Server in eine Drain-Phase (`http.drain_secs`); `/ready` liefert dann 503, damit der Load Balancer den Verkehr abzieht, *bevor* Verbindungen hart gekappt werden; neue Anfragen während des Schließens bekommen 503 statt eines Verbindungsabbruchs.

9. **OpenAPI wächst mit, statt nachträglich angeflanscht zu werden.** `RouteDoc` hängt an der Routendefinition; `/openapi.json` und `/docs` entstehen aus derselben Quelle der Wahrheit wie die Handler.

10. **Geschichtete Konfiguration, dev/prod-Trennung, Hot Reload.** Schichten aus YAML / ini / env, `app.env` schaltet das Verhalten um, ein Datei-Watcher lädt Änderungen ohne Neustart.

## Schnellstart

```bash
cargo run --example hello
curl -i http://127.0.0.1:8000/api/hello/rsboot
```

Minimalform:

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

`serve` installiert die Signalbehandlung selbst: Annahme stoppen, Worker auslaufen lassen, Module in umgekehrter Reihenfolge schließen. Keine zusätzliche Verdrahtung nötig.

## Bekannte Einschränkungen

Klar benannt, denn ein Framework, das seine Lücken verbirgt, ist schlechter als eines, das sie auflistet:

- SSE, WebSocket und JSON-RPC 2.0 sind noch nicht implementiert (geplant als S11–S13)
- gRPC ist bewusst außerhalb des Umfangs: Es benötigt HTTP/2, was `h2` in den Baum zieht und die Behauptung „keine Laufzeitabhängigkeiten“ aufgibt
- Authentifizierung endet bei den Extraktoren `Bearer` / `ApiKey` — ein vollständiges Rollen- und Rechte-System fehlt
- Coroutine-Stacks sind fest 64 KB, 10 000 gleichzeitige Anfragen kosten also rund 640 MB Stack-Speicher
- Das **Verbinden** auf Client-Seite blockiert weiterhin; erst Lesen/Schreiben nach dem Verbindungsaufbau gibt frei
- Das Lesen von Anfrage-Headern und -Bodies auf dem Hauptpfad kann einen Worker noch blockieren

## Lizenz

MIT OR Apache-2.0, nach Wahl.
