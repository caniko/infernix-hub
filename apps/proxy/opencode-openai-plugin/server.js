// Route OpenCode V2's stock `openai` provider through fleetix-agent-proxy.
// Detaches `integrationID` from OpenCode's native SQLite OAuth store so
// OpenCode never refreshes or competes with the proxy's pooled ChatGPT grants,
// and injects the proxy client key from a runtime file at request time so no
// secret enters the Nix store.
import { readFile } from "node:fs/promises";

const SYNTHETIC_INTEGRATION_ID = "fleetix-agent-proxy-openai";
const PLACEHOLDER_API_KEY = "fleetix-agent-proxy";
const PRINTABLE_ASCII = /^[\x21-\x7e]+$/;

function isLoopbackHost(hostname) {
  const host = hostname.replace(/^\[|\]$/g, "").toLowerCase();
  return host === "localhost" || host === "::1" || host === "127.0.0.1" || host.startsWith("127.");
}

export function validateOptions(options) {
  const rawUrl = typeof options?.baseURL === "string" ? options.baseURL.trim() : "";
  const rawKeyFile = typeof options?.clientKeyFile === "string" ? options.clientKeyFile.trim() : "";
  if (!rawUrl) {
    throw new Error("fleetix.agent-proxy.openai requires options.baseURL");
  }
  const parsed = new URL(rawUrl);
  if (
    parsed.protocol !== "http:"
    || !isLoopbackHost(parsed.hostname)
    || parsed.username
    || parsed.password
    || parsed.search
    || parsed.hash
  ) {
    throw new Error("fleetix.agent-proxy.openai baseURL must be a plain loopback HTTP URL");
  }
  const basePath = parsed.pathname.replace(/\/+$/, "");
  if (!basePath.endsWith("/backend-api/codex")) {
    throw new Error("fleetix.agent-proxy.openai baseURL must end with /backend-api/codex");
  }
  if (!rawKeyFile.startsWith("/") || rawKeyFile.startsWith("/nix/store/")) {
    throw new Error("fleetix.agent-proxy.openai clientKeyFile must be a runtime absolute path outside /nix/store");
  }
  return {
    baseURL: `${parsed.origin}${basePath}`,
    expectedOrigin: parsed.origin,
    expectedPrefix: `${basePath}/`,
    clientKeyFile: rawKeyFile,
  };
}

export async function readClientKey(clientKeyFile) {
  const raw = (await readFile(clientKeyFile, "utf8")).trim();
  if (!raw || raw.length > 32768 || !PRINTABLE_ASCII.test(raw)) {
    throw new Error("fleetix-agent-proxy client key file is empty or invalid");
  }
  return raw;
}

export function assertProxyDestination(requestUrl, expectedOrigin, expectedPrefix) {
  const url = new URL(requestUrl);
  if (
    url.protocol !== "http:"
    || url.origin !== expectedOrigin
    || url.username
    || url.password
    || !url.pathname.startsWith(expectedPrefix)
  ) {
    throw new Error("Refusing to send fleetix-agent-proxy client key to an unexpected destination");
  }
}

export async function install(ctx, options, readKey = readClientKey) {
  const { baseURL, expectedOrigin, expectedPrefix, clientKeyFile } = validateOptions(options);

  await ctx.provider.transform((editor) => {
    const existing = editor.get("openai");
    if (!existing) return;
    editor.update("openai", (draft) => {
      draft.activation = "enabled";
      draft.integrationID = SYNTHETIC_INTEGRATION_ID;
      draft.settings = {
        ...(draft.settings ?? {}),
        baseURL,
        transport: "http",
        apiKey: PLACEHOLDER_API_KEY,
      };
      draft.headers = {
        ...(draft.headers ?? {}),
        originator: "opencode",
        "x-codex-beta-features": "remote_compaction_v2",
      };
      draft.body = {
        ...(draft.body ?? {}),
        store: false,
      };
    });
  });

  await ctx.model.transform((models) => {
    for (const model of models.list()) {
      if (model.providerID !== "openai") continue;
      models.update("openai", model.id, (draft) => {
        draft.cost = [];
        draft.body = {
          ...(draft.body ?? {}),
          store: false,
        };
      });
    }
  });

  await ctx.session.hook(
    "model.request",
    (event) => {
      event.baseURL = baseURL;
      event.headers = event.headers ?? {};
      event.headers.originator = "opencode";
      event.headers["x-codex-beta-features"] = "remote_compaction_v2";
      if (typeof event.sessionID === "string" && event.sessionID.length > 0) {
        event.headers["session-id"] = event.sessionID;
        event.headers["x-session-id"] = event.sessionID;
      }
      if (event.body && typeof event.body === "object") {
        event.body.store = false;
      }
    },
    { providerID: "openai" },
  );

  await ctx.session.hook(
    "http.request",
    async (event) => {
      assertProxyDestination(event.request.url, expectedOrigin, expectedPrefix);
      const clientKey = await readKey(clientKeyFile);
      const headers = new Headers(event.request.headers);
      headers.set("authorization", `Bearer ${clientKey}`);
      headers.delete("chatgpt-account-id");
      event.request = new Request(event.request, { headers, redirect: "error" });
    },
    { providerID: "openai" },
  );
}

export default {
  id: "fleetix.agent-proxy.openai",
  async setup(ctx) {
    await install(ctx, ctx.options);
  },
};
