import { copyFile, mkdir, rm, writeFile } from "node:fs/promises";

// Emit Vercel's Build Output API directly; the explorer needs no bundler or functions.
const root = new URL("../", import.meta.url);
const output = new URL(".vercel/output/", root);
const value = process.env.IRONWOOD_API_ORIGIN?.trim();
let api;
try {
  api = new URL(value);
} catch {
  console.error(
    "Set IRONWOOD_API_ORIGIN to the public HTTPS origin of the Rust API.",
  );
  process.exit(1);
}
if (
  api.protocol !== "https:" ||
  api.username ||
  api.password ||
  api.pathname !== "/" ||
  api.search ||
  api.hash
) {
  console.error(
    "IRONWOOD_API_ORIGIN must be an HTTPS origin without credentials, a path, query, or fragment.",
  );
  process.exit(1);
}

const config = {
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
      dest: `${api.origin}/$1`,
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
    { handle: "filesystem" },
  ],
  overrides: {
    "tree-model.mjs": { contentType: "text/javascript; charset=utf-8" },
  },
};

// Only web assets enter the deployment. Never copy the database or the repo root.
await rm(output, { recursive: true, force: true });
await mkdir(new URL("static/", output), { recursive: true });
for (const file of [
  "index.html",
  "dashboard.css",
  "dashboard.js",
  "tree.js",
  "tree-model.mjs",
]) {
  await copyFile(new URL(`web/${file}`, root), new URL(`static/${file}`, output));
}
await writeFile(
  new URL("config.json", output),
  `${JSON.stringify(config, null, 2)}\n`,
);
console.log("Built Vercel static UI with same-origin API forwarding.");
