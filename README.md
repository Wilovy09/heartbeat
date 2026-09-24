# Heartbeat

Dashboard interno para ver los logs de las apps de Adquiere que exponen un endpoint tipo
`GET /admin/logs` (ver `pulso-backend`'s `GET /api/v1/admin/logs` para la referencia de
contrato). Registras cada app con un nombre + la URL de su endpoint de logs, y desde un
solo lugar ves/filtras/parseas el JSON de stdout y el texto crudo de stderr de todas.

## Cómo funciona

- **Login**: el formulario en `/login` reenvía email/password a `LOGIN_URL` (por defecto,
  el login de `pulso-backend`). Esa respuesta ya trae `is_admin` — calculado ahí mismo
  contra su base de datos, no a partir de lo que diga el JWT — así que esta app no
  reimplementa la verificación de admin, solo lee el veredicto. Si `is_admin` es `false`,
  no entra.
- **Sesión**: el `access_token` que regresa el login se guarda tal cual en una cookie
  HttpOnly (`heartbeat_session`). Nunca llega a JavaScript del navegador.
- **Apps registradas**: en `/apps`, agregas `{ nombre, url del endpoint de logs }`.
  Se guardan en un archivo JSON (`APPS_FILE`, por defecto `./data/apps.json`) — no hay
  base de datos, son un puñado de filas que casi no cambian.
- **Uptime**: cada app registrada lleva una URL de health. Un loop en background la
  consulta cada `UPTIME_INTERVAL_SECS` (60 por defecto), como semáforo: 2xx = up (verde),
  2xx pero más lento que `UPTIME_DEGRADED_MS` (1000 por defecto) = degradado (amarillo, cuenta
  como disponible para el % de uptime), cualquier otra cosa
  (status no-2xx, timeout de 10 s, error de conexión) = down. El historial de los últimos
  30 días se guarda en `UPTIME_DIR/<slug>.jsonl`. `/` muestra el dashboard de uptime
  (estado, % de uptime 24 h / 30 días, latencia, gráfica y eventos); `/logs` muestra una
  card por app con sus últimos heartbeats.
- **Embed en otras webs**: cada app tiene un `embed_token` secreto (se genera al
  registrarla). En `/apps` está la vista previa y el botón "Copiar código", que da:
  ```html
  <script src="https://HEARTBEAT_HOST/static/embed.js" defer></script>
  <heartbeat-status app="SLUG" token="EMBED_TOKEN"></heartbeat-status>
  ```
  Atributos opcionales: `label="Mi API"` (texto en lugar del nombre de la app),
  `theme="light"`, `bars="N"` (máximo de barras; por defecto las que quepan en el ancho,
  hasta 100), `refresh="30"` (segundos) y colores — `color-up`, `color-degraded`,
  `color-down`, `color-empty`, `color-bg`, `color-text`, `color-border`,
  `color-pill-text` — con cualquier color CSS. Los colores también se pueden poner desde
  el CSS de la página con variables (`heartbeat-status { --hb-up: #22c55e; }`); el
  atributo gana. El
  componente usa Shadow DOM (el CSS de la otra web no lo afecta) y solo habla con
  `GET /embed/{slug}?token=...` — público, con CORS, sin sesión — que devuelve nombre,
  estado, % de uptime 24 h y los últimos heartbeats; nunca la URL de health ni los
  mensajes de error. El token queda visible en el HTML de la página que lo incruste: si se
  filtra donde no debe, "Rotar token" en `/apps` invalida todos los embeds viejos.
- **Ver logs**: el navegador solo le pide logs a esta misma app (`/api/apps/{slug}/logs`,
  mismo origen, cookie automática). El servidor reenvía esa llamada a la URL real
  registrada.
- **Por qué no basta con reenviar el token del usuario**: cada ambiente (test, prod) tiene
  su propia base de datos de usuarios/roles, provisionada por separado — un JWT emitido por
  el login de prod no tiene fila correspondiente en la base de test, aunque sea la misma
  persona. Reenviar solo el token del usuario funcionaría para apps del MISMO ambiente que
  `LOGIN_URL`, pero fallaría (401/403) contra cualquier otro. Por eso, además del token,
  cada llamada manda el header `X-Admin-Logs-Key` con `ADMIN_LOGS_KEY` — una llave
  compartida que cada app registrada puede aceptar sin depender de su propia tabla de
  usuarios (ver `pulso-backend`'s `routes::logs::admin_logs_key_matches`). Ser admin en el
  ambiente donde hiciste login basta para ver logs de cualquier app registrada, sea cual
  sea su ambiente. Una app que no reconozca el header simplemente sigue validando por el
  token del usuario, como antes.

## Correr localmente

```bash
cp .env.example .env
# para desarrollo sin TLS:
#   COOKIE_SECURE=false
cargo run
```

Abre `http://localhost:8090`.

## Variables de entorno (`.env`)

```env
HOST=0.0.0.0
PORT=8090
LOGIN_URL=https://api-pulso-test.adquiere.co/api/v1/auth/login
APPS_FILE=./data/apps.json
COOKIE_SECURE=true
ADMIN_LOGS_KEY=
UPTIME_DIR=./data/uptime
UPTIME_INTERVAL_SECS=60
UPTIME_DEGRADED_MS=1000
```

## Despliegue en EC2 (Ubuntu 24.04)

Misma forma que `pulso-backend`: binario Rust corriendo bajo pm2, nginx como reverse proxy
con TLS de Let's Encrypt. Puerto de la app: `8090` (interno, nunca expuesto directo —
solo nginx habla con `localhost:8090`).

```bash
# En el servidor, con el código ya ahí (git clone o transferido):
./prepare_ec2.sh /arena/heartbeat   # instala nginx/certbot/node/pm2/rust, compila

cp .env.example .env
# editar .env con valores reales de PROD -- en particular:
#   LOGIN_URL=https://api-pulso.adquiere.co/api/v1/auth/login   (prod: la fuente real de
#     "quién es admin" -- ver la sección de arriba sobre por qué no importa en qué
#     ambiente vive la app que vas a registrar)
#   COOKIE_SECURE=true
#   ADMIN_LOGS_KEY=<mismo valor que en cada app registrada>

pm2 start ./target/release/heartbeat --name heartbeat --cwd /arena/heartbeat
pm2 save && pm2 startup   # para que sobreviva a un reboot
```

Luego, nginx + certbot (ver `deploy/nginx.conf.example`):

```bash
sudo cp deploy/nginx.conf.example /etc/nginx/sites-available/logs.adquiere.co
# editar REPLACE_ME_DOMAIN -> el subdominio real elegido
sudo ln -s /etc/nginx/sites-available/logs.adquiere.co /etc/nginx/sites-enabled/
sudo nginx -t && sudo systemctl reload nginx
sudo certbot --nginx -d logs.adquiere.co
```

Y en el proveedor de DNS: registro A del subdominio elegido apuntando a la IP pública de
esta instancia.

Después de un `git pull`/actualización de código: `cargo build --release && pm2 restart heartbeat`.

## Stack

- `actix-web` — servidor HTTP.
- `tera` — templates server-side (`templates/*.html`).
- Alpine.js (vendorizado en `libs/alpinejs/`) — interactividad del dashboard (filtros,
  auto-refresh, expandir línea) sin build step ni framework de frontend.
- Sin base de datos: el registro de apps es un archivo JSON en disco.
