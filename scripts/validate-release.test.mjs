import assert from "node:assert/strict";
import {
  createHash,
  generateKeyPairSync,
  sign as signEd25519,
} from "node:crypto";
import { mkdtemp, mkdir, readFile, unlink, writeFile } from "node:fs/promises";
import os from "node:os";
import path from "node:path";
import test from "node:test";

import {
  checkDraftRelease,
  classifyReleaseLookup,
  generateLatest,
  parseTag,
  validateAssets,
  validateVersions,
  verifyDirectoryParity,
} from "./validate-release.mjs";

const TAG = "v1.2.3";

function createTestSigner() {
  const { privateKey, publicKey } = generateKeyPairSync("ed25519");
  const rawPublicKey = Buffer.from(
    publicKey.export({ format: "jwk" }).x,
    "base64url",
  );
  const keyId = createHash("sha256")
    .update(rawPublicKey)
    .digest()
    .subarray(0, 8);
  const publicKeyPacket = Buffer.concat([
    Buffer.from("Ed", "ascii"),
    keyId,
    rawPublicKey,
  ]);
  const publicKeyBox = [
    "untrusted comment: minisign public key: RELEASE TEST ONLY",
    publicKeyPacket.toString("base64"),
    "",
  ].join("\n");
  return {
    privateKey,
    keyId,
    publicKey: Buffer.from(publicKeyBox, "utf8").toString("base64"),
  };
}

const TEST_SIGNER = createTestSigner();
const VALIDATION_OPTIONS = { publicKey: TEST_SIGNER.publicKey };

function artifactContents(file) {
  return Buffer.from(`artifact:${file}`, "utf8");
}

function signUpdaterArtifact(file, signer = TEST_SIGNER) {
  const signature = signEd25519(
    null,
    createHash("blake2b512").update(artifactContents(file)).digest(),
    signer.privateKey,
  );
  const trustedComment = `timestamp:1700000000\tfile:${file}`;
  const globalSignature = signEd25519(
    null,
    Buffer.concat([signature, Buffer.from(trustedComment, "utf8")]),
    signer.privateKey,
  );
  const signaturePacket = Buffer.concat([
    Buffer.from("ED", "ascii"),
    signer.keyId,
    signature,
  ]);
  const signatureBox = [
    "untrusted comment: signature from release test key",
    signaturePacket.toString("base64"),
    `trusted comment: ${trustedComment}`,
    globalSignature.toString("base64"),
    "",
  ].join("\n");
  return Buffer.from(signatureBox, "utf8").toString("base64");
}

function validateTestAssets(directory, tag) {
  return validateAssets(directory, tag, VALIDATION_OPTIONS);
}

function generateTestLatest(directory, tag, repository, output) {
  return generateLatest(directory, tag, repository, output, VALIDATION_OPTIONS);
}

async function fixture() {
  return mkdtemp(path.join(os.tmpdir(), "release-validation-"));
}

async function versionFixture() {
  const root = await mkdtemp(path.join(os.tmpdir(), "release-validation-"));
  await mkdir(path.join(root, "src-tauri"));
  await writeFile(path.join(root, "package.json"), '{"version":"1.2.3"}');
  await writeFile(
    path.join(root, "src-tauri/Cargo.toml"),
    '[package]\nname = "app"\nversion = "1.2.3"\n',
  );
  await writeFile(
    path.join(root, "src-tauri/tauri.conf.json"),
    '{"version":"1.2.3"}',
  );
  return root;
}

async function populateAssets(directory, tag, signed = true) {
  const prefix = `CC-Switch-${tag}`;
  const files = [
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
  if (signed) files.push(`${prefix}-macOS.tar.gz`);
  for (const file of files) {
    await writeFile(path.join(directory, file), artifactContents(file));
  }
  if (signed) {
    for (const file of [
      `${prefix}-macOS.tar.gz`,
      `${prefix}-Windows.msi`,
      `${prefix}-Windows-arm64.msi`,
      `${prefix}-Linux-x86_64.AppImage`,
      `${prefix}-Linux-arm64.AppImage`,
    ]) {
      await writeFile(
        path.join(directory, `${file}.sig`),
        signUpdaterArtifact(file),
      );
    }
  }
}

test("accepts strict stable and prerelease SemVer tags", () => {
  assert.deepEqual(parseTag("v1.2.3"), { version: "1.2.3", prerelease: false });
  assert.equal(parseTag("v1.2.3-rc.1+build.5").prerelease, true);
  for (const tag of [
    "1.2.3",
    "v01.2.3",
    "v1.2",
    "v1.2.3;echo pwned",
    "v1.2.3$(touch /tmp/pwned)",
    "v1.2.3$IFS-curl$IFS.example.invalid",
  ]) {
    assert.throws(() => parseTag(tag), /strict SemVer/);
  }
});

test("release lookup is fail-closed except for an explicit 404", () => {
  assert.equal(classifyReleaseLookup(404), "missing");
  assert.equal(classifyReleaseLookup(200, { draft: true }), "draft");
  assert.throws(
    () => classifyReleaseLookup(200, { draft: false }),
    /already published/,
  );
  for (const status of [0, 401, 403, 429, 500, 503]) {
    assert.throws(
      () => classifyReleaseLookup(status),
      new RegExp(`HTTP ${status}`),
    );
  }
});

test("retryable release lookup failures stop after three attempts", async (t) => {
  const originalFetch = globalThis.fetch;
  const originalToken = process.env.GH_TOKEN;
  t.after(() => {
    globalThis.fetch = originalFetch;
    if (originalToken === undefined) delete process.env.GH_TOKEN;
    else process.env.GH_TOKEN = originalToken;
  });
  process.env.GH_TOKEN = "test-token";
  let attempts = 0;
  globalThis.fetch = async () => {
    attempts += 1;
    return new Response("unavailable", {
      status: 503,
      headers: { "retry-after": "0" },
    });
  };
  await assert.rejects(checkDraftRelease("owner/repo", TAG), /HTTP 503/);
  assert.equal(attempts, 3);
});

test("requires all three version sources to match the tag", async () => {
  const root = await versionFixture();
  await validateVersions(root, TAG);
  await writeFile(
    path.join(root, "src-tauri/tauri.conf.json"),
    '{"version":"1.2.4"}',
  );
  await assert.rejects(
    validateVersions(root, TAG),
    /tauri\.conf\.json=1\.2\.4/,
  );
});

test("accepts only complete signed stable assets", async () => {
  const root = await fixture();
  await populateAssets(root, TAG);
  assert.deepEqual(
    await validateTestAssets(root, TAG).then(({ signed, publishLatest }) => ({
      signed,
      publishLatest,
    })),
    {
      signed: true,
      publishLatest: true,
    },
  );
  await writeFile(
    path.join(root, `CC-Switch-${TAG}-Linux-arm64.AppImage.sig`),
    "",
  );
  await assert.rejects(validateTestAssets(root, TAG), /partially signed/);
});

test("rejects a zero-byte updater artifact in signed mode", async () => {
  const root = await fixture();
  await populateAssets(root, TAG);
  await writeFile(path.join(root, `CC-Switch-${TAG}-macOS.tar.gz`), "");
  await assert.rejects(
    validateTestAssets(root, TAG),
    /missing or empty updater artifacts/,
  );
});

test("rejects unsigned stable and partially signed releases", async () => {
  const unsigned = await fixture();
  await populateAssets(unsigned, TAG, false);
  await assert.rejects(
    validateTestAssets(unsigned, TAG),
    /explicit SemVer prerelease/,
  );

  const partial = await fixture();
  await populateAssets(partial, "v1.2.3-rc.1", false);
  await writeFile(
    path.join(partial, "CC-Switch-v1.2.3-rc.1-Windows.msi.sig"),
    "signature",
  );
  await assert.rejects(
    validateTestAssets(partial, "v1.2.3-rc.1"),
    /partially signed/,
  );
});

test("rejects a release with any required platform asset missing", async () => {
  const root = await fixture();
  await populateAssets(root, TAG);
  await unlink(path.join(root, `CC-Switch-${TAG}-Linux-arm64.rpm`));
  await assert.rejects(
    validateTestAssets(root, TAG),
    /missing or empty required platform assets/,
  );
});

test("rejects empty asset directories and zero-byte platform artifacts", async () => {
  const empty = await fixture();
  await assert.rejects(
    validateTestAssets(empty, "v1.2.3-rc.1"),
    /missing or empty required platform assets/,
  );

  const zeroByte = await fixture();
  await populateAssets(zeroByte, TAG);
  await writeFile(path.join(zeroByte, `CC-Switch-${TAG}-macOS.dmg`), "");
  await assert.rejects(
    validateTestAssets(zeroByte, TAG),
    /missing or empty required platform assets/,
  );
});

test("remote parity requires exact names, sizes, and SHA-256 digests", async () => {
  const root = await mkdtemp(path.join(os.tmpdir(), "release-parity-"));
  const local = path.join(root, "local");
  const remote = path.join(root, "remote");
  await Promise.all([mkdir(local), mkdir(remote)]);
  await Promise.all([
    writeFile(path.join(local, "asset.bin"), "same"),
    writeFile(path.join(remote, "asset.bin"), "same"),
  ]);
  await verifyDirectoryParity(local, remote);

  await writeFile(path.join(remote, "asset.bin"), "different-size");
  await assert.rejects(
    verifyDirectoryParity(local, remote),
    /differs from local bytes/,
  );

  await writeFile(path.join(remote, "asset.bin"), "diff");
  await assert.rejects(
    verifyDirectoryParity(local, remote),
    /differs from local bytes/,
  );

  await writeFile(path.join(remote, "extra.bin"), "extra");
  await assert.rejects(
    verifyDirectoryParity(local, remote),
    /name sets differ/,
  );
});

test("allows complete unsigned prereleases but never latest.json", async () => {
  const root = await fixture();
  const tag = "v1.2.3-rc.1";
  await populateAssets(root, tag, false);
  const result = await validateTestAssets(root, tag);
  assert.equal(result.signed, false);
  assert.equal(result.publishLatest, false);
  await assert.rejects(
    generateTestLatest(root, tag, "owner/repo", path.join(root, "latest.json")),
    /only be generated/,
  );
  await writeFile(path.join(root, "latest.json"), "{}");
  await assert.rejects(
    validateTestAssets(root, tag),
    /prerelease must not contain latest\.json/,
  );
});

test("generates all updater platforms for a complete signed stable release", async () => {
  const root = await fixture();
  await populateAssets(root, TAG);
  const output = path.join(root, "latest.json");
  await generateTestLatest(root, TAG, "owner/repo", output);
  const manifest = JSON.parse(await readFile(output, "utf8"));
  assert.equal(manifest.version, "1.2.3");
  assert.deepEqual(Object.keys(manifest.platforms).sort(), [
    "darwin-aarch64",
    "darwin-x86_64",
    "linux-aarch64",
    "linux-x86_64",
    "windows-aarch64",
    "windows-x86_64",
  ]);
});

test("cryptographically rejects malformed, tampered, swapped, and wrong-key signatures", async (t) => {
  await t.test("malformed signature", async () => {
    const root = await fixture();
    await populateAssets(root, TAG);
    await writeFile(
      path.join(root, `CC-Switch-${TAG}-Windows.msi.sig`),
      "definitely-not-a-signature",
    );
    await assert.rejects(
      validateTestAssets(root, TAG),
      /invalid updater signature for .*Windows\.msi/,
    );
  });

  await t.test("tampered artifact", async () => {
    const root = await fixture();
    await populateAssets(root, TAG);
    await writeFile(
      path.join(root, `CC-Switch-${TAG}-Linux-x86_64.AppImage`),
      "tampered",
    );
    await assert.rejects(
      validateTestAssets(root, TAG),
      /artifact signature verification failed/,
    );
  });

  await t.test("swapped signatures", async () => {
    const root = await fixture();
    await populateAssets(root, TAG);
    const windowsSignaturePath = path.join(
      root,
      `CC-Switch-${TAG}-Windows.msi.sig`,
    );
    const linuxSignaturePath = path.join(
      root,
      `CC-Switch-${TAG}-Linux-x86_64.AppImage.sig`,
    );
    const [windowsSignature, linuxSignature] = await Promise.all([
      readFile(windowsSignaturePath, "utf8"),
      readFile(linuxSignaturePath, "utf8"),
    ]);
    await Promise.all([
      writeFile(windowsSignaturePath, linuxSignature),
      writeFile(linuxSignaturePath, windowsSignature),
    ]);
    await assert.rejects(
      validateTestAssets(root, TAG),
      /artifact signature verification failed/,
    );
  });

  await t.test("wrong signing key", async () => {
    const root = await fixture();
    await populateAssets(root, TAG);
    const file = `CC-Switch-${TAG}-macOS.tar.gz`;
    await writeFile(
      path.join(root, `${file}.sig`),
      signUpdaterArtifact(file, createTestSigner()),
    );
    await assert.rejects(
      validateTestAssets(root, TAG),
      /created by a different key/,
    );
  });
});

test("rejects unexpected release assets", async () => {
  const root = await fixture();
  await populateAssets(root, TAG);
  await writeFile(path.join(root, "debug-symbols.zip"), "debug data");

  await assert.rejects(
    validateTestAssets(root, TAG),
    /unexpected assets: debug-symbols\.zip/,
  );
});
