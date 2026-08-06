import { existsSync, mkdirSync, readdirSync, writeFileSync } from "node:fs";
import { createHash } from "node:crypto";
import { join, resolve } from "node:path";
import { fileURLToPath } from "node:url";
import { readRawUsers, scaleUsers, readScaleEnv } from "./social-scale.mjs";

const SCRIPT_DIR = resolve(fileURLToPath(new URL(".", import.meta.url)));
const APP_ROOT = resolve(SCRIPT_DIR, "..");
const CONFIG_DIR = join(APP_ROOT, "config");
const AVATARS_DIR = join(APP_ROOT, "public", "avatars");
const DICEBEAR_VERSION = "10.x";

mkdirSync(AVATARS_DIR, { recursive: true });

const { userScale } = readScaleEnv();

// Do not infer gender from names or imply a user's appearance. Use only
// non-human and abstract styles so the mock data has visual variety without
// adding an unowned demographic attribute to the user profiles.
const avatarStyleForUser = (userId) => {
  const bucket =
    createHash("sha256")
      .update(`social-demo:avatar-style:${userId}`, "utf8")
      .digest()[0] % 5;
  return ["bottts", "fun-emoji", "identicon", "rings", "shapes"][bucket];
};

const { users: rawUsers } = readRawUsers(CONFIG_DIR);
const users = scaleUsers(rawUsers, userScale);

let fetched = 0;
let skipped = 0;
for (const user of users) {
  const userId = user.id;
  const target = join(AVATARS_DIR, `${userId}.svg`);
  // Avatars for the default user set (userScale=5, 140 users) are committed under
  // public/avatars/, so an offline build needs no fetch. Only avatars that are
  // missing (e.g. after a userScale change) trigger a live DiceBear round-trip.
  if (existsSync(target)) {
    skipped += 1;
    continue;
  }
  const style = avatarStyleForUser(userId);
  const url = `https://api.dicebear.com/${DICEBEAR_VERSION}/${style}/svg?seed=${encodeURIComponent(userId)}`;
  let response;
  try {
    response = await fetch(url);
  } catch {
    // Missing avatars are a degraded-visual problem, not a build blocker: the
    // UI falls back to an initial-letter chip. Keep offline builds green.
    console.warn(`Skipping avatar fetch for ${userId}: network unavailable`);
    continue;
  }
  if (!response.ok) {
    throw new Error(
      `DiceBear rejected ${userId}: ${response.status} ${response.statusText}`,
    );
  }
  writeFileSync(target, await response.text());
  fetched += 1;
}

console.log(
  `Avatars ready in public/avatars: ${readdirSync(AVATARS_DIR).filter((n) => n.endsWith(".svg")).length} files (${fetched} fetched, ${skipped} reused)`,
);
