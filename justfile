# Autokan task runner. `just` lists everything; run inside `nix-shell` (or with cargo, node,
# docker and helm on your PATH).

set dotenv-load := true

image := "autokan:dev"
pg_url := "postgres://postgres:autokan@localhost:55432/autokan"
mysql_url := "mysql://root:autokan@localhost:53306/autokan"
mssql_url := "mssql://sa:Autokan-Dev-1@localhost:51433/autokan?encrypt=true&trust_cert=true"

# List the available recipes
default:
    @just --list --unsorted

# ---- run ------------------------------------------------------------------------------------

# Run a development build on http://localhost:3000 (extra arguments go to autokan)
run *args:
    cargo run -- {{ args }}

# Run with verbose logging (requests, SQL errors with context) and backtraces
debug *args:
    RUST_LOG="autokan=debug,tower_http=debug" RUST_BACKTRACE=1 cargo run -- {{ args }}

# Run an optimized build
run-release *args:
    cargo run --release -- {{ args }}

# Run against a throwaway data directory, leaving ./data alone
run-scratch:
    AUTOKAN_DATA_DIR="$(mktemp -d)" AUTOKAN_BREAKGLASS_PASSWORD=breakglass-dev cargo run

# Print a new break glass password (signs the account out everywhere)
reset-breakglass:
    cargo run -- reset-breakglass

# ---- build and check ------------------------------------------------------------------------

# Compile a development build
build:
    cargo build

# Compile an optimized build into target/release/autokan
build-release:
    cargo build --release

# Compile without SAML (no libxml2 / xmlsec1 needed)
build-no-saml:
    cargo build --release --no-default-features

# Type-check quickly without producing a binary
check:
    cargo check

# Run clippy
lint:
    cargo clippy

# Check both feature sets and lint: the things worth doing before pushing
verify: lint
    cargo check --no-default-features
    helm lint charts/autokan

# Remove build output
clean:
    cargo clean

# ---- front-end assets -----------------------------------------------------------------------

# Install the Node packages (Tailwind, daisyUI and the dev mock servers' dependencies)
setup:
    npm install

# Rebuild static/app.css (needed after changing CSS classes in src/ or static/app.js)
css:
    npm run css

# Rebuild static/app.css whenever a source file changes
css-watch:
    npm run css:watch

# ---- mock services for local development ---------------------------------------------------

# Start one mock server: oidc, ldap, saml, llm, gitlab, github or jira
mock name:
    node dev/mock-{{ name }}.mjs

# Start every mock server at once (Ctrl-C stops them all)
mocks:
    #!/usr/bin/env bash
    trap 'kill 0' EXIT
    for name in oidc ldap saml llm gitlab github jira; do node "dev/mock-$name.mjs" & done
    wait

# ---- databases ------------------------------------------------------------------------------

# Start a throwaway PostgreSQL in Docker
db-postgres:
    docker run -d --rm --name autokan-postgres -e POSTGRES_PASSWORD=autokan -e POSTGRES_DB=autokan -p 55432:5432 postgres:17-alpine
    @echo "AUTOKAN_DATABASE_URL={{ pg_url }}"

# Start a throwaway MySQL in Docker
db-mysql:
    docker run -d --rm --name autokan-mysql -e MYSQL_ROOT_PASSWORD=autokan -e MYSQL_DATABASE=autokan -p 53306:3306 mysql:8.4
    @echo "AUTOKAN_DATABASE_URL={{ mysql_url }}"

# Start a throwaway SQL Server in Docker and create the database (accepts Microsoft's EULA)
db-mssql:
    docker run -d --rm --name autokan-mssql -e ACCEPT_EULA=Y -e MSSQL_SA_PASSWORD=Autokan-Dev-1 -p 51433:1433 mcr.microsoft.com/mssql/server:2022-latest
    @echo "waiting for SQL Server..."
    @until docker exec autokan-mssql /opt/mssql-tools18/bin/sqlcmd -C -S localhost -U sa -P Autokan-Dev-1 -Q "IF DB_ID('autokan') IS NULL CREATE DATABASE autokan" >/dev/null 2>&1; do sleep 2; done
    @echo "AUTOKAN_DATABASE_URL={{ mssql_url }}"

# Stop the throwaway database containers (their data is discarded)
db-stop:
    -docker stop autokan-postgres autokan-mysql autokan-mssql

# Run against the database started by `just db-postgres`
run-postgres:
    AUTOKAN_DATABASE_URL="{{ pg_url }}" cargo run

# Run against the database started by `just db-mysql`
run-mysql:
    AUTOKAN_DATABASE_URL="{{ mysql_url }}" cargo run

# Run against the database started by `just db-mssql`
run-mssql:
    AUTOKAN_DATABASE_URL="{{ mssql_url }}" cargo run

# ---- containers and Kubernetes --------------------------------------------------------------

# Build the distroless container image
docker-build:
    docker build -t {{ image }} .

# Run the container image on http://localhost:3000 with a named data volume
docker-run: docker-build
    docker run --rm -p 3000:3000 -v autokan-data:/data --read-only --cap-drop ALL {{ image }}

# Start with Docker Compose (SQLite)
up:
    docker compose up -d --build

# Start with Docker Compose plus the bundled PostgreSQL (set AUTOKAN_DATABASE_URL in .env; see compose.yaml)
up-postgres:
    docker compose --profile postgres up -d --build

# Follow the Compose logs (the first start prints the break glass password)
logs:
    docker compose logs -f autokan

# Stop Docker Compose (volumes are kept)
down:
    docker compose --profile postgres down

# Lint the Helm chart
helm-lint:
    helm lint charts/autokan

# Render the Helm chart to stdout (extra arguments go to helm, e.g. --set ingress.enabled=true)
helm-template *args:
    helm template autokan charts/autokan {{ args }}

# Install or upgrade the chart into the current Kubernetes context
helm-install *args:
    helm upgrade --install autokan charts/autokan {{ args }}
