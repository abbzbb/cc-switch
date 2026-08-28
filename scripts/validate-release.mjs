#!/usr/bin/env node

import {
  createHash,
  createPublicKey,
  verify as verifyEd25519,
} from "node:crypto";
import { mkdir, readFile, readdir, stat, writeFile } from "node:fs/promises";
import path from "node:path";
import { fileURLToPath } from "node:url";
import { TextDecoder } from "node:util";

const SEMVER_TAG =
  /^v(0|[1-9]\d*)\.(0|[1-9]\d*)\.(0|[1-9]\d*)(?:-((?:0|[1-9]\d*|\d*[A-Za-z-][0-9A-Za-z-]*)(?:\.(?:0|[1-9]\d*|\d*[A-Za-z-][0-9A-Za-z-]*))*))?(?:\+[0-9A-Za-z-]+(?:\.[0-9A-Za-z-]+)*)?$/;
const MAX_API_ATTEMPTS = 3;
const SCRIPT_DIRECTORY = path.dirname(fileURLToPath(import.meta.url));
const DEFAULT_TAURI_CONFIG = path.resolve(
  SCRIPT_DIRECTORY,
  "../src-tauri/tauri.conf.json",
);
const ED25519_SPKI_PREFIX = Buffer.from("302a300506032b6570032100", "hex");

export function parseTag(tag) {
  const match = SEMVER_TAG.exec(tag);
  if (!match) {
    throw new Error(
      `release tag must be strict SemVer prefixed with v: ${tag}`,
    );
  }
  return { version: tag.slice(1), prerelease: match[4] !== undefined };
}

function parseCargoVersion(source) {
  const packageSection = source.match(
    /^\[package\]\s*$([\s\S]*?)(?=^\[|(?![\s\S]))/m,
  )?.[1];
  const version = packageSection?.match(/^version\s*=\s*"([^"]+)"\s*$/m)?.[1];
  if (!version)
    throw new Error(
      "could not read [package].version from src-tauri/Cargo.toml",
    );
  return version;
}

export async function validateVersions(root, tag) {
  const { version } = parseTag(tag);
  const [packageJson, cargoToml, tauriJson] = await Promise.all([
    readFile(path.join(root, "package.json"), "utf8").then(JSON.parse),
    readFile(path.join(root, "src-tauri/Cargo.toml"), "utf8"),
    readFile(path.join(root, "src-tauri/tauri.conf.json"), "utf8").then(
      JSON.parse,
    ),
  ]);
  const versions = {
    "package.json": packageJson.version,
    "src-tauri/Cargo.toml": parseCargoVersion(cargoToml),
    "src-tauri/tauri.conf.json": tauriJson.version,
  };
  const mismatches = Object.entries(versions).filter(
    ([, value]) => value !== version,
  );
  if (mismatches.length) {
    throw new Error(
      `tag ${tag} does not match release versions: ${Object.entries(versions)
        .map(([file, value]) => `${file}=${value}`)
        .join(", ")}`,
    );
  }
  return { version, versions };
}

function expectedAssets(tag) {
  const prefix = `CC-Switch-${tag}`;
  const updater = [
    `${prefix}-macOS.tar.gz`,
    `${prefix}-Windows.msi`,
    `${prefix}-Windows-arm64.msi`,
    `${prefix}-Linux-x86_64.AppImage`,
    `${prefix}-Linux-arm64.AppImage`,
  ];
  const installers = [
    `${prefix}-macOS.dmg`,
    `${prefix}-macOS.zip`,
    `${prefix}-Windows.msi`,
    `${prefix}-Windows-arm64.msi`,
    `${prefix}-Windows-Portable.zip`,
    `${prefix}-Windows-arm64-Portable.zip`,
    `${prefix}-Linux-x86_64.AppImage`,
    `${prefix}-Linux-x86_64.deb`,
    `${prefix}-Linux-x86_64.rpm`,
    `${prefix}-Linux-arm64.AppImage`,
    `${prefix}-Linux-arm64.deb`,
    `${prefix}-Linux-arm64.rpm`,
  ];
  return {
    updater,
    installers,
    signatures: updater.map((name) => `${name}.sig`),
  };
}

async function missingOrEmptyFiles(directory, names) {
  const invalid = [];
  for (const name of names) {
    try {
      const metadata = await stat(path.join(directory, name));
      if (!metadata.isFile() || metadata.size === 0) invalid.push(name);
    } catch (error) {
      if (error?.code !== "ENOENT") throw error;
      invalid.push(name);
    }
  }
  return invalid;
}

function decodeCanonicalBase64(value, label) {
  if (typeof value !== "string") {
    throw new Error(`${label} must be a base64 string`);
  }
  const encoded = value.trim();
  if (
    !encoded ||
    encoded.length % 4 !== 0 ||
    !/^(?:[A-Za-z0-9+/]{4})*(?:[A-Za-z0-9+/]{2}==|[A-Za-z0-9+/]{3}=)?$/.test(
      encoded,
    )
  ) {
    throw new Error(`${label} is not canonical base64`);
  }
  const decoded = Buffer.from(encoded, "base64");
  if (decoded.toString("base64") !== encoded) {
    throw new Error(`${label} is not canonical base64`);
  }
  return decoded;
}

function decodeUtf8(buffer, label) {
  try {
    return new TextDecoder("utf-8", { fatal: true }).decode(buffer);
  } catch {
    throw new Error(`${label} is not valid UTF-8`);
  }
}

function minisignLines(box, expectedCount, label) {
  const withoutFinalNewline = box.endsWith("\n") ? box.slice(0, -1) : box;
  const lines = withoutFinalNewline.split(/\r?\n/);
  if (lines.length !== expectedCount || lines.some((line) => !line)) {
    throw new Error(`${label} has an invalid Minisign box`);
  }
  return lines;
}

function parseUpdaterPublicKey(encodedPublicKey) {
  const publicKeyBox = decodeUtf8(
    decodeCanonicalBase64(encodedPublicKey, "updater public key"),
    "updater public key",
  );
  const lines = minisignLines(publicKeyBox, 2, "updater public key");
  if (!lines[0].startsWith("untrusted comment:")) {
    throw new Error("updater public key has an invalid untrusted comment");
  }
  const packet = decodeCanonicalBase64(lines[1], "Minisign public key packet");
  if (packet.length !== 42) {
    throw new Error("Minisign public key packet must contain 42 bytes");
  }
  const algorithm = packet.subarray(0, 2).toString("ascii");
  if (algorithm !== "Ed" && algorithm !== "ED") {
    throw new Error(`unsupported Minisign public key algorithm: ${algorithm}`);
  }
  return {
    keyId: packet.subarray(2, 10),
    key: createPublicKey({
      key: Buffer.concat([ED25519_SPKI_PREFIX, packet.subarray(10, 42)]),
      format: "der",
      type: "spki",
    }),
  };
}

function parseUpdaterSignature(encodedSignature) {
  const signatureBox = decodeUtf8(
    decodeCanonicalBase64(encodedSignature, "updater signature"),
    "updater signature",
  );
  const lines = minisignLines(signatureBox, 4, "updater signature");
  if (!lines[0].startsWith("untrusted comment:")) {
    throw new Error("updater signature has an invalid untrusted comment");
  }
  const packet = decodeCanonicalBase64(lines[1], "Minisign signature packet");
  if (packet.length !== 74) {
    throw new Error("Minisign signature packet must contain 74 bytes");
  }
  const algorithm = packet.subarray(0, 2).toString("ascii");
  if (algorithm !== "Ed" && algorithm !== "ED") {
    throw new Error(`unsupported Minisign signature algorithm: ${algorithm}`);
  }
  const trustedCommentPrefix = "trusted comment: ";
  if (!lines[2].startsWith(trustedCommentPrefix)) {
    throw new Error("updater signature has an invalid trusted comment");
  }
  const globalSignature = decodeCanonicalBase64(
    lines[3],
    "Minisign global signature",
  );
  if (globalSignature.length !== 64) {
    throw new Error("Minisign global signature must contain 64 bytes");
  }
  return {
    algorithm,
    keyId: packet.subarray(2, 10),
    signature: packet.subarray(10, 74),
    trustedComment: lines[2].slice(trustedCommentPrefix.length),
    globalSignature,
  };
}

async function configuredUpdaterPublicKey(tauriConfigPath) {
  const config = JSON.parse(await readFile(tauriConfigPath, "utf8"));
  const publicKey = config?.plugins?.updater?.pubkey;
  if (typeof publicKey !== "string" || !publicKey.trim()) {
    throw new Error(`missing plugins.updater.pubkey in ${tauriConfigPath}`);
  }
  return publicKey;
}

export async function verifyUpdaterSignature(
  artifactPath,
  signaturePath,
  encodedPublicKey,
) {
  const publicKey = parseUpdaterPublicKey(encodedPublicKey);
  const signature = parseUpdaterSignature(
    await readFile(signaturePath, "utf8"),
  );
  if (!publicKey.keyId.equals(signature.keyId)) {
    throw new Error("updater signature was created by a different key");
  }

  const artifact = await readFile(artifactPath);
  const signedContent =
    signature.algorithm === "ED"
      ? createHash("blake2b512").update(artifact).digest()
      : artifact;
  if (!verifyEd25519(null, signedContent, publicKey.key, signature.signature)) {
    throw new Error("artifact signature verification failed");
  }

  const globalContent = Buffer.concat([
    signature.signature,
    Buffer.from(signature.trustedComment, "utf8"),
  ]);
  if (
    !verifyEd25519(
      null,
      globalContent,
      publicKey.key,
      signature.globalSignature,
    )
  ) {
    throw new Error("trusted comment signature verification failed");
  }
}

function validateRepository(repository) {
  if (!/^[A-Za-z0-9_.-]+\/[A-Za-z0-9_.-]+$/.test(repository)) {
    throw new Error(`invalid GitHub repository: ${repository}`);
  }
}

function githubApiBase() {
  const url = new URL(process.env.GITHUB_API_URL ?? "https://api.github.com");
  if (url.protocol !== "https:" || url.username || url.password) {
    throw new Error("GITHUB_API_URL must be an HTTPS URL without credentials");
  }
  return url;
}

function retryDelay(response, attempt) {
  const retryAfter = Number(response?.headers.get("retry-after"));
  if (Number.isFinite(retryAfter) && retryAfter >= 0) {
    return Math.min(retryAfter * 1000, 5000);
  }
  return attempt * 1000;
}

async function apiRequest(endpoint, accept = "application/vnd.github+json") {
  const token = process.env.GH_TOKEN;
  if (!token)
    throw new Error("GH_TOKEN is required for release API validation");
  const url = new URL(
    endpoint,
    `${githubApiBase().toString().replace(/\/$/, "")}/`,
  );
  let lastError;
  for (let attempt = 1; attempt <= MAX_API_ATTEMPTS; attempt += 1) {
    let response;
    try {
      response = await fetch(url, {
        headers: {
          Accept: accept,
          Authorization: `Bearer ${token}`,
          "X-GitHub-Api-Version": "2022-11-28",
        },
        signal: AbortSignal.timeout(15_000),
      });
    } catch (error) {
      lastError = error;
      if (attempt === MAX_API_ATTEMPTS) break;
      await new Promise((resolve) => setTimeout(resolve, attempt * 1000));
      continue;
    }

    if (response.status !== 429 && response.status < 500) return response;
    lastError = new Error(
      `GitHub API returned retryable HTTP ${response.status}`,
    );
    if (attempt === MAX_API_ATTEMPTS) return response;
    await response.arrayBuffer();
    await new Promise((resolve) =>
      setTimeout(resolve, retryDelay(response, attempt)),
    );
  }
  throw new Error(
    `GitHub API request failed after ${MAX_API_ATTEMPTS} attempts: ${lastError?.message ?? "unknown network error"}`,
  );
}

export function classifyReleaseLookup(status, release) {
  if (status === 404) return "missing";
  if (status !== 200) {
    throw new Error(`release lookup failed with HTTP ${status}`);
  }
  if (
    !release ||
    typeof release !== "object" ||
    typeof release.draft !== "boolean"
  ) {
    throw new Error("release lookup returned an invalid response");
  }
  if (!release.draft) {
    throw new Error(
      "release is already published; refusing a non-atomic update",
    );
  }
  return "draft";
}

async function lookupRelease(repository, tag) {
  validateRepository(repository);
  parseTag(tag);
  const response = await apiRequest(
    `repos/${repository}/releases/tags/${encodeURIComponent(tag)}`,
  );
  let release;
  if (response.status === 200) {
    release = await response.json();
  } else {
    await response.arrayBuffer();
  }
  return { state: classifyReleaseLookup(response.status, release), release };
}

export async function checkDraftRelease(repository, tag) {
  const result = await lookupRelease(repository, tag);
  console.log(
    result.state === "draft"
      ? `Reusing existing draft release for ${tag}`
      : `No release exists for ${tag}; a draft may be created`,
  );
  return result;
}

async function fileInventory(directory) {
  const entries = await readdir(directory, { withFileTypes: true });
  const inventory = new Map();
  for (const entry of entries) {
    if (!entry.isFile()) {
      throw new Error(
        `release asset directory contains a non-file: ${entry.name}`,
      );
    }
    const contents = await readFile(path.join(directory, entry.name));
    inventory.set(entry.name, {
      size: contents.length,
      sha256: createHash("sha256").update(contents).digest("hex"),
    });
  }
  return inventory;
}

export async function verifyDirectoryParity(localDirectory, remoteDirectory) {
  const [local, remote] = await Promise.all([
    fileInventory(localDirectory),
    fileInventory(remoteDirectory),
  ]);
  const localNames = [...local.keys()].sort();
  const remoteNames = [...remote.keys()].sort();
  if (JSON.stringify(localNames) !== JSON.stringify(remoteNames)) {
    throw new Error(
      `remote/local asset name sets differ: local=[${localNames.join(", ")}], remote=[${remoteNames.join(", ")}]`,
    );
  }
  for (const name of localNames) {
    const localFile = local.get(name);
    const remoteFile = remote.get(name);
    if (
      localFile.size !== remoteFile.size ||
      localFile.sha256 !== remoteFile.sha256
    ) {
      throw new Error(`remote asset differs from local bytes: ${name}`);
    }
  }
  return localNames;
}

async function listReleaseAssets(repository, releaseId) {
  const assets = [];
  for (let page = 1; page <= 10; page += 1) {
    const response = await apiRequest(
      `repos/${repository}/releases/${releaseId}/assets?per_page=100&page=${page}`,
    );
    if (response.status !== 200) {
      await response.arrayBuffer();
      throw new Error(
        `release asset listing failed with HTTP ${response.status}`,
      );
    }
    const batch = await response.json();
    if (!Array.isArray(batch))
      throw new Error("release asset listing was not an array");
    assets.push(...batch);
    if (batch.length < 100) return assets;
  }
  throw new Error("release has more assets than the verification limit");
}

export async function verifyRemoteRelease(
  localDirectory,
  remoteDirectory,
  repository,
  tag,
) {
  const localValidation = await validateAssets(localDirectory, tag);
  const { state, release } = await lookupRelease(repository, tag);
  if (state !== "draft")
    throw new Error("release draft disappeared before verification");
  if (!Number.isSafeInteger(release.id))
    throw new Error("release has an invalid ID");

  const assets = await listReleaseAssets(repository, release.id);
  const localInventory = await fileInventory(localDirectory);
  const remoteNames = assets.map((asset) => asset.name).sort();
  const localNames = [...localInventory.keys()].sort();
  if (JSON.stringify(remoteNames) !== JSON.stringify(localNames)) {
    throw new Error(
      `remote/local asset name sets differ: local=[${localNames.join(", ")}], remote=[${remoteNames.join(", ")}]`,
    );
  }

  await mkdir(remoteDirectory);
  for (const asset of assets) {
    if (!Number.isSafeInteger(asset.id) || !localInventory.has(asset.name)) {
      throw new Error("release asset metadata is invalid");
    }
    const response = await apiRequest(
      `repos/${repository}/releases/assets/${asset.id}`,
      "application/octet-stream",
    );
    if (response.status !== 200) {
      await response.arrayBuffer();
      throw new Error(
        `release asset download failed with HTTP ${response.status}`,
      );
    }
    const contents = Buffer.from(await response.arrayBuffer());
    if (asset.size !== contents.length) {
      throw new Error(
        `downloaded asset size differs from API metadata: ${asset.name}`,
      );
    }
    await writeFile(path.join(remoteDirectory, asset.name), contents, {
      flag: "wx",
    });
  }

  await verifyDirectoryParity(localDirectory, remoteDirectory);
  const remoteValidation = await validateAssets(remoteDirectory, tag);
  if (
    remoteValidation.prerelease !== localValidation.prerelease ||
    remoteValidation.signed !== localValidation.signed ||
    remoteValidation.publishLatest !== localValidation.publishLatest
  ) {
    throw new Error("remote release mode differs from local validation");
  }
  const hasLatest = localInventory.has("latest.json");
  if (localValidation.publishLatest !== hasLatest) {
    throw new Error(
      localValidation.publishLatest
        ? "stable release is missing latest.json"
        : "prerelease must not contain latest.json",
    );
  }
  console.log(
    `Verified ${localNames.length} remote draft assets byte-for-byte`,
  );
}

export async function validateAssets(directory, tag, options = {}) {
  const { prerelease } = parseTag(tag);
  const entries = await readdir(directory, { withFileTypes: true });
  const nonFiles = entries.filter((entry) => !entry.isFile());
  if (nonFiles.length) {
    throw new Error(
      `release asset directory contains non-files: ${nonFiles.map((entry) => entry.name).join(", ")}`,
    );
  }
  const files = new Set(entries.map((entry) => entry.name));
  if (prerelease && files.has("latest.json")) {
    throw new Error("prerelease must not contain latest.json");
  }
  const expected = expectedAssets(tag);
  const invalidInstallers = await missingOrEmptyFiles(
    directory,
    expected.installers,
  );
  if (invalidInstallers.length) {
    throw new Error(
      `release has missing or empty required platform assets: ${invalidInstallers.join(", ")}`,
    );
  }

  const allSignatures = [...files].filter((name) => name.endsWith(".sig"));
  const presentSignatures = [];
  for (const name of expected.signatures) {
    if (
      files.has(name) &&
      (await readFile(path.join(directory, name), "utf8")).trim()
    ) {
      presentSignatures.push(name);
    }
  }
  const signed = presentSignatures.length === expected.signatures.length;
  if (allSignatures.length > 0 && !signed) {
    const invalid = await missingOrEmptyFiles(directory, expected.signatures);
    throw new Error(
      `partially signed release is forbidden; missing or empty: ${invalid.join(", ")}`,
    );
  }
  if (signed) {
    const invalidUpdater = await missingOrEmptyFiles(
      directory,
      expected.updater,
    );
    if (invalidUpdater.length) {
      throw new Error(
        `signed release has missing or empty updater artifacts: ${invalidUpdater.join(", ")}`,
      );
    }
  } else if (!prerelease) {
    throw new Error(
      "unsigned releases require an explicit SemVer prerelease tag",
    );
  }

  const publishLatest = signed && !prerelease;
  const allowedFiles = new Set(expected.installers);
  if (signed) {
    for (const name of [...expected.updater, ...expected.signatures]) {
      allowedFiles.add(name);
    }
  }
  if (publishLatest) allowedFiles.add("latest.json");
  const unexpectedFiles = [...files].filter((name) => !allowedFiles.has(name));
  if (unexpectedFiles.length) {
    throw new Error(
      `release contains unexpected assets: ${unexpectedFiles.sort().join(", ")}`,
    );
  }

  if (signed) {
    const publicKey =
      options.publicKey ??
      (await configuredUpdaterPublicKey(
        options.tauriConfigPath ?? DEFAULT_TAURI_CONFIG,
      ));
    for (const artifact of expected.updater) {
      try {
        await verifyUpdaterSignature(
          path.join(directory, artifact),
          path.join(directory, `${artifact}.sig`),
          publicKey,
        );
      } catch (error) {
        throw new Error(
          `invalid updater signature for ${artifact}: ${error?.message ?? error}`,
        );
      }
    }
  }

  return { prerelease, signed, publishLatest, expected };
}

export async function generateLatest(
  directory,
  tag,
  repository,
  output,
  options = {},
) {
  const validation = await validateAssets(directory, tag, options);
  if (!validation.publishLatest) {
    throw new Error(
      "latest.json may only be generated for a complete, signed stable release",
    );
  }
  validateRepository(repository);

  const { version } = parseTag(tag);
  const prefix = `CC-Switch-${tag}`;
  const baseUrl = `https://github.com/${repository}/releases/download/${tag}`;
  const platformFiles = {
    "darwin-aarch64": `${prefix}-macOS.tar.gz`,
    "darwin-x86_64": `${prefix}-macOS.tar.gz`,
    "windows-x86_64": `${prefix}-Windows.msi`,
    "windows-aarch64": `${prefix}-Windows-arm64.msi`,
    "linux-x86_64": `${prefix}-Linux-x86_64.AppImage`,
    "linux-aarch64": `${prefix}-Linux-arm64.AppImage`,
  };
  const platforms = {};
  for (const [platform, file] of Object.entries(platformFiles)) {
    platforms[platform] = {
      signature: (
        await readFile(path.join(directory, `${file}.sig`), "utf8")
      ).trim(),
      url: `${baseUrl}/${file}`,
    };
  }
  const manifest = {
    version,
    notes: `Release ${tag}`,
    pub_date: new Date().toISOString(),
    platforms,
  };
  await writeFile(output, `${JSON.stringify(manifest, null, 2)}\n`, "utf8");
}

async function appendOutputs(outputPath, values) {
  const lines = Object.entries(values)
    .map(([key, value]) => `${key}=${value}\n`)
    .join("");
  await writeFile(outputPath, lines, { encoding: "utf8", flag: "a" });
}

async function main([command, ...args]) {
  if (command === "versions" && args.length === 2) {
    await validateVersions(args[0], args[1]);
    console.log(`Validated release versions for ${args[1]}`);
    return;
  }
  if (command === "assets" && args.length === 3) {
    const result = await validateAssets(args[0], args[1]);
    await appendOutputs(args[2], {
      prerelease: result.prerelease,
      signed: result.signed,
      publish_latest: result.publishLatest,
    });
    console.log(
      `Validated ${result.signed ? "signed" : "unsigned"} release assets for ${args[1]}`,
    );
    return;
  }
  if (command === "latest" && args.length === 4) {
    await generateLatest(args[0], args[1], args[2], args[3]);
    console.log(`Generated complete updater manifest at ${args[3]}`);
    return;
  }
  if (command === "draft" && args.length === 2) {
    await checkDraftRelease(args[0], args[1]);
    return;
  }
  if (command === "remote" && args.length === 4) {
    await verifyRemoteRelease(args[0], args[1], args[2], args[3]);
    return;
  }
  throw new Error(
    "usage: validate-release.mjs versions <root> <tag> | assets <dir> <tag> <github-output> | latest <dir> <tag> <owner/repo> <output> | draft <owner/repo> <tag> | remote <local-dir> <download-dir> <owner/repo> <tag>",
  );
}

if (
  process.argv[1] &&
  path.resolve(process.argv[1]) === path.resolve(fileURLToPath(import.meta.url))
) {
  main(process.argv.slice(2)).catch((error) => {
    console.error(error.message);
    process.exitCode = 1;
  });
}
