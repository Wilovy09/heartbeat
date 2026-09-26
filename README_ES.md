# Heartbeat

[English](README.md) | **Español**

Monitor de uptime y visor de logs para tus servicios, autohospedado. Registras cada app
con una URL de health y una de logs, y desde un solo lugar:

- ves si está arriba, lenta o caída, con su historial, latencia y certificado SSL;
- recibes una alerta en Slack, Discord o un webhook cuando se cae o se recupera;
- lees su stdout y stderr en vivo;
- publicas su estado en su propia página pública (`/status/{slug}`) o en cualquier web con un
  componente embebible.

Un solo binario de Rust, sin base de datos: todo se guarda en archivos bajo `data/`.

## Capturas

### Dashboard

<details open>
  <summary>Ver captura</summary>
<img src=".github/public/dashboard_demo.png" alt="Dashboard: estado de todas las apps, barra de conteo por estado, apps que requieren atención y eventos recientes"/>
</details>

### Dashboard de una app

<details>
  <summary>Ver captura</summary>
<img src=".github/public/app_dashboard_demo.png" alt="Detalle de una app: latencia actual, uptime, gráfica de tiempo de respuesta, incidentes con MTTR y eventos"/>
</details>

### Logs

<details>
  <summary>Ver captura</summary>
<img src=".github/public/logs_demo.png" alt="Logs: lista de apps con su tira de checks para abrir su stdout y stderr"/>
</details>

### Logs de una app

<details>
  <summary>Ver captura</summary>
<img src=".github/public/app_logs_demo.png" alt="Visor de logs: stdout y stderr lado a lado, con filtros por nivel y módulo, búsqueda y modo en vivo"/>
</details>

### Apps

<details>
  <summary>Ver captura</summary>
<img src=".github/public/apps_demo.png" alt="Apps: estado de cada app, menú de pausa, edición y publicación con embed y badge"/>
</details>

### Avisos

<details>
  <summary>Ver captura</summary>
<img src=".github/public/notice_demo.png" alt="Avisos: publicar avisos de incidente en la página de estado de una app o de todas"/>
</details>

### Página de estado de una app

<details>
  <summary>Ver captura</summary>
<img src=".github/public/status_app_demo.png" alt="Página de estado pública de una app: titular, avisos abiertos, 30 días de uptime diario e incidentes recientes"/>
</details>

### Configuración

<details>
  <summary>Ver captura</summary>
<img src=".github/public/settings_demo.png" alt="Configuración: webhooks, editor de mensajes de alerta con vista previa estilo Slack, menciones y configuración actual"/>
</details>

## Funciones

- **Semáforo por app**, cada `UPTIME_INTERVAL_SECS` (60 por defecto) o con el intervalo
  propio de la app:
  - **Up (verde)**: responde 2xx, o uno de los códigos esperados de la app (p. ej. `401`).
  - **Degradado (amarillo)**: responde 2xx pero tarda más que el umbral (global
    `UPTIME_DEGRADED_MS`, o uno propio por app), o su certificado vence en menos de
    `UPTIME_CERT_WARN_DAYS` días. Cuenta como disponible para el % de uptime.
  - **Down (rojo)**: responde con otro status, tarda más que el timeout
    (`UPTIME_TIMEOUT_SECS`, 10 s, o uno propio por app), no conecta, o la respuesta no
    contiene la palabra clave configurada.

  Cada app puede mandar headers propios en el chequeo (p. ej. un token), y una URL
  `tcp://host:puerto` chequea servicios que no son HTTP (bases de datos, colas) abriendo
  una conexión, con la misma política de hosts.

  Una caída se reintenta `UPTIME_RETRIES` veces (5 s entre intentos) antes de marcarse,
  así que un paquete perdido no pinta la app de rojo ni manda una alerta.
- **Alertas** (`ALERT_WEBHOOK_URLS`): solo en cambios de estado confirmados (caída y
  recuperación; degradado es opcional con `ALERT_ON_DEGRADED`), con el nombre de la app y
  un link a ella. Slack y Discord reciben su formato nativo; cualquier otra URL recibe un
  JSON con el evento. `ALERT_MENTIONS` etiqueta a gente cuando una app se cae: `here`,
  `channel`, IDs de usuario (`U…`) o grupo (`S…`) de Slack, usuarios o roles (`&…`) de
  Discord; ver `.env.example`.
  - Cada app puede sumar **sus propios webhooks y menciones** desde `/apps`, además de los
    globales, y probarlos desde ahí.
  - Un envío fallido se **reintenta** (2 s, 10 s, 30 s) y `/settings` muestra el último
    resultado de cada webhook.
  - Mientras una app siga caída, llega un **recordatorio** cada `ALERT_REMIND_MINS` (60
    por defecto) con cuánto tiempo lleva caída.
  - **Caída masiva**: si caen a la vez al menos 3 apps y el `UPTIME_MASS_DOWN_PCT` (50 %)
    de las monitoreadas, lo más probable es que falle la red de Heartbeat, no las apps.
    Llega un solo aviso y las alertas individuales esperan; al terminar, solo avisan las
    apps que siguen caídas.
- **Página de configuración** (`/settings`): haz ping a cada webhook y ve su respuesta
  ("pong"), edita el texto de cada tipo de alerta (caída, sigue caída, degradada,
  recuperación) con vista previa en vivo y las variables `{app}`, `{message}`,
  `{latency}`, `{link}`, `{mentions}` y `{duration}`, envía una prueba de cada una y consulta la configuración actual (los
  secretos solo aparecen como configurados o no). Los textos editados se guardan en
  `ALERT_TEMPLATES_FILE` y aplican sin reiniciar; los de por defecto siguen `APP_LANG`.
- **Frontends y SPAs**: la URL de logs es opcional, así que una app puede ser solo de
  monitoreo. Para SPAs (Vue, React…), "Verificar bundles JS/CSS" lee las referencias
  `<script>`/`<link rel="stylesheet">` de la página y confirma que cada una cargue como
  JS/CSS de verdad. El servidor de una SPA responde 200 con el mismo `index.html` en
  todas las rutas (muchas veces incluso para un bundle que no existe), así que un deploy
  incompleto que deja la página en blanco se vería como arriba.
- **Pausa y mantenimiento**: una app pausada no se consulta, y ese tiempo no cuenta para
  su uptime. La pausa puede durar un tiempo fijo (1 h a 7 días) y termina sola; una pausa
  sin fin de más de un día se marca en el dashboard, por si se olvidó.
- **Edición**: todo se cambia sin perder el historial ni el token del embed.
- **Incidentes**: cada racha de chequeos caídos es un incidente con inicio, duración y
  causa; el detalle de cada app muestra los de la ventana, el MTTR y el tiempo total caído.
- **Página de estado pública por app** (`/status/{slug}`): solo para las apps que
  publiques, sin URLs ni mensajes de error. Muestra el estado actual, 30 días de uptime
  diario y los **avisos** que publiques desde `/notices` (investigando, identificado,
  monitoreando, resuelto) para esa app; un aviso sin apps elegidas aparece en todas. Los
  resueltos quedan 7 días como incidentes recientes. Cualquier otro slug responde el
  mismo 404.
- **Embed** para otras webs, con un token por app, y **badge SVG** para READMEs (ver
  abajo).
- **Integraciones**: `GET /metrics` en formato Prometheus (con `METRICS_TOKEN`) y
  exportación del historial de cada app a CSV o JSON desde el dashboard.
- **Visor de logs** con filtros por nivel, módulo y texto.
- **Dead man's switch** (`HEARTBEAT_PING_URL`): Heartbeat hace ping a esa URL después de
  cada ronda, para que un servicio externo te avise si Heartbeat mismo se detiene.
  `GET /healthz` responde `ok` para balanceadores.
- **Interfaz en español o inglés** (`APP_LANG`).

## Autenticación

Dos modos (`AUTH_MODE`):

- **`upstream`** (por defecto): el login reenvía email y password a `LOGIN_URL`. La
  respuesta debe traer `access_token` e `is_admin`; esa decisión es del servidor de login,
  Heartbeat no la reimplementa. El token se reenvía como Bearer a los endpoints de logs.
- **`password`**: un admin local, sin servidor externo:
  ```bash
  echo 'tu-password' | heartbeat hash-password   # imprime el hash argon2
  ```
  y en `.env`: `AUTH_MODE=password`, `ADMIN_EMAIL=...`, `ADMIN_PASSWORD_HASH=<hash>`.

**Solo lectura**: un viewer ve el dashboard y los datos de uptime, pero no logs, apps,
avisos ni configuración, y no puede cambiar nada. En modo `password` se define con
`VIEWER_EMAIL` + `VIEWER_PASSWORD_HASH`; en modo `upstream`, `UPSTREAM_VIEWERS=true` deja
entrar como viewers a los usuarios sin `is_admin`.

En los dos modos, la cookie solo lleva un ID de sesión aleatorio; el token queda en el
servidor (`SESSIONS_FILE`, permisos 600), así que las sesiones sobreviven a un reinicio.

## Embed

Cada app tiene un `embed_token`. En `/apps` está la vista previa y el botón "Copiar
código":

```html
<script src="https://HEARTBEAT_HOST/static/embed.js" defer></script>
<heartbeat-status app="SLUG" token="EMBED_TOKEN"></heartbeat-status>
```

Atributos opcionales:
- `label="Mi API"`: texto en lugar del nombre de la app.
- `theme="light"`: el tema por defecto es oscuro.
- `lang="en"`: `es` o `en`; por defecto, el `APP_LANG` del servidor.
- `bars="N"`: máximo de barras; por defecto las que quepan en el ancho, hasta 100.
- `refresh="30"`: segundos entre actualizaciones.
- Colores: `color-up`, `color-degraded`, `color-down`, `color-empty`, `color-bg`,
  `color-text`, `color-border`, con cualquier color CSS. También desde el CSS de la página
  (`heartbeat-status { --hb-up: #22c55e; }`); si hay ambos, gana el atributo.

El componente usa Shadow DOM y solo habla con `GET /embed/{slug}?token=...` (público, con
CORS), que nunca devuelve la URL de health ni los mensajes de error. El token queda
visible en el HTML de la página: si se filtra, "Rotar token" en `/apps` invalida todos los
embeds anteriores.

**Badge**, con el mismo token, para un README o una wiki:

```markdown
![Estado](https://HEARTBEAT_HOST/badge/SLUG.svg?token=EMBED_TOKEN&label=API)
```

El estado elige el texto y el color; el texto sigue `APP_LANG`:

| Estado de la app | Badge |
|---|---|
| Up | <img src=".github/public/badges/es-up.svg" alt="API: Operativo"/> |
| Degradado (lento o con el certificado por vencer) | <img src=".github/public/badges/es-degraded.svg" alt="API: Lento"/> |
| Down | <img src=".github/public/badges/es-down.svg" alt="API: Caído"/> |
| Pausada, incluido el mantenimiento programado | <img src=".github/public/badges/es-paused.svg" alt="API: En mantenimiento"/> |
| Sin chequeos todavía, o sin URL de health | <img src=".github/public/badges/es-unknown.svg" alt="API: Sin datos"/> |

Parámetros:
- `token` (obligatorio): el token del embed de la app. Uno incorrecto responde el mismo
  404 que un slug que no existe.
- `label` (opcional): texto de la izquierda en lugar del nombre de la app, hasta 40
  caracteres.

Se cachea 60 s y responde con CORS abierto. "Rotar token" en `/apps` también invalida los
badges ya publicados.

## Métricas

Con `METRICS_TOKEN` configurado, `GET /metrics` expone el estado de cada app en formato
Prometheus (`heartbeat_up`, `heartbeat_degraded`, `heartbeat_paused`,
`heartbeat_latency_milliseconds`, `heartbeat_uptime_24h_ratio`,
`heartbeat_uptime_30d_ratio`, `heartbeat_cert_expiry_timestamp_seconds`):

```yaml
scrape_configs:
  - job_name: heartbeat
    scheme: https
    authorization: { credentials: METRICS_TOKEN }
    static_configs: [{ targets: ["HEARTBEAT_HOST"] }]
```

## Contrato de los endpoints

Lo que tus apps deben exponer para registrarse.

**Health** (`GET`, sin autenticación): cualquier 2xx cuenta como arriba. Si configuras una
palabra clave, el cuerpo debe contenerla (se leen hasta 256 KB).

**Logs** (`GET`, opcional: las apps sin él son solo de monitoreo): Heartbeat manda:

- `stream=out|error|both` y `lines=N` (query).
- `Authorization: Bearer <access_token del admin>` (modo `upstream`).
- `X-Admin-Logs-Key: <ADMIN_LOGS_KEY>`, si está configurada.

Y espera JSON con esta forma (líneas de la más vieja a la más nueva, como `tail -n`):

```json
{
  "out":   { "lines": ["{\"timestamp\":\"...\",\"level\":\"INFO\",\"target\":\"...\",\"fields\":{...}}"], "error": null },
  "error": { "lines": ["texto crudo de stderr"], "error": null }
}
```

- `out.lines`: una línea JSON por evento, en el formato de
  [`tracing_subscriber::fmt::format::Json`](https://docs.rs/tracing-subscriber/latest/tracing_subscriber/fmt/format/struct.Json.html).
- `error.lines`: texto crudo.
- Un `error` por stream (string) indica que ese stream no se pudo leer.

La app autoriza la llamada con el JWT del usuario o, para ver logs entre ambientes, con la
llave compartida del header `X-Admin-Logs-Key` (comparada en tiempo constante).

## Seguridad

- **Sesiones del lado del servidor** con IDs de 256 bits; una cookie inventada no da
  acceso y "Salir" (un POST) invalida la sesión de verdad.
- **CSRF**: todo POST exige un `Origin` (o `Referer`) del mismo host.
- **Headers**: CSP (solo recursos del propio origen), `frame-ancestors 'none'` y
  `X-Frame-Options: DENY` (sin clickjacking), `nosniff`, `Referrer-Policy`.
- **Límite de intentos de login**: 5 fallos en 15 minutos bloquean esa IP y ese email.
  Detrás de un proxy local, la IP real se toma de `X-Real-IP` solo si la conexión viene
  de loopback.
- **Política de salida** (`src/outbound.rs`): el proxy de logs y los checks solo van a
  URLs `https://` (o `tcp://`) de hosts en `ALLOWED_HOSTS`, sin seguir redirects, y
  rechazan nombres que resuelvan a IPs privadas, loopback o link-local (incluida la de
  metadatos de la nube, `169.254.169.254`). Se valida al guardar y en cada request. Los
  webhooks propios de cada app deben ser `https://` y tampoco pueden apuntar a IPs
  privadas.
- El proxy de logs no devuelve el cuerpo crudo de respuestas que no son JSON.
- Las fuentes y todos los assets se sirven desde el propio servidor.

## Configuración

Todas las variables, con su valor por defecto, están en [`.env.example`](.env.example).
Solo dos son obligatorias:

- `ALLOWED_HOSTS`: los hosts a los que pueden apuntar las URLs registradas.
- `LOGIN_URL` en modo `upstream`, o `ADMIN_EMAIL` + `ADMIN_PASSWORD_HASH` en modo
  `password`.

## Correr localmente

```bash
cp .env.example .env    # editar ALLOWED_HOSTS y la autenticación; COOKIE_SECURE=false sin TLS
cargo run
```

Abre `http://localhost:8090`.

Con [`just`](https://github.com/casey/just):

- `just demo` (español) o `just demo en` (inglés): arranca con datos de ejemplo (una app en cada estado) y un admin local;
  entra en `http://localhost:8090` con `demo@example.com` / `demo` (admin) o
  `viewer@example.com` / `demo` (solo lectura). Los logs de cada app se generan en vivo
  (feature `demo`, nunca en un release) según su estado: la caída tiene errores y panics,
  la estable casi todo INFO. Requiere Python 3.
- `just check`: formato, `cargo deny` (vulnerabilidades y licencias), clippy pedantic y
  tests, lo mismo que el CI.
- `just e2e`: smoke test en navegador contra un servidor temporal (requiere Node).

## Despliegue

### Con Docker

```bash
cp .env.example .env   # editar
docker compose up -d
```

La imagen corre como usuario sin privilegios y guarda todo en el volumen `./data`. Cada
release también publica una imagen multi-arquitectura (amd64 y arm64):

```bash
docker run -d --env-file .env -p 8090:8090 -v ./data:/app/data ghcr.io/OWNER/heartbeat:latest
```

### En un servidor (pm2 + nginx)

Binario bajo pm2, con nginx como reverse proxy y TLS de Let's Encrypt. La app escucha en
`8090`; ese puerto nunca se expone directo.

Primera vez:

```bash
./prepare_ec2.sh /arena/heartbeat   # instala nginx/certbot/node/pm2/rust y compila
cp .env.example .env                # editar
pm2 start ./target/release/heartbeat --name heartbeat --cwd /arena/heartbeat
pm2 save && pm2 startup
```

nginx y certbot (ver `deploy/nginx.conf.example`):

```bash
sudo cp deploy/nginx.conf.example /etc/nginx/sites-available/status.example.com
# editar REPLACE_ME_DOMAIN -> tu dominio
sudo ln -s /etc/nginx/sites-available/status.example.com /etc/nginx/sites-enabled/
sudo nginx -t && sudo systemctl reload nginx
sudo certbot --nginx -d status.example.com
```

Actualizar sin compilar en el servidor: cada tag `v*` publica un release para Linux
x86_64 y aarch64 (`.github/workflows/release.yml`), y `deploy/update.sh` instala el de la
arquitectura del servidor. Templates, estáticos y librerías van dentro del binario. No toca `.env` ni
`data/`, verifica el checksum, guarda el binario anterior y comprueba `/healthz`:

```bash
HEARTBEAT_REPO=owner/heartbeat deploy/update.sh          # último release
HEARTBEAT_REPO=owner/heartbeat deploy/update.sh v1.2.0   # uno específico
deploy/update.sh --rollback                               # volver al binario anterior
```

## Límites

Todo el historial vive en memoria y en un archivo JSONL por app, compactado cada hora.
Con un chequeo por minuto y 30 días de retención son unos 43 000 registros por app: va
bien hasta unas 100 apps. Más allá conviene subir el intervalo, bajar la retención o
pasar el almacenamiento a SQLite.

Todos los chequeos salen de una sola máquina: la detección de caída masiva evita la
tormenta de alertas cuando falla su red, pero no reemplaza sondas desde varias regiones.

## Licencia

[Elastic License 2.0](LICENSE) (ELv2). Puedes usar, copiar, modificar y redistribuir
Heartbeat, incluso dentro de tu empresa y para los servicios de tus clientes, pero no
puedes ofrecerlo a terceros como servicio hosteado o administrado (un SaaS construido
sobre él). Es código disponible (source-available), no una licencia open source
aprobada por la OSI.

## Stack

- `actix-web` (servidor HTTP) y `tera` (templates del lado del servidor), con templates,
  estáticos y librerías embebidos en el binario (`rust-embed`; en debug se leen del
  disco).
- Alpine.js vendorizado en `libs/alpinejs/`: interactividad sin build step.
- IBM Plex servida localmente (`static/fonts`, licencia OFL).
- Textos en `locales/<lang>.json` (servidor) y `static/i18n/<lang>.js` (cliente).
