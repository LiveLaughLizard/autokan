// Minimal SAML 2.0 identity provider for local development and testing. NOT for production.
//
//   node dev/mock-saml.mjs            # http://localhost:9200
//
// Paste http://localhost:9200/metadata (the XML it returns) into Admin → Single sign-on → SAML.
// Every request is approved as the "current user". Switch user or simulate a bad IdP with:
//   curl -X POST localhost:9200/_user -d '{"nameId":"bob@example.org","username":"bob","name":"Bob B","groups":["devs"]}'
//   curl -X POST localhost:9200/_mode -d '{"mode":"tampered"}'
// Modes: ok | unsigned | tampered (attribute changed after signing) | wrong-key | wrong-audience
//        | expired | unsolicited (InResponseTo names a request that was never made)
import http from "node:http";
import zlib from "node:zlib";
import crypto from "node:crypto";
import { SignedXml } from "xml-crypto";
import selfsigned from "selfsigned";

const PORT = Number(process.env.PORT || 9200);
const BASE = `http://localhost:${PORT}`;
const ENTITY_ID = `${BASE}/metadata`;
const make = async (cn) => await selfsigned.generate([{ name: "commonName", value: cn }], { keySize: 2048, algorithm: "sha256" });
const idp = await make("Mock SAML IdP");
const impostor = await make("Not The IdP");
const certBody = (pem) => pem.replace(/-----[A-Z ]+-----|\s/g, "");

let user = { nameId: "alice@example.org", username: "alice", name: "Alice Admin", email: "alice@example.org", groups: ["autokan-admins", "staff"] };
let mode = "ok";

const esc = (s) => String(s).replace(/[<>&"]/g, (c) => ({ "<": "&lt;", ">": "&gt;", "&": "&amp;", '"': "&quot;" })[c]);
const attr = (name, values) =>
  `<saml:Attribute Name="${name}">${[].concat(values).map((v) => `<saml:AttributeValue>${esc(v)}</saml:AttributeValue>`).join("")}</saml:Attribute>`;

function buildResponse({ requestId, acs, audience }) {
  const now = Date.now();
  const iso = (ms) => new Date(ms).toISOString().replace(/\.\d+Z$/, "Z");
  const id = () => "_" + crypto.randomBytes(16).toString("hex");
  const past = mode === "expired" ? 3600_000 : 0;
  const inResponseTo = mode === "unsolicited" ? "id-never-issued" : requestId;
  const aud = mode === "wrong-audience" ? "https://some-other-sp.example/metadata" : audience;
  const assertion =
    `<saml:Assertion xmlns:saml="urn:oasis:names:tc:SAML:2.0:assertion" ID="${id()}" IssueInstant="${iso(now - past)}" Version="2.0">` +
    `<saml:Issuer>${ENTITY_ID}</saml:Issuer>` +
    `<saml:Subject><saml:NameID Format="urn:oasis:names:tc:SAML:1.1:nameid-format:emailAddress">${esc(user.nameId)}</saml:NameID>` +
    `<saml:SubjectConfirmation Method="urn:oasis:names:tc:SAML:2.0:cm:bearer"><saml:SubjectConfirmationData InResponseTo="${inResponseTo}" NotOnOrAfter="${iso(now - past + 300_000)}" Recipient="${acs}"/></saml:SubjectConfirmation></saml:Subject>` +
    `<saml:Conditions NotBefore="${iso(now - past - 60_000)}" NotOnOrAfter="${iso(now - past + 300_000)}"><saml:AudienceRestriction><saml:Audience>${aud}</saml:Audience></saml:AudienceRestriction></saml:Conditions>` +
    `<saml:AuthnStatement AuthnInstant="${iso(now - past)}" SessionIndex="${id()}"><saml:AuthnContext><saml:AuthnContextClassRef>urn:oasis:names:tc:SAML:2.0:ac:classes:PasswordProtectedTransport</saml:AuthnContextClassRef></saml:AuthnContext></saml:AuthnStatement>` +
    `<saml:AttributeStatement>${attr("uid", user.username)}${attr("displayName", user.name)}${attr("mail", user.email)}${attr("groups", user.groups)}</saml:AttributeStatement>` +
    `</saml:Assertion>`;
  let xml =
    `<samlp:Response xmlns:samlp="urn:oasis:names:tc:SAML:2.0:protocol" ID="${id()}" Version="2.0" IssueInstant="${iso(now - past)}" Destination="${acs}" InResponseTo="${inResponseTo}">` +
    `<saml:Issuer xmlns:saml="urn:oasis:names:tc:SAML:2.0:assertion">${ENTITY_ID}</saml:Issuer>` +
    `<samlp:Status><samlp:StatusCode Value="urn:oasis:names:tc:SAML:2.0:status:Success"/></samlp:Status>` +
    assertion +
    `</samlp:Response>`;
  if (mode !== "unsigned") {
    const signer = mode === "wrong-key" ? impostor : idp;
    const sig = new SignedXml({
      privateKey: signer.private,
      publicCert: signer.cert,
      signatureAlgorithm: "http://www.w3.org/2001/04/xmldsig-more#rsa-sha256",
      canonicalizationAlgorithm: "http://www.w3.org/2001/10/xml-exc-c14n#",
    });
    sig.addReference({
      xpath: "//*[local-name(.)='Assertion']",
      transforms: ["http://www.w3.org/2000/09/xmldsig#enveloped-signature", "http://www.w3.org/2001/10/xml-exc-c14n#"],
      digestAlgorithm: "http://www.w3.org/2001/04/xmlenc#sha256",
    });
    sig.computeSignature(xml, { location: { reference: "//*[local-name(.)='Assertion']/*[local-name(.)='Issuer']", action: "after" } });
    xml = sig.getSignedXml();
  }
  if (mode === "tampered") xml = xml.replace(/<saml:AttributeValue>autokan-admins<\/saml:AttributeValue>|<saml:Attribute Name="groups">/, (m) => (m.includes("Name=") ? `${m}<saml:AttributeValue>injected-admins</saml:AttributeValue>` : m));
  return Buffer.from(xml).toString("base64");
}

const metadata = () =>
  `<?xml version="1.0"?><md:EntityDescriptor xmlns:md="urn:oasis:names:tc:SAML:2.0:metadata" entityID="${ENTITY_ID}">` +
  `<md:IDPSSODescriptor WantAuthnRequestsSigned="false" protocolSupportEnumeration="urn:oasis:names:tc:SAML:2.0:protocol">` +
  `<md:KeyDescriptor use="signing"><ds:KeyInfo xmlns:ds="http://www.w3.org/2000/09/xmldsig#"><ds:X509Data><ds:X509Certificate>${certBody(idp.cert)}</ds:X509Certificate></ds:X509Data></ds:KeyInfo></md:KeyDescriptor>` +
  `<md:NameIDFormat>urn:oasis:names:tc:SAML:1.1:nameid-format:emailAddress</md:NameIDFormat>` +
  `<md:SingleSignOnService Binding="urn:oasis:names:tc:SAML:2.0:bindings:HTTP-Redirect" Location="${BASE}/sso"/>` +
  `</md:IDPSSODescriptor></md:EntityDescriptor>`;

const readBody = (req) => new Promise((resolve) => { let d = ""; req.on("data", (c) => (d += c)); req.on("end", () => resolve(d)); });

http
  .createServer(async (req, res) => {
    const url = new URL(req.url, BASE);
    if (url.pathname === "/metadata") return res.writeHead(200, { "content-type": "application/samlmetadata+xml" }).end(metadata());
    if (url.pathname === "/_user" && req.method === "POST") { user = { ...user, ...JSON.parse(await readBody(req)) }; return res.end(JSON.stringify(user)); }
    if (url.pathname === "/_mode" && req.method === "POST") { mode = JSON.parse(await readBody(req)).mode; return res.end(mode); }
    if (url.pathname === "/sso") {
      const request = zlib.inflateRawSync(Buffer.from(url.searchParams.get("SAMLRequest"), "base64")).toString();
      const requestId = request.match(/\bID="([^"]+)"/)[1];
      const acs = request.match(/AssertionConsumerServiceURL="([^"]+)"/)[1];
      const audience = request.match(/<(?:\w+:)?Issuer[^>]*>([^<]+)</)[1];
      const relay = url.searchParams.get("RelayState") || "";
      res.writeHead(200, { "content-type": "text/html" });
      return res.end(
        `<!doctype html><body onload="document.forms[0].submit()"><form method="post" action="${esc(acs)}">` +
          `<input type="hidden" name="SAMLResponse" value="${buildResponse({ requestId, acs, audience })}">` +
          `<input type="hidden" name="RelayState" value="${esc(relay)}"><noscript><button>Continue</button></noscript></form></body>`,
      );
    }
    res.writeHead(404).end();
  })
  .listen(PORT, () => console.log(`mock SAML IdP on ${BASE} (metadata at ${BASE}/metadata)`));
