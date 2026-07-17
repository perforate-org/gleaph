import { mkdirSync, readdirSync, writeFileSync } from "node:fs";
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
  const bucket = createHash("sha256")
    .update(`social-demo:avatar-style:${userId}`, "utf8")
    .digest()[0] % 5;
  return ["bottts", "fun-emoji", "identicon", "rings", "shapes"][bucket];
};

const { users: rawUsers } = readRawUsers(CONFIG_DIR);
const users = scaleUsers(rawUsers, userScale);

for (const user of users) {
  const userId = user.id;
  const style = avatarStyleForUser(userId);
  const url = `https://api.dicebear.com/${DICEBEAR_VERSION}/${style}/svg?seed=${encodeURIComponent(userId)}`;
  const response = await fetch(url);
  if (!response.ok) {
    throw new Error(`DiceBear rejected ${userId}: ${response.status} ${response.statusText}`);
  }
  writeFileSync(join(AVATARS_DIR, `${userId}.svg`), await response.text());
}

console.log(`Generated avatars for ${readdirSync(AVATARS_DIR).filter((n) => n.endsWith(".svg")).length} users in public/avatars`);
