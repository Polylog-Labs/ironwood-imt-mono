import { test } from "node:test";
import assert from "node:assert/strict";
import { spawnSync } from "node:child_process";
import {
  cp,
  mkdir,
  mkdtemp,
  readFile,
  readdir,
  rm,
  writeFile,
} from "node:fs/promises";
import { tmpdir } from "node:os";
import { join } from "node:path";

async function fixture(t) {
  const root = await mkdtemp(join(tmpdir(), "ironwood-vercel-"));
  t.after(() => rm(root, { recursive: true, force: true }));
  await cp(new URL("../scripts/", import.meta.url), join(root, "scripts"), {
    recursive: true,
  });
  await cp(new URL("../web/", import.meta.url), join(root, "web"), {
    recursive: true,
  });
  return root;
}

function build(root, origin, token) {
  const env = { ...process.env };
  delete env.IRONWOOD_API_ORIGIN;
  delete env.IRONWOOD_PROXY_TOKEN;
  if (origin !== undefined) env.IRONWOOD_API_ORIGIN = origin;
  if (token !== undefined) env.IRONWOOD_PROXY_TOKEN = token;
  return spawnSync(process.execPath, ["scripts/build-vercel.mjs"], {
    cwd: root,
    env,
    encoding: "utf8",
  });
}

test("Vercel build packages the UI, isolates local data, and forwards only API routes", async (t) => {
  const root = await fixture(t);
  await mkdir(join(root, "data"));
  await writeFile(join(root, "data/ironwood.sqlite"), "private database");
  await writeFile(join(root, ".env"), "private credentials");
  const result = build(root, "https://api.example.com/");
  assert.equal(result.status, 0, result.stderr);

  const output = join(root, ".vercel/output");
  assert.deepEqual((await readdir(output)).sort(), ["config.json", "static"]);
  const assets = await readdir(join(output, "static"));
  assert.deepEqual(assets, await readdir(join(root, "web")));
  for (const file of assets) {
    assert.deepEqual(
      await readFile(join(output, "static", file)),
      await readFile(join(root, "web", file)),
    );
  }
  const config = JSON.parse(
    await readFile(join(output, "config.json"), "utf8"),
  );
  assert.equal(config.version, 3);
  const explorer = config.routes.find(route => route.dest === "/ironwood.html");
  assert.ok(new RegExp(explorer.src).test("/ironwood"));
  assert.ok(new RegExp(explorer.src).test("/ironwood/"));
  assert.ok(!new RegExp(explorer.src).test("/ironwood-extra"));
  assert.match(await readFile(join(output, "static/index.html"), "utf8"), /href="\/ironwood"/);
  assert.match(config.overrides["tree-model.mjs"].contentType, /javascript/);
  const proxy = config.routes.find((route) => route.dest);
  const match = new RegExp(proxy.src);
  for (const path of [
    "/v1/dashboard",
    "/v1/leaves",
    "/v1/tree/nodes/32/0",
    "/healthz",
    "/readyz",
  ]) {
    assert.ok(match.test(path));
    assert.equal(path.replace(match, proxy.dest), `https://api.example.com${path}`);
  }
  for (const path of [
    "/",
    "/dashboard.js",
    "/v10/dashboard",
    "/healthz-extra",
    "/data/ironwood.sqlite",
  ]) {
    assert.equal(match.test(path), false);
  }
  assert.equal(proxy.headers["Cache-Control"], "no-store");
  assert.equal(proxy.headers["x-vercel-enable-rewrite-caching"], "0");
  assert.ok(
    config.routes.some((route) =>
      route.headers?.["Content-Security-Policy"]?.includes("connect-src 'self'"),
    ),
  );

  // Rebuilding must not retain assets from an earlier deployment.
  await writeFile(join(output, "static/obsolete.js"), "obsolete");
  assert.equal(build(root, "https://next.example.com").status, 0);
  assert.deepEqual(await readdir(join(output, "static")), assets);
});

test("proxy authentication stays in request transforms and never enters browser assets or logs", async (t) => {
  const root = await fixture(t);
  const token = "test-only-proxy-secret-32-characters";
  const result = build(root, "https://api.example.com", token);
  assert.equal(result.status, 0, result.stderr);
  assert.ok(!(result.stdout + result.stderr).includes(token));
  const output = join(root, ".vercel/output");
  const config = JSON.parse(await readFile(join(output, "config.json"), "utf8"));
  assert.deepEqual(config.routes.find(route => route.dest).transforms, [{
    type: "request.headers", op: "set", target: {key: "x-ironwood-proxy-token"}, args: token,
  }]);
  for (const file of await readdir(join(output, "static"))) {
    assert.ok(!(await readFile(join(output, "static", file))).includes(Buffer.from(token)));
  }
  const invalid = build(root, "https://api.example.com", "short-secret");
  assert.notEqual(invalid.status, 0);
  assert.ok(!(invalid.stdout + invalid.stderr).includes("short-secret"));
});

test("Vercel can deploy before the API is configured without inventing live data", async (t) => {
  const root = await fixture(t);
  const output = join(root, ".vercel/output");
  for (const origin of [undefined, "", "  "]) {
    const result = build(root, origin);
    assert.equal(result.status, 0, result.stderr);
    const config = JSON.parse(await readFile(join(output, "config.json"), "utf8"));
    const fallback = config.routes.find((route) => route.dest);
    assert.equal(fallback.status, 503);
    assert.equal(fallback.headers["Cache-Control"], "no-store");
    const body = JSON.parse(await readFile(join(output, "static", fallback.dest), "utf8"));
    assert.deepEqual(body, { error: "api_not_configured" });
    assert.ok((await readdir(join(output, "static"))).includes("index.html"));
  }
  assert.equal(build(root, "https://api.example.com").status, 0);
  assert.ok(!(await readdir(join(output, "static"))).includes("api-unavailable.json"));
});

test("Vercel build rejects invalid origins without logging credentials", async (t) => {
  const root = await fixture(t);
  for (const origin of [
    "not a URL",
    "http://api.example.com",
    "https://user:private-password@api.example.com",
    "https://api.example.com/v1",
    "https://api.example.com?token=private-token",
    "https://api.example.com#fragment",
  ]) {
    const result = build(root, origin);
    assert.notEqual(result.status, 0);
    assert.match(result.stderr, /IRONWOOD_API_ORIGIN/);
    assert.doesNotMatch(result.stdout + result.stderr, /private-password|private-token/);
  }
  await assert.rejects(readdir(join(root, ".vercel")), { code: "ENOENT" });
});
