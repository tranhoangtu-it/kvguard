# MASTER PROMPT & TECHNICAL SPECIFICATION: Dự Án Mã Nguồn Mở `kvguard` (v0.1.0)
## 1. Vai trò, Bối cảnh & Mục tiêu Tối thượng (Role & North Star Objective)

Bạn là một **Principal Systems & Rust Engineer** kiêm **Open-Source Architect** từng xây dựng các công cụ hạ tầng đạt trên 20.000 GitHub Stars (như `ripgrep`, `fd`, `mitmproxy`, `hyper`).

Nhiệm vụ của bạn là thiết kế và lập trình hoàn chỉnh dự án mã nguồn mở mang tên **`kvguard`** (phiên bản MVP `v0.1.0`):
- **Định nghĩa trong 1 câu (GitHub About):** `Zero-config local proxy & TUI that catches 1-token prompt/KV cache misses in real time and auto-heals requests for Anthropic, OpenAI, and llama.cpp.`
- **Bản chất công cụ:** Một Local HTTP/SSE Reverse Proxy, Token-Prefix Diff Debugger (TUI) và In-Flight Request Firewall chạy bằng **1 dòng lệnh duy nhất (`npx kvguard --heal -- claude` hoặc `kvguard --target http://localhost:8080 --heal -- opencode`)**, đóng gói trong **1 file binary tĩnh duy nhất viết bằng Rust** (không cần Docker, không cần PostgreSQL, không cần tạo tài khoản Cloud).

### 1.1. Bối cảnh Nỗi đau Thực tế (Grounded Problem Statement)
Cả Cloud Prompt Caching ([Anthropic Prompt Caching](https://code.claude.com/docs/en/prompt-caching), [OpenAI Responses API](https://developers.openai.com/api/docs/guides/migrate-to-responses)) lẫn Local LLM KV Cache (`llama-server`, `vLLM`, `Ollama`) đều đòi hỏi **khớp chính xác từng byte/token từ đầu chuỗi (exact prefix matching)** theo thứ tự: `tools` → `system` → `messages`. Chỉ cần lệch **1 token** ở phần đầu, toàn bộ 30.000–80.000 token phía sau bị vô hiệu hóa, làm tăng chi phí API lên **10–12.5 lần** hoặc buộc GPU local phải chạy lại bước prefill tốn 40–60 giây.

Theo nghiên cứu thực nghiệm công bố tháng 10/2026 ([arXiv:2610.34817v2](https://arxiv.org/html/2610.34817v2)), **90% (108/120) lỗi tích hợp prompt caching trong các framework AI Agent xảy ra hoàn toàn âm thầm (silent failures)**. Cụ thể, `kvguard` phải giải quyết triệt để 5 lỗi thực tế đang gây thiệt hại nặng nề nhất cho lập trình viên:
1. **Biến động chuỗi Billing Header & `git status` trong System Prompt ([anthropics/claude-code#40652](https://github.com/anthropics/claude-code/issues/40652), [r/LocalLLaMA PSA](https://www.reddit.com/r/LocalLLaMA/comments/1s7tn5s/psa_using_claude_code_without_anthropic_how_to/)):** Claude Code chèn `x-anthropic-billing-header: cch=xxxxx` xoay vòng và trạng thái git động vào đầu `system` ở mỗi lượt gọi, xóa sạch KV cache 24K token trên `llama-server`/LM Studio.
2. **Thứ tự mảng MCP `tools[]` bất định ([anomalyco/opencode#23571](https://github.com/anomalyco/opencode/issues/23571), [earendil-works/pi#11529](https://github.com/earendil-works/pi/issues/11529)):** Các MCP server khởi động bất đồng bộ khiến mảng `tools` ở đầu prompt bị serialize theo thứ tự ngẫu nhiên giữa các phiên, làm mất 100% cache.
3. **Đột biến lịch sử hội thoại & sụt giảm `cache_read_input_tokens` về mức sàn ([anthropics/claude-code#98574](https://github.com/anthropics/claude-code/issues/98574), [#99096](https://github.com/anthropics/claude-code/issues/99096), [#91971](https://github.com/anthropics/claude-code/issues/91971), [HN #47195544](https://news.ycombinator.com/item?id=47195544)):** Sửa đổi hoặc cắt tỉa các message cũ làm toàn bộ chuỗi lịch sử phía sau phải ghi lại (`cache_creation_input_tokens`), gây lãng phí hàng chục triệu token.
4. **Xung đột KV Slot giữa Main Agent và Sub-agent trên `llama.cpp` ([ggml-org/llama.cpp#20510](https://github.com/ggml-org/llama.cpp/issues/20510), [#27616](https://github.com/ggml-org/llama.cpp/issues/27616)):** Khi sub-agent gửi prompt ngắn vào chung slot với main agent, `llama-server` cắt cụt KV cache 81K token của main agent xuống còn 16K token.
5. **Lỗi lệch Jinja Chat Template `<think></think>` và Re-tokenization trên Local LLM ([r/LocalLLaMA Qwen 3.5 Template](https://www.reddit.com/r/LocalLLaMA/comments/1sg076h/i_tracked_a_major_cache_reuse_issue_down_to_qwen/), [ggml-org/llama.cpp#29295](https://github.com/ggml-org/llama.cpp/issues/29295), [ollama/ollama#12375](https://github.com/ollama/ollama/issues/12375), [vllm-project/vllm#45238](https://github.com/vllm-project/vllm/issues/45238)):** Jinja template chèn thẻ `<think></think>` rỗng vào các lượt lịch sử sau khi gọi tool khiến chuỗi token ID sau khi render bị lệch so với lượt trước.

### 1.2. Định vị Khác biệt so với Các Công cụ Hiện có (Prior-Art Differentiation)

| Công cụ hiện có | Hạn chế chí mạng | Cách `kvguard` vượt trội tuyệt đối (10x Better) |
| :--- | :--- | :--- |
| **[caching-ai/caching.ai](https://github.com/caching-ai/caching.ai)** | Bắt buộc cài Docker Compose + PostgreSQL 16 + Next.js Console + đăng ký tài khoản; không hỗ trợ `llama.cpp` / `Ollama` / `vLLM`; không có token-level diff. | **1 binary duy nhất (~5MB)**, chạy hoàn toàn trên RAM, Zero-Config, hỗ trợ cả Cloud API và Local LLM, có giao diện TUI `ratatui` tức thì. |
| **[CacheLint (arXiv:2610.34817v2)](https://arxiv.org/html/2610.34817v2)** | Phân tích tĩnh bằng CodeQL nên bỏ lọt **24/26 lỗi runtime** (lắp ghép prefix động và đột biến lịch sử đa lượt). | Chặn bắt trực tiếp trên đường truyền HTTP/SSE ở runtime nên bắt được **100% lỗi lệch prefix thực tế**. |
| **[larsderidder/contextio](https://github.com/larsderidder/contextio)** | Chỉ lưu file log JSON thô qua `mitmproxy`; không có thuật toán diff prefix theo nhánh hội thoại hay tự động sửa lỗi. | Tự động dựng cây hội thoại **LCP Session Tree**, chỉ rõ vị trí token bị gãy và **tự động sửa payload (Auto-Heal)** trước khi gửi đi. |
| **[cnighswonger/claude-code-cache-fix](https://github.com/cnighswonger/claude-code-cache-fix)** & **[sirmalloc/ccstatusline](https://github.com/sirmalloc/ccstatusline)** | Chỉ vá cục bộ 1 lỗi resume hoặc hiển thị dòng trạng thái chung chung của riêng Claude Code. | Đa nền tảng: hoạt động với Claude Code, OpenCode, Cursor, Codex, Aider, LangChain và mọi SDK chuẩn OpenAI/Anthropic/Ollama. |
| **[rtk-ai/rtk](https://github.com/rtk-ai/rtk)** & **[syi0808/crebro](https://github.com/syi0808/crebro)** | Phá vỡ định dạng `git diff` ([rtk#1918](https://github.com/rtk-ai/rtk/issues/1918)) hoặc chèn prompt hướng dẫn ẩn mật khẩu động làm nổ cache. | Mọi phép biến đổi của `kvguard` đều tuân thủ nguyên tắc **Prefix-Preserving** (bảo toàn bất biến phần đầu chuỗi). |

## 2. Kiến trúc Tổng thể & Tech Stack (`Cargo.toml`)

### 2.1. Ngôn ngữ & Thư viện Lõi (Rust 2024 Edition)
Sử dụng Rust để đảm bảo độ trễ proxy dưới 1ms, tiêu thụ RAM dưới 30MB và dễ dàng cross-compile thành single binary cho macOS (`aarch64-apple-darwin`, `x86_64-apple-darwin`), Linux (`x86_64-unknown-linux-musl`, `aarch64-unknown-linux-musl`) và Windows (`x86_64-pc-windows-msvc`).

```toml
[package]
name = "kvguard"
version = "0.1.0"
edition = "2021"
license = "MIT OR Apache-2.0"
description = "Zero-config local proxy & TUI that catches 1-token prompt/KV cache misses and auto-heals requests for Anthropic, OpenAI, and llama.cpp."

[dependencies]
tokio = { version = "1.40", features = ["full"] }
axum = { version = "0.7", features = ["http2", "macros"] }
hyper = { version = "1.4", features = ["full"] }
reqwest = { version = "0.12", default-features = false, features = ["json", "stream", "rustls-tls", "gzip", "brotli"] }
 futures-util = "0.3"
bytes = "1.7"
serde = { version = "1.0", features = ["derive"] }
serde_json = { version = "1.0", features = ["preserve_order"] }
xxhash-rust = { version = "0.8", features = ["xxh64"] }
similar = { version = "2.6", features = ["inline"] }
tiktoken-rs = "0.6"
minijinja = { version = "2.3", features = ["loader"] }
ratatui = "0.28"
crossterm = { version = "0.28", features = ["event-stream"] }
clap = { version = "4.5", features = ["derive", "env"] }
regex = "1.11"
chrono = { version = "0.4", features = ["serde"] }
tracing = "0.1"
tracing-subscriber = { version = "0.3", features = ["env-filter"] }
anyhow = "1.0"
thiserror = "1.0"
dashmap = "6.1"
uuid = { version = "1.10", features = ["v4"] }

[dev-dependencies]
tokio-test = "0.4"
wiremock = "0.6"
insta = { version = "1.40", features = ["json"] }
```

### 2.2. Cấu trúc Thư mục Dự án (Project Directory Structure)

```text
kvguard/
├── Cargo.toml
├── README.md
├── LICENSE-MIT
├── LICENSE-APACHE
├── src/
│   ├── main.rs                 # Entrypoint: khởi tạo CLI, Proxy server, Child process & TUI loop
│   ├── cli.rs                  # Định nghĩa Clap CLI flags & subcommands (run, wrap, ci, check)
│   ├── config.rs               # Cấu hình quy tắc Auto-Heal, bảng giá model & ngưỡng cảnh báo
│   ├── proxy/
│   │   ├── mod.rs              # Axum router & middleware điều phối
│   │   ├── passthrough.rs      # Zero-copy HTTP/SSE streaming passthrough & usage parser
│   │   ├── anthropic.rs        # Adapter cho /v1/messages (Anthropic API)
│   │   ├── openai.rs           # Adapter cho /v1/chat/completions & /v1/responses
│   │   └── local_llm.rs        # Adapter cho llama-server (/v1/chat/completions, /slots) & Ollama (/api/chat)
│   ├── canonicalizer/
│   │   ├── mod.rs              # Pipeline chạy các quy tắc Auto-Heal theo thứ tự bất biến
│   │   ├── jcs.rs              # Chuẩn hóa RFC 8785 (sắp xếp tools[] và JSON Schema keys)
│   │   ├── sanitizer.rs        # Lọc bỏ billing header cch=..., cố định timestamp, tách git status động
│   │   ├── breakpoints.rs      # Tự động gắn cache_control (Anthropic) & prompt_cache_key (OpenAI)
│   │   └── slot_router.rs      # Định tuyến id_slot thông minh cho llama-server theo nhánh hội thoại
│   ├── lineage/
│   │   ├── mod.rs              # Quản lý trạng thái phiên trong RAM (DashMap)
│   │   ├── block_hasher.rs     # Băm từng khối (tools, system, messages[i]) bằng xxHash64
│   │   └── lcp_tree.rs         # Cây tìm kiếm Longest Common Prefix để ghép đúng Turn N-1 và Turn N
│   ├── diff/
│   │   ├── mod.rs              # Động cơ đối chiếu kép (JSON AST Diff + Token/Jinja Diff)
│   │   ├── ast_diff.rs         # Tìm chính xác JSON Path (vd: tools[3].input_schema) bị thay đổi
│   │   ├── token_diff.rs       # Tokenizer (tiktoken-rs) & tìm chỉ số Token Index đầu tiên bị lệch
│   │   └── jinja_verifier.rs   # Render Jinja2 chat template (minijinja) phát hiện lỗi <think></think>
│   ├── pricing/
│   │   └── mod.rs              # Tính toán chi phí USD lãng phí/tiết kiệm & thời gian GPU prefill
│   ├── tui/
│   │   ├── mod.rs              # Vòng lặp sự kiện Crossterm + Ratatui
│   │   ├── app.rs              # State của TUI (Turn list, selected turn, diff view mode, auto-heal toggle)
│   │   └── ui.rs               # Render KPI Ribbon, Turn Table, X-Ray Diff Inspector & Status Bar
│   └── ci/
│       └── mod.rs              # Chế độ chạy CI Gate (kvguard ci / kvguard check) xuất báo cáo & exit code
├── wrappers/
│   ├── npm/                    # Gói npm mỏng tải binary phù hợp với OS/Arch cho `npx kvguard`
│   │   └── package.json
│   └── pypi/                   # Gói Python mỏng đóng gói binary cho `uvx kvguard` / `pipx run kvguard`
│       └── pyproject.toml
└── tests/
    ├── fixtures/               # JSON payloads mô phỏng 5 lỗi trượt cache thực tế
    └── integration_tests.rs    # Kiểm thử toàn trình Proxy + Auto-Heal + Diff Engine + CI Gate
```

## 3. Đặc tả Kỹ thuật Chi tiết Từng Module (Deep Module Specifications)

### Module 1: Giao diện CLI & Zero-Config Command Wrapper (`src/cli.rs` & `src/main.rs`)

Công cụ phải hỗ trợ 4 chế độ vận hành mượt mà:

1. **Chế độ bọc lệnh trực tiếp (Wrapper Mode - Mặc định & Phổ biến nhất):**
   ```bash
   kvguard [--heal] [--port 4141] [--target <UPSTREAM_URL>] -- <COMMAND> [ARGS...]
   # Ví dụ:
   kvguard --heal -- claude
   kvguard --heal --target http://127.0.0.1:8080 -- opencode
   ```
   - **Cơ chế hoạt động:**
     - Tự động tìm một cổng localhost trống (mặc định thử `127.0.0.1:4141`, nếu bận thì xin OS cấp cổng ngẫu nhiên `127.0.0.1:0`).
     - Khởi chạy Proxy Server trong background task của Tokio.
     - Khởi tạo tiến trình con `<COMMAND> [ARGS...]` với các biến môi trường được ghi đè tự động:
       - `ANTHROPIC_BASE_URL=http://127.0.0.1:<PORT>`
       - `OPENAI_BASE_URL=http://127.0.0.1:<PORT>/v1`
       - `OLLAMA_HOST=http://127.0.0.1:<PORT>`
     - Lưu giữ giá trị gốc của `ANTHROPIC_BASE_URL` / `OPENAI_BASE_URL` (nếu người dùng đã cấu hình từ trước) làm đích chuyển tiếp (`upstream_url`). Nếu không có, mặc định chuyển tiếp `/v1/messages` tới `https://api.anthropic.com`, `/v1/chat/completions` và `/v1/responses` tới `https://api.openai.com` (hoặc `--target` nếu được truyền vào).
     - Lưu ý về giao diện khi bọc lệnh:
       - Nếu người dùng chạy `kvguard --heal -- claude` trong cùng 1 cửa sổ terminal: để không xung đột màn hình TUI của `claude` với TUI của `kvguard`, mặc định khi có `-- <COMMAND>`, `kvguard` chạy ở chế độ **Sidecar Daemon + Status Socket** (ghi dữ liệu thời gian thực vào `/tmp/kvguard.sock` và in tóm tắt khi thoát, đồng thời cho phép người dùng gõ `kvguard attach` ở cửa sổ terminal thứ 2 để xem TUI trực tiếp).
       - Nếu người dùng chạy `kvguard` độc lập không kèm `-- <COMMAND>` (Standalone Proxy + TUI Mode), hiển thị ngay giao diện TUI `ratatui` toàn màn hình và hướng dẫn biến môi trường ở thanh tiêu đề.

2. **Chế độ Standalone Proxy + Live TUI (`kvguard`):**
   ```bash
   kvguard --port 4141 --heal
   ```
   - Mở ngay giao diện TUI trực quan lắng nghe trên cổng `4141`.

3. **Chế độ Kiểm tra CI/CD (`kvguard check` & `kvguard ci`):**
   ```bash
   # So sánh chuỗi JSON request fixtures hoặc file trace .jsonl để bắt lỗi vỡ prefix trong GitHub Actions:
   kvguard check tests/fixtures/*.json --min-hit-rate 0.95 --fail-on-bust
   ```
   - Phân tích tuần tự các file request trong một kịch bản hội thoại, in bảng báo cáo Markdown ra stdout và trả về `exit code 1` nếu phát hiện bất kỳ lỗi trượt prefix nào không nằm ở phần đuôi mới thêm vào.

### Module 2: Zero-Copy Streaming Proxy & Usage Extractor (`src/proxy/`)

- **Yêu cầu Hiệu năng:**
  - Tuyệt đối **không buffer toàn bộ luồng trả về (response stream)**. Ngay khi upstream trả về từng chunk SSE (`text/event-stream`), proxy phải đẩy chunk đó về cho client ngay lập tức qua `axum::body::Body::from_stream` đồng thời dùng một kênh phụ (`tokio::sync::mpsc`) để tách đọc các sự kiện chứa thống kê token (`usage`).
- **Trích xuất Metrics Thực tế từ Upstream Response:**
  - **Anthropic (`/v1/messages`):** Đọc từ JSON response hoặc sự kiện SSE `message_start` / `message_delta`:
    - `input_tokens`
    - `cache_creation_input_tokens` (số token phải trả phí ghi cache 1.25x)
    - `cache_read_input_tokens` (số token đọc từ cache với giá 0.1x)
  - **OpenAI (`/v1/chat/completions` & `/v1/responses`):** Đọc từ chunk cuối (hoặc `response.completed`):
    - `usage.prompt_tokens`
    - `usage.prompt_tokens_details.cached_tokens`
  - **Local `llama.cpp` (`llama-server`):** Đọc từ trường `timings` trong JSON/SSE chunk cuối:
    - `timings.cache_n` (số lượng token được tái sử dụng từ KV cache)
    - `timings.prompt_n` (số lượng token phải chạy prefill mới trên GPU/CPU)
    - `timings.prompt_ms` (thời gian chạy prefill tính bằng mili-giây)

### Module 3: Cây Lịch sử Phiên LCP Session Lineage Tree (`src/lineage/`)

Đây là thuật toán cốt lõi giúp `kvguard` vượt qua bài Stress-Test trên Hacker News: **Không bao giờ báo lỗi giả (false positive) khi Agent gọi song song Sub-agent hoặc chạy nhiều luồng hội thoại.**

1. **Chuẩn hóa Khối (Block Decomposition):**
   Mỗi request LLM đi vào được tách thành một danh sách có thứ tự các **Khối Ngữ cảnh (`ContextBlock`)**:
   - `Block[0..T]`: Từng định nghĩa công cụ trong mảng `tools[]` (mỗi tool là 1 block).
   - `Block[T..S]`: Từng phần tử trong `system` prompt (hoặc message có `role == "system"` / `"developer"`).
   - `Block[S..M]`: Từng message trong mảng `messages[]` theo đúng thứ tự thời gian.
2. **Băm Khối & Token hóa (`BlockHasher`):**
   - Mỗi `ContextBlock` được tính mã băm nhanh `xxh64` (trên nội dung chuẩn hóa) và chuyển thành mảng `Vec<u32>` Token IDs (dùng `tiktoken-rs` `cl100k_base` / `o200k_base` hoặc Jinja renderer nếu cấu hình model local).
3. **Thuật toán Ghép Nhánh Hội thoại Cha (`LCP Matching`):**
   - Khi Request $R_{new}$ đến, duyệt qua các nhánh phiên đang hoạt động (`ActiveBranches` trong `DashMap`):
   - Tính độ dài tiền tố chung dài nhất (**Longest Common Token Prefix - LCP**) và độ tương đồng cấu trúc khối giữa $R_{new}$ và lượt cuối cùng của mỗi nhánh.
   - **Phân loại tự động:**
     - **Trường hợp 1 (Healthy Continuation):** $R_{new}$ khớp 100% chuỗi token của một nhánh cũ và chỉ nối thêm message mới ở cuối (`messages[last]`) → Ghép vào nhánh đó, trạng thái `CACHE HIT`.
     - **Trường hợp 2 (Broken Prefix Mutation - LỖI CẦN BẮT):** $R_{new}$ có cùng cấu trúc hội thoại với nhánh cha $B$ (khớp trên 70% nội dung các message giữa/cuối hoặc có cùng session metadata/system core), nhưng bị lệch một vài token ở phần đầu (`tools`, `system`, hoặc `messages` lịch sử) khiến LCP bị đứt ngang tại chỉ số token $K \ll \text{len}(B)$ → Ghép vào nhánh $B$ và kích hoạt cảnh báo **`CACHE BUST AT TOKEN #K`**!
     - **Trường hợp 3 (Sub-agent / New Session Branch):** $R_{new}$ có `system` prompt hoàn toàn khác (ví dụ Sub-agent tìm kiếm file hoặc trình tạo tiêu đề chỉ có 1–2 message) → Tự động tạo một nhánh con (`Sub-agent Branch`) độc lập, không báo lỗi vỡ cache trên nhánh chính.

### Module 4: Động cơ Diff Kép (JSON-AST + Token/Jinja Diff) (`src/diff/`)

Khi phát hiện `CACHE BUST` giữa $R_{parent}$ (Turn $N-1$) và $R_{current}$ (Turn $N$), chạy đồng thời 2 bộ phân tích để xuất chẩn đoán chính xác tuyệt đối:

1. **JSON-AST Path Diff (`ast_diff.rs`):**
   - Duyệt đệ quy cây `serde_json::Value` của $R_{parent}$ và $R_{current}$ theo thứ tự xuất hiện trong prompt (`tools` → `system` → `messages`).
   - Tìm vị trí đầu tiên có sự khác biệt và trả về:
     - **JSON Path chính xác:** Ví dụ `tools[2].name`, `tools[5].input_schema.properties`, `system[0].text`, hoặc `messages[4].content[0].text`.
     - **Loại đột biến (`MutationKind`):**
       - `ToolOrderShuffled`: Danh sách `tools` có cùng tập hợp tên nhưng bị đổi thứ tự (ví dụ `mcp__github` và `mcp__linear` đổi chỗ).
       - `SchemaKeyOrderChanged`: Các trường trong `input_schema` bị đảo thứ tự key khi serialize.
       - `VolatileSystemHeader`: Phát hiện mẫu `x-anthropic-billing-header: cch=` hoặc chuỗi hash/UUID thay đổi trong `system`.
       - `DynamicTimestampInPrefix`: Phát hiện chuỗi thời gian ISO-8601 / Unix timestamp thay đổi trong `system` hoặc `tools`.
       - `HistoricalMessageMutated`: Một message ở lượt cũ $i < N-1$ bị sửa đổi hoặc cắt bớt (do bộ nén ngữ cảnh hoặc chèn placeholder).
       - `JinjaTemplateDrift`: JSON input của các lượt cũ giống hệt nhau nhưng sau khi đi qua Jinja2 chat template lại sinh ra chuỗi token khác nhau.
2. **Jinja2 Template Verifier (`jinja_verifier.rs` - Dành cho Local LLM):**
   - Tích hợp sẵn các mẫu Jinja2 phổ biến (Qwen 3 / 3.5, Llama 3 / 4, Gemma 3, ChatML) và cho phép nạp file `tokenizer_config.json` tùy chỉnh qua `--chat-template <path>`.
   - Render toàn bộ mảng `messages` + `tools` của Turn $N-1$ và Turn $N$ thành chuỗi văn bản thô trước khi tokenize.
   - Bắt tại trận lỗi kinh điển của Qwen 3.5 ([r/LocalLLaMA](https://www.reddit.com/r/LocalLLaMA/comments/1sg076h/i_tracked_a_major_cache_reuse_issue_down_to_qwen/)): ở Turn $N-1$, message assistant có chứa `<think>...</think>`, nhưng sang Turn $N$ (sau khi có tool result), template lại render lượt cũ đó mà bỏ mất hoặc thêm rỗng `<think></think>`, làm gãy KV cache ngay tại lượt đó.
3. **Token-Exact Index & Cost Calculator (`token_diff.rs` & `pricing/mod.rs`):**
   - Tìm chỉ số `divergence_token_idx` (token đầu tiên khác nhau giữa 2 mảng Token IDs).
   - Tính số lượng token bị mất bộ nhớ đệm:
     $$\text{InvalidatedTokens} = \max(0, \text{ParentPrefixTokens} - \text{DivergenceTokenIdx})$$
   - Tính thiệt hại tài chính (USD) theo bảng giá model (cập nhật sẵn cho Claude 4.5/4.6 Opus/Sonnet/Haiku, GPT-5/4.1/o3, Gemini 2.5/3):
     $$\text{WastedUSD} = \frac{\text{InvalidatedTokens}}{1,000,000} \times (\text{CacheWriteOrBaseRate} - \text{CacheReadRate})$$
   - Tính thời gian GPU local bị lãng phí (mặc định tốc độ prefill tham chiếu `850 tok/s`, hoặc tự động hiệu chỉnh từ `timings.prompt_ms` thực tế của `llama-server`):
     $$\text{WastedSeconds} = \frac{\text{InvalidatedTokens}}{\text{MeasuredPrefillTokensPerSec}}$$

### Module 5: Tường lửa Tự động Chữa lành (In-Flight Auto-Healer) (`src/canonicalizer/`)

Khi người dùng bật cờ `--heal` (hoặc bấm phím `H` trên giao diện TUI), mọi request đi qua proxy sẽ được áp dụng chuỗi 5 phép biến đổi bảo toàn ngữ nghĩa (**Semantics-Preserving Canonicalization**) trước khi tính hash và gửi tới LLM:

1. **Quy tắc 1 — Chuẩn hóa `tools[]` và JSON Schema theo RFC 8785 (`jcs.rs`):**
   - Sắp xếp mảng `tools[]` theo thứ tự từ điển (lexicographical order) của tên công cụ (`tool.name` hoặc `tool.function.name`).
   - Đệ quy chuẩn hóa mọi `JsonObject` bên trong `input_schema` / `parameters`: sắp xếp toàn bộ object keys theo bảng chữ cái Unicode.
   - *Lưu ý an toàn:* Giữ nguyên thứ tự các phần tử bên trong mảng `enum` hoặc mảng dữ liệu, trừ mảng `required` trong JSON Schema (có thể sắp xếp theo bảng chữ cái vì `required` là tập hợp tên thuộc tính).
2. **Quy tắc 2 — Khử nhiễu System Prompt (`sanitizer.rs`):**
   - **Cắt bỏ Billing Header xoay vòng:** Dùng regex tìm và xóa chuỗi `x-anthropic-billing-header:\s*cch=[0-9a-fA-F]+;?\s*` bên trong các khối `system` ([anthropics/claude-code#40652](https://github.com/anthropics/claude-code/issues/40652)).
   - **Đóng băng Timestamp cấp phiên (Session-Scoped Timestamp Pinning):** Nếu phát hiện chuỗi thời gian thực tính đến giây/mili-giây (ví dụ `Current time: 2026-11-03T22:40:19.123Z`) nằm trong khối `system` mở đầu, tự động thay thế nó bằng mốc thời gian cố định lúc khởi tạo nhánh phiên (`session_start_timestamp`).
   - **Tách khối biến động xuống cuối (Volatile Tail Relocation - Tùy chọn `--relocate-git-status`):** Nếu phát hiện khối `gitStatus:` hoặc danh sách file untracked thay đổi liên tục nằm trong `system` prompt, thay thế khối đó trong `system` bằng chuỗi tĩnh `[GIT_STATUS_MOVED_TO_TAIL]` và đính kèm nội dung `gitStatus` mới nhất vào cuối message user hiện tại — giữ cho 25.000 token của `system` + lịch sử hội thoại luôn đạt 100% Cache Hit!
3. **Quy tắc 3 — Tự động gắn `cache_control` & `prompt_cache_key` (`breakpoints.rs`):**
   - **Với Anthropic (`/v1/messages`):** Anthropic giới hạn tối đa 4 điểm ngắt `cache_control: {"type": "ephemeral"}`. Nếu client quên gắn hoặc gắn sai vị trí, `kvguard` tự động đặt 3–4 điểm ngắt tối ưu tại: (1) phần tử cuối của `tools`, (2) phần tử cuối của `system`, và (3) 2 message gần cuối nhất trong `messages[]` để tạo cửa sổ trượt (rolling cache breakpoint).
   - **Với OpenAI (`/v1/chat/completions` & `/v1/responses`):** Nếu request chưa có `prompt_cache_key`, tự động băm tổ hợp `(model + tools_hash + system_prefix_hash)` để gán một `prompt_cache_key` ổn định xuyên suốt phiên, giúp bộ định tuyến của OpenAI luôn gửi request về đúng máy chủ đang giữ KV cache ([TrueFoundry Prompt Caching Analysis](https://www.truefoundry.com/blog/provider-agnostic-prompt-caching-llm-gateway)).
4. **Quy tắc 4 — Định tuyến KV Slot thông minh cho `llama-server` (`slot_router.rs`):**
   - Khi đích đến là `llama-server` (`llama.cpp`), `kvguard` duy trì một bảng ánh xạ `SlotPool` (ví dụ 4 slots: `0, 1, 2, 3`).
   - Nhánh hội thoại chính (`Main Agent`) được ghim cố định vào `id_slot: 0`.
   - Các nhánh `Sub-agent` hoặc tác vụ chạy nền được gán sang `id_slot: 1..N` (theo chính sách LRU trên các slot phụ).
   - Tự động chèn `"id_slot": assigned_slot` và `"cache_prompt": true` vào JSON body trước khi gửi tới `llama-server`, chấm dứt hoàn toàn tình trạng sub-agent xóa sổ KV cache 80K token của main agent ([ggml-org/llama.cpp#20510](https://github.com/ggml-org/llama.cpp/issues/20510)).
5. **Quy tắc 5 — Sửa lỗi `<think></think>` trong Lịch sử cho Local LLM:**
   - Khi chạy với cờ `--normalize-think-tags` cho các model họ Qwen/DeepSeek trên `llama-server` hoặc `vLLM`, tự động chuẩn hóa các khối `<think>...</think>` trong các lượt `assistant` lịch sử để tránh lỗi template render lệch chuỗi token sau khi gọi tool.

### Module 6: Giao diện Terminal UI (`ratatui`) (`src/tui/`)

Thiết kế giao diện Terminal UI sắc nét, tốc độ 60fps, chia làm 3 vùng rõ rệt:

1. **Vùng 1 — Thanh Chỉ số KPI trên cùng (Height: 3 dòng):**
   - Hiển thị 5 chỉ số thời gian thực:
     - `MODE`: `[🔥 AUTO-HEAL: ON]` (màu xanh lá) hoặc `[🔍 INSPECT ONLY]` (màu vàng) — bấm phím `h` để chuyển đổi nóng ngay khi đang chạy!
     - `CACHE HIT RATE`: Phần trăm token đọc từ cache trên tổng số token prefix (ví dụ `97.4%`).
     - `TOKENS SAVED`: Tổng số token không phải tính phí/prefill lại (ví dụ `1.42M tok`).
     - `COST SAVED / WASTED`: `$4.82 saved` (xanh) | `$0.14 wasted` (đỏ).
     - `LOCAL GPU TIME SAVED`: `+18m 42s prefill avoided`.
2. **Vùng 2 — Bảng Luồng Request theo Nhánh (Height: 45% màn hình):**
   - Các cột: `Turn #` | `Branch / Slot` (`main [slot 0]`, `sub-1 [slot 1]`) | `Model` | `Prefix Health Bar` (thanh tiến trình trực quan hiển thị tỷ lệ Cached màu xanh vs Uncached màu đỏ) | `Hit %` | `Root Cause / Action Taken`.
   - Ví dụ dòng hiển thị khi chưa bật Auto-Heal:
     `#04 | main [slot 0] | claude-sonnet-4-5 | [██░░░░░░░░] 12% | ❌ BUST @ tok #418 (system cch= header)`
   - Ví dụ dòng hiển thị khi đã bật Auto-Heal:
     `#05 | main [slot 0] | claude-sonnet-4-5 | [█████████░] 96% | ⚡ HEALED (sorted 48 tools + stripped cch=)`
3. **Vùng 3 — Khung Soi X-Ray Diff Chi tiết (Height: 45% màn hình):**
   - Hiển thị chi tiết cho lượt đang được chọn (dùng phím `↑`/`↓` hoặc `j`/`k`):
     - Dòng tiêu đề chẩn đoán: `Divergence at Token #418 / 41,950 | JSON Path: system[0].text (line 4) | Impact: -$0.142 (+42.1s prefill)`
     - Khung Diff trực quan (màu đỏ cho `Turn N-1`, màu xanh cho `Turn N` tại đúng đoạn ký tự/token bị lệch, ẩn bớt hàng chục nghìn token giống nhau ở hai đầu).
   - Phím tắt thanh trạng thái dưới cùng:
     `[h] Toggle Auto-Heal  [d] View: JSON AST / Jinja Tokens  [f] Filter: Busts Only  [e] Export Trace (.jsonl)  [q] Quit`

## 4. Kịch bản Kiểm thử Tự động Bắt buộc (`tests/integration_tests.rs`)

Bạn BẮT BUỘC phải viết đầy đủ 5 bài kiểm thử tích hợp (Integration Tests) sử dụng `wiremock` để chứng minh `kvguard` hoạt động chính xác 100%:

1. **`test_detect_and_heal_claude_code_billing_header`:**
   - Gửi Turn 1 với `system: "x-anthropic-billing-header: cch=11111\nYou are Claude..."` + 10.000 token ngữ cảnh.
   - Gửi Turn 2 với `system: "x-anthropic-billing-header: cch=22222\nYou are Claude..."` + 10.000 token ngữ cảnh + 1 message mới.
   - Kiểm tra: Ở chế độ `Inspect`, `kvguard` báo `MutationKind::VolatileSystemHeader` tại đúng token đầu tiên. Ở chế độ `Heal`, cả 2 request gửi tới upstream đều đã bị cắt bỏ `x-anthropic-billing-header: cch=...` và khớp 100% prefix.
2. **`test_detect_and_heal_mcp_tool_shuffle`:**
   - Turn 1 có mảng `tools: [tool_github, tool_linear, tool_supabase]`.
   - Turn 2 có mảng `tools: [tool_supabase, tool_github, tool_linear]` và đảo thứ tự key trong `input_schema`.
   - Kiểm tra: Chế độ `Inspect` bắt được `MutationKind::ToolOrderShuffled`. Chế độ `Heal` sắp xếp lại mảng `tools` theo tên (`github` → `linear` → `supabase`) và chuẩn hóa JCS cho `input_schema` khiến chuỗi byte của `tools` ở Turn 1 và Turn 2 giống hệt nhau.
3. **`test_subagent_interleaving_no_false_positive_and_slot_pinning`:**
   - Gửi Turn 1 của Main Agent (20.000 token).
   - Gửi xen kẽ 1 request của Sub-agent (chỉ có 1.500 token với system prompt khác).
   - Gửi Turn 2 của Main Agent (20.000 token cũ + 500 token mới).
   - Kiểm tra: `LcpTree` tự động tách Sub-agent thành nhánh riêng, gán `id_slot: 0` cho Main Agent và `id_slot: 1` cho Sub-agent khi đích là `llama-server`, và ghép Turn 2 của Main Agent với Turn 1 (báo `CACHE HIT` thay vì báo lỗi giả so với Sub-agent).
4. **`test_detect_qwen_jinja_think_tag_drift`:**
   - Nạp Jinja chat template mô phỏng hành vi cắt bỏ `<think></think>` ở các lượt lịch sử trước lượt gọi tool cuối cùng.
   - Kiểm tra: `jinja_verifier` phát hiện chính xác vị trí token bị lệch sau khi render template dù các message JSON lịch sử không bị sửa đổi trực tiếp.
5. **`test_ci_gate_exit_code`:**
   - Chạy `kvguard check` trên tập fixture có chứa lỗi đột biến message lịch sử (`HistoricalMessageMutated`) và xác nhận lệnh trả về mã thoát khác 0 kèm báo cáo vị trí JSON path.

## 5. Quy tắc Thực thi & Luật Cắt Bỏ Phạm Vi (Strict Execution Rules)

Khi bắt đầu viết code cho dự án này, bạn phải tuân thủ tuyệt đối các nguyên tắc sau:

1. **Không viết code giả (No Placeholders / No `todo!()`):** Mọi module được liệt kê ở trên phải được cài đặt với logic hoạt động thực tế, xử lý lỗi đầy đủ bằng `anyhow` / `thiserror` và biên dịch thành công với `cargo test` không có lỗi.
2. **Bảo toàn Streaming SSE 100%:** Không bao giờ đọc toàn bộ body của response streaming vào một chuỗi `String` rồi mới trả về cho client. Phải stream từng `Bytes` chunk ngay khi nhận được từ `reqwest` để giữ Time-To-First-Token (TTFT) không đổi.
3. **Ba thứ TUYỆT ĐỐI CẤM đưa vào code (Anti-Bloat Guardrails):**
   - Không cài đặt hay yêu cầu bất kỳ cơ sở dữ liệu ngoài nào (PostgreSQL, Redis, MySQL).
   - Không xây dựng Web Dashboard bằng React/Next.js/HTML. Chỉ sử dụng giao diện TUI `ratatui` và CLI output.
   - Không cài đặt Semantic Vector Cache (Embeddings/FAISS). `kvguard` chỉ giải quyết bài toán **Exact Token-Prefix KV/Prompt Cache**.
4. **Trình tự triển khai từng bước (Step-by-Step Implementation Order):**
   - **Bước 1:** Khởi tạo `Cargo.toml` và các cấu trúc dữ liệu cốt lõi trong `src/config.rs`, `src/lineage/block_hasher.rs`, `src/lineage/lcp_tree.rs`.
   - **Bước 2:** Lập trình bộ chuẩn hóa `src/canonicalizer/` (`jcs.rs`, `sanitizer.rs`, `breakpoints.rs`, `slot_router.rs`) và Động cơ Diff `src/diff/` (`ast_diff.rs`, `token_diff.rs`, `jinja_verifier.rs`).
   - **Bước 3:** Lập trình HTTP/SSE Streaming Proxy trong `src/proxy/` và bộ tính toán chi phí `src/pricing/mod.rs`.
   - **Bước 4:** Lập trình giao diện TUI `src/tui/`, chế độ CI `src/ci/mod.rs` và CLI wrapper `src/cli.rs` + `src/main.rs`.
   - **Bước 5:** Viết toàn bộ 5 Integration Tests trong `tests/integration_tests.rs`, chạy `cargo test` để xác minh mọi bài test đều PASS, sau đó tạo file `README.md` chuẩn Hacker News (kèm hướng dẫn câu lệnh 1 dòng, sơ đồ kiến trúc, bảng so sánh benchmark và kịch bản GIF demo bằng công cụ `vhs`).
