# Autokan

A self-hosted Trello-style kanban with single sign-on (OIDC, LDAP, SAML), group-based RBAC and a
per-board LLM agent. Rust (axum + maud), htmx + SortableJS, Tailwind CSS 4 + daisyUI 5.
One binary; SQLite by default, or PostgreSQL, MySQL / MariaDB or SQL Server.

## Run

```sh
nix-shell                 # or any environment with cargo, a C compiler, node, and (for SAML) libxml2 + xmlsec1
cargo run --release
```

On first start the **break glass** account is created and its generated password is printed
once to stderr. Sign in at <http://localhost:3000>, open **Admin**, and configure single sign-on.

| Variable | Default | |
|---|---|---|
| `AUTOKAN_BIND` | `127.0.0.1:3000` | listen address |
| `AUTOKAN_PUBLIC_URL` | `http://localhost:3000` | external URL; used for OIDC / SAML callback URLs. `https://` turns on `Secure` cookies |
| `AUTOKAN_DATA_DIR` | `data` | uploads, and the SQLite database when that backend is used |
| `AUTOKAN_DATABASE_URL` | `sqlite://<data dir>/autokan.db` | see *Database* |
| `AUTOKAN_BREAKGLASS_USER` | `breakglass` | only read when the account is first created |
| `AUTOKAN_BREAKGLASS_PASSWORD` | generated | only read when the account is first created |
| `AUTOKAN_MAX_UPLOAD_MB` | `200` | per-file upload limit |

Lost the break glass password? `autokan reset-breakglass` prints a new one and signs the account out.

Run it behind a TLS-terminating reverse proxy that preserves the `Host` header.

### Building without SAML

SAML signature verification uses the xmlsec1 C library, so the default build needs `libxml2`,
`xmlsec1` and `libclang` (the provided `shell.nix` has them). To build without those:

```sh
cargo build --release --no-default-features
```

## Database

Set `AUTOKAN_DATABASE_URL`; the schema is created (and upgraded) on start.

| Backend | URL |
|---|---|
| SQLite (default) | `sqlite://data/autokan.db` |
| PostgreSQL | `postgres://user:password@host/database` |
| MySQL 8.0.13+ / MariaDB | `mysql://user:password@host/database` |
| SQL Server | `mssql://user:password@host:1433/database?encrypt=true&trust_cert=false`, or an ADO.NET connection string |

The database must already exist (except SQLite). Data is not copied between backends.

Database access goes through the traits in `src/db/mod.rs` (`Conn`, `Database`, `Transaction`).
Application SQL sticks to a common subset — `?` placeholders, no vendor functions or upserts,
integers for booleans, UTC timestamps written as text — and the schema is declared once in
`src/db/schema.rs` and rendered per dialect. A new backend implements the three traits and adds
its dialect to the schema renderer.

## Access model

**Platform** — the break glass account can do everything. Everyone else starts with no platform
permissions. In *Admin → Roles & groups* you create roles (bundles of the permissions below)
and map them to group names delivered by the identity provider.

`platform.admin` · `admin.oidc` (all single sign-on settings) · `admin.rbac` · `admin.users` · `admin.agent` · `teams.create` · `teams.manage`

The Admin link only appears for the break glass account and for users holding an `admin.*` permission.

**Teams** — created by anyone with `teams.create`. A *team admin* (or `teams.manage`) creates
projects, manages every board in the team and grants project permissions. Team *members* can
see the team's projects read-only and join them.

**Projects** — hold any number of boards. Each user who has joined a project has five
switches, set ad hoc by team admins: *Manage* (boards, columns, labels, agent settings),
*Create cards*, *Comment*, *Edit own cards*, *Edit all cards*. Joining gives create / comment /
edit-own.

Team names are unique; project names are unique within a team; board names within a project.

## Single sign-on

Under *Admin → Single sign-on* you pick a method, configure it and enable it; any combination can be enabled, and groups from all of them feed the same role mappings. Each has a
**Custom CA certificate** box: paste the PEM of a private certificate authority and it is trusted,
in addition to the system CAs, for that provider's connections only.

**OpenID Connect** — authorization code flow with PKCE. Groups come from a claim in the access
token (then the ID token, then userinfo).

**LDAP / Active Directory** — a username and password form on the sign-in page. Autokan binds
with a service account, finds the user, and verifies the password by binding as the user.
* *Search bases and depth*: one or more base DNs for users and for groups (the directory root for
  the full tree, or specific OUs), each searched as a whole subtree, one level, or the base entry only.
* *Nested groups*: off; followed by walking parent groups (any server, cycle-safe); or resolved
  by Active Directory's in-chain matching rule in one query.
* The **Look up** form under the settings shows the entry and groups the directory returns for a
  username without signing anyone in.

**SAML 2.0** — SP-initiated, HTTP-Redirect to the IdP and HTTP-POST back. Paste the IdP's metadata
(or give an `https://` metadata URL); register Autokan using the entity ID and ACS URL shown in the
panel, or its metadata at `/auth/saml/metadata`. Assertions must be signed (RSA or ECDSA with
SHA-256 or stronger) by a certificate from that metadata. IdP-initiated sign-in and single logout
are not supported. Groups come from a multi-valued assertion attribute (`groups` by default).

## GitHub and GitLab synchronization

A board can be linked (*board settings → Synchronization*, team admins only) to a **GitHub
project** (Projects v2: the options of its single-select "Status" field are the columns) or a
**GitLab issue board** (its label lists are the columns, plus the built-in Open and Closed).

* When linking, columns with the same name are paired; every other column and card is copied
  across in both directions.
* Afterwards columns (added, renamed, removed, re-ordered) and cards (added, edited, removed,
  moved between columns, re-ordered) are kept the same on both sides. Changes made in Autokan are
  sent within a few seconds; changes made on GitHub / GitLab are picked up about once a minute, or
  on **Sync now**. If both sides changed the same thing, the remote side wins.
* Cards created in Autokan become draft issues on GitHub and issues on GitLab. Comments, labels
  and assignees are not synchronized. Built-in remote columns (GitLab's Open / Closed, GitHub's
  "No Status") cannot be renamed, removed or moved.
* Tokens: GitHub needs the `project` scope (and `repo` to edit real issues); GitLab needs `api`.
  For GitHub Enterprise Server or self-managed GitLab, give the server's address when linking.

## Agent

Each board has an agent console, and each column a **Send To Agent** button that sends the
column's cards with a saved prompt. The agent's tools are scoped to the open board and run with
the permissions of the user who is chatting.

**Services** are configured once, under *Admin → Agent services* (`admin.agent`): a name, a
provider, an API key and the list of model ids to offer.

| Provider | Wire format | Default base URL |
|---|---|---|
| Claude (Anthropic) | Messages API (`/v1/messages`) | `https://api.anthropic.com` |
| ChatGPT (OpenAI) | Chat Completions | `https://api.openai.com/v1` |
| Gemini (Google) | Google's OpenAI-compatible endpoint | `https://generativelanguage.googleapis.com/v1beta/openai` |
| OpenAI-compatible | Chat Completions (Ollama, vLLM, LiteLLM, …) | you supply it |

**Which model runs** is resolved per request: the column's override (Send To Agent only) →
the project's default (*project page → Agent*) → the platform default (*Admin → Agent services*).
Conversations are stored in a provider-neutral form, so a board's history carries across services.

## Development

```sh
npm install && npm run css      # rebuild static/app.css after changing classes in src/ or static/app.js
node dev/mock-oidc.mjs          # OIDC provider on :9000 (issuer http://localhost:9000, client "autokan")
node dev/mock-ldap.mjs          # LDAP directory on :3893 with nested groups (see the file header for accounts)
node dev/mock-saml.mjs          # SAML IdP on :9200 (metadata at /metadata; can also emit forged responses)
node dev/mock-gitlab.mjs        # GitLab REST imitation on :9300 for the board sync (token glpat-test, project group/proj)
node dev/mock-github.mjs        # GitHub GraphQL imitation on :9400 for the board sync (token ghp-test, owner my-org, project 1)
node dev/mock-llm.mjs           # scripted LLM on :9100 (OpenAI-compatible at /v1, Anthropic Messages at /)
```

The mock OIDC and LDAP servers serve TLS when `TLS_CERT` and `TLS_KEY` are set, for trying the
custom CA setting. `static/` is compiled into the binary, so rebuild after changing CSS or JS.
