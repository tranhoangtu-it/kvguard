# kvguard — Phân tích lại bài toán & lộ trình tính năng

*Cập nhật: 2026-11-04 · Áp dụng cho v0.1.1*

## 1. Bài toán thật sự là gì

Người dùng không quan tâm "prefix có bị lệch không". Họ quan tâm **hai câu hỏi**:

1. *"Tại sao hóa đơn / thời gian chờ của tôi cao hơn mức nên có?"*
2. *"Lỗi đó do tôi (client/agent) gây ra hay do phía provider?"*

v0.1 trả lời tốt nhánh **"do client"** (prefix thay đổi: header xoay vòng, timestamp, thứ tự tool, lịch sử bị sửa, template drift). Nhưng nghiên cứu mới cho thấy nhóm lỗi gây thiệt hại lớn nhất hiện nay lại nằm ở nhánh còn lại:

| Nhóm lỗi | Ví dụ thực tế | v0.1 xử lý? |
|---|---|---|
| A. Prefix thay đổi (client-side) | billing header `cch=`, MCP tool shuffle, timestamp, compaction | ✅ phát hiện + heal |
| B. Prefix **không đổi** nhưng provider đọc cache **một phần** | Claude Code: `cache_read_input_tokens` tụt từ ~640K về đúng 25,165, lặp 17 lần trong 3 giờ, ~10.4M token ghi cache thừa | ❌ chỉ báo MISS khi đọc = 0 |
| C. Prefix không đổi nhưng **vượt luật cache của provider** | cửa sổ lookback 20 block của Anthropic, ngưỡng token tối thiểu theo model, TTL 5 phút hết hạn | ⚠️ một phần (ngưỡng cứng 1024) |
| D. Định tuyến / dung lượng phía server | OpenAI >15 req/phút cho cùng prefix+key gây tràn; llama-server sub-agent chiếm slot | ⚠️ slot đã xử lý; OpenAI chưa |

**Kết luận:** giá trị cốt lõi của kvguard nên được định nghĩa lại thành *"kiểm toán cache mỗi request: kỳ vọng vs thực tế, và ai chịu trách nhiệm"* — không chỉ "diff prefix".

## 2. Sự thật về luật cache (nguồn ở cuối)

- **Anthropic:** breakpoint TTL 5m hoặc 1h; ghi cache 5m = 1.25×, 1h = 2.0×, đọc = 0.1× giá input; tối đa 4 breakpoint; **chỉ dò ngược 20 block** từ mỗi breakpoint; ngưỡng tối thiểu khác nhau theo model (bên thứ ba ghi Opus 4.5/4.6: 4096, Sonnet 4.6: 2048 — *cần xác minh với bảng chính thức*); 1h phải đứng trước 5m.
- **OpenAI:** cache từ 1024 token; `prompt_cache_key` kết hợp với hash prefix để định tuyến; nên giữ < ~15 req/phút cho mỗi cặp prefix+key; `prompt_cache_retention: "24h"`; một số model mới tính phí ghi cache.
- **llama.cpp:** mỗi slot giữ KV của request trước; `cache_prompt` mặc định bật; `--cache-reuse N` tái dùng chunk bằng KV shifting; `--slot-prompt-similarity`; lưu/khôi phục slot qua `/slots/{id}?action=save|restore` (cần `--slot-save-path`).
- **Claude Code:** dùng TTL 1h trên subscription, 5m với API key trừ khi đặt `ENABLE_PROMPT_CACHING_1H=1` (theo bài phân tích bên thứ ba).

## 3. Đánh giá v0.1.1 hiện tại

**Điểm mạnh đã kiểm chứng:** proxy streaming không buffer; phân nhánh sub-agent không báo nhầm; diff JSON-path + token; 5 quy tắc heal; CI gate; xác minh với Ollama thật (HIT, 1,619/1,635 token cache); release 5 nền tảng.

**Điểm yếu cần sửa:**
1. Chi phí "lãng phí" là **ước tính** từ diff, trong khi provider đã trả số thật (`cache_creation_input_tokens`).
2. Trạng thái MISS chỉ bắt khi đọc cache = 0 → bỏ sót nhóm B.
3. Ngưỡng cache tối thiểu cứng 1024 → sai với Opus/Sonnet mới (MISS giả hoặc bỏ sót).
4. Giá ghi cache cố định 1.25× → sai khi dùng TTL 1h (2.0×).
5. Chưa chạy thật với Claude Code + Anthropic API.
6. Token index cho model local là ước tính o200k, trong khi llama-server có `/tokenize` chính xác.

## 4. Lộ trình — Phải / Nên / Có thể

### P0 — Phải làm (đúng đắn & uy tín) → v0.2

| # | Tính năng | Vì sao | Ước lượng |
|---|---|---|---|
| P0-1 | **Kế toán chi phí theo số thật của provider**: dùng `cache_creation_input_tokens`, `cache_read_input_tokens`, `cached_tokens`, `cache_n` làm nguồn chính; ước tính chỉ là phương án dự phòng | KPI phải khớp hóa đơn | S |
| P0-2 | **Phát hiện "đọc cache một phần" (PARTIAL / COLLAPSE)**: khi lineage nói prefix nguyên vẹn nhưng `cached / prefix_kỳ_vọng < 0.8` → trạng thái riêng, chỉ rõ "lỗi phía provider", ghi số token ghi thừa thật | Nhóm B — thiệt hại lớn nhất, hiện vô hình | M |
| P0-3 | **Luật cache theo model**: bảng ngưỡng tối thiểu theo model; chẩn đoán **vượt cửa sổ lookback 20 block** (số block giữa breakpoint trước và breakpoint mới > 20) | Nhóm C — giải thích MISS "vô lý" | M |
| P0-4 | **Giá theo TTL**: đọc `cache_control.ttl` (5m/1h) để tính 1.25× hay 2.0× | Số tiền hiện sai khi dùng 1h | S |
| P0-5 | **Kiểm chứng thật với Claude Code** (Anthropic API + llama-server), ghi lại trace làm fixture hồi quy | Uy tín khi ra mắt | M |

### P1 — Nên làm (khác biệt cạnh tranh) → v0.3

| # | Tính năng | Vì sao |
|---|---|---|
| P1-1 | **Token chính xác cho local**: gọi `llama-server /tokenize` + `/apply-template` thay cho o200k + minijinja | Bỏ chữ "ước tính" với backend local |
| P1-2 | **Heal lookback**: thêm breakpoint trung gian khi chuỗi tool-loop dài > 20 block (chỉ khi client không tự quản breakpoint) | Biến chẩn đoán P0-3 thành tự sửa |
| P1-3 | **`/metrics` Prometheus + `--log-json`** | Team/CI cần dashboard, không cần web UI |
| P1-4 | **OpenAI**: cảnh báo > 15 req/phút cùng prefix+key; tùy chọn `--openai-retention 24h` | Nhóm D |
| P1-5 | **Dòng trạng thái** `kvguard status --line` cho statusline của Claude Code / tmux | Thấy cache hit mà không cần mở TUI |
| P1-6 | **Phát hành tự động**: npm/PyPI Trusted Publishing từ GitHub Actions khi có tag | Bỏ thao tác tay, hết lỗi token hết hạn |

### P2 — Có thể làm

- Lưu/khôi phục slot llama-server qua restart (`/slots?action=save|restore`).
- Gợi ý cờ llama-server (`--cache-reuse`, `-np`, `--slot-prompt-similarity`) dựa trên lưu lượng quan sát được.
- Giảm RAM: chỉ giữ dữ liệu cần thiết của request cha (tên tool + văn bản block) thay vì toàn bộ JSON.
- Token chính xác cho Claude qua endpoint `count_tokens` (tùy chọn, có giới hạn tốc độ).

### Không làm (giữ đúng tinh thần anti-bloat)

Web dashboard, cơ sở dữ liệu ngoài, semantic/embedding cache, response cache.

## 5. Đề xuất thứ tự thực hiện

1. **v0.2 = P0-1 → P0-4** (một PR mỗi mục, mỗi mục có test với fixture usage thật), sau đó **P0-5** để lấy trace thật làm fixture.
2. **v0.3 = P1-1, P1-2, P1-3, P1-6**.
3. P1-4, P1-5 và P2 theo phản hồi người dùng.

## 6. Trạng thái (v0.2.0)

| Mục | Trạng thái |
|---|---|
| P0-1 Chi phí theo số thật của provider | ✅ `expected − served` từ usage của provider |
| P0-2 Phát hiện PARTIAL | ✅ trạng thái `PARTIAL`, ngưỡng `--partial-threshold` (mặc định 0.8) |
| P0-3 Luật cache theo model + lookback 20 block | ✅ ngưỡng tối thiểu thận trọng theo model; ghi chú khi > 20 content block |
| P0-4 Giá theo TTL | ✅ 1.25× (5m) / 2.0× (1h) |
| P0-5 Kiểm chứng thật với Claude Code | ✅ lượt 2 đọc đúng 95,836 token như kỳ vọng → `HIT` |
| P1-3 Prometheus `/__kvguard/metrics` | ✅ |
| P1-4 Cảnh báo OpenAI > 15 req/phút/key | ✅ (ghi chú trên lượt) |
| P1-5 `kvguard status --line` | ✅ |
| P1-6 Trusted Publishing npm/PyPI | ✅ workflow sẵn sàng, bật bằng biến `PUBLISH_NPM` / `PUBLISH_PYPI` |
| P1-1 Token chính xác qua llama-server | ✅ `/apply-template` + `/tokenize`; tự phát hiện template drift không cần `--chat-template` (bỏ qua ≤ 4 token lệch ở ranh giới BPE) |
| P2 Gợi ý cờ llama-server | ✅ ghi chú `-np 2` khi server 1 slot có thêm sub-agent |
| P1-2 Heal lookback | ❌ không làm — client (Claude Code) thường đã dùng đủ 4 breakpoint; tự chèn sẽ phá cơ chế client tự quản |
| P2 Lưu/khôi phục slot | ⏳ để sau — cần `--slot-save-path`, lợi ích thấp |
| P2 Giảm RAM | ⏳ để sau — phát hiện template drift cần JSON của lượt cha |
| P2 `count_tokens` cho Claude | ❌ không làm — tốn quota người dùng |

**Quyết định cho các câu hỏi mở:** ngưỡng PARTIAL cấu hình được (mặc định 0.8); bảng ngưỡng tối thiểu dùng giá trị thận trọng (Opus 4096, Haiku/Sonnet 4.6+ 2048, còn lại 1024) — chỉ có thể *giảm* cảnh báo, không tạo cảnh báo sai; không gọi `count_tokens`.

## 7. Câu hỏi còn mở

- Ngưỡng PARTIAL (đề xuất 0.8) nên cố định hay cho cấu hình?
- Bảng ngưỡng tối thiểu theo model cần xác minh với tài liệu chính thức của Anthropic trước khi đưa vào code.
- Có muốn kvguard tự gửi request `count_tokens` (tốn quota) để có token index chính xác cho Claude không?

## Nguồn

- [Anthropic — Prompt caching](https://platform.claude.com/docs/en/build-with-claude/prompt-caching)
- [Portkey — Anthropic prompt caching](https://portkey.ai/docs/aigw/integrations/llms/anthropic/prompt-caching)
- [orq.ai — Prompt caching (ngưỡng theo model)](https://docs.orq.ai/docs/proxy/prompt-caching)
- [OpenAI — Prompt caching](https://developers.openai.com/api/docs/guides/prompt-caching)
- [Azure OpenAI — Prompt caching](https://learn.microsoft.com/azure/foundry/openai/how-to/prompt-caching)
- [llama.cpp — Tutorial: KV cache reuse with llama-server #13606](https://github.com/ggml-org/llama.cpp/discussions/13606)
- [Liquid AI — llama.cpp chat](https://docs.liquid.ai/deployment/on-device/llama-cpp/chat.md)
- [Báo cáo: prompt cache bị drop 17 lần/3 giờ](https://claudeissues.com/issue/83542-prompt-cache-repeatedly-dropped-mid-session-17x-in-3h-causing-10-4m-tokens-of-re)
- [Báo cáo: cache lookup thất bại giữa phiên](https://claudeissues.com/issue/87966-bug-prompt-cache-lookup-fails-intermittently-mid-session-cache-read-pinned-to-th)
- [Why your custom system prompt silently kills Claude's prompt cache](https://mer.vin/2026/08/why-your-custom-system-prompt-silently-kills-claudes-prompt-cache/)
- [cachebench](https://pypi.org/project/cachebench/)
