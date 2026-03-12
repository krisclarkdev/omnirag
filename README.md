# 🔄 OmniRAG

A Rust application that synchronizes a local directory of files with an [Open WebUI](https://github.com/open-webui/open-webui) knowledge base. It hashes files, tracks state in Redis, dynamically prepends configurable context metadata, and interfaces with the Open WebUI REST API — deployed as a 1:1 Pod (one Rust container + one Redis container per Knowledge Base).

---

### ⚡ Quick Start

```bash
git clone https://github.com/krisclarkdev/omnirag.git && cd omnirag
# Edit docker-compose.yml → set your documents path in the volume mount
docker compose up --build -d
# Open http://localhost:3000 → Configure → Trigger Sync
```

---

## 📑 Table of Contents

- [✨ Features](#-features)
- [🚀 Quick Start](#-quick-start-1)
- [🖥️ Production Deployment](#️-production-deployment)
- [🏗️ Architecture: The 1:1 Pod](#️-architecture-the-11-pod)
- [🧩 Best Practices: Chunking](#-best-practices-chunking)
- [🤖 REST API (Automation)](#-rest-api-automation)
- [💻 CLI](#-cli)
- [🌐 Web UI](#-web-ui)
- [🐳 Docker](#-docker)
- [📐 Hardware Requirements & Scaling](#-hardware-requirements--scaling)
- [🧪 Testing](#-testing)
- [🔄 CI/CD](#-cicd)
- [📁 Source Files](#-source-files)
- [🧪 End-to-End Test Results](#-end-to-end-test-results)
- [📄 License](#-license)

---

## ✨ Features

- **Three-phase sync** — Reconciles state, ingests new/updated files, and cleans up orphans
- **Pre-sync reconciliation** — Queries Open WebUI before syncing to heal state and prevent duplicates; skips ghost files
- **SHA-256 change detection** — Only re-uploads files whose contents actually changed
- **Context-dirty flag** — Context changes via Web UI or CLI trigger re-upload on next sync without file modification
- **Configurable context header** — Set a custom label per deployment (e.g., "Solutions Architect Context", "Project Lore")
- **Dynamic KB picker** — Fetches available Knowledge Bases from Open WebUI and presents a dropdown selector
- **Convert to Markdown** — Optionally wraps text files in language-specific code fences and uploads as `.md` for better LLM chunking
- **Configurable concurrency** — Tune max simultaneous uploads (default 5, 0 = unlimited)
- **Context Manager dialog** — Searchable, paginated modal for managing per-file context strings
- **Redis Functions (Lua)** — Atomic server-side operations for state management
- **Redis AOF persistence** — Native append-only file for durable state across container restarts
- **File filtering** — Extension whitelist, OS file ignore list, and `.ragignore` support
- **API retry with backoff** — Exponential backoff (3 attempts) on all Open WebUI API calls
- **Binary-safe context injection** — Prepends metadata to text files only; binary files (PDF) are uploaded raw to prevent corruption
- **JSON REST API** — Automation-ready endpoints for cron, n8n, webhooks, and external triggers
- **Swagger / OpenAPI** — Interactive API docs at `/docs` with auto-generated OpenAPI 3.0 spec
- **Embedded Web UI** — HTMX + Tailwind glassmorphism dark-mode dashboard with contextual help icons
- **CLI tools** — `set-context`, `get-context`, `sync`, `serve`
- **1:1 Pod architecture** — Each Knowledge Base gets its own isolated Rust + Redis container pair
- **CI/CD ready** — GitHub Actions workflow for automated Docker builds and GHCR publishing

---

## 🚀 Quick Start

### Deploy with Docker Compose

```bash
# 1. Clone
git clone https://github.com/krisclarkdev/omnirag.git && cd omnirag

# 2. Create your documents directory
mkdir -p my-docs && cp /path/to/your/docs/* my-docs/

# 3. Deploy (starts both the Rust app and its dedicated Redis)
docker compose up --build -d

# 4. Configure via http://localhost:3000
# 5. Click "▶ Trigger Sync" in the UI
```

### Pull from GitHub Container Registry (GHCR)

Once the GitHub Actions workflow runs on your `main` branch, the image is published to GHCR:

```yaml
services:
  redis:
    image: redis:7-alpine
    command: redis-server --appendonly yes
    volumes:
      - ./redis-data:/data
    restart: unless-stopped

  omnirag:
    image: ghcr.io/krisclarkdev/omnirag:main
    ports:
      - "3000:3000"
    volumes:
      - ./my-documents:/rag
    environment:
      - REDIS_URL=redis://redis:6379/0
    depends_on:
      - redis
    restart: unless-stopped
```

---

## 🖥️ Production Deployment

Once the GitHub Actions workflow completes on `main`, the Docker image is published to GHCR. Deploy to your server with:

```bash
# 1. SSH into your production server
ssh user@your-server

# 2. Create working directory
mkdir -p ~/omnirag && cd ~/omnirag

# 3. Create a docker-compose.yml that pulls from GHCR
cat > docker-compose.yml << 'EOF'
services:
  redis:
    image: redis:7-alpine
    command: redis-server --appendonly yes
    volumes:
      - ./redis-data:/data
    restart: unless-stopped

  omnirag:
    image: ghcr.io/krisclarkdev/omnirag:main
    ports:
      - "3000:3000"
    volumes:
      - /path/to/your/documents:/rag:ro
    environment:
      - REDIS_URL=redis://redis:6379/0
    depends_on:
      - redis
    restart: unless-stopped
EOF

# 4. ⚠️ Edit docker-compose.yml — replace /path/to/your/documents
#    with the absolute path to your local documents folder, e.g.:
#    - /home/kris/knowledge-docs:/rag:ro

# 5. Pull and start
docker compose pull
docker compose up -d

# 6. Verify
docker compose ps
docker compose logs -f omnirag

# 7. Open http://your-server:3000 → Configure → Trigger Sync
```

### Updating to Latest Version

```bash
cd ~/omnirag
docker compose pull
docker compose up -d
```

### Optional: Reverse Proxy (Nginx)

```nginx
server {
    listen 443 ssl;
    server_name omnirag.yourdomain.com;

    location / {
        proxy_pass http://127.0.0.1:3000;
        proxy_set_header Host $host;
        proxy_set_header X-Real-IP $remote_addr;
    }
}
```

---

## 🏗️ Architecture: The 1:1 Pod

OmniRAG stores its global configuration (`config:global`) and all per-file sync state in Redis. Because of this, **each directory/Knowledge Base pair MUST have its own dedicated Redis container.** Sharing a Redis container across multiple OmniRAG instances will result in config collisions and corrupted vector states.

The system uses a strict **1:1 Pod** architecture:

```
┌─── Pod (docker-compose) ─────────────────────────────────┐
│                                                          │
│  ┌────────────────┐      ┌────────────────────────────┐  │
│  │ redis:7-alpine │ ◄──► │     omnirag (Rust)         │  │
│  │  :6379 (AOF)   │      │                            │  │
│  └────────────────┘      │  ┌────────┐  ┌──────────┐  │  │
│         │                │  │ Axum   │  │ Sync     │  │  │
│  ┌──────▼──────┐         │  │ Web UI │  │ Engine   │  │  │
│  │ ./redis-*/  │         │  │ + REST │  │          │  │  │
│  │  appendonly │         │  └────────┘  └──────────┘  │  │
│  │   .aof      │            ▲              │          │  │
│  └─────────────┘            │              ▼          │  │
│        /rag ◄───────────────┘      Open WebUI API     │  │
└──────────────────────────────────────────────────────────┘
```

### Multi-Knowledge Base Deployment

To sync multiple directories to separate Knowledge Bases, run multiple isolated Pods in a single `docker-compose.yml`:

```yaml
services:
  # ── Pod 1: first project ─────────────────────────────
  redis-alpha:
    image: redis:7-alpine
    command: redis-server --appendonly yes
    volumes:
      - ./redis-alpha:/data

  omnirag-alpha:
    build: .
    ports:
      - "3000:3000"
    volumes:
      - ./alpha-docs:/rag
    environment:
      - REDIS_URL=redis://redis-alpha:6379/0
    depends_on:
      - redis-alpha

  # ── Pod 2: second project ────────────────────────────
  redis-beta:
    image: redis:7-alpine
    command: redis-server --appendonly yes
    volumes:
      - ./redis-beta:/data

  omnirag-beta:
    build: .
    ports:
      - "3001:3000"
    volumes:
      - ./beta-docs:/rag
    environment:
      - REDIS_URL=redis://redis-beta:6379/0
    depends_on:
      - redis-beta
```

Each Pod gets its own Redis data directory, port, and document volume. They are completely isolated — no shared state.

### Data Flow

1. **Reconcile** — Query Open WebUI KB file list, heal Redis state to prevent duplicates (skips ghost files not on disk)
2. **Walk** `/rag` directory recursively (filtered by extension whitelist + `.ragignore`)
3. **Hash** each file's contents (SHA-256)
4. **Compare** against stored hash in Redis
5. **Upload** new/changed files via Open WebUI API with retry (context prepended for text files only; binary files uploaded raw)
6. **Clean up** orphaned files (deleted from disk → deleted from Open WebUI)

### Redis Data Model

**File State** — Key: `<sha256(absolute_path)>_<filename>`

| Field | Description |
|---|---|
| `absolute_path` | Full filesystem path |
| `content_hash` | SHA-256 of file contents |
| `openwebui_file_id` | UUID from Open WebUI |
| `context_text` | User-defined context string |
| `context_dirty` | `"true"` if context was changed and needs re-upload |

**Config** — Key: `config:global`

| Field | Default |
|---|---|
| `target_directory` | `/rag` |
| `openwebui_url` | *(set via UI)* |
| `openwebui_token` | *(set via UI)* |
| `openwebui_knowledge_id` | *(set via UI)* |
| `redis_url` | `redis://127.0.0.1:6379/0` |
| `context_header_label` | `File Context` |

### Redis Persistence

Redis runs with **AOF (Append-Only File)** enabled (`--appendonly yes`). The AOF log is written to `/data/appendonly.aof` inside the Redis container, which is bind-mounted to `./redis-<name>` on the host. This ensures all state survives container restarts natively — no external backup layer needed.

### Redis Functions (Lua)

Five server-side functions loaded via `FUNCTION LOAD REPLACE` at startup:

| Function | Purpose |
|---|---|
| `get_formatted_context` | Returns context as Markdown blockquote |
| `check_file_exists` | Key existence check |
| `verify_file_hash` | Content hash comparison |
| `upsert_sync_state` | Atomic field update (preserves `context_text`) |
| `get_cleanup_batch` | SCAN-based batch retrieval for orphan cleanup |

### Three-Phase Sync

**Phase 0 — Reconciliation:**

Queries the Open WebUI API for all files in the target knowledge base. Matches each remote file against local Redis state by filename. If a file exists remotely but the UUID isn't tracked locally (e.g., crash between upload and state save), it heals the Redis entry to prevent duplicate uploads.

**Ghost file detection:** If a file exists in Open WebUI but the corresponding local file has been deleted from disk while the container was offline, the reconciliation phase skips healing (`[GHOST]`). Phase 2 will naturally detect these as orphans and clean them up from Open WebUI, avoiding unnecessary state churn.

**Phase 1 — Ingestion** (5 concurrent uploads via semaphore):

| Condition | Action |
|---|---|
| File not in Redis | `[NEW]` Upload → poll status → attach to KB → save state |
| File in Redis, hash matches, context clean | `[SKIP]` No action |
| File in Redis, hash matches, context dirty | `[UPDATE]` Re-upload with new context (set via Web UI or CLI) |
| File in Redis, hash differs | `[UPDATE]` Delete old → upload new → save state |

**Phase 2 — Orphan Cleanup:**

| Condition | Action |
|---|---|
| Redis key but file missing from disk | `[ORPHAN:missing]` Delete from Open WebUI → remove Redis key |
| Redis key but file now matches `.ragignore` / OS ignore / disallowed extension | `[ORPHAN:filtered]` Delete from Open WebUI → remove Redis key |

### File Filtering

Files are filtered at three levels before processing:

| Filter | Behavior |
|---|---|
| **Hidden/OS files** | `.DS_Store`, `Thumbs.db`, `desktop.ini`, `.gitkeep` always skipped |
| **Extension whitelist** | Only processes: `.md`, `.txt`, `.pdf`, `.csv`, `.json`, `.yaml`, `.yml`, `.toml`, `.xml`, `.html`, `.htm`, `.rst`, `.log`, `.cfg`, `.ini`, `.conf`, `.py`, `.rs`, `.go`, `.js`, `.ts`, `.sh`, `.bat`, `.ps1` |
| **`.ragignore`** | Place a `.ragignore` file in the root of `/rag` with one pattern per line (supports `*` wildcards, directory names, and exact paths). Lines starting with `#` are comments. |

Example `.ragignore`:
```
# Skip the drafts folder
drafts
# Skip all log files in archives
archives/*.log
# Skip a specific file
notes/scratch.txt
```

### Context Injection (Binary-Safe)

Context strings set via `set-context` are prepended to files as a Markdown header during upload. This happens **in-memory only** — local files are never modified.

| File Type | Behavior |
|---|---|
| **Text** (`.md`, `.txt`, `.csv`, `.json`, `.yaml`, `.py`, `.rs`, etc.) | Context prepended as Markdown blockquote |
| **Binary** (`.pdf`, and any non-text extension) | Uploaded raw — no context injection to prevent file corruption |

### API Resiliency

All Open WebUI API calls use **exponential backoff retry** (3 attempts: 500ms → 1s → 2s):

- **5xx / network errors** → retry
- **429 (rate limit)** → retry
- **4xx client errors** → fail immediately (no retry)
- **Failed file** → logged, skipped, continues sync (retried on next trigger)

### Open WebUI API Integration

| Operation | Endpoint | When |
|---|---|---|
| List KB files | `GET /api/v1/knowledge/{kb}` | Phase 0 reconciliation |
| Upload | `POST /api/v1/files/` | New or updated file |
| Poll status | `GET /api/v1/files/{id}/process/status` | After upload |
| Attach to KB | `POST /api/v1/knowledge/{kb}/file/add` | After processing complete |
| Delete | `DELETE /api/v1/files/{id}` | Update (old version) or orphan |

---

## 🧩 Best Practices: Chunking

The context string prepended by OmniRAG becomes part of the document text sent to Open WebUI for vector embedding. To ensure the context stays attached to the first logical section during chunking:

- **Chunk size:** Set to ≥1500 tokens in your Open WebUI settings. This ensures the context blockquote and the first section of the document land in the same vector chunk.
- **Chunk overlap:** Set to ≥150 tokens. This creates redundancy at chunk boundaries, so context fragments aren't lost if a split happens nearby.
- **No horizontal rule:** The context format deliberately omits `---` (Markdown horizontal rule) because modern text splitters (LangChain, LlamaIndex) treat `---` as a hard chunk boundary, which would orphan the context from the document text.

> **Tip:** If you're using Open WebUI's default settings, `1500` chunk size with `150` overlap is a reliable starting point for context-injected documents.

---

## 🤖 REST API (Automation)

OmniRAG exposes a JSON REST API under `/api/v1/` designed for cron jobs, n8n workflows, webhooks, and any external automation. No authentication is required — secure access via network policy or reverse proxy.

### Endpoints

| Endpoint | Method | Description |
|---|---|---|
| `/api/v1/health` | `GET` | Liveness probe — returns service name and version |
| `/api/v1/sync` | `POST` | Trigger a sync — returns 202 Accepted or 409 Conflict |
| `/api/v1/sync/status` | `GET` | Poll current sync status as JSON |
| `/api/v1/openapi.json` | `GET` | OpenAPI 3.0 specification (JSON) |

### `GET /api/v1/health`

```bash
curl http://localhost:3000/api/v1/health
```

```json
{
  "status": "ok",
  "service": "omnirag",
  "version": "0.1.0"
}
```

### `POST /api/v1/sync`

```bash
curl -X POST http://localhost:3000/api/v1/sync
```

**202 Accepted** — sync started:
```json
{
  "status": "triggered",
  "message": "Sync started"
}
```

**409 Conflict** — sync already in progress:
```json
{
  "status": "already_running",
  "message": "A sync operation is already in progress"
}
```

### `GET /api/v1/sync/status`

```bash
curl http://localhost:3000/api/v1/sync/status
```

```json
{
  "status": "completed",
  "detail": "Completed successfully"
}
```

Status values: `idle`, `running`, `completed`, `error`, `unknown`.

### Automation Examples

**Cron (every 15 minutes):**
```cron
*/15 * * * * curl -s -X POST http://localhost:3000/api/v1/sync > /dev/null
```

**n8n HTTP Request Node:**
- Method: `POST`
- URL: `http://omnirag:3000/api/v1/sync`
- Authentication: None
- Response Format: JSON

**Docker Healthcheck:**
```yaml
services:
  omnirag:
    image: ghcr.io/<owner>/omnirag:main
    healthcheck:
      test: ["CMD", "curl", "-f", "http://localhost:3000/api/v1/health"]
      interval: 30s
      timeout: 5s
      retries: 3
```

**Bash Trigger + Wait:**
```bash
#!/bin/bash
# Trigger sync and poll until complete
curl -s -X POST http://localhost:3000/api/v1/sync
while true; do
    STATUS=$(curl -s http://localhost:3000/api/v1/sync/status | jq -r .status)
    echo "Status: $STATUS"
    [ "$STATUS" = "completed" ] || [ "$STATUS" = "error" ] && break
    sleep 5
done
```

---

## 💻 CLI

```bash
# Start the web server
omnirag serve

# Run sync manually (all 3 phases)
omnirag sync

# Set context for a file (prepended on upload)
omnirag set-context /rag/doc.md "Architecture reference for EVE-OS edge deployments."

# Get formatted context
omnirag get-context /rag/doc.md
# > **Solutions Architect Context:**
# > Architecture reference for EVE-OS edge deployments.
# ---
```

### CLI Flags

| Flag | Env Var | Default | Description |
|---|---|---|---|
| `--redis-url` | `REDIS_URL` | `redis://127.0.0.1:6379/0` | Redis connection |

---

## 🌐 Web UI

Glassmorphism dark-mode dashboard served at `http://localhost:3000`:

- **Configuration panel** — Set Open WebUI URL, API token, and Knowledge Base ID
- **Sync control** — Trigger sync and monitor status in real-time (polls every 3s)
- **HTMX-powered** — Dynamic updates without page reloads

### HTMX Endpoints (Web UI)

| Endpoint | Method | Purpose |
|---|---|---|
| `/` | GET | Full page |
| `/api/config` | GET | Config form partial |
| `/api/config` | POST | Save config |
| `/api/sync` | POST | Trigger sync (returns HTML) |
| `/api/sync/status` | GET | Poll status (returns HTML) |
| `/api/contexts` | GET | Context Manager file list |
| `/api/contexts` | POST | Update file context_text |
| `/docs` | GET | Swagger UI (interactive API docs) |
| `/api/v1/openapi.json` | GET | OpenAPI 3.0 specification |

---

## 🐳 Docker

### Dockerfile

Multi-stage build:
1. **Builder** (`rust:1.85-bookworm`) — Compiles release binary
2. **Runtime** (`debian:bookworm-slim`) — Installs `redis-server`, `gosu`, `ca-certificates`

### Boot Sequence

```
entrypoint.sh
├── chown -R redis:redis /data
├── gosu redis redis-server --appendonly yes --dir /data --daemonize yes
├── Wait for Redis PONG (readiness check)
└── exec omnirag serve
    ├── Connect to Redis
    ├── Load Lua functions
    └── Start Axum on :3000
```

> **Volume permissions:** The entrypoint runs initially as root to `chown` the `/data` bind-mount, then uses `gosu` to drop privileges and start Redis as the `redis` user. This prevents "permission denied" errors when host directories have strict permissions.

### docker-compose.yml

```yaml
services:
  omnirag:
    build: .
    ports:
      - "3000:3000"
    volumes:
      - ./my-documents:/rag  # Your files (change path as needed)
      - ./redis_data:/data   # Redis AOF persistence
    environment:
      - RUST_LOG=info,omnirag=debug
    restart: unless-stopped
```

### Multi-Container Deployment (One Container per Collection)

OmniRAG uses a single global config per container — each instance syncs **one local directory to one Open WebUI collection (Knowledge Base)**. This is by design: to sync multiple directories to different collections, run one Pod per collection:

```yaml
services:
  # ── Pod 1 ────────────────────────────────────────────
  redis-alpha:
    image: redis:7-alpine
    command: redis-server --appendonly yes
    volumes:
      - ./redis-alpha:/data

  omnirag-alpha:
    build: .
    ports:
      - "3000:3000"
    volumes:
      - ./alpha-docs:/rag
    environment:
      - REDIS_URL=redis://redis-alpha:6379/0
      - RUST_LOG=info,omnirag=debug
    depends_on:
      - redis-alpha
    restart: unless-stopped

  # ── Pod 2 ────────────────────────────────────────────
  redis-beta:
    image: redis:7-alpine
    command: redis-server --appendonly yes
    volumes:
      - ./redis-beta:/data

  omnirag-beta:
    build: .
    ports:
      - "3001:3000"
    volumes:
      - ./beta-docs:/rag
    environment:
      - REDIS_URL=redis://redis-beta:6379/0
      - RUST_LOG=info,omnirag=debug
    depends_on:
      - redis-beta
    restart: unless-stopped
```

Each Pod gets its own Redis data volume, independent configuration, and a separate Web UI port. Configure each via its own UI (`localhost:3000`, `localhost:3001`, etc.) with the target collection's Knowledge Base ID. Pods are fully isolated — they share nothing and can be started, stopped, or scaled independently.

### Logging Configuration

OmniRAG uses the `RUST_LOG` environment variable for log verbosity:

| Level | What you see |
|---|---|
| `RUST_LOG=info` | Sync phases, file actions, errors |
| `RUST_LOG=info,omnirag=debug` | All of the above + API calls, filter decisions |
| `RUST_LOG=warn` | Only warnings and errors |

```bash
# View container logs
docker logs -f omnirag

# Override at runtime
docker run -e RUST_LOG=debug ...
```

---

## 📐 Hardware Requirements & Scaling

The following T-shirt sizing guide provides realistic estimates based on OmniRAG's resource profile:

- **Redis RAM:** ~300 bytes per tracked file (path + SHA-256 hash + UUID + context string)
- **App RAM:** Up to 5 concurrent file buffers in memory (text files only; binary files passed through)
- **CPU:** SHA-256 hash computation for every file during sync
- **Storage:** Redis AOF file grows proportionally to total state size

| Tier | Files | Container RAM | CPU | AOF Storage |
|---|---|---|---|---|
| **XS** (Tiny) | < 1,000 | 128 MB | 0.5 vCPU | < 5 MB |
| **S** (Small) | 1K – 5K | 256 MB | 1 vCPU | 5 – 25 MB |
| **M** (Medium) | 5K – 20K | 512 MB | 1 vCPU | 25 – 100 MB |
| **L** (Large) | 20K – 100K | 1 GB | 2 vCPU | 100 – 500 MB |
| **XL** (Enterprise) | 100K+ | 2 GB+ | 4 vCPU | 500 MB+ |

### XL Tier Bottleneck Analysis

At the **Enterprise** tier (100K+ files), the primary bottleneck is **Phase 1 directory walk + SHA-256 hashing**, not memory. Redis comfortably holds 100K entries in ~30 MB of RAM. However:

- **SHA-256 hashing** of 100K+ files on each sync cycle becomes CPU-bound. Each hash requires a full file read, so I/O throughput of the `/rag` volume also matters.
- **Open WebUI API rate limits** may throttle uploads during initial ingestion. The 5-concurrent-upload semaphore prevents overwhelming the API, but the first-ever sync of 100K files will take hours.
- **Redis AOF rewrite** can momentarily spike memory to 2× during background `BGREWRITEAOF`. The 2 GB recommendation accounts for this.

**Recommendation for XL:** Run on dedicated infrastructure with SSD-backed `/rag` volume. Consider increasing the semaphore limit (currently 5) for faster initial ingestion if your Open WebUI instance can handle it.

---

## 🧪 Testing

### Test Suite

OmniRAG includes a comprehensive test suite covering all core components:

```bash
# Run all tests
cargo test

# Run with output
cargo test -- --nocapture
```

| Test Module | Tests | Coverage | Location |
|---|---|---|---|
| **API Client** | 10 | Upload, delete, KB operations, exponential backoff retry (5xx/429/4xx), poll status | `src/api.rs` (inline) |
| **Web REST API** | 9 | Health endpoint, sync trigger (202/409), status phases (idle/running/completed/error), method validation | `tests/web_api_tests.rs` |
| **Sync Engine** | 15 | Extension whitelist, OS file filtering, `.ragignore` parsing, binary-safe context injection, full pipeline | `tests/sync_tests.rs` |
| **Hashing & Config** | 11 | SHA-256 identity, deterministic keys, empty file hash, config serialization roundtrip | `tests/hashing_config_tests.rs` |

### API Retry Test Matrix

| Scenario | Expected | Verified |
|---|---|---|
| 5xx → 5xx → 200 | Retry twice, succeed | ✅ |
| 429 → 429 → 200 | Retry twice, succeed | ✅ |
| 400 (client error) | Fail immediately, no retry | ✅ |
| 500 → 500 → 500 | Fail after 3 attempts | ✅ |

### REST API Test Matrix

| Scenario | Expected | Verified |
|---|---|---|
| `GET /api/v1/health` | 200 + service info | ✅ |
| `POST /api/v1/sync` (idle) | 202 Accepted | ✅ |
| `POST /api/v1/sync` (running) | 409 Conflict | ✅ |
| `GET /api/v1/sync/status` (all phases) | Correct phase string | ✅ |
| `GET /api/v1/sync` (wrong method) | 405 Method Not Allowed | ✅ |

---

## 🔄 CI/CD

GitHub Actions workflow at `.github/workflows/docker-publish.yml`:

| Trigger | Action |
|---|---|
| Push to `main` | Build + push to GHCR |
| Pull Request | Build only (verify compilation) |
| Release tag (`v*`) | Build + push with semver tags |

The workflow uses Docker Buildx with GitHub Actions cache for fast rebuilds.

---

## 📁 Source Files

| File | Purpose |
|---|---|
| `src/sync.rs` | Three-phase sync, file filtering, `.ragignore`, binary-safe context |
| `src/api.rs` | Open WebUI API client with retry + inline tests |
| `src/web.rs` | Axum routes (HTMX + JSON REST API), HTML rendering |
| `src/main.rs` | CLI parsing, Redis bootstrap |
| `src/redis_client.rs` | Lua function loading + FCALL wrappers |
| `src/lua/rag_helpers.lua` | 5 Redis Functions |
| `src/config.rs` | AppConfig struct + Redis load/save |
| `src/hashing.rs` | SHA-256 key/content hashing |

### Dependencies

`tokio` · `axum` · `clap` · `redis` · `reqwest` · `walkdir` · `sha2` · `serde` · `serde_json` · `hex` · `dotenvy` · `tower-http` · `tracing` · `tracing-subscriber`

**Dev:** `wiremock` · `tempfile` · `tower` · `http-body-util` · `serde_urlencoded`

---

## 🧪 End-to-End Test Results

### Test Environment

| Component | Value |
|---|---|
| **Deployment** | Docker Compose 1:1 Pod (omnirag-alpha + redis-alpha) |
| **Test Directory** | `alpha-docs/` mounted at `/rag` |
| **Open WebUI** | Local Open WebUI instance |
| **Target KB** | "Test" (auto-discovered via Fetch KBs) |

### Test Files

| File | Type | Expected Behavior |
|---|---|---|
| `architecture.md` | Markdown | Upload as-is |
| `data_processor.py` | Python | Convert → `data_processor.md` (code fence) |
| `test_config.json` | JSON | Convert → `test_config.md` (code fence) |
| `release_notes.txt` | Plain text | Upload as-is (`.txt` not converted) |
| `secret_notes.txt` | Plain text | **Excluded** by `.ragignore` |
| `.ragignore` | Config | Patterns: `drafts`, `*.tmp`, `secret_notes.txt` |

### Results

| Test Case | Status | Notes |
|---|---|---|
| **KB Dropdown Picker** | ✅ PASS | Fetched 3 KBs from API, "Test" selected via dropdown |
| **Help Icon Modals** | ✅ PASS | All 6 fields have working `?` icons with dialog content |
| **Config Save/Load** | ✅ PASS | All fields persisted to Redis and restored on reload |
| **Convert to Markdown** | ✅ PASS | `.py` → `.md` with Python fence, `.json` → `.md` with JSON fence |
| **Markdown Passthrough** | ✅ PASS | `.md` and `.txt` files uploaded without conversion |
| **Binary Passthrough** | ✅ PASS | PDFs uploaded without context injection |
| **.ragignore Filtering** | ✅ PASS | `secret_notes.txt` excluded, 3 patterns loaded |
| **Phase 1 Ingestion** | ✅ PASS | 4 files uploaded as `[NEW]` |
| **Phase 2 Cleanup** | ✅ PASS | No orphans found |
| **Open WebUI Verification** | ✅ PASS | 4 files visible in "Test" KB with correct names/sizes |
| **Swagger Docs Link** | ✅ PASS | Header link navigates to `/docs` |
| **Footer Credits** | ✅ PASS | GitHub link + "Built by Kristopher Clark" |
| **Max Concurrent Uploads** | ✅ PASS | Configurable via UI, default 5 |

### Docker Logs (Abbreviated)

```
omnirag-alpha-1 | Loaded 3 patterns from .ragignore
omnirag-alpha-1 | Found 4 files to process (skipped: 0 unsupported ext, 1 ragignored)
omnirag-alpha-1 | [NEW] data_processor.py
omnirag-alpha-1 | [NEW] release_notes.txt
omnirag-alpha-1 | [NEW] test_config.json
omnirag-alpha-1 | [NEW] architecture.md
omnirag-alpha-1 | Uploaded 'data_processor.md' → file_id=...
omnirag-alpha-1 | Sync complete.
```

---

## 📄 License

Apache License 2.0 — see [LICENSE](LICENSE) for details.
