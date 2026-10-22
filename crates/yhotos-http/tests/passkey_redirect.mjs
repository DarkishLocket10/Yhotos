import { readFile } from "node:fs/promises";
import assert from "node:assert/strict";
import test from "node:test";
import vm from "node:vm";

const asset = await readFile(new URL("../assets/js/passkey.js", import.meta.url), "utf8");

for (const url of [
  "https://example.test/..//evil.invalid",
  "https://example.test/a/../..//evil.invalid?query=1#fragment",
  "https://example.test/u/security?query=1#fragment",
  "https://evil.invalid/",
]) {
  test(`bundled passkey navigation checks and preserves the full URL: ${url}`, async () => {
    let click;
    let navigated;
    let requests = 0;
    const status = { textContent: "" };
    const button = {
      dataset: { base: "/login/mfa", csrf: "test", passkey: "authenticate" },
      addEventListener: (_, handler) => { click = handler; },
    };
    class Assertion {
      clientDataJSON = new ArrayBuffer(0);
      authenticatorData = new ArrayBuffer(0);
      signature = new ArrayBuffer(0);
      userHandle = null;
    }
    class Credential {
      id = "test";
      rawId = new ArrayBuffer(0);
      type = "public-key";
      response = new Assertion();
      getClientExtensionResults() { return {}; }
    }
    vm.runInNewContext(asset, {
      URL, atob, btoa, Uint8Array,
      PublicKeyCredential: Credential,
      AuthenticatorAssertionResponse: Assertion,
      AuthenticatorAttestationResponse: class {},
      document: { querySelectorAll: () => [button], getElementById: () => status },
      navigator: { credentials: { get: async () => new Credential() } },
      location: { origin: "https://example.test", assign: value => { navigated = value; } },
      fetch: async () => ++requests === 1
        ? { ok: true, json: async () => ({ publicKey: { challenge: "" } }) }
        : { ok: true, redirected: true, url },
    });
    await click();
    if (new URL(url).origin === "https://example.test") {
      assert.equal(navigated, new URL(url).href);
      assert.equal(new URL(navigated, "https://example.test").origin, "https://example.test");
    } else {
      assert.equal(navigated, undefined);
      assert.equal(status.textContent, "Sign in failed");
    }
  });
}
