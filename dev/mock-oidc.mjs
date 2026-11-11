// Minimal OpenID Connect provider for local development and testing. NOT for production.
//
//   node dev/mock-oidc.mjs            # listens on http://localhost:9000
//
// Admin panel settings: issuer http://localhost:9000, client id "autokan", secret "secret".
// Every sign-in is auto-approved as the "current user". Switch user with:
//   curl -X POST localhost:9000/_user -d '{"sub":"u2","username":"bob","name":"Bob B","groups":["devs"]}'
import http from "node:http";
import https from "node:https";
import fs from "node:fs";
import crypto from "node:crypto";

const PORT = Number(process.env.PORT || 9000);
// Set TLS_CERT and TLS_KEY to serve HTTPS (for trying the "custom CA certificate" setting).
const TLS = process.env.TLS_CERT ? { cert: fs.readFileSync(process.env.TLS_CERT), key: fs.readFileSync(process.env.TLS_KEY) } : null;
const ISSUER = process.env.ISSUER || `${TLS ? "https" : "http"}://localhost:${PORT}`;
const { publicKey, privateKey } = crypto.generateKeyPairSync("rsa", { modulusLength: 2048 });
const jwk = { ...publicKey.export({ format: "jwk" }), kid: "dev-key", use: "sig", alg: "RS256" };

let user = { sub: "u1", username: "alice", name: "Alice Admin", email: "alice@example.com", groups: ["autokan-admins"] };
const codes = new Map();

const b64 = (v) => Buffer.from(typeof v === "string" ? v : JSON.stringify(v)).toString("base64url");
function jwt(claims) {
  const now = Math.floor(Date.now() / 1000);
  const body = `${b64({ alg: "RS256", typ: "JWT", kid: jwk.kid })}.${b64({ iss: ISSUER, iat: now, exp: now + 600, ...claims })}`;
  return `${body}.${crypto.sign("RSA-SHA256", Buffer.from(body), privateKey).toString("base64url")}`;
}
const json = (res, status, body) => {
  res.writeHead(status, { "content-type": "application/json" });
  res.end(JSON.stringify(body));
};
const readBody = (req) =>
  new Promise((resolve) => {
    let data = "";
    req.on("data", (c) => (data += c));
    req.on("end", () => resolve(data));
  });

const handler = async (req, res) => {
    const url = new URL(req.url, ISSUER);
    if (url.pathname === "/.well-known/openid-configuration") {
      return json(res, 200, {
        issuer: ISSUER,
        authorization_endpoint: `${ISSUER}/authorize`,
        token_endpoint: `${ISSUER}/token`,
        jwks_uri: `${ISSUER}/jwks`,
        userinfo_endpoint: `${ISSUER}/userinfo`,
        token_endpoint_auth_methods_supported: ["client_secret_basic", "client_secret_post"],
      });
    }
    if (url.pathname === "/jwks") return json(res, 200, { keys: [jwk] });
    if (url.pathname === "/_user" && req.method === "POST") {
      user = { ...user, ...JSON.parse(await readBody(req)) };
      return json(res, 200, user);
    }
    if (url.pathname === "/authorize") {
      const q = url.searchParams;
      const code = crypto.randomBytes(16).toString("hex");
      codes.set(code, { nonce: q.get("nonce"), challenge: q.get("code_challenge"), client: q.get("client_id"), user: { ...user } });
      const back = new URL(q.get("redirect_uri"));
      back.searchParams.set("code", code);
      back.searchParams.set("state", q.get("state"));
      res.writeHead(302, { location: back.toString() });
      return res.end();
    }
    if (url.pathname === "/token" && req.method === "POST") {
      const form = new URLSearchParams(await readBody(req));
      const grant = codes.get(form.get("code"));
      codes.delete(form.get("code"));
      const challenge = crypto.createHash("sha256").update(form.get("code_verifier") || "").digest("base64url");
      if (!grant || grant.challenge !== challenge) return json(res, 400, { error: "invalid_grant" });
      const u = grant.user;
      return json(res, 200, {
        token_type: "Bearer",
        expires_in: 600,
        // Groups deliberately appear only in the access token, as Autokan reads them from there first.
        access_token: jwt({ sub: u.sub, aud: "api", groups: u.groups }),
        id_token: jwt({ sub: u.sub, aud: grant.client, nonce: grant.nonce, preferred_username: u.username, name: u.name, email: u.email }),
      });
    }
    if (url.pathname === "/userinfo") return json(res, 200, { sub: user.sub, name: user.name, email: user.email });
    json(res, 404, { error: "not_found" });
  };
(TLS ? https.createServer(TLS, handler) : http.createServer(handler))
  .listen(PORT, () => console.log(`mock OIDC provider on ${ISSUER} (client id: autokan, any secret)`));
