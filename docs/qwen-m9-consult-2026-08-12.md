Here is the pragmatic blueprint for your `llm-ctl` dashboard.

### 1. Static Serving in Axum 0.7
**Recommendation:** Use `tower-http::services::ServeDir` for dev, switch to `rust-embed` only when you package a final release binary. Recompiling Rust every time you tweak a CSS margin or fix a JS typo will destroy your iteration speed. 

**Gotcha:** Axum evaluates routes top-down, but `fallback_service` catches *everything* unmatched. If your `/v1/*` reverse proxy is implemented as a fallback, it will swallow your static assets. 
**Fix:** Define the proxy as a nested router or explicit wildcard route *before* the static fallback.

```rust
use tower_http::services::ServeDir;

let app = Router::new()
    .nest("/api", api_routes)
    // Explicitly route the proxy so it doesn't become the catch-all
    .nest("/v1", llama_proxy_routes) 
    // Fallback serves index.html for "/" and files for "/assets/*"
    .fallback_service(ServeDir::new("web").append_index_html_on_directories(true));
```

### 2. Live Updates
**Recommendation:** **Plain fetch polling (every 2-3s).** 
Do not use SSE or WebSockets. You noted the daemon has no global pub/sub and assembles data per-request. If you use SSE, your SSE handler will just be a `loop { tokio::time::sleep(2s); assemble_data(); send() }`. This wastes a Tokio task per open tab, requires JS `EventSource` reconnection logic, and provides zero architectural benefit over polling for a single-user LAN tool.

**JS Sketch:**
```javascript
async function poll() {
    try {
        const res = await fetch('/api/status-rollup'); // Create one aggregated endpoint
        if (res.ok) updateUI(await res.json());
    } catch (e) { console.warn("Poll failed", e); }
}
setInterval(poll, 2500);
```
*Tip:* Add a single `/api/status-rollup` endpoint that returns models, health, and session summaries in one payload to avoid 4 parallel fetches every 2 seconds. Keep the log tails (`/api/hf/download` GET) on a separate, slower poll (e.g., 5s) or fetch only when the job is active.

### 3. Tailwind v4 Build
**Recommendation:** Use the standalone CLI. Tailwind v4 ditches `tailwind.config.js` for CSS-first configuration. Auto-detection works fine for a single HTML file if you run the CLI from your project root, but explicitly declaring `@source` prevents edge-case misses.

**Invocation:**
```bash
npx @tailwindcss/cli -i ./web/input.css -o ./web/assets/app.css --minify
```

**`web/input.css` Sketch:**
```css
@import "tailwindcss";

/* Explicitly tell v4 where to scan for classes */
@source "./**/*.html";
@source "./**/*.js";

@theme {
  --color-bg: #0a0a0a;
  --color-surface: #101010;
  --color-raised: #1a1a1a;
  --color-line: #3c3c3c;
  --color-text: #d0d0d0;
  --color-cyan: #00ffff;
  --color-lime: #33ff00;
  --color-magenta: #ff0099;
  --color-red: #ff0000;
  --color-blue: #0066ff;
  --color-purple: #cc00ff;
  
  /* Override default font if desired */
  --font-sans: 'Inter', system-ui, sans-serif; 
}
```
*Pitfall:* In v4, custom colors defined in `@theme` automatically generate utilities (e.g., `bg-bg`, `text-cyan`, `border-line`). You don't need the `extend` syntax from v3.

### 4. Panel Structure
Keep it to a single viewport using CSS Grid. No routing, no modals. 

**Layout Sketch:**
*   **Header:** `llm-ctl` title, Health indicator (pulsing `bg-lime` or `bg-red` dot), PVE/DB status text.
*   **Left Column (Actions - 40% width):**
    *   *Models:* List of models. Each row has name, VRAM size, and a Start/Stop toggle button. Use `bg-surface` for rows, `bg-raised` on hover.
    *   *HF Download:* Text input for repo ID + "Download" button.
    *   *Rebuild:* Big "Rebuild Worker" button (`bg-magenta` text, `border-magenta`).
*   **Right Column (Telemetry - 60% width):**
    *   *Sessions Table:* Sortable table (Client, Requests, Cache Hit %). Highlight >80% cache hits in `text-lime`.
    *   *Job Log Tail:* A fixed-height `<pre>` block (`bg-bg`, `text-text`, `font-mono`, `overflow-y-auto`) that displays the stdout of the active HF download or Rebuild job. Auto-scrolls to bottom via JS.

**Interaction Pattern:**
When a user clicks "Start" or "Download", immediately disable the button and show a spinner (optimistic UI). The POST request fires. The next 2.5s poll will return the updated state, at which point you re-enable the button and update the UI. Keep JS state minimal; let the server be the source of truth.

### 5. Security Note
Since this is a LAN tool with no auth, you are primarily defending against accidental exposure (e.g., a forgotten Cloudflare tunnel or Tailscale misconfiguration) and basic CSRF/XSS.

**Recommendation:** Keep it simple, but enforce a strict Content Security Policy (CSP) via Axum middleware. To allow a strict CSP without dealing with nonces, **do not use inline JS or inline CSS**. Put your JS in `web/assets/app.js` and use the compiled `app.css`.

**Axum Middleware Sketch:**
```rust
use tower_http::set_header::SetResponseHeaderLayer;
use http::header::{CONTENT_SECURITY_POLICY, X_FRAME_OPTIONS, X_CONTENT_TYPE_OPTIONS};

let security_headers = ServiceBuilder::new()
    .layer(SetResponseHeaderLayer::overriding(
        CONTENT_SECURITY_POLICY, 
        "default-src 'self'; script-src 'self'; style-src 'self' 'unsafe-inline'; img-src 'self' data:; connect-src 'self'".parse().unwrap()
    ))
    .layer(SetResponseHeaderLayer::overriding(X_FRAME_OPTIONS, "DENY".parse().unwrap()))
    .layer(SetResponseHeaderLayer::overriding(X_CONTENT_TYPE_OPTIONS, "nosniff".parse().unwrap()));

let app = Router::new()
    // ... routes ...
    .layer(security_headers);
```
*Note on `unsafe-inline` for styles:* Tailwind's CLI output is a static CSS file, but if you ever use dynamic style attributes in JS (e.g., setting a progress bar width), `style-src 'unsafe-inline'` saves you a massive headache. It's an acceptable tradeoff for a home lab tool. 

Finally, ensure your POST endpoints (`/api/models/:id/start`, etc.) check for `Content-Type: application/json` or require a specific custom header (like `X-LLM-CTL: 1`) to trivially block cross-origin form submissions (CSRF) from malicious websites you might visit on the same network.