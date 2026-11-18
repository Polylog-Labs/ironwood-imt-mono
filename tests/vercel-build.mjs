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

function build(root, origin) {
  const env = { ...process.env };
  delete env.IRONWOOD_API_ORIGIN;
  if (origin !== undefined) env.IRONWOOD_API_ORIGIN = origin;
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

test("Vercel build rejects missing or invalid origins without logging credentials", async (t) => {
  const root = await fixture(t);
  for (const origin of [
    undefined,
    "",
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
