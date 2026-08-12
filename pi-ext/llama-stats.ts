/**
 * llama-stats: Pi extension -> sets a "llama" status that pi-bar renders.
 *
 * Polls the llm-ctl daemon (/api/status-rollup) and publishes
 *   cache <hit%> · ctx <used%>
 * derived from the llama-server worker slot:
 *   cache% = (n_prompt_tokens - n_prompt_tokens_processed) / n_prompt_tokens
 *            (processed already EXCLUDES cache-reused tokens; n_prompt_tokens_cache
 *             stays 0 in current llama.cpp builds)
 *   ctx%   = n_prompt_tokens / n_ctx
 *
 * Plain text; pi-bar applies the isotope color states in its config.toml.
 */

import type { ExtensionAPI } from "@earendil-works/pi-coding-agent";

const ROLLUP_URL = "http://localhost:8082/api/status-rollup";

function pct(n: number, d: number): string {
  if (!d || d <= 0) return "--";
  return ((n / d) * 100).toFixed(1) + "%";
}

function buildText(slots: any[]): string {
  const s = (slots || [])[0];
  if (!s) return "cache -- · ctx --";
  const total = s.n_prompt_tokens ?? 0;
  const processed = s.n_prompt_tokens_processed ?? 0;
  const cached = Math.max(total - processed, 0);
  const nctx = s.n_ctx ?? 0;
  return `cache ${pct(cached, total)} · ctx ${pct(total, nctx)}`;
}

export default function (pi: ExtensionAPI) {
  let text = "cache -- · ctx --";
  let timer: ReturnType<typeof setInterval> | null = null;

  async function poll(): Promise<string> {
    try {
      const resp = await fetch(ROLLUP_URL, { signal: AbortSignal.timeout(2000) });
      if (!resp.ok) throw new Error(String(resp.status));
      const data = await resp.json();
      return buildText(data?.worker?.slots);
    } catch {
      return "cache -- · ctx --";
    }
  }

  function refresh(ctx: any) {
    ctx.ui.setStatus("llama", text);
  }

  pi.on("session_start", async (_event, ctx) => {
    ctx.ui.setStatus("llama", "cache -- · ctx --");
    text = await poll();
    refresh(ctx);
    if (timer) clearInterval(timer);
    timer = setInterval(async () => {
      text = await poll();
      refresh(ctx);
    }, 2000);
  });

  pi.on("message_end", async (_event, ctx) => {
    text = await poll();
    refresh(ctx);
    return {};
  });

  pi.on("session_shutdown", async () => {
    if (timer) {
      clearInterval(timer);
      timer = null;
    }
  });
}
