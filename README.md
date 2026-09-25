# Heartbeat

Monitor de uptime y visor de logs para tus servicios, autohospedado. Registras cada app
con una URL de health y una de logs, y desde un solo lugar:

- ves si está arriba, lenta o caída, con su historial, latencia y certificado SSL;
- recibes una alerta en Slack, Discord o un webhook cuando se cae o se recupera;
- lees su stdout y stderr en vivo;
- publicas su estado en una página pública (`/status`) o en cualquier web con un
  componente embebible.

Un solo binario de Rust, sin base de datos: todo se guarda en archivos bajo `data/`.

## Funciones

- **Semáforo por app**, cada `UPTIME_INTERVAL_SECS` (60 por defecto):
  - **Up (verde)**: responde 2xx.
  - **Degradado (amarillo)**: responde 2xx pero tarda más que el umbral (global
    `UPTIME_DEGRADED_MS`, o uno propio por app), o su certificado vence en menos de
    `UPTIME_CERT_WARN_DAYS` días. Cuenta como disponible para el % de uptime.
  - **Down (rojo)**: responde con otro status, tarda más de 10 s, no conecta, o la
    respuesta no contiene la palabra clave configurada.

  Una caída se reintenta `UPTIME_RETRIES` veces (5 s entre intentos) antes de marcarse,
  así que un paquete perdido no pinta la app de rojo ni manda una alerta.
- **Alertas** (`ALERT_WEBHOOK_URLS`): solo en cambios de estado confirmados (caída y
  recuperación; degradado es opcional con `ALERT_ON_DEGRADED`). Slack y Discord reciben su
  formato nativo; cualquier otra URL recibe un JSON con el evento.
- **Pausa y mantenimiento**: una app pausada no se consulta, y ese tiempo no cuenta para
  su uptime.
- **Edición**: nombre, URLs, umbral y palabra clave se cambian sin perder el historial ni
  el token del embed.
- **Página de estado pública** (`/status`): solo las apps que marques como públicas, sin
  URLs ni mensajes de error.
- **Embed** para otras webs, con un token por app (ver abajo).
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
- **`password`**: un solo admin local, sin servidor externo:
  ```bash
  echo 'tu-password' | heartbeat hash-password   # imprime el hash argon2
  ```
  y en `.env`: `AUTH_MODE=password`, `ADMIN_EMAIL=...`, `ADMIN_PASSWORD_HASH=<hash>`.

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

## Contrato de los endpoints

Lo que tus apps deben exponer para registrarse.

**Health** (`GET`, sin autenticación): cualquier 2xx cuenta como arriba. Si configuras una
palabra clave, el cuerpo debe contenerla (se leen hasta 256 KB).

**Logs** (`GET`): Heartbeat manda:

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
  URLs `https://` de hosts en `ALLOWED_HOSTS`, sin seguir redirects, y rechazan nombres que
  resuelvan a IPs privadas, loopback o link-local (incluida la de metadatos de la nube,
  `169.254.169.254`). Se valida al guardar y en cada request.
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

- `just demo`: arranca con datos de ejemplo (una app en cada estado) y un admin local;
  entra en `http://localhost:8090` con `demo@example.com` / `demo`. Requiere Python 3.
- `just check`: formato, clippy pedantic y tests, lo mismo que el CI.
- `just e2e`: smoke test en navegador contra un servidor temporal (requiere Node).

## Despliegue

### Con Docker

```bash
cp .env.example .env   # editar
docker compose up -d
```

La imagen corre como usuario sin privilegios y guarda todo en el volumen `./data`.

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

Actualizar sin compilar en el servidor: cada tag `v*` publica un release para Linux x86_64
(`.github/workflows/release.yml`), y `deploy/update.sh` lo instala. No toca `.env` ni
`data/`, verifica el checksum, guarda el binario anterior y comprueba `/healthz`:

```bash
HEARTBEAT_REPO=owner/heartbeat deploy/update.sh          # último release
HEARTBEAT_REPO=owner/heartbeat deploy/update.sh v1.2.0   # uno específico
deploy/update.sh --rollback                               # volver al binario anterior
```

## Stack

- `actix-web` (servidor HTTP) y `tera` (templates del lado del servidor).
- Alpine.js vendorizado en `libs/alpinejs/`: interactividad sin build step.
- IBM Plex servida localmente (`static/fonts`, licencia OFL).
- Textos en `locales/<lang>.json` (servidor) y `static/i18n/<lang>.js` (cliente).
