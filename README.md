# Heartbeat

Monitor de uptime y visor de logs para tus servicios, autohospedado. Registras cada app
con una URL de health y una de logs. Desde un solo lugar ves:

- si está arriba, degradada o caída;
- su historial de 30 días y su latencia;
- su stdout y stderr en vivo.

Además puedes publicar su estado en cualquier web con un componente embebible.

## Cómo funciona

- **Login**: el formulario en `/login` reenvía email y password a `LOGIN_URL`. La
  respuesta debe traer `is_admin`, que decide el servidor de login (idealmente consultando
  su base de datos, no un claim del JWT). Heartbeat no reimplementa esa verificación: solo
  lee el veredicto. Si `is_admin` es `false`, no entra.
- **Sesión**: el `access_token` que regresa el login se queda en memoria del servidor. El
  navegador solo recibe una cookie HttpOnly (`heartbeat_session`) con un ID de sesión
  aleatorio (ver [Seguridad](#seguridad)).
- **Apps registradas**: en `/apps` agregas `{ nombre, URL de logs, URL de health }`. Se
  guardan en un archivo JSON (`APPS_FILE`, por defecto `./data/apps.json`). No hay base de
  datos: son pocas filas que casi no cambian.
- **Uptime**: un loop en background consulta la URL de health de cada app cada
  `UPTIME_INTERVAL_SECS` segundos (60 por defecto). Funciona como semáforo:
  - **Up (verde)**: responde 2xx.
  - **Degradado (amarillo)**: responde 2xx pero tarda más que `UPTIME_DEGRADED_MS` (800
    por defecto). Cuenta como disponible para el % de uptime.
  - **Down (rojo)**: cualquier otra cosa (status no-2xx, timeout de 10 s o error de
    conexión).

  El historial de 30 días se guarda en `UPTIME_DIR/<slug>.jsonl`. `/` es el dashboard de
  uptime y `/logs` lista las apps con sus últimos checks.
- **Ver logs**: el navegador solo le pide logs a Heartbeat (`/api/apps/{slug}/logs`,
  mismo origen, con la cookie). El servidor reenvía la llamada a la URL registrada.
- **Embed en otras webs**: cada app tiene un `embed_token` secreto, que se genera al
  registrarla. En `/apps` está la vista previa y el botón "Copiar código", que da:
  ```html
  <script src="https://HEARTBEAT_HOST/static/embed.js" defer></script>
  <heartbeat-status app="SLUG" token="EMBED_TOKEN"></heartbeat-status>
  ```
  Atributos opcionales:
  - `label="Mi API"`: texto en lugar del nombre de la app.
  - `theme="light"`: el tema por defecto es oscuro.
  - `bars="N"`: máximo de barras; por defecto las que quepan en el ancho, hasta 100.
  - `refresh="30"`: segundos entre actualizaciones.
  - Colores: `color-up`, `color-degraded`, `color-down`, `color-empty`, `color-bg`,
    `color-text`, `color-border`, con cualquier color CSS. También se pueden poner desde el
    CSS de la página con variables (`heartbeat-status { --hb-up: #22c55e; }`); si hay
    ambos, gana el atributo.

  El componente usa Shadow DOM, así que el CSS de la otra web no lo afecta. Solo habla con
  `GET /embed/{slug}?token=...`, que es público, con CORS y sin sesión. Ese endpoint
  devuelve nombre, estado, % de uptime de 24 h y los últimos checks; nunca la URL de health
  ni los mensajes de error. El token queda visible en el HTML de la página que lo incrusta:
  si se filtra, "Rotar token" en `/apps` invalida todos los embeds anteriores.

## Contrato de los endpoints

Lo que tus apps deben exponer para registrarse en Heartbeat.

**Health** (`GET`, sin autenticación): cualquier respuesta 2xx cuenta como arriba. El
cuerpo se ignora.

**Logs** (`GET`): Heartbeat manda estos parámetros y headers:

- `stream=out|error|both` y `lines=N` (query).
- `Authorization: Bearer <access_token del admin>`.
- `X-Admin-Logs-Key: <ADMIN_LOGS_KEY>`, si está configurada.

Y espera JSON con esta forma (las líneas, de la más vieja a la más nueva, como `tail -n`):

```json
{
  "out":   { "lines": ["{\"timestamp\":\"...\",\"level\":\"INFO\",\"target\":\"...\",\"fields\":{...}}"], "error": null },
  "error": { "lines": ["texto crudo de stderr"], "error": null }
}
```

- `out.lines`: una línea JSON por evento, en el formato de
  [`tracing_subscriber::fmt::format::Json`](https://docs.rs/tracing-subscriber/latest/tracing_subscriber/fmt/format/struct.Json.html).
  El visor las parsea para filtrar por nivel y módulo.
- `error.lines`: texto crudo.
- Un `error` por stream (string) indica que ese stream no se pudo leer.

La app debe autorizar la llamada. Puede validar el JWT del usuario o, para ver logs entre
ambientes, aceptar la llave compartida del header `X-Admin-Logs-Key`, comparándola en
tiempo constante con su propio `ADMIN_LOGS_KEY`.

**Por qué existe la llave compartida**: si cada ambiente (test, prod) tiene su propia base
de usuarios, un JWT emitido por el login de prod no significa nada en test. Reenviar solo
el token funciona para apps del mismo ambiente que `LOGIN_URL`, pero falla contra
cualquier otro. Con `ADMIN_LOGS_KEY`, ser admin en el ambiente donde hiciste login basta
para ver los logs de cualquier app registrada.

## Seguridad

- **Sesiones del lado del servidor**: la cookie solo lleva un ID aleatorio de 256 bits; el
  JWT se queda en memoria del proceso. Una cookie inventada no da acceso y "Salir"
  invalida la sesión de verdad. Reiniciar el proceso cierra todas las sesiones.
- **Política de salida** (`src/outbound.rs`): el proxy de logs y el monitor de uptime solo
  hacen requests a URLs `https://` cuyo host esté en `ALLOWED_HOSTS`, no siguen redirects
  y rechazan nombres que resuelvan a IPs privadas, loopback o link-local (incluida la de
  metadatos de la nube, `169.254.169.254`).
  - Se valida al registrar la app y otra vez en cada request, así que las entradas viejas
    tampoco pasan.
  - Esas requests llevan `ADMIN_LOGS_KEY` y el JWT del admin, que nunca salen hacia otro
    host ni en texto plano.
- El proxy de logs no devuelve el cuerpo crudo cuando la respuesta no es JSON.
- En AWS, forzar IMDSv2 en la instancia como defensa extra
  (`aws ec2 modify-instance-metadata-options --http-tokens required`).

## Correr localmente

```bash
cp .env.example .env
# editar LOGIN_URL y ALLOWED_HOSTS (obligatorias); para desarrollo sin TLS:
#   COOKIE_SECURE=false
cargo run
```

Abre `http://localhost:8090`.

## Variables de entorno (`.env`)

| Variable | Por defecto | |
|---|---|---|
| `LOGIN_URL` | — | **Obligatoria.** Endpoint de login (ver arriba). |
| `ALLOWED_HOSTS` | — | **Obligatoria.** Hosts permitidos para las URLs registradas, separados por comas; `*.dominio` = cualquier subdominio. |
| `HOST` / `PORT` | `0.0.0.0` / `8090` | |
| `APPS_FILE` | `./data/apps.json` | Registro de apps. |
| `COOKIE_SECURE` | `true` | Ponerla en `false` solo en desarrollo sin TLS. |
| `ADMIN_LOGS_KEY` | — | Llave compartida para ver logs entre ambientes. |
| `UPTIME_DIR` | `./data/uptime` | Historial de checks. |
| `UPTIME_INTERVAL_SECS` | `60` | Segundos entre checks. |
| `UPTIME_DEGRADED_MS` | `800` | Umbral de "degradado". |

## Despliegue (Ubuntu 24.04)

Binario Rust bajo pm2, con nginx como reverse proxy y TLS de Let's Encrypt. La app
escucha en `8090`; ese puerto nunca se expone directo, solo nginx habla con
`localhost:8090`.

```bash
# En el servidor, con el código ya ahí:
./prepare_ec2.sh /arena/heartbeat   # instala nginx/certbot/node/pm2/rust y compila

cp .env.example .env
# editar .env: LOGIN_URL, ALLOWED_HOSTS, COOKIE_SECURE=true y, si aplica, ADMIN_LOGS_KEY

pm2 start ./target/release/heartbeat --name heartbeat --cwd /arena/heartbeat
pm2 save && pm2 startup   # para que sobreviva a un reboot
```

Luego, nginx y certbot (ver `deploy/nginx.conf.example`):

```bash
sudo cp deploy/nginx.conf.example /etc/nginx/sites-available/status.example.com
# editar REPLACE_ME_DOMAIN -> tu dominio
sudo ln -s /etc/nginx/sites-available/status.example.com /etc/nginx/sites-enabled/
sudo nginx -t && sudo systemctl reload nginx
sudo certbot --nginx -d status.example.com
```

En tu proveedor de DNS, crea un registro A del dominio apuntando a la IP pública del
servidor.

Para actualizar: `git pull && cargo build --release && pm2 restart heartbeat`.

## Stack

- `actix-web`: servidor HTTP.
- `tera`: templates del lado del servidor (`templates/*.html`).
- Alpine.js (vendorizado en `libs/alpinejs/`): interactividad sin build step ni framework
  de frontend.
- Sin base de datos: el registro de apps y el historial de uptime son archivos en disco.
