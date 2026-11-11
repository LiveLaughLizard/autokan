// Small in-memory LDAP directory for local development and testing. NOT for production.
//
//   node dev/mock-ldap.mjs                      # ldap://localhost:3893
//   TLS_CERT=cert.pem TLS_KEY=key.pem node dev/mock-ldap.mjs   # ldaps://localhost:3893
//
// Service account: cn=svc,dc=example,dc=org / svc-secret
// People (password = "<uid>-pw"):
//   uid=alice,ou=people            member of autokan-admins
//   uid=bob,ou=people              member of devs; devs is inside engineering, which is inside all-staff
//   uid=dave,ou=contractors,ou=people   member of contractors (group lives in ou=external,ou=groups)
// devs also lists all-staff as a member, so the nesting contains a cycle on purpose.
import ldap from "ldapjs";
import fs from "node:fs";

const PORT = Number(process.env.PORT || 3893);
const BASE = "dc=example,dc=org";
const SVC = { dn: `cn=svc,${BASE}`, password: "svc-secret" };

const person = (uid, cn, ou = "ou=people") => ({
  dn: `uid=${uid},${ou},${BASE}`,
  attributes: { objectClass: ["person", "inetOrgPerson"], uid, cn, mail: `${uid}@example.org`, userPassword: `${uid}-pw` },
});
const group = (cn, members, ou = "ou=groups") => ({
  dn: `cn=${cn},${ou},${BASE}`,
  attributes: { objectClass: ["groupOfNames"], cn, member: members },
});
const P = (uid, ou = "ou=people") => `uid=${uid},${ou},${BASE}`;
const G = (cn, ou = "ou=groups") => `cn=${cn},${ou},${BASE}`;

const entries = [
  { dn: BASE, attributes: { objectClass: ["domain"], dc: "example" } },
  { dn: `ou=people,${BASE}`, attributes: { objectClass: ["organizationalUnit"], ou: "people" } },
  { dn: `ou=contractors,ou=people,${BASE}`, attributes: { objectClass: ["organizationalUnit"], ou: "contractors" } },
  { dn: `ou=groups,${BASE}`, attributes: { objectClass: ["organizationalUnit"], ou: "groups" } },
  person("alice", "Alice Admin"),
  person("bob", "Bob Builder"),
  person("dave", "Dave Contractor", "ou=contractors,ou=people"),
  group("autokan-admins", [P("alice")]),
  group("devs", [P("bob"), G("all-staff")]),
  group("engineering", [G("devs")]),
  group("all-staff", [G("engineering")]),
  group("contractors", [P("dave", "ou=contractors,ou=people")], "ou=external,ou=groups"),
];

const norm = (dn) => dn.toString().replace(/\s*,\s*/g, ",").toLowerCase();
const parent = (dn) => dn.slice(dn.indexOf(",") + 1);
function inScope(entryDn, baseDn, scope) {
  const e = norm(entryDn), b = norm(baseDn);
  // ldapjs reports the scope as a name or as its protocol number, depending on version.
  if (scope === "base" || scope === 0) return e === b;
  if (scope === "one" || scope === "single" || scope === 1) return parent(e) === b;
  return e === b || e.endsWith("," + b);
}

// Every group that contains `dn`, directly or through other groups (for the AD in-chain rule).
function groupsContaining(dn) {
  const found = new Set();
  let frontier = [norm(dn)];
  while (frontier.length) {
    const next = [];
    for (const g of entries.filter((e) => e.attributes.member)) {
      if (!found.has(g.dn) && g.attributes.member.some((m) => frontier.includes(norm(m)))) {
        found.add(g.dn);
        next.push(norm(g.dn));
      }
    }
    frontier = next;
  }
  return found;
}

const tls = process.env.TLS_CERT ? { certificate: fs.readFileSync(process.env.TLS_CERT), key: fs.readFileSync(process.env.TLS_KEY) } : {};
const server = ldap.createServer(tls);

server.bind(BASE, (req, res, next) => {
  const dn = norm(req.dn);
  const password = req.credentials;
  const user = entries.find((e) => norm(e.dn) === dn);
  const ok = (dn === norm(SVC.dn) && password === SVC.password) || (user?.attributes.userPassword && password === user.attributes.userPassword);
  if (!ok) return next(new ldap.InvalidCredentialsError());
  res.end();
  return next();
});

server.search(BASE, (req, res, next) => {
  if (norm(req.connection.ldap.bindDN) !== norm(SVC.dn)) return next(new ldap.InsufficientAccessRightsError());
  const filter = req.filter.toString();
  if (process.env.DEBUG) console.log(`search base=${req.dn} scope=${req.scope} filter=${filter}`);
  const chain = filter.match(/member:1\.2\.840\.113556\.1\.4\.1941:=([^)]*)/i);
  const chained = chain ? groupsContaining(chain[1]) : null;
  for (const e of entries) {
    if (!inScope(e.dn, req.dn, req.scope)) continue;
    const { userPassword, ...attributes } = e.attributes;
    const matches = chained ? chained.has(e.dn) : req.filter.matches(attributes);
    if (matches) res.send({ dn: e.dn, attributes });
  }
  res.end();
  return next();
});

server.listen(PORT, "127.0.0.1", () => console.log(`mock LDAP on ${process.env.TLS_CERT ? "ldaps" : "ldap"}://localhost:${PORT} (base ${BASE}, bind ${SVC.dn} / ${SVC.password})`));
