// Only ceremony glue: challenges and verification state stay on the server.
function decode(value: string): Uint8Array<ArrayBuffer> {
  const binary = atob(value.replace(/-/g, "+").replace(/_/g, "/"));
  return Uint8Array.from(binary, c => c.charCodeAt(0));
}
function encode(value: ArrayBuffer | null): string | null {
  if (value === null) return null;
  return btoa(String.fromCharCode(...new Uint8Array(value))).replace(/\+/g, "-").replace(/\//g, "_").replace(/=+$/, "");
}
type EncodedDescriptor = Omit<PublicKeyCredentialDescriptor, "id"> & { id: string };
async function ceremony(button: HTMLButtonElement): Promise<void> {
  const base = button.dataset.base;
  const csrf = button.dataset.csrf;
  const registering = button.dataset.passkey === "register";
  if (!base || !csrf) throw new Error("Sign in failed");
  const headers = { "Content-Type": "application/json", "X-CSRF-Token": csrf };
  const started = await fetch(`${base}/passkey/start`, { method: "POST", headers, body: "{}", credentials: "same-origin" });
  if (!started.ok) throw new Error("Sign in failed");
  const challenge = await started.json();
  const options = challenge.publicKey;
  options.challenge = decode(options.challenge);
  let credential: Credential | null;
  if (registering) {
    options.user.id = decode(options.user.id);
    options.excludeCredentials = options.excludeCredentials?.map((c: EncodedDescriptor) => ({ ...c, id: decode(c.id) }));
    credential = await navigator.credentials.create({ publicKey: options });
  } else {
    options.allowCredentials = options.allowCredentials?.map((c: EncodedDescriptor) => ({ ...c, id: decode(c.id) }));
    credential = await navigator.credentials.get({ publicKey: options });
  }
  if (!(credential instanceof PublicKeyCredential)) throw new Error("Sign in failed");
  const response = credential.response;
  let encodedResponse: object;
  if (response instanceof AuthenticatorAttestationResponse) {
    encodedResponse = { clientDataJSON: encode(response.clientDataJSON), attestationObject: encode(response.attestationObject), transports: response.getTransports() };
  } else if (response instanceof AuthenticatorAssertionResponse) {
    encodedResponse = { clientDataJSON: encode(response.clientDataJSON), authenticatorData: encode(response.authenticatorData), signature: encode(response.signature), userHandle: encode(response.userHandle) };
  } else throw new Error("Sign in failed");
  const finished = await fetch(`${base}/passkey/finish`, { method: "POST", headers, credentials: "same-origin", redirect: "follow", body: JSON.stringify({ id: credential.id, rawId: encode(credential.rawId), type: credential.type, response: encodedResponse, extensions: credential.getClientExtensionResults() }) });
  // Follow the server's validated destination, including password/setup gates.
  // Never navigate to an origin supplied by a different service.
  if (finished.redirected) {
    const destination = new URL(finished.url);
    if (destination.origin !== location.origin) throw new Error("Sign in failed");
    location.assign(destination.href);
    return;
  }
  if (!finished.ok) throw new Error("Sign in failed");
  location.assign(base.startsWith("/admin/") ? "/admin/u/security" : "/u/security");
}
for (const button of document.querySelectorAll<HTMLButtonElement>("button[data-passkey]")) {
  button.addEventListener("click", async () => {
    button.disabled = true;
    try { await ceremony(button); }
    catch { const status = document.getElementById("passkey-status"); if (status) status.textContent = "Sign in failed"; }
    finally { button.disabled = false; }
  });
}
