import { execFileSync } from "node:child_process";
import { copyFile, mkdir, rm } from "node:fs/promises";
import { resolve } from "node:path";
import { fileURLToPath } from "node:url";

const root = fileURLToPath(new URL("../", import.meta.url));
const staticAssets = [
  "index.html",
  "ironwood.html",
  "dashboard.css",
  "activity.css",
  "favicon.svg",
  "apple-touch-icon.png",
  "share.png",
] as const;

export const webAssets = [
  ...staticAssets,
  "dashboard.js",
  "activity.js",
  "navigation.js",
  "tree.js",
  "tree-model.js",
  "dom.js",
] as const;

// Cargo supplies its own OUT_DIR so independent builds never share generated assets.
export async function buildWeb(output = resolve(root, "dist/web")): Promise<string> {
  await rm(output, { recursive: true, force: true });
  await mkdir(output, { recursive: true });
  execFileSync(
    process.execPath,
    [
      resolve(root, "node_modules/typescript/bin/tsc"),
      "--project", resolve(root, "tsconfig.web.json"),
      "--outDir", output,
    ],
    { cwd: root, stdio: "inherit" },
  );
  await Promise.all(
    staticAssets.map(file =>
      copyFile(resolve(root, "web", file), resolve(output, file)),
    ),
  );
  return output;
}

if (process.argv[1] && resolve(process.argv[1]) === fileURLToPath(import.meta.url)) {
  await buildWeb(process.argv[2]);
}
