import { execFileSync } from "node:child_process";
import { createPrivateKey } from "node:crypto";
import fs from "node:fs";
import { HttpAgent, Actor } from "@icp-sdk/core/agent";
import { Principal } from "@icp-sdk/core/principal";
import { Secp256k1KeyIdentity } from "@icp-sdk/core/identity/secp256k1";
import { idlFactory as routerIdlFactory } from "../src/generated/gleaph_router/declarations/gleaph_router.did.js";
import { idlFactory as graphIdlFactory } from "../src/generated/gleaph_graph/declarations/gleaph_graph.did.js";
import { idlFactory as gatewayIdlFactory } from "../src/generated/declarations/social_demo_gateway.did.js";

function getIcpHome() {
  if (process.env.ICP_CLI_HOME) {
    return process.env.ICP_CLI_HOME;
  }
  const home = process.env.HOME || process.cwd();
  // When the caller has already set HOME to the icp-cli home directory,
  // use it directly rather than appending a nested .icp/home segment.
  if (home.endsWith("/.icp/home")) {
    return home;
  }
  return `${home}/.icp/home`;
}

function getIcpHost() {
  const status = process.env.GLEAPH_DEMO_ICP_NETWORK_STATUS;
  if (status) {
    try {
      const parsed = JSON.parse(status);
      return parsed.api_url || parsed.gateway_url;
    } catch {}
  }
  if (process.env.VITE_IC_HOST) {
    return process.env.VITE_IC_HOST;
  }
  const networkStatus = getLocalNetworkStatus();
  if (networkStatus) {
    return networkStatus.api_url || networkStatus.gateway_url || "http://localhost:4943";
  }
  return "http://localhost:4943";
}

function getDeployerIdentity() {
  const identityName = process.env.ICP_IDENTITY_NAME || "gleaph-demo-deployer";
  const cliHome = getIcpHome();
  const pemPath = `${cliHome}/Library/Application Support/org.dfinity.icp-cli/identity/keys/${identityName}.pem`;
  if (!fs.existsSync(pemPath)) {
    throw new Error(`Deployer PEM not found: ${pemPath}`);
  }
  const pem = fs.readFileSync(pemPath, "utf8");
  const privateKey = createPrivateKey(pem);
  const jwk = privateKey.export({ format: "jwk" });
  const d = Buffer.from(jwk.d.replace(/-/g, "+").replace(/_/g, "/"), "base64");
  return Secp256k1KeyIdentity.fromSecretKey(d);
}

function looksLikePrincipal(text) {
  // ICP textual principals are base32 groups separated by hyphens; names like
  // 'gleaph-router' are also hyphenated but much shorter and contain letters
  // outside the principal alphabet. A principal has at least 5 groups of 5
  // alphanumeric chars (excluding i,l,o,0,1).
  const groups = text.split("-");
  if (groups.length < 5) return false;
  return groups.every((g) => /^[a-z2-7]+$/.test(g) && g.length >= 5);
}

function resolveCanisterId(canisterIdOrName) {
  if (looksLikePrincipal(canisterIdOrName)) {
    return canisterIdOrName;
  }
  return canisterIdFromLocalNetwork(canisterIdOrName);
}

let cachedStatus = null;
function getLocalNetworkStatus() {
  if (cachedStatus) return cachedStatus;
  if (process.env.GLEAPH_DEMO_ICP_NETWORK_STATUS) {
    try {
      cachedStatus = JSON.parse(process.env.GLEAPH_DEMO_ICP_NETWORK_STATUS);
      return cachedStatus;
    } catch {}
  }
  try {
    const env = { ...process.env, HOME: getIcpHome() };
    const out = execFileSync("icp", ["network", "status", "local", "--json"], {
      encoding: "utf8",
      env,
      timeout: 5000,
    });
    cachedStatus = JSON.parse(out);
    return cachedStatus;
  } catch {
    return null;
  }
}

function canisterIdFromLocalNetwork(name) {
  try {
    const env = { ...process.env, HOME: getIcpHome() };
    const out = execFileSync(
      "icp",
      ["canister", "status", "-e", "local", "-i", name],
      { encoding: "utf8", env, timeout: 5000 }
    );
    const id = out.split("\n")[0].trim();

    if (id) {
      return id;
    }
  } catch (err) {
    throw new Error(`Could not resolve canister id for ${name}: ${err.message}`);
  }
  throw new Error(`Canister ${name} not found in local network`);
}

async function createAgent() {
  const host = getIcpHost();
  const identity = getDeployerIdentity();
  const agent = HttpAgent.createSync({ host, identity, verifyQuerySignatures: false });
  // Local replicas use a root key different from the mainnet default. Await
  // this before creating an actor so the first certified response cannot be
  // verified with a stale or missing key.
  await agent.fetchRootKey();
  return agent;
}

export async function createRouterActor(canisterIdOrName) {
  const agent = await createAgent();
  const canisterId = resolveCanisterId(canisterIdOrName);
  const actor = Actor.createActor(routerIdlFactory, { agent, canisterId });
  return actor;
}

export async function createGraphActor(canisterIdOrName) {
  const agent = await createAgent();
  const canisterId = resolveCanisterId(canisterIdOrName);
  const actor = Actor.createActor(graphIdlFactory, { agent, canisterId });
  return actor;
}

export async function createGatewayActor(canisterIdOrName) {
  const agent = await createAgent();
  const canisterId = resolveCanisterId(canisterIdOrName);
  const actor = Actor.createActor(gatewayIdlFactory, { agent, canisterId });
  return actor;
}
