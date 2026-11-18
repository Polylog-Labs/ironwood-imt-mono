import { copyFile, mkdir, readFile, rm, writeFile } from "node:fs/promises";
import { join } from "node:path";
import { buildWeb, webAssets } from "./build-web.ts";

export interface SourceRoute {
  src: string;
  dest?: string;
  status?: number;
  continue?: boolean;
  headers?: Record<string, string>;
  transforms?: {
    type: "request.headers";
    op: "set";
    target: { key: string };
    args: string;
  }[];
}

export interface VercelConfig {
  version: 3;
  routes: (SourceRoute | { handle: "filesystem" })[];
}

// Emit Vercel's Build Output API directly; the explorer needs no bundler or functions.
const root = new URL("../", import.meta.url);
const output = new URL(".vercel/output/", root);
const value = process.env.IRONWOOD_API_ORIGIN?.trim();
const proxyToken = process.env.IRONWOOD_PROXY_TOKEN;
if (proxyToken !== undefined && !/^[A-Za-z0-9_-]{32,}$/.test(proxyToken)) {
  console.error("IRONWOOD_PROXY_TOKEN must contain at least 32 ASCII letters, digits, hyphens or underscores.");
  process.exit(1);
}
let api: URL | null = null;
if (value) {
  try {
    api = new URL(value);
    if (
      api.protocol !== "https:" ||
      api.username ||
      api.password ||
      api.pathname !== "/" ||
      api.search ||
      api.hash
    ) {
      throw new Error("invalid origin");
    }
  } catch {
    console.error(
      "IRONWOOD_API_ORIGIN must be an HTTPS origin without credentials, a path, query, or fragment.",
    );
    process.exit(1);
  }
}

const config: VercelConfig = {
  version: 3,
  routes: [
    {
      src: "/.*",
      headers: {
        "X-Content-Type-Options": "nosniff",
        "Content-Security-Policy":
          "default-src 'self'; script-src 'self'; style-src 'self'; img-src 'self' data:; connect-src 'self'; object-src 'none'; base-uri 'none'; frame-ancestors 'none'",
        "Referrer-Policy": "strict-origin-when-cross-origin",
      },
      continue: true,
    },
    {
      src: "^/(v1(?:/.*)?|healthz|readyz)$",
      ...(api
        ? { dest: `${api.origin}/$1` }
        : { dest: "/api-unavailable.json", status: 503 }),
      ...(api && proxyToken ? {
        transforms: [{
          type: "request.headers",
          op: "set",
          target: { key: "x-ironwood-proxy-token" },
          args: proxyToken,
        }],
      } : {}),
      headers: {
        "Cache-Control": "no-store",
        "CDN-Cache-Control": "no-store",
        "Vercel-CDN-Cache-Control": "no-store",
        "x-vercel-enable-rewrite-caching": "0",
      },
    },
    {
      src: "/.*",
      headers: { "Cache-Control": "no-cache" },
      continue: true,
    },
    { src: "^/ironwood/?$", dest: "/ironwood.html" },
    // Support tabs opened before the TypeScript migration.
    { src: "^/tree-model\\.mjs$", dest: "/tree-model.js" },
    { handle: "filesystem" },
  ],
};

// Only web assets enter the deployment. Never copy the database or the repo root.
const assets = await buildWeb();
await rm(output, { recursive: true, force: true });
await mkdir(new URL("static/", output), { recursive: true });
for (const file of webAssets) {
  const source = join(assets, file);
  const destination = new URL(`static/${file}`, output);
  if (file.endsWith(".html")) {
    // Vercel's script records pageviews automatically. Keep local/Rust builds
    // free of tracking and use the existing same-origin CSP without inline JS.
    const html = await readFile(source, "utf8");
    await writeFile(destination, html.replace("</head>", '<script defer src="/_vercel/insights/script.js"></script>\n  </head>'));
  } else {
    await copyFile(source, destination);
  }
}
if (!api) {
  await writeFile(
    new URL("static/api-unavailable.json", output),
    `${JSON.stringify({ error: "api_not_configured" })}\n`,
  );
}
await writeFile(
  new URL("config.json", output),
  `${JSON.stringify(config, null, 2)}\n`,
);
console.log(
  api
    ? "Built Vercel static UI with same-origin API forwarding."
    : "Built Vercel UI without live data. Set IRONWOOD_API_ORIGIN and redeploy to connect the indexer.",
);
