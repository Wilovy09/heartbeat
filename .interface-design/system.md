# Heartbeat — sistema de diseño

## Dirección

**Monitor de signos vitales.** Una sala de monitoreo de noche: fría, precisa y densa.
Mientras todo está verde, la pantalla está tranquila. Solo llama la atención cuando algo
falla.

- **Quién la usa:** el equipo que opera los servicios y revisa si siguen vivos. Casi
  siempre de un vistazo; entra al detalle solo cuando algo falla.
- **Qué hace:** confirmar que todo está bien, o encontrar lo que falló y ver por qué.
- **Elemento propio:** el punto de estado (`.vital`). Late como un corazón si la app está
  arriba, más lento si está degradada y se queda quieto (línea plana) si está caída. La
  gráfica de latencia va sobre cuadrícula de papel de ECG. Las listas se ordenan por
  triage: caídas, degradadas, arriba, sin datos.

## Tokens (`templates/base.html`, `:root`)

Un solo tono; las superficies solo cambian en luminosidad.

| Token | Valor | Uso |
|---|---|---|
| `--glass` | `#0b0c0e` | Fondo de la página (el vidrio del monitor) |
| `--bay` | `#111316` | Paneles |
| `--bay-raised` | `#171a1e` | Hover, elemento seleccionado, tooltips |
| `--well` | `#08090a` | Inputs, bloques de código, control segmentado (hundidos) |
| `--graticule-soft` | `rgba(255,255,255,.045)` | Divisores dentro de un panel |
| `--graticule` | `rgba(255,255,255,.075)` | Borde de panel |
| `--graticule-strong` | `rgba(255,255,255,.12)` | Borde de inputs y botones secundarios |
| `--ink` … `--ink-4` | `#e7e9ec` `#a4a9b1` `#6e737c` `#454a52` | Texto: primario, secundario, metadatos, deshabilitado |
| `--rhythm-up` | `#3fd68a` | Estado arriba |
| `--rhythm-degraded` | `#f0b43c` | Estado degradado |
| `--rhythm-down` | `#f0514e` | Estado caído |
| `--rhythm-flat` | `#262a30` | Sin datos, barra vacía |
| `--pulse` | `#dd1818` | Marca. **Solo en el logo, nunca como estado.** |

Reglas de color:
- El color **solo comunica estado**. No hay acento de marca en la interfaz.
- Las acciones son neutras: el botón primario es `--ink` sobre `--glass`.
- Un número se colorea solo si importa. En el conteo por estado, degradado y caído van en
  color solo cuando son mayores que cero.
- `static/uptime.js` (constante `COLOR`) y `static/embed.js` repiten los valores de
  estado, porque el SVG y el Shadow DOM no leen estas variables. Si se cambia un color,
  hay que cambiarlo en los tres lugares.
- Los nombres viejos (`--bg`, `--panel`, `--border`, `--text`, `--muted`, `--accent`,
  etc.) siguen como alias porque los usa `log_viewer.html`. No se deben usar en código
  nuevo.

## Tipografía

- **IBM Plex Sans** para etiquetas y texto; **IBM Plex Mono** con `tabular-nums` para toda
  lectura: latencia, porcentajes, horas, slugs y URLs.
- Escala 1.25 sobre 14 px: **11 / 12 / 13 / 14 / 16 / 22 / 28 / 44**.
- La jerarquía sale del peso y el color antes que del tamaño: valor 500–600 en `--ink`,
  etiqueta 500 en `--ink-3`.
- Etiqueta de lectura o sección (`.readout-label`, `.bay-title`): 11 px, peso 500,
  mayúsculas, espaciado 0.06em, `--ink-3`.
- Títulos: 22 px/600 con tracking −0.015em (`.page-title`). El veredicto del resumen:
  28 px/600, −0.02em.
- La lectura principal del detalle: mono 44 px/500, −0.03em, con la unidad en 16 px
  `--ink-3`.

## Profundidad, espaciado y radios

- **Profundidad: solo bordes.** Bordes a baja opacidad más los cambios mínimos de
  superficie. No hay sombras, salvo el tooltip de la gráfica.
- **Espaciado:** base 4 px, densidad de mesa de trabajo. Paneles de 16 px (20 px en los de
  foco). Filas de 10–12 px. Separación de 16 px entre paneles y 24 px entre columnas.
- **Radios:** 2 px en barras, 6 px en botones e inputs, 8 px en filas y el control
  segmentado, 10 px en paneles.
- **Movimiento:** 120 ms en color y fondo, `scale(.97)` al presionar un botón, curva
  `--ease-out` (`cubic-bezier(.23,1,.32,1)`). Se respeta `prefers-reduced-motion`.

## Componentes

- **Barra superior (`.topbar`):** 52 px, fija, fondo `--glass` al 86% con blur. Logo de
  22 px más "Heartbeat" 15 px/600. Pestañas de 13 px/500; la activa lleva `--ink` sobre
  `--bay-raised`.
- **Panel (`.bay` / `.card`):** `--bay`, borde `--graticule`, radio 10 px, padding 16 px.
  Los divisores internos usan `--graticule-soft`.
- **Botón:** 32 px de alto, padding 0 12 px, radio 6 px, 13 px/500.
  - `primario`: `--ink` sobre `--glass`.
  - `.secondary`: transparente con borde `--graticule-strong`.
  - `.ghost`: solo texto `--ink-3`.
  - `.danger`: texto `--rhythm-down` con borde rojo al 28%. **Nunca un bloque rojo
    sólido.**
- **Input:** 34 px de alto, fondo `--well`, borde `--graticule-strong`, radio 6 px. Las
  URLs se capturan en mono 12 px (`.mono-input`).
- **Punto de estado (`.vital`):** 8 px; 14 px en el veredicto (`.vital-lg`). El latido es
  un `::after` que escala a 2.6× y se desvanece: 2.4 s arriba, 3.6 s degradado. Caído: sin
  latido y con un halo rojo de 3 px. Sin datos: gris con borde interno. Pausada
  (`.st-paused`): anillo hueco en `--ink-3`, sin latido.
- **Tira de checks (`.beats` / `.beat`):** barras de radio 2 px. Canal del rail: 14 px de
  alto, 40 checks. Detalle: 32 px de alto, 100 checks, separación 2 px. Fila de Logs:
  16 px, 30 checks. Alineada a la derecha: los espacios vacíos quedan a la izquierda.
- **Lectura (`.readout`):** etiqueta en `.readout-label` sobre el valor en mono 22 px/500.
- **Conteo por estado (`.census`):** celdas `auto-fit` (mín. 120 px) separadas por
  `--graticule-soft`, valor mono 28 px. "Pausadas" solo aparece si hay alguna.
- **Chip (`.chip`):** 20 px de alto, radio 4 px, `--bay-raised` con borde `--graticule`,
  11 px/500 `--ink-2`. Marca estados de una ficha ("Pausada", "Pública").
- **Aviso (`.notice`):** panel con punto de estado + texto 13 px `--ink-2`, para estados
  que el usuario eligió (p. ej. monitoreo en pausa). No es un error.
- **Desplegable (`details` con `summary` 12 px `--ink-3` y flecha ▸ que gira 90°):**
  opciones avanzadas, edición y código del embed. Se abre solo si hay un error adentro.
- **Control segmentado (`.segmented`):** fondo `--well`, botones mono de 26 px. El activo
  va en `--bay-raised` con un anillo de 1 px.
- **Línea de tiempo (`.timeline` / `.event`):** columnas hora (130 px, mono 12) · estado
  (110 px) · app · mensaje (mono 12, `--ink-3`). Muestra 8 eventos y se expande con
  "Mostrar los N eventos".
- **Gráfica:** cuadrícula de ECG (menor cada 8 px, mayor cada 40 px), línea de 2 px que se
  vuelve ámbar por encima del umbral, franja roja con marca de 3 px arriba en las caídas,
  ejes en mono 11 px, tooltip en `--bay-raised`.
- **Embed (`<heartbeat-status>`):** padding 12/14, radio 10 px. Cabecera: punto · nombre
  (14 px/500) · estado en texto de color · % en mono al 70% de opacidad. Barras de 6 px de
  ancho, 22 px de alto, separación 3 px; la cantidad depende del ancho. Tiene temas oscuro
  y claro. Nunca carga fuentes en la web que lo incrusta.

## Distribución por pantalla

- **Dashboard:** rail de 300 px (fijo) más el área principal.
  - Resumen: el veredicto es el foco, luego el conteo por estado, "Requieren atención" y
    los eventos.
  - Detalle de una app: la lectura de latencia es el foco, luego la gráfica y los eventos.
  - Por debajo de 900 px, el área principal va primero.
- **Logs:** un solo panel con filas: punto · nombre y slug · tira · % · "Abrir logs →".
- **Apps:** formulario fijo de 340 px más fichas con URLs, embed y acciones. Los hosts
  permitidos se muestran en el formulario. Cada ficha: cabecera con chips y acciones
  (Pausar/Reanudar, Rotar token, Eliminar), URLs, edición desplegable y embed con
  "Publicar en /status". Un error de validación aparece dentro del formulario que falló y
  conserva lo escrito.
- **Estado público (`/status`):** columna de 760 px, sin navegación de admin (bloque
  `topbar` reemplazado). Veredicto 28 px, filas con punto · nombre · estado en texto ·
  % · tira de 60 checks (30 en móvil).
- **Login:** panel único de 360 px centrado, logo + punto vivo, mismos tokens.

## Textos e idiomas

- Nada de texto fijo en plantillas ni JS: servidor `{{ t(k="...") }}` (`locales/*.json`),
  cliente `T('...')` (`static/i18n/*.js`). Un test exige las mismas claves en `es` y `en`.
- Voz: directa y concreta, sin jerga. Los estados se nombran igual en todas partes
  (Up / Degradado / Down / Pausada / Sin datos).
