# Autokan

A self-hosted Trello-style kanban with single sign-on (OIDC, LDAP, SAML), group-based RBAC and a
per-board LLM agent. Rust (axum + maud), htmx + SortableJS, Tailwind CSS 4 + daisyUI 5.
One binary; SQLite by default, or PostgreSQL, MySQL / MariaDB or SQL Server.

## Run

```sh
nix-shell                 # or any environment with cargo, a C compiler, node, and (for SAML) libxml2 + xmlsec1
just run                  # or: cargo run
```

On first start the **break glass** account is created and its generated password is printed
once to stderr. Sign in at <http://localhost:3000>, open **Admin**, and configure single sign-on.

### Common tasks (`just`)

`just` with no arguments lists every recipe. The ones used most:

| | |
|---|---|
| `just run` / `just run-release` | run a development / optimized build |
| `just debug` | run with request and debug logging and backtraces |
| `just run-scratch` | run against a throwaway data directory (break glass password `breakglass-dev`) |
| `just reset-breakglass` | print a new break glass password |
| `just check` / `just lint` / `just verify` | type-check; clippy; both feature sets, clippy and the Helm chart |
| `just css` / `just css-watch` | rebuild `static/app.css` |
| `just mocks` / `just mock ldap` | start all, or one, of the mock services under `dev/` |
| `just db-postgres` + `just run-postgres` | throwaway PostgreSQL in Docker, and run against it (also `-mysql`, `-mssql`; `just db-stop`) |
| `just docker-build` / `just docker-run` | build / run the container image |
| `just up` / `just logs` / `just down` | Docker Compose |
| `just helm-lint` / `just helm-template` / `just helm-install` | the Helm chart |

### Containers

The `Dockerfile` builds a **distroless** image (Debian 12 `cc`, non-root, no shell): the binary,
the C libraries SAML needs, and nothing else. Data lives in `/data`.

```sh
docker build -t autokan .
docker run -p 3000:3000 -v autokan-data:/data --read-only --cap-drop ALL autokan
```

`compose.yaml` does the same with Docker Compose (`docker compose up -d --build`), with an optional
bundled PostgreSQL (`--profile postgres`; see the comments in the file). Since the image has no
shell, its health check is the binary itself (`autokan healthcheck`), and `GET /healthz` answers
`ok` when the process is up and the database responds.

### Kubernetes

`charts/autokan` is a Helm chart for the same image:

```sh
helm upgrade --install autokan charts/autokan \
  --set image.repository=registry.example.com/autokan --set image.tag=0.1.0 \
  --set publicUrl=https://autokan.example.com \
  --set ingress.enabled=true --set 'ingress.hosts[0].host=autokan.example.com'
```

It runs a single replica (uploads, and SQLite if used, live on one volume) as a non-root user
with a read-only root filesystem, stores data on a PersistentVolumeClaim that is kept on
uninstall, and probes `/healthz`. Set `database.url` (or `database.existingSecret`) to use
PostgreSQL, MySQL or SQL Server. `values.yaml` documents every setting.

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

`platform.admin` · `admin.oidc` (all single sign-on settings) · `admin.rbac` · `admin.users` · `admin.agent` · `admin.jira` · `teams.create` · `teams.manage` · `api.keys` (create and use personal API keys)

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

## Jira

Jira works at the level of a single card rather than a whole board. The connection (Jira Cloud
with an email and API token, or self-hosted Jira with a personal access token or username and
password; a private CA can be pasted) can live in either of two places:

* **For the whole organization** — *Admin → Jira* (`admin.jira`). A team admin then switches Jira
  on for a project on the *project page → Jira* by choosing "The organization's Jira" and naming
  the Jira project key; no credentials are handled at project level. Switching the organization's
  connection off pauses Jira for every project that uses it.
* **For one project** — on the same project page, choose "A connection just for this project"
  and enter that project's own address and credentials.

Then, in any card of a connected project:

* **Create Jira issue** makes an issue in the configured Jira project. Its description starts
  with a link back to the card, followed a couple of lines down by the card's description.
* From then on the card's title, description, comments (new, edited and deleted) and attached
  files are sent to that issue as they change. Images appear inline in Jira; videos and other
  files become attachment links.
* The card shows the issue's current status and a button for each workflow transition Jira
  offers, so the issue can be moved through its stages from Autokan.

Nothing is copied from Jira back into the card. If Jira is unreachable the card still saves; the
card's Jira panel shows the failure and **Resend** sends everything again.

## Agent

Each board has an agent console, and each column a **Send To Agent** button that sends the
column's cards with a saved prompt. The agent's tools are scoped to the open board and run with
the permissions of the user who is chatting.

**Send To Agent** adds a tab at the bottom of the board. Clicking it opens a console that streams
what the agent is doing with the column: the model in use, each tool call and whether it worked,
and the agent's own messages. The ✕ on a tab stops a run in progress (changes already made are
kept) or closes a finished one. Tabs live in the page, so reloading the board ends a run. A run
keeps going until the model stops calling tools; there is no step limit.

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

## API keys, JSON API and MCP server

Users holding the `api.keys` permission can create personal keys under *user menu → API keys*.
A key acts as its owner, with exactly that user's team and project permissions, checked on every
call. It is shown once and stored as a hash; it stops working when it expires or is revoked, when
the account is disabled, or when the owner no longer holds `api.keys`. Keys reach the two
interfaces below and nothing else (no admin pages, no session).

Both interfaces offer the board agent's tools: `get_board`, `get_card`, `create_card`,
`update_card`, `move_card`, `delete_card`, `add_comment`, `set_card_label`,
`set_card_assignee`, `create_column`, `update_column`, `delete_column`, `create_label`.

**JSON API** — send `Authorization: Bearer <key>`.

| | |
|---|---|
| `GET /api/v1/boards` | boards you can work on |
| `GET /api/v1/boards/{id}` | columns, cards, labels and members |
| `GET /api/v1/tools` | the tools and their arguments (JSON Schema) |
| `POST /api/v1/boards/{id}/tools/{tool}` | run a tool; the body is its arguments as a JSON object |

```sh
curl -H "Authorization: Bearer $AUTOKAN_API_KEY" -d '{"card_id": 7, "column_id": 3}' \
     https://autokan.example.com/api/v1/boards/1/tools/move_card
```

Errors are `{"error": "..."}` with a 400, 401, 403 or 404 status.

**MCP server** — `POST /mcp` (Streamable HTTP, stateless, tools only) with the same header. The
tools take an extra `board_id`, and `list_boards` finds one.

```sh
claude mcp add --transport http autokan https://autokan.example.com/mcp \
  --header "Authorization: Bearer $AUTOKAN_API_KEY"
```

**opencode** — copy `integrations/opencode/autokan.ts` to `.opencode/plugins/` (or
`~/.config/opencode/plugins/`) and set `AUTOKAN_URL` and `AUTOKAN_API_KEY`; the plugin connects
opencode to the MCP server. Without the plugin, the same thing in `opencode.jsonc`:

```jsonc
{ "mcp": { "servers": { "autokan": {
  "type": "remote", "url": "https://autokan.example.com/mcp", "oauth": false,
  "headers": { "Authorization": "Bearer {env:AUTOKAN_API_KEY}" }
} } } }
```

An open board does not notice changes made through a key until it is next refreshed.

## Development

`just mocks` starts all of the following at once.

```sh
npm install && npm run css      # rebuild static/app.css after changing classes in src/ or static/app.js
node dev/mock-oidc.mjs          # OIDC provider on :9000 (issuer http://localhost:9000, client "autokan")
node dev/mock-ldap.mjs          # LDAP directory on :3893 with nested groups (see the file header for accounts)
node dev/mock-saml.mjs          # SAML IdP on :9200 (metadata at /metadata; can also emit forged responses)
node dev/mock-gitlab.mjs        # GitLab REST imitation on :9300 for the board sync (token glpat-test, project group/proj)
node dev/mock-github.mjs        # GitHub GraphQL imitation on :9400 for the board sync (token ghp-test, owner my-org, project 1)
node dev/mock-jira.mjs          # Jira REST imitation on :9500 (project ABC; see the file header for credentials)
node dev/mock-llm.mjs           # scripted LLM on :9100 (OpenAI-compatible at /v1, Anthropic Messages at /)
```

The mock OIDC and LDAP servers serve TLS when `TLS_CERT` and `TLS_KEY` are set, for trying the
custom CA setting. `static/` is compiled into the binary, so rebuild after changing CSS or JS.
