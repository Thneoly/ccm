# CCM 使用指南

CCM（Claude Code Model Manager）让你把 Claude Code 指向本地或第三方模型网关（zai、minimax 这类 anthropic / anthropic-compatible 端点，以及 DeepSeek 等 OpenAI 兼容端点——ccm 在代理内做双向协议翻译），并在中间加一层路由：fallback、重试、熔断、指标与运行时切换。

本文是操作手册。架构背景见 [docs/DESIGN.md](DESIGN.md)，v0.4 的规划与逐里程碑落地记录见 [docs/V0.4_PLAN.md](V0.4_PLAN.md)（v0.3 的发布与验证记录见 [docs/V0.3_PLAN.md](V0.3_PLAN.md)）。

---

## 1. 三十秒理解 CCM

三个核心概念：

| 概念 | 是什么 | 定义位置 |
|---|---|---|
| 模型（model） | 一个具体可用的模型：所属 provider + `model_id` + 相对成本/质量权重 | config.toml |
| 路由（route） | 候选列表（primary + fallback 顺序）+ 选择策略 + 重试/超时/熔断参数 | config.toml |
| 本地代理（proxy） | 运行在 `127.0.0.1:13521` 的反向代理：注入凭据、执行路由/fallback/熔断、暴露 `/_ccm/*` 观测接口 | 进程（`ccm proxy`） |

另有 **profile**（模型别名，如 `coding` → `claude`）：凡是接受模型名的命令也接受 profile 名。

数据的存放位置——理解这个划分能解释 90% 的"为什么找不到"：

| 位置 | 存什么 | 路径 / 形式 |
|---|---|---|
| `config.toml` | 声明式配置：providers、models、profiles、routes | `$CCM_HOME/config.toml`，默认 `~/.ccm/config.toml` |
| `state.toml` | 只有一项 `current`：持久化的默认目标 | `$CCM_HOME/state.toml`，默认 `~/.ccm/state.toml` |
| `clients.toml` | 客户端会话：scoped 条目的 id / target / last_seen_ms（只由代理写，v0.5） | `$CCM_HOME/clients.toml` |
| `history/` | 观测历史 JSONL：decisions / metrics / circuit / usage（v0.4） | `$CCM_HOME/history/`，见 6.3 |
| 系统凭据管理器 | 各 provider 的 API key，**永不写入任何文件** | Windows 凭据管理器（service 名为 `ccm`，条目名 = provider 名，如 `LegacyGeneric:target=zai.ccm`）；也可用环境变量 |

两个直接推论：

- API key 在 config.toml 里永远找不到，这是设计（见第 8 节 FAQ）。
- 路由策略（fallback/熔断/多候选选择）只在**代理模式**下生效；直连模式是单模型直通。

---

## 2. 安装

两种方式：**预编译二进制**（v0.4.0 起随发布提供，无需 Rust 工具链，推荐）或**源码构建**（需要 Rust 工具链 + 仓库）。两种方式都要求 Claude Code 已安装（终端里 `claude --version` 可用）。

### 2.1 预编译二进制

从 [GitHub Releases](https://github.com/Thneoly/ccm/releases/latest) 下载对应平台产物（版本号以发布页为准；每次发布附带 `checksums.txt`）：

| 平台 | 产物文件名 |
|---|---|
| Windows x64 | `ccm-v<版本>-x86_64-pc-windows-msvc.exe` |
| Linux x64 | `ccm-v<版本>-x86_64-unknown-linux-gnu` |

macOS 没有预编译产物（macOS 本就不在支持声明内，见 2.2 末尾）。

**完整性校验**（建议）：下载同一发布的 `checksums.txt` 后对照 SHA-256——

```powershell
# Windows PowerShell（以 v0.4.0 为例）
Get-FileHash .\ccm-v0.4.0-x86_64-pc-windows-msvc.exe -Algorithm SHA256
```

```sh
# Linux（只校验你下载的那一行；checksums.txt 列出了两个平台的产物）
grep x86_64-unknown-linux-gnu checksums.txt | sha256sum -c
```

输出的哈希与 `checksums.txt` 中对应行一致才继续安装。

**放入 PATH**：

```powershell
# Windows：装进用户程序目录（install.ps1 默认也装这里；确认该目录在用户
# PATH 里，不在就手动加入——加完需要新开终端）
mkdir $env:LOCALAPPDATA\Programs\ccm -Force
Copy-Item .\ccm-v0.4.0-x86_64-pc-windows-msvc.exe $env:LOCALAPPDATA\Programs\ccm\ccm.exe
```

```sh
# Linux：重命名为 ccm 后放进 ~/.local/bin（确认该目录在 PATH 中）
chmod +x ccm-v*-x86_64-unknown-linux-gnu
mkdir -p ~/.local/bin
mv ccm-v*-x86_64-unknown-linux-gnu ~/.local/bin/ccm
```

> Linux 产物在 Ubuntu 24.04 上构建，要求 **glibc ≥ 2.39**——Debian 12 / Ubuntu 22.04 及更早的发行版无法直接运行，请改用源码构建（2.2；产物链接的是构建机自己的 glibc，适应性更广）。

### 2.2 源码构建

前置条件：Rust 工具链、本仓库（`git clone` 或发布页的源码包）。

#### Windows（PowerShell，仓库根目录）

```powershell
.\scripts\install.ps1
# 如被执行策略拦截：
powershell -ExecutionPolicy Bypass -File scripts/install.ps1
```

脚本会先 `cargo build --release`，再把 `ccm.exe` 装到 `%LOCALAPPDATA%\Programs\ccm\ccm.exe`，并把该目录追加进用户 PATH（幂等，重复执行无副作用），最后用 `ccm --version` 验证。

**注意：装完后需要新开一个终端**，当前会话看不到 PATH 变更。

#### Linux / macOS（sh，仓库根目录）

```sh
./scripts/install.sh
# 自定义安装目录：
CCM_INSTALL_DIR=/usr/local/bin ./scripts/install.sh
```

默认装到 `~/.local/bin`（请确认该目录在你的 PATH 中），同样先构建、后用 `ccm --version` 验证。

#### 手动构建（两平台通用）

```powershell
cargo build --release
# 产物：target\release\ccm.exe —— 复制到任意 PATH 目录
```

```sh
cargo build --release
cp target/release/ccm ~/.local/bin/
```

**注意：v0.4 只声称支持 Windows 和 Linux**（Linux 在 WSL2 Ubuntu 24.04 上验证）。macOS 没有可用主机跑验证，**不声称支持**——`install.sh` 是通用 POSIX sh，理论上可跑，但未经确认。

### 2.3 验证

```powershell
ccm --version
```

输出 `ccm 0.4.0`。

---

## 3. 首次配置

完整流程：`init` → `auth set` →（按需）`add provider` / `add model` → `use` → `doctor` / `health` 验证。

### 3.1 初始化

```powershell
ccm init
```

```text
Initialized C:\Users\you\.ccm\config.toml
Initialized C:\Users\you\.ccm\state.toml
```

初始内容（开箱即用的一套样例）：

| 类型 | 名称 | 内容 |
|---|---|---|
| provider | `anthropic` | kind `anthropic`，`https://api.anthropic.com`，`x-api-key` |
| provider | `zai` | kind `anthropic-compatible`，`https://api.z.ai/api/anthropic`，`x-api-key` |
| model | `claude` | provider `anthropic`，model_id `claude-sonnet-5-5` |
| model | `glm` | provider `zai`，model_id `glm-5.3`，cost_weight 0.25，quality_weight 0.85 |
| profile | `coding` | → `claude` |
| profile | `fast` | → `glm` |
| route | `coding-route` | primary `claude`，fallback `[glm]`，默认策略 |
| route | `fast-route` | primary `glm`，无 fallback，默认策略 |

state.toml 初始 `current = "claude"`。

已有 config.toml 时 `ccm init` 会报错并提示 `--force`；`ccm init --force` 会覆盖 config.toml 和 state.toml 两个文件（state.toml 若已存在且不加 `--force`，则保持不动）。

### 3.2 存 API key（进系统凭据管理器，不进文件）

```powershell
ccm auth set zai      # 提示 "API key for zai: "，隐藏输入
ccm auth set minimax
```

```text
Stored credential for zai
Stored credential for minimax
```

删除：`ccm auth delete <provider>` → `Deleted credential for <provider>`。

### 3.3 添加 provider 和模型

`init` 已经带好了 zai + glm。以 MiniMax 为例添加一个新网关：

```powershell
ccm add provider minimax --base-url https://api.minimax.cn/anthropic --kind anthropic-compatible --auth x-api-key
ccm add model minimax --provider minimax --model-id MiniMax-M3
```

```text
Saved provider minimax
Saved model minimax
```

等价地，重建 zai / glm 的命令（供参考）：

```powershell
ccm add provider zai --base-url https://api.z.ai/api/anthropic --kind anthropic-compatible --auth x-api-key
ccm add model glm --provider zai --model-id glm-5.3 --cost-weight 0.25 --quality-weight 0.85
```

`ccm add provider` 参数：

| Flag | 取值 | 省略时 |
|---|---|---|
| `--base-url` | 网关地址 | 进入交互提示 `Base URL:`（空输入报错） |
| `--kind` | `anthropic` / `anthropic-compatible` / `compatible` / `openai-compatible` / `openai`（不分大小写） | 交互提示 `Kind (anthropic / anthropic-compatible / openai-compatible):` |
| `--auth` | `x-api-key`（别名 `x_api_key` / `apikey` / `api-key`）或 `bearer`（别名 `authorization`） | 按 kind 决定：anthropic 类默认 `x-api-key`，openai-compatible 默认 `bearer` |

`ccm add model` 参数：`--provider`（必须指向已存在的 provider）、`--model-id`（省略则交互提示）；`--cost-weight` / `--quality-weight` 默认各 `1.0`（语义见 5.3）。

注意：`add provider` 是 upsert，同名会**静默覆盖**。另外所有 `ccm add` 都是交互友好的，但在脚本/CI 里请把 flag 传全，避免卡在提示上。

provider 配好之后，模型不用逐个手抄：网关支持 `GET /v1/models` 的话 `ccm discover` 能拉清单勾选注册（见 3.5）。

### 3.4 验证

```powershell
ccm use glm
ccm doctor
ccm health glm
ccm health minimax
```

```text
Selected glm as persisted default
```

`ccm doctor` 是本地体检（不发真实推理请求），按链路逐项检查：

```text
CCM doctor

✓ Claude Code: installed (2.1.261)
✓ settings.json: no ANTHROPIC_* env overrides
✓ history: 3 file(s) at C:\Users\you\.ccm\history
✓ current target: glm
✓ primary model: glm
✓ model id: glm-5.3
✓ provider: zai
✓ base URL: https://api.z.ai/api/anthropic
✓ credential: present
✓ discovery endpoint: /v1/models answered (200)

For a full authenticated model check, run `ccm health glm`.
```

- `endpoint` 一行（v0.5 起改名 discovery endpoint）对 `{base_url}/v1/models` 发起**不带凭据**、**不跟随重定向**的 GET（10 秒超时）：200 且 JSON（或无 content-type）→ `✓ answered`；200 但响应不是 JSON（登录页/兜底页也答 200）→ `!` 提示并由 `ccm discover <provider>` 确证；401/403 → `✓ exists (needs auth)`，此时带凭据的 `ccm discover <provider>`（见 3.5）能拉到清单；404/405 → `! not exposed`，该网关不支持清单发现，模型得手动加；其余状态（含 SSO 的 302）→ `! answered ({status})`。
- `Claude Code` 一行解析 `claude --version` 输出并带版本号；低于 2.1.227 时追加一行警告——该版本起才支持 `ANTHROPIC_CUSTOM_HEADERS`（ccm 注入的客户端身份头），更早的版本客户端身份只能走 `ccm-local-<id>` token 通道。
- 当前目标属于 openai-compatible provider 时，会多一行 `! current target: openai-compatible models are proxy-only (...)`——该目标只能走代理模式（见 4.2 / 5.5）。
- `settings.json` 一行检查 Claude Code 自己的 `~/.claude/settings.json` / `settings.local.json` 的 `env` 块——那里的 `ANTHROPIC_*` 键（含 `ANTHROPIC_CUSTOM_HEADERS`，会顶掉 ccm 注入的客户端身份头）会覆盖 ccm 的注入（见 FAQ）。
- `history` 一行显示持久化历史目录状态（v0.4，见 6.3）：文件数；目录还空着或 `[observability] history_enabled = false` 时是 `!` 提示（非致命，不影响后面各项）。
- 链路中途失败会提前结束（例如 `✗ target `...`: ...`），后面的检查不再打印。
- 刚 `init` 完（current 还是 `claude` 且没存 anthropic key）时，凭据行会是：
  `✗ credential: missing (set CCM_ANTHROPIC_API_KEY or run `ccm auth set anthropic`)`

`ccm health <target>` 发一条真实的最小请求（`max_tokens: 1`、内容 `ping`；anthropic 类走 `/v1/messages`，openai-compatible 类走 `/v1/chat/completions`），验证鉴权和模型名：

```text
healthy: zai / glm-5.3
healthy: minimax / MiniMax-M3
```

注意 `health` 只接受**模型名或 profile 名，不接受路由名**（`use` / `switch` / `proxy` 三者才接受路由名）。常见报错：401/403 → `provider reachable but authentication failed (401)`；名字拼错 → `unknown model/profile `...``（解析发生在发请求之前）。

### 3.5 从网关发现模型清单：`ccm discover`（v0.5）

`ccm discover [provider]` 拉取网关的 `GET /v1/models`，列出全部模型供你勾选注册——省去从网关控制台逐个手抄 `model_id`（手抄路径 `ccm add model` 依然可用，且仍是覆盖/upsert 路径）。

```powershell
ccm discover zai          # 指定 provider
ccm discover              # 交互选择已配置的 provider
ccm discover zai --all    # 非交互全选（脚本/CI 友好）
```

输出形如：

```text
== ccm discover — provider zai (anthropic-compatible) @ https://api.z.ai/api/anthropic
   1. glm-4.5  GLM-4.5
   2. glm-4.5-air  GLM-4.5-Air
   ...
   9. glm-5.3  GLM-5.3
  10. glm-5.3-flash  GLM-5.3-Flash
  11. glm-5.3-flashx  GLM-5.3-FlashX
11 listed, 1 already registered
Register which? (numbers like 1,3-5, `all`, or Enter for none): 9,10
Skipped glm-5.3 — already registered as `glm`
Saved model glm-5-3-flash

# hand-enter prices to make `ccm advise` useful — paste into config.toml:
# [models.glm-5-3-flash.pricing]
# input = 0.0        # USD per 1M input tokens
# output = 0.0       # USD per 1M output tokens
# cache_read = 0.0   # USD per 1M cache-read tokens
# cache_write = 0.0  # USD per 1M cache-write tokens
```

行为要点：

- **拉清单是只读的**：确认选择之前不写任何配置；注册只写 config.toml（新模型路由权重 1.0/1.0）。
- **skip 不覆盖**：同 provider 下已注册的 `model_id` 跳过并提示——手工调过的权重和定价永不被 discover 动到（想覆盖走 `ccm add model`）。
- **别名** = 规范化的 model_id（非 `[A-Za-z0-9_-]` 字符替换成 `-`；与现有配置里的名字冲突——**含其他 provider 的模型别名和 profile 名**——或同批内冲突时，加 `-2`/`-3` 后缀避开，绝不覆盖已有条目）。
- **定价是手填事实，不由 CLI 代填**：新注册模型不带定价表，discover 为每个选中的未定价模型打印注释掉的 `[models.<alias>.pricing]` 骨架。填好价格，`ccm advise`（6.6）才有依据——discover → 手填价 → advise 是设计好的闭环（见 5.3）。
- **分页**：anthropic 类 `has_more` 游标分页自动跟进（上限 10 页，触顶提示清单可能不完整）；z.ai 这类不分页的网关一次拉完。清单行带 `display_name`（anthropic 类）或 `(owned by ...)`（openai 类）时一并显示。
- **鉴权**：按 provider 的 `auth` 配置注入凭据（anthropic 类附 `anthropic-version: 2023-06-01`），单请求 10 秒超时；凭据只发给配置的网关主机——**不跟随重定向**（302 按原状态报错），错误文案里引用的网关响应体会先抹掉凭据本身（网关把 key 回显进错误体也打印不出来）。清单形状与声明的 `kind` 明显不符时（如 openai 类网关返回 anthropic 形状）会先打一行警告，请求仍按声明的 kind 走。

降级路径（直接报错退出，不写配置）：

| 网关应答 | 行为 |
|---|---|
| 404 / 405 | 网关不暴露 /v1/models——退回手动路径（`ccm add provider` / `ccm add model`），报错文案里写明 |
| 401 / 403 | 网关可达但鉴权失败（`provider reachable but authentication failed (...)`）——先 `ccm auth set <provider>` |
| 3xx 重定向 | **不跟随**——凭据不出配置主机；302 等按原状态报错（通常是 base_url 该更新） |
| 200 但响应体没有 `data` 数组 | 不是模型清单——报错原样引用网关原话（如 z.ai 对无效 key 返回 200 + `{"code":401,"msg":"token expired or incorrect"}`，鉴权失败不会伪装成"0 个模型"） |
| 凭据未配置 | 与 health 相同的凭据缺失提示（`CCM_<PROVIDER>_API_KEY` 或 `ccm auth set`） |

**诚实边界**：清单是便利，不是契约——列出的 id 真正请求时可能 400，没列出的模型也可能可用；注册后拿 `ccm health <模型>` 验证（见 3.4）。另外：provider 的 base_url 指向**另一个 ccm 代理**时，今天会得到 404 路径（ccm 代理不服务 /v1/models），此时按手动路径配置即可。

---

## 4. 日常使用

### 4.1 两种模式

| | 直连：`ccm run [target]` | 代理：`ccm proxy` + `ccm run --proxy` |
|---|---|---|
| claude 的 base URL | 模型 provider 的真实 `base_url` | `http://127.0.0.1:13521`（本地代理） |
| 凭据 | ccm 解析后以环境变量注入 claude | claude 只拿占位凭据，真实凭据由代理注入上游请求 |
| target 取值 | 模型名 / profile 名 | 模型名 / profile 名 / 路由名 |
| fallback / 重试 / 熔断 | 无 | 有 |
| 指标 / traces / decisions | 无 | `/_ccm/*` 全套 |
| 会话中切模型 | 需退出重启 claude | `ccm switch` 或 `/switch` 即时生效 |
| 改 config.toml 后 | 下次启动生效 | 每个请求都重载配置，免重启（活动 target 除外，见 FAQ） |

**代理模式是主推方式**：只有它有 fallback/熔断/观测，且能在不退出 claude 的情况下切换目标。直连模式适合临时、单模型、不想多开一个终端的场景。另外：**openai-compatible 类模型只能在代理模式下使用**——协议翻译发生在代理内部，直连没有翻译层（见 4.2 与 5.5）。

### 4.2 直连模式

```powershell
ccm run          # 使用 state.toml 里的持久默认
ccm run glm      # 指定模型/Profile，仅本次生效
```

ccm 启动 claude 时设置的环境变量：

| 环境变量 | 值 |
|---|---|
| `ANTHROPIC_BASE_URL` | provider 的 `base_url` |
| `ANTHROPIC_MODEL` | 模型的 `model_id` |
| `ANTHROPIC_API_KEY` | `x-api-key` 类 provider 的 key |
| `ANTHROPIC_AUTH_TOKEN` | `bearer` 类 provider 的 token |
| `CCM_PROXY_URL` | 被主动移除（防止残留的代理地址泄漏进直连会话） |

外部已存在的 `ANTHROPIC_API_KEY` / `ANTHROPIC_AUTH_TOKEN` 会先被清除、再注入 ccm 解析的凭据——claude 子进程只会用到 ccm 认可的那一个。直连会话也不携带 ccm 客户端标识：`CCM_CLIENT_ID` 被移除，父环境 `ANTHROPIC_CUSTOM_HEADERS` 里继承的 `x-ccm-client` 行会被剥掉（其余行保留）。stdio 直接继承，claude 的输出原样透传。Windows 上 claude 通过 `cmd /c claude` 启动，npm 的 `claude.cmd` shim 和原生 `claude.exe` 都兼容。

**openai-compatible 模型不支持直连**：直连模式没有翻译层，ccm 会在启动前直接拒绝并报错 `cannot launch ... directly: openai-compatible models are proxy-only ...`，不会把任何请求发给上游。openai 类模型请走代理模式（`ccm proxy` + `ccm run --proxy`）。

没选过默认目标时：`no current model selected; run `ccm use <name>` first`。直连模式下路由名不可用——解析口径只有模型/profile，传路由名会报 `unknown model/profile `...``。

### 4.3 代理模式

终端 A 启动代理（需要先有持久默认目标，否则报 `no current target selected; run `ccm use <name>` first`）：

```powershell
ccm proxy
```

```text
History: C:\Users\you\.ccm\history (decisions, metric snapshots, circuit transitions)
Clients: 0 persisted session(s) from C:\Users\you\.ccm\clients.toml
CCM proxy listening on http://127.0.0.1:13521
Claude Code base URL: http://127.0.0.1:13521
Runtime switch: `ccm switch <model-or-profile-or-route>`.
```

首行的 `History:` 只在 history 存储成功打开时出现（默认配置即有）；被 `[observability]` 关掉或单写者锁被另一个代理占用时，改为 stderr 一条警告、代理照常路由（见 6.3）。

终端 B 启动 claude：

```powershell
ccm run --proxy                # 用代理当前的目标（自动生成随机 client id，见 4.6）
ccm run --proxy coding-route   # 把本会话切到 coding-route 再启动（只作用于本会话，见 4.6）
ccm run --proxy --client dev1  # 显式指定 client id
```

claude 拿到的是占位配置：`ANTHROPIC_BASE_URL=<代理地址>`、`ANTHROPIC_MODEL=ccm`、`ANTHROPIC_AUTH_TOKEN=ccm-local-<client-id>`、`CCM_PROXY_URL=<代理地址>`，外加客户端标识 `CCM_CLIENT_ID=<client-id>` 和 `ANTHROPIC_CUSTOM_HEADERS`（内容为一行 `x-ccm-client: <client-id>`；父环境已有该变量时，ccm 保留其余行、只把 `x-ccm-client` 行替换成自己的）——真实路由与鉴权全部由运行中的代理完成。

client id 默认每次启动随机生成（8 位十六进制短 id），`--client` 或 `CCM_CLIENT_ID` 环境变量可显式指定。代理识别身份有两条等价通道：优先读 `x-ccm-client` 请求头（需要 claude ≥ 2.1.227），否则从 `ccm-local-<id>` 占位 token 里解析。

代理运行期间，每次路由尝试都会向 stderr 打一行日志（fallback 时追加 ` action=fallback`）：

```text
ccm route=coding-route attempt=1 model=glm result=HTTP 429 action=fallback
ccm route=coding-route attempt=2 model=minimax result=HTTP 200
```

`--bind` 可改监听地址，但**只允许回环地址**（默认 `127.0.0.1:13521`，`[::1]` 也可以；`0.0.0.0`、局域网 IP、`[::]` 一律拒绝），原因见 FAQ。

代理转发 `/v1/messages` 和 `/v1/messages/count_tokens`（v0.5 起转发后者）。v0.5 之前该端点返回 404——实测接近上下文窗口时（约 190k 估算 token）Claude Code 会重试 count_tokens 多达 17 次，这正是转发它的动机。count 请求按当前目标解析（含 scoped 客户端条目，读取无副作用——不刷新计数器与 `last_seen`），只发往**主模型**、单次尝试、不进 fallback，整个上游调用（响应头 + 响应体）受 10 秒超时约束；计数流量对决策、指标、用量统计完全不可见。openai-compatible 主模型没有计数端点，返回 404 Anthropic 错误信封（实测 Claude Code 容忍 404；不做本地估算）。部分网关的 count_tokens 是存根——实测 z.ai 对任意输入返回 `input_tokens:0`，代理原样透传，仅在大请求（>8KiB）收到 0 计数时向 stderr 打一次进程级警告。其余端点（`/v1/models` 等）仍不转发——实测 Claude Code 从不调用 `/v1/models`。

### 4.4 `use` vs `switch`（持久默认 vs 运行时切换）

| | `ccm use <target>` | `ccm switch <target>` |
|---|---|---|
| 改什么 | state.toml 的 `current`（持久默认） | 全局：运行中代理的内存目标；`--client <id>`：该客户端条目（v0.5 起持久化到 clients.toml，跨重启） |
| 是否要求代理在运行 | 否 | 是（否则 `failed to contact CCM proxy control API`） |
| 生效范围 | 之后的 `ccm run`、下次 `ccm proxy` 启动、`ccm doctor` 等 | 全局目标仅当前代理进程；scoped 客户端条目持久化（TTL 7 天） |
| 客户端作用域 | — | 默认全局；`--client <id>` 只切该客户端的运行时目标 |
| 输出 | `Selected {target} as persisted default` | 全局：`Runtime target switched to {target}`；客户端：`Runtime target for client {id} switched to {target}` |

两者都接受模型 / profile / 路由名（解析顺序：路由 → 模型 → profile）。`switch` 的代理地址解析顺序：`--proxy-url` flag > `CCM_PROXY_URL` 环境变量 > `http://127.0.0.1:13521`。

`switch` 的客户端作用域（v0.4；v0.5 起持久化）：id 解析顺序 `--client` > `CCM_CLIENT_ID` 环境变量 > 全局；`--global` 强制切全局（即使设了 `CCM_CLIENT_ID`）。ccm 代理模式启动的 claude 会话继承 `CCM_CLIENT_ID`，所以会话内 `/switch` 自动只切本会话（见 4.5 / 4.6）。从未被 scoped switch 过的客户端跟随全局目标；被切过的客户端条目**持久化到 `$CCM_HOME/clients.toml`**（v0.5 M2，只存 id / target / last_seen_ms），代理重启后自动恢复——重启时逐条对照当前 config 校验，目标已不存在的条目丢弃并打一条警告。全局切换仍然 runtime-only（不碰 state.toml）。client id 字符集 `[A-Za-z0-9._-]{1,64}`，非法值在发出请求前就报 `invalid client id ...: must be 1-64 characters of [A-Za-z0-9._-]`（与代理侧校验同一条消息）。

### 4.5 集成 Claude Code 的 `/switch`

```powershell
ccm integrate claude
```

```text
Installed Claude Code skill: /switch <model-or-profile-or-route>
Path: C:\Users\you\.claude\skills\switch\SKILL.md
The skill switches CCM's in-memory runtime target.
```

安装后，在**代理模式**的 claude 会话里直接输入：

```text
/switch fast-route
```

Claude 会执行 `ccm switch "fast-route"` 并回报当前目标。要点：

- 只切换当前代理的运行时目标，**不改持久默认**。
- 会话由 `ccm run --proxy` 启动时，skill 继承 `CCM_CLIENT_ID`，`/switch` 只切本会话的客户端目标，**不影响其他会话**（v0.4，见 4.6）。
- 需要代理在运行，否则报 `failed to contact CCM proxy control API`。
- skill 带 `disable-model-invocation: true` 和 `allowed-tools: ["Bash(ccm switch:*)"]`——只有你手动 `/switch` 才会触发，且只允许执行 `ccm switch` 命令。
- 安装时会顺带删除旧版命令文件 `~/.claude/commands/switch.md`（如果存在）。
- 卸载：`ccm integrate claude --remove` → `Removed Claude Code /switch integration`（文件已不存在也不报错）。

### 4.6 多个终端用不同模型（单代理多客户端）

v0.4 起一个代理就能按客户端分流：每个 `ccm run --proxy` 会话带一个 client id（默认每次启动随机生成 8 位十六进制短 id；`--client <id>` 或 `CCM_CLIENT_ID` 显式指定），代理为每个**被 scoped switch 过**的客户端维护独立的运行时目标，其余请求走全局目标。两个终端挂同一个代理、各用各的模型，互不干扰。

**标准用法**：

```powershell
# ── 终端 A：唯一的一个代理 ──
ccm proxy

# ── 终端 1：固定 id，先切到 glm 再启动 ──
ccm run --proxy glm --client term1

# ── 终端 2：不指定 id（自动随机）、自动切到 minimax，只作用于这个会话 ──
ccm run --proxy minimax

# ── 任意终端：运行中改 term1 的目标，不影响终端 2 和全局 ──
ccm switch fast-route --client term1

# ── 任意终端：切全局默认（未单独切过的客户端都跟着它） ──
ccm switch glm --global
```

要点：

- **id 解析顺序**：`ccm switch` 为 `--client` > `CCM_CLIENT_ID` 环境变量 > 全局，`--global` 强制全局；`ccm run --proxy` 为 `--client` > `CCM_CLIENT_ID` > 随机短 id。id 字符集 `[A-Za-z0-9._-]{1,64}`，非法值直接报错。
- **`ccm run --proxy <target>` 的预切换严格只作用于本会话的 client id**，永不改全局目标。
- **`/switch` 自动按会话隔离**：skill 里执行的就是 `ccm switch`，它继承该会话的 `CCM_CLIENT_ID`，所以只切本会话。
- **条目跨重启持久**（v0.5 M2）：scoped 条目落 `$CCM_HOME/clients.toml`（只存 id / target / last_seen_ms；`requests` 计数器重启归零——按会话记账看 `ccm history cost --client <id>`）。重启时逐条对照当前 config：目标已不存在的条目丢弃并打一条警告；超过 7 天没流量的条目按 TTL 丢弃；条目上限 256，超出按最久未见淘汰。`[clients] persist = false` 恢复 v0.4 的纯内存语义。
- **查看**：`ccm clients`（或 `GET /_ccm/clients`）列出各客户端的 target / 请求计数 / last_seen；`/_ccm/status`、`/_ccm/traces`、`/_ccm/decisions` 支持 `?client=<id>` 过滤。
- 熔断 / 指标 / decisions 仍是**模型级共享**的：某个上游模型挂了，对所有客户端一起生效（同一上游、同一凭据）。
- **client id 不是认证**：本地任何进程都能伪造任意 id（或裸用别人的 id），它与未鉴权的控制 API 同属回环信任域，不能当安全边界用。
- claude < 2.1.227 不支持 `ANTHROPIC_CUSTOM_HEADERS`，客户端身份走 `ccm-local-<id>` token 通道，效果等价（`ccm doctor` 会提示版本）。

**仍然可用：双代理双端口**（v0.3 的老办法，现在一般不再需要）——两个 `ccm proxy --bind 127.0.0.1:135xx` 各自独立的内存目标 / 熔断 / 指标，用 `--proxy-url` 区分。缺点依旧：电路、指标、decisions 全部割裂，还要占两个端口。

> **两个代理共用一个 `CCM_HOME` 的边界**（三个文件、三种答案）：`history/.lock` 是 OS 文件锁——第二个代理写不了历史，自动降级（路由不受影响）；`state.toml` 没有锁，稳态下代理不写它（写入者是 `ccm use` 和 `ccm init`；唯一的例外是 pre-v0.4 配置首次加载时的一次性 legacy 迁移——顶层 `current` 被搬进 `state.toml`，任何命令包括代理启动都可能触发这一次）；`clients.toml` 没有锁、**后写者赢**——两个代理都会持久化自己的条目快照，互相覆盖。要用双代理就用双端口 + 各自的 `CCM_HOME`，别共享。

**备选：直连模式**——两个终端各跑 `ccm run claude` / `ccm run glm`，独立进程独立环境变量。代价是没有 fallback / 熔断 / 观测，且 openai-compatible 模型不可直连（见 4.2）。

---

## 5. 路由策略怎么选

### 5.1 五种选择策略

`--selection` 决定每次请求在候选列表（primary + fallback）里怎么排序：

| 策略 | 行为 | 适合场景 |
|---|---|---|
| `ordered`（默认） | 严格按 primary → fallback 配置顺序 | 主模型优先、行为完全可预测 |
| `healthiest` | 成功率降序（尝试 < 3 次视为 1.0），并列时延迟 EWMA 低者优先 | 多个同等地位的模型，自动绕开最近出错的 |
| `lowest-latency` | 延迟 EWMA 升序；从未采样过的模型排最后 | 交互式对话，响应速度优先 |
| `lowest-cost` | 静态 `cost_weight` 升序 | 成本敏感、批量任务 |
| `weighted` | 加权综合分降序 | 均衡策略，权重可调，适合作为常驻默认 |

CLI 别名：`lowest_latency` / `latency`、`lowest_cost` / `cost`，大小写不敏感。

### 5.2 创建路由与默认参数

```powershell
ccm add route balanced --primary glm --fallback minimax,claude --selection weighted
```

（`--fallback` 是逗号分隔的模型名列表，可省略。）全部参数及默认值——**这些默认值同时是手写 TOML 路由省略 `[policy]` 时的语义**：

| Flag | 默认值 | 含义 |
|---|---|---|
| `--primary` | （必填，省略则交互提示） | 首选模型 |
| `--fallback` | 空 | 后备模型，逗号分隔 |
| `--selection` | `ordered` | 选择策略 |
| `--reliability-weight` | `0.4` | weighted 的可靠性权重 |
| `--latency-weight` | `0.2` | weighted 的延迟权重 |
| `--cost-weight` | `0.2` | weighted 的成本权重 |
| `--quality-weight` | `0.2` | weighted 的质量权重 |
| `--header-timeout-ms` | `30000` | 等待上游响应头的超时 |
| `--fallback-on` | `429,502,503,504` | 触发 fallback 的 HTTP 状态码 |
| `--max-attempts` | `3` | 每个请求的最大尝试次数 |
| `--backoff-ms` | `200` | 尝试之间的退避间隔 |
| `--circuit-enabled` | `true` | 熔断开关（见下方注意） |
| `--failure-threshold` | `3` | 连续失败多少次熔断 |
| `--circuit-open-ms` | `30000` | 熔断冷却时长 |

注意：`--circuit-enabled` 走的是"显式传值"语义，**不是普通开关**——关闭要写 `--circuit-enabled false`，只写 `--circuit-enabled` 不带值不行。

weighted 综合分公式（权重归一化后加权平均，权重和 <= 0 时报配置错误）：

```text
weighted_score = (w_r×reliability + w_l×latency + w_c×cost + w_q×quality) / (w_r+w_l+w_c+w_q)

reliability = 成功率（尝试 < 3 次时视为 1.0）
latency     = 1 / (1 + latency_ewma_ms/1000)（无样本时 0.5）
cost        = 1 / (1 + max(cost_weight, 0))（静态，来自模型元数据）
quality     = quality_weight，截断到 [0,1]
```

### 5.3 模型的 cost / quality 权重与定价表

`ccm add model` 的 `--cost-weight` / `--quality-weight`（默认各 `1.0`）：

- `cost_weight` 是**相对成本**，越小越便宜：`lowest-cost` 直接按它升序，`weighted` 里它得分更高。样例配置给 glm 设 0.25、claude 设 1.0，即"glm 约便宜 4 倍"。
- `quality_weight` 是**相对质量**（0~1），只影响 `weighted` 策略的质量分量。

两者都只是**路由元数据**，和钱无关。想看手填的权重离真实花费多远：`ccm advise`（见 6.6）。要算真实花费，给模型加一张手填的每百万 token 定价表（v0.4 M6，直接编辑 `config.toml`；`ccm discover` 注册的模型会顺手打印这张表的注释骨架，见 3.5）：

```toml
[models.glm.pricing]
input = 3.0        # 每百万输入 token 的美元单价
output = 15.0      # 每百万输出 token
cache_read = 0.3   # 每百万缓存命中（读）token
cache_write = 3.75 # 每百万缓存写入 token
```

费用按 `Σ tokens/1e6 × 单价` 计算，单价快照随记录落盘（改价不影响已记的账）。没填定价表的模型按"无价"记账——`cost_usd` 是 `null`，**绝不猜一个数**；聚合视图里单独计数（见 6.4）。

### 5.4 熔断与 fallback 行为

**fallback 触发条件**（满足其一即尝试下一候选）：

1. 上游 HTTP 状态码在 `fallback_on` 里（默认 `429,502,503,504`；**500、400 默认不触发**——500 会被原样透传给客户端，见下）；
2. 响应头超时（`header_timeout_ms`）；
3. 请求/连接错误。

fallback 还要求同时满足"还有下一个候选 **且** 还有剩余尝试预算（`max_attempts`）"。预算耗尽仍全失败时，代理返回 502：`CCM proxy error: all available route candidates failed for ...`。若最后一次可用尝试遇到 fallback 状态码，上游的错误响应会原样透传而不是报错。

**尝试预算与退避**：默认最多 3 次尝试，候选之间退避 200 ms。**被熔断跳过的候选不消耗尝试预算**。

**熔断状态机**（`/_ccm/circuits` 可查）：

| 状态 | 含义 |
|---|---|
| `CLOSED` | 正常放行 |
| `OPEN` | 冷却期内（`circuit_open_ms`，默认 30 s），该模型被跳过：`skipped: circuit OPEN until {until}` |
| `HALF_OPEN_READY` | 冷却已结束、还没有探测请求——下一个请求会成为**唯一**被放行的探测 |
| `HALF_OPEN` | 冷却已结束且探测请求在途，其余请求跳过：`skipped: HALF_OPEN probe already in flight` |

- 连续失败达到 `failure_threshold`（默认 3）→ OPEN；
- 冷却结束后放行恰好一个探测：探测成功 → 完全重置回 CLOSED；探测失败 → 重新 OPEN；
- 熔断跳过**不打上游、不占 `max_attempts` 预算**，直接换下一候选；
- `--circuit-enabled false` 时状态恒为 CLOSED，成败不再更新熔断。

一个典型时间线（实测）：glm 连续 503 → 熔断 OPEN → 后续请求跳过 glm、直接走 minimax（无上游调用）→ 30 s 冷却后第一个请求作为探测打到 glm（HALF_OPEN）→ 成功 → CLOSED。

**陷阱**：不在 `fallback_on` 里的状态码（包括 500）按"终态成功"处理——响应透传给客户端，且会**重置熔断计数**。metrics 里仍计为 http_errors（非 2xx），429 另计 rate_limited，但熔断视角它是"成功"。想让 500 也触发 fallback，加进 `--fallback-on` 即可。

### 5.5 OpenAI 兼容网关（openai-compatible）

v0.4 起 provider 有第三种 kind：`openai-compatible`，面向只提供 OpenAI `chat/completions` 协议的上游（DeepSeek 及各类 OpenAI 兼容网关）。客户端侧（Claude Code）仍然是 Anthropic `/v1/messages` 协议——ccm 在代理内部做双向协议翻译，对 Claude Code 完全透明。

以 DeepSeek 为例：

```powershell
ccm auth set deepseek
ccm add provider deepseek --base-url https://api.deepseek.com --kind openai-compatible
ccm add model deepseek-chat --provider deepseek --model-id deepseek-chat --cost-weight 0.1 --quality-weight 0.8
ccm use deepseek-chat
```

- `--kind` 也接受别名 `openai`；
- 省略 `--auth` 时按 kind 取默认：openai-compatible → `bearer`（`Authorization: Bearer ...`）；anthropic 类仍是 `x-api-key`（显式传 `--auth` 永远优先）；
- 上游地址固定是 `base_url + /v1/chat/completions`，所以 `base_url` 填网关根地址即可（末尾 `/` 会被去掉）；
- **仅代理模式可用**：协议翻译在代理内部完成，直连模式（`ccm run <模型>` 不带 `--proxy`）会被 ccm 在启动前直接拒绝（报错见 4.2）——openai 类模型请配 `ccm proxy` + `ccm run --proxy`。

**混合协议路由**是合法的：一条 fallback 链里同时有 anthropic 类和 openai 类候选，fallback 按状态码判断，与协议无关。例：

```toml
[routes.budget-route]
primary = "deepseek-chat"
fallback = ["glm"]
```

校验到混合协议路由时 ccm 会打一条**非阻断**警告（每条路由每进程一次），提醒 openai 候选上会丢 prompt cache 和 extended thinking。

**明确的降级清单**（只影响 openai-compatible 候选；anthropic 类候选不受影响）：

| 项 | 行为 |
|---|---|
| prompt cache | `cache_control` 被丢弃，无 prompt-cache 收益（详见 FAQ"为什么经 openai 网关没有缓存折扣"） |
| extended thinking | `thinking` 字段整体丢弃，无扩展思考；`thinking` 内容块也不回传 |
| 长度/预算参数 | 只透传 `max_tokens`（`budget_tokens` 随 thinking 丢弃）；`temperature` / `top_p` 照常透传，`top_k` 丢弃，`stop_sequences` 截断为 4 条（OpenAI 上限） |
| 图像 / 不可映射内容块 | 仅 base64 source 的 `image` 块映射为 `image_url`；URL source 等其他形式丢弃。若整条消息没有任何可映射块，降级为一条占位文本消息（`[ccm: unmappable message content dropped ...]`），不会整条消失 |
| 推理内容 | 上游 `reasoning_content`（DeepSeek R 系风格）被丢弃，不会回传给客户端 |
| usage / 计费 | 上报的缓存命中 tokens（`cached_tokens` / `prompt_cache_hit_tokens`）翻译为 `cache_read_input_tokens` 并从 `input_tokens` 中扣除；`cache_creation_input_tokens` 无 OpenAI 对应物，不回填 |

错误与流式的语义与 anthropic 类一致：上游错误体翻译成 Anthropic 错误信封（状态码保留）；上游也可能在 200 流式响应**中途**发一个 error 帧（OpenAI 过载、Azure 内容过滤、OneAPI 类聚合器常见）——ccm 同样把它翻译成 Anthropic `error` 事件终止流，绝不伪装成正常完成。已开始流式返回后翻译失败（如上游断流、坏帧），同样在已提交的流上发一个 `error` 事件然后结束响应体——**不会中途换模型**（v0.3 的"不中途切换"不变量继续生效）。

---

## 6. 观察与诊断

| 工具 | 一句话 |
|---|---|
| `ccm doctor` | 本地体检：PATH、目标解析、配置链、凭据在不在、端点通不通（见 3.4） |
| `ccm health <model-or-profile>` | 发真实请求验证"鉴权 + 模型名"是否可用 |
| `ccm discover [provider]` | 拉取网关 `/v1/models` 模型清单并勾选注册（见 3.5；skip 不覆盖已有模型） |
| `ccm history decisions\|metrics\|circuit\|cost` | 离线查看持久化的决策 / 指标快照 / 熔断转换 / 按天成本（见 6.3、6.4，代理停着也能查） |
| `ccm advise` | 从已落盘的使用量重算 `cost_weight` 建议并打印（只读不写配置，见 6.6） |
| `/_ccm/*` 控制接口 | 代理运行时的实时观测与切换（本节） |

### 6.1 控制接口一览（默认 `http://127.0.0.1:13521`）

| 端点 | 用途 / 返回要点 |
|---|---|
| `GET /health` | 存活检查，返回字面量 `ok` |
| `GET /metrics` | Prometheus 文本格式导出（v0.4 M7，见 6.5）：与全部 `/_ccm/*` 共用同一个仅回环的监听端口，`[observability] prometheus_enabled = false` 时该路由不存在（404） |
| `GET /_ccm/status` | 当前目标的解析结果：primary、model_id、provider、`kind`（与 provider 平级的顶层字段）、fallback 列表、完整 policy；`?client=<id>` 查询该客户端的生效目标（未切过的客户端会标注跟随全局） |
| `GET /_ccm/models` | 模型清单：model_id、provider、`kind`（`anthropic` / `anthropic-compatible` / `openai-compatible`；模型引用了未配置的 provider 时为 `unknown`）、cost_weight、quality_weight |
| `GET /_ccm/routes` | 路由清单 + `active` 标记（哪条是当前内存目标） |
| `GET /_ccm/traces` | 最近 100 条请求尝试记录；`?client=<id>` 只看该客户端 |
| `GET /_ccm/circuits` | 各模型熔断状态（CLOSED / OPEN / HALF_OPEN / HALF_OPEN_READY） |
| `GET /_ccm/metrics` | 各模型指标：attempts、successes、success_rate、health_score、http_errors、fallback_failures、timeouts、request_errors、rate_limited、latency_ewma_ms、last_success_ms、last_failure_ms |
| `GET /_ccm/scores` | 当前路由候选的打分明细（reliability/latency/cost/quality/weighted，按加权分降序） |
| `GET /_ccm/decisions` | 最近 100 条路由决策（见下）；`?client=<id>` 只看该客户端；`?since=&until=&model=`（unix-ms，含边界）转为读取磁盘上的持久化历史（见 6.3）；`?limit=` 限制返回条数、保留最新 N（内存/磁盘两条路径都生效，磁盘查询缺省 1000），无运行中的 history 存储时该组合返回 400 |
| `GET /_ccm/usage` | 最近 100 条使用量记录（见 6.4）；过滤参数与 `/_ccm/decisions` 完全一致（`?client=` / `?since=&until=&model=` / `?limit=`，无 history 存储时过滤查询同样 400） |
| `GET /_ccm/cost` | 按 UTC 日聚合的使用量与成本（见 6.4）：`?day=YYYY-MM-DD`（缺省今天），`?client=<id>` 缩小到该客户端；始终读磁盘历史，无 history 存储时 400 |
| `GET /_ccm/clients` | 各客户端的运行时目标条目：client、target、requests、last_seen_ms（按 client 排序；条目由 scoped switch 产生，v0.5 起持久化到 clients.toml 跨重启恢复，requests 计数重启归零） |
| `POST /_ccm/switch/{target}` | 运行时切换（`ccm switch` 即调它；未知目标返回 400）；`?client=<id>` 只切该客户端，id 非法返回 400 |
| `POST /v1/messages` | 反向代理本体，Claude Code 的流量入口；非 POST 返回 405 `POST required` |

```powershell
Invoke-RestMethod http://127.0.0.1:13521/_ccm/status
Invoke-RestMethod http://127.0.0.1:13521/_ccm/circuits
Invoke-RestMethod http://127.0.0.1:13521/_ccm/metrics
```

```sh
curl -s http://127.0.0.1:13521/_ccm/decisions
```

traces 是进程内环形缓冲，保留**最近 100 条**，重启代理即清零；metrics/circuits 同样只活在进程内。decisions 的无参数视图同样在内存里，但 v0.4 起每条决策同时持久化到磁盘（见 6.3），带时间/模型过滤的查询走磁盘历史（默认返回最新 1000 条，`?limit=` 可调）。

### 6.2 怎么读 `/_ccm/decisions`

每条决策记录一次请求的路由过程，关注四个层次：

1. `selection` + `ranked_candidates`：这次按什么策略、候选打分排序如何（`ordered` 时排序即配置顺序）；
2. `attempts`：实际尝试序列，每项含 model、当时的熔断状态、result、是否 fallback。`result` 的取值包括 `HTTP {status}`、`timeout after {ms}ms`、`request error: {err}`、`skipped: circuit OPEN until {until}`、`skipped: HALF_OPEN probe already in flight`；
3. `selected`：最终承接下来的模型；全部候选失败且无可透传响应时，代理返回 502。**例外**（见 5.4）：最后一次可用尝试遇到 `fallback_on` 状态码时，上游的错误响应会原样透传给客户端（openai 类经翻译），此时 `selected` 已设置、`outcome` 形如 `HTTP 429`——看起来像成功，实为透传的失败；
4. `outcome`：结果概述——成功是 `HTTP {status}`；请求前解析失败是 `resolve error: ...`（该请求整体 502）；全部候选失败是 `failed: {候选: 原因; ...}`，或没有候选被放行 / 预算耗尽时是 `no candidate admitted or attempt budget exhausted`；上述第 3 条的透传例外下同样是 `HTTP {status}`。

一个"熔断跳过 + fallback 成功"的决策示例（结构示意，个别字段名以实际响应为准）：

```json
{
  "id": 12,
  "timestamp_ms": 1790985600000,
  "target": "coding-route",
  "selection": "ordered",
  "configured_candidates": ["glm", "minimax"],
  "ranked_candidates": [
    {
      "model": "glm",
      "reliability": 1.0,
      "latency": 0.83,
      "cost": 0.8,
      "quality": 0.85,
      "weighted": 0.896,
      "attempts": 8,
      "latency_ewma_ms": 204
    }
  ],
  "attempts": [
    { "model": "glm", "circuit": "OPEN", "result": "skipped: circuit OPEN until 1790985630000", "fallback": true },
    { "model": "minimax", "circuit": "CLOSED", "result": "HTTP 200", "fallback": false }
  ],
  "selected": "minimax",
  "outcome": "..."
}
```

读法：glm 熔断中，被跳过（没打上游、不占尝试预算）→ 立刻落到 minimax → 成功。配合 stderr 的 `ccm route=... attempt=... result=...` 日志可以实时看到同样的信息。

### 6.3 历史持久化（v0.4）

代理运行时把四类记录追加写入 `$CCM_HOME/history/`（默认 `~/.ccm/history/`）：

| 文件 | 内容 |
|---|---|
| `decisions.jsonl` | 每条路由决策（与 `/_ccm/decisions` 记录同结构） |
| `metrics.jsonl` | 全模型指标快照，默认每 30 秒一条（原始计数，比率在读取时计算） |
| `circuit.jsonl` | 熔断状态转换（CLOSED / OPEN / HALF_OPEN + 原因） |
| `usage.jsonl` | 每个被接受的 2xx 响应一条使用量记录（见 6.4；`decision_id` 关联到 decisions.jsonl） |

行为要点：

- **轮转与保留**：单文件满 5 万条或 8 MiB 即轮转成 `<kind>-<unix_ms>.jsonl`，已轮转文件默认保留 14 天（按修改时间清理；活跃文件与 `.lock` 永不清理）。单条记录大到连空文件都装不下（`max_bytes_per_file` 配得比一条记录还小）时，该条直接丢弃并计数（见下），不会为它轮转出超限文件。
- **绝不阻塞请求**：写入走有界队列，队列满就丢弃并计数（stderr 提示一次）；磁盘 IO 失败也只是停写历史，路由不受影响。
- **退出语义**：代理正常退出时排空队列、落盘最后一批记录并汇总丢弃计数；被强杀（Ctrl+C）则没有最终落盘，依赖周期性写入和 torn-tail 容忍。
- **单写者规则**：同一 `CCM_HOME` 只允许一个代理写历史（目录里的 OS 文件锁）。第二个代理照常路由，只是历史禁用并打印一条警告。
- **重启语义**：decision id 跨重启连续（启动时从磁盘尾部恢复）；运行时指标/熔断状态从零开始——过期数据不参与 healthiest/weighted 排序，历史查询读磁盘。
- **无凭据保证**：记录是白名单 serde 结构，测试断言落盘内容不含任何凭据子串。
- 崩溃残留的半行（torn tail）读取时自动跳过。

查询方式（两者都支持 `--model` / `--client` / 时间过滤；CLI 无参数查询不受影响）：

```powershell
# CLI 离线查询（代理停着也能用）
ccm history decisions --model glm --limit 20
ccm history decisions --since 1790985600000 --until 1791071999999 --client term1
ccm history metrics            # 最新一次快照（--limit 5 看最近 5 次）
ccm history circuit --model glm
ccm history cost               # 今天（UTC）按模型聚合的花费（--day 2026-10-03 指定日）

# 控制接口（代理运行中；unix-ms 含边界；与 ?client= 可组合）
Invoke-RestMethod "http://127.0.0.1:13521/_ccm/decisions?since=1790985600000&model=glm"
Invoke-RestMethod "http://127.0.0.1:13521/_ccm/cost?day=2026-10-03&client=term1"
```

HTTP 磁盘查询默认返回最新 **1000 条**（`?limit=` 可调）——查询按最新优先扫描文件、凑够条数即停，读取代价随 limit 而不是历史总量增长，运行中的代理不会被一次全量反序列化拖住；CLI 的 `--limit` 默认不设上限（N ≥ 1，传 `0` 会被直接拒绝——空选择几乎总是失误），离线分析不受影响。

`[observability]` 配置节（全部有默认值，v0.4 之前的配置文件不用改；任一阈值填 0 会在加载时报错）：

```toml
[observability]
history_enabled = true            # 关掉后代理不写历史，带过滤参数的查询返回 400
retention_days = 14
max_records_per_file = 50000
max_bytes_per_file = 8388608      # 8 MiB
metrics_snapshot_interval_secs = 30
prometheus_enabled = true         # 关掉后 /metrics 路由整个不存在（404）

# [observability.otlp]            # v0.5：OTLP/HTTP JSON 主动推送（节缺省 = 关闭，见 6.7）
# endpoint = "http://localhost:4318"
# interval_secs = 30
```

`ccm doctor` 增加一行历史目录状态：`✓ history: N file(s) at <dir>`，目录为空或已禁用时是 `!` 提示（非致命）。

### 6.4 使用量捕获与成本核算（v0.4 M6）

代理在每个**被接受的 2xx 响应体**外面包一层增量扫描器：字节流原样透传（不缓冲整个流），边流边认出 Anthropic 的 usage 帧，流结束时落一条使用量记录。关键语义：

- **一条决策一条记录**：记录的 `decision_id` 关联 `decisions.jsonl` 里的决策，`model` 是别名（账目按别名键），`client` 有会话 id 时带上——按会话算成本是免费的。
- **token 合并**：流式响应里 `message_start` 带输入/缓存 token、`message_delta` 带输出 token，逐字段合并、后到者覆盖。翻译流（openai-compatible）的 `message_start` 是诚实的零占位，真实数字随最后一个 delta 到达。`message_start` 没有 usage 字段（v0.3 的 mock 就这样）也容忍，字段保持 0 直到后续帧补上。
- **`complete: false` 的三种情况**：客户端中途断开（记录照出，已见 token 保留）、传输错误、或上游在 200 流里发过 `error` 帧——最后一种补上了 v0.4 的一个盲区：指标在响应头时刻就把流式失败计成了成功，扫描器在这里把它如实记为不完整。
- **错误透传不记账**：终态 429/5xx 透传（无"被接受的响应体"）不产生使用量记录。
- **扫描有界**：单行超过 64 KiB（如 base64 工具输出）整行丢弃、下一行重新同步；非流式 JSON 体积超 16 MiB 按 0 token 诚实记账，不无限耗内存。

成本查询：

```powershell
# 代理运行中：按 UTC 日聚合（缺省 ?day= 是今天）
Invoke-RestMethod "http://127.0.0.1:13521/_ccm/cost"           # 全局
Invoke-RestMethod "http://127.0.0.1:13521/_ccm/cost?client=term1" # 某个会话

# 代理停了也能查（离线读 usage.jsonl）
ccm history cost --day 2026-10-03
```

`cost` 视图按模型给出 requests / complete / 四类 token / `cost_usd`（模型名排序），外加合计与**无价请求数**（没配 `[pricing]` 表的模型——它们的成本未知而非零，合计不会虚报）。`/_ccm/usage` 则是逐条记录视图，参数语义与 `/_ccm/decisions` 完全一致。

> 边界声明：这是**代理侧计量**，不是账单真相。token 数来自上游上报（native 流或翻译流），单价是手填的；只能用于相对比较与异常发现（比如某会话今天烧了 10 倍于平时的 input token），不能对账。

### 6.5 Prometheus `/metrics`（v0.4 M7）

`GET /metrics`（默认 `http://127.0.0.1:13521/metrics`）输出 Prometheus 文本格式（`text/plain; version=0.0.4`），可以直接喂给 Prometheus / Grafana / VictoriaMetrics 抓取：

```powershell
Invoke-WebRequest http://127.0.0.1:13521/metrics
```

```sh
curl -s http://127.0.0.1:13521/metrics | grep ccm_
```

指标族（全部 `ccm_` 前缀）：

| 指标 | 类型 / 标签 | 含义 |
|---|---|---|
| `ccm_up` | gauge | 代理在服务即 1 |
| `ccm_process_start_time_seconds` | gauge | 代理开始监听的时间（用于算 uptime / 重启告警） |
| `ccm_proxy_requests_total` | counter `{target,outcome}` | 每个请求的最终裁决；`outcome` 只有 `success`（被接受的 2xx）和 `error` 两种 |
| `ccm_attempts_total` | counter `{model,outcome}` | 每次上游尝试；五个互斥 outcome（success / http_error / rate_limited / timeout / request_error），已结算的尝试加和 = attempts（进行中或中途夭折的等待只体现在 `/_ccm/metrics` 的 attempts 里） |
| `ccm_header_latency_seconds` | histogram `{model}` | 响应头延迟，固定桶 5ms…10s |
| `ccm_latency_ewma_ms` | gauge `{model}` | 进程内 EWMA（首条采样前不出线） |
| `ccm_decision_duration_seconds` | histogram `{target}` | 一次请求从到达到终态裁决的全程耗时（含 fallback 链） |
| `ccm_circuit_open` | gauge `{model}` | 熔断正在跳过该模型时为 1（OPEN 冷却中或 HALF_OPEN 探测占用中） |
| `ccm_circuit_consecutive_failures` | gauge `{model}` | 熔断当前连续失败计数 |
| `ccm_tokens_total` | counter `{model,kind}` | 被接受响应上报的 token（kind：input / output / cache_read / cache_write；零值 kind 不出线） |
| `ccm_cost_micro_usd_total` | counter `{model}` | 累计成本，整数微美元（没配定价表的模型不出线——成本未知而非零） |
| `ccm_history_dropped_total` | counter | 历史丢弃的记录数（写入队列满/写者已停，或单条超过 `max_bytes_per_file` 被丢弃） |

要点与边界：

- **同端口、仅回环**：`/metrics` 与 `/_ccm/*`、`/v1/messages` 共用同一个监听器，回环绑定保护对它同样生效——没有第二个端口。想彻底关掉导出：`[observability] prometheus_enabled = false`。
- **双轨延迟**：直方图（固定桶）让 Prometheus 侧能对任意抓取窗口做 `histogram_quantile`（含跨代理重启，每次抓取导出的是累计桶）；EWMA gauge 保留给不接 Prometheus 时的快速浏览。
- **与 `/_ccm/metrics` 同源**：`ccm_attempts_total` 渲染时从同一份每模型计数推导（429 只记进 `rate_limited`，不重复计入 `http_error`），两个观测面不会打架。
- **重启清零**：所有运行时计数器随代理重启归零（counter 语义，Prometheus 侧靠 `increase()`/`rate()` 自然处理）；跨天的账要查 6.3/6.4 的磁盘历史。
- 成本/token 的口径与 6.4 完全一致：只统计被接受的 2xx 响应，代理侧计量、手填单价，不能对账。

### 6.6 `ccm advise`：cost_weight 建议（v0.5 M1）

`cost_weight` 是手填的先验（5.3），v0.4 M6 又已把每请求四类 token 落了盘——`ccm advise` 把两边接起来：按**当前**定价表对窗口内使用量重算每模型真实 USD/请求，归一化成建议权重。**只打印，绝不写配置**（TOML 片段是给你粘贴的）。

```powershell
ccm advise                     # 最近 7 天，min-samples 20
ccm advise --window 30         # 30 天窗口
ccm advise --min-samples 50    # 更保守的样本门槛
ccm advise --model glm         # 只看一个模型
```

输出示例（合成数据）：

```text
== ccm advise — cost_weight suggestions from realized spend
   window: last 7 day(s) (since 2026-10-02T12:37:22Z), min-samples: 20, prices: current [models.<name>.pricing] tables
model                 reqs incomplete analyzed    usd/req usd/1ktok weight suggest  score  status
claude                   6          0        0          -         -  1.000       -      -  cost unknown — never guessed (add a [models.claude.pricing] table)
glm                     43          3       40   0.034930  0.000406  0.250   1.000  0.500
m3                      25          0       25   0.022080  0.001082  1.000   0.632  0.613
anchor: glm — highest realized usd/req among analyzed candidates, maps to 1.0
# suggested cost_weight values — paste into config.toml (ccm advise never writes config)

[models.glm.routing]
cost_weight = 1.000

[models.m3.routing]
cost_weight = 0.632
（后接 caveats，见下）
```

规则：

- **锚点归一化**：窗口内样本达标的已定价模型中，USD/请求最高的那个 → 建议 1.0（与样例配置"claude=1.0 最贵"的惯例一致），其余按相对倍数缩放。`score` 列是建议值代入 `1/(1+w)` 后的成本分（从**取整后的建议值**算——你实际会填进去的就是那个数）。
- **重算而非重放**：费用按**当前** `[models.<name>.pricing]` 从 token 重算，不用记录里内嵌的价格快照——中途改过价不会把新旧价位混在一起（改价后重跑 advise 即得新价位下的建议；usage 记录本身不受影响，见 6.4）。
- **窗口的上限是保留期**：已轮转的历史文件按 `[observability] retention_days`（默认 14 天，见 6.3）清理，窗口比它宽时折算只能看到还在磁盘上的部分——此时报告会打一行 note 明说，不会默默按窄窗口充数。
- **三种行状态**：正常分析（给建议）；样本不足（`insufficient — unchanged`——贵但稀的模型不构成证据，也不能当锚点）；未定价（`cost unknown — never guessed`，并注明要补的 `[models.<name>.pricing]` 表；模型已从 config 删掉的会标注 `model not in config.toml`）。
- **不完整记录**（客户端断连 / 传输错误 / 上游 error 帧）计数但不进折算——token 可能是半截的。
- 全部模型都实现 $0/请求时（比如全 0 单价），所有建议为 0.0（一样免费），不会除零。

边界（每份报告都会原样打印）：per-token 数字混合各家上游自己的 tokenizer，**跨协议家族不可比**；usage 只记实际被路由到的流量，路由从没选过的模型在这里没有数据（选择偏差）；这是按手填单价做的代理侧计量，**不是账单真相**；`quality_weight` 保持手工——代理可见信号里没有诚实的推导路径。

### 6.7 OTLP 推送导出（v0.5 M4）

不想为一台代理单独跑 Prometheus 抓取时，可以让 ccm 主动把同一套指标推给任何 OTLP/HTTP JSON 收集器（otelcol、Grafana Alloy、SigNoz 等，默认接收端口 4318）：

```toml
[observability.otlp]
endpoint = "http://localhost:4318"   # 收集器地址；/v1/metrics 由 ccm 拼接（恰好一次）
interval_secs = 30                    # 推送间隔，默认 30
```

- **同一份状态、两个出口**：推送循环与 `/metrics`（6.5）渲染完全相同的 12 个指标族快照，两个观测面不会打架。`[observability.otlp]` 与 `prometheus_enabled` 相互独立——只开推送时 `/metrics` 路由仍然关闭（404），只开抓取时没有任何推送。
- **口径与边界同 6.5**：token/成本只统计被接受的 2xx 响应、代理侧计量、不能对账；count_tokens 流量对所有指标族不可见（SRE 面板会低估请求数）。直方图按 OTLP 规范输出 11 个秒制桶界 + 12 个桶增量，计数器为 CUMULATIVE 累计语义（累计值随推送全量重发，重启归零由收集器侧按 counter 语义处理）。
- **失败隔离**：每次推送都是全量重发，失败的 POST **不重试**——只按失败连击向 stderr 打一条警告，下个周期自然补齐；收集器挂掉完全不影响代理路由。推送客户端自带 3s 连接 / 5s 总超时，卡住的收集器不会堆积卡住的任务。
- **凭据边界**：负载里只有指标名、标签值和计数。收集器若要认证头，用标准环境变量 `OTEL_EXPORTER_OTLP_HEADERS`（`k1=v1,k2=v2` 形式，代理启动时读取一次）——**永不写入 config.toml、永不打印、永不进负载**。
- `service.instance.id` 是 `CCM_HOME` 路径的稳定 16 位哈希：多个 ccm 实例推给同一收集器时可以区分，且哈希不泄漏原始路径。

---

## 7. 凭据与环境

### 7.1 凭据解析顺序

对任何 provider：**环境变量优先（设了且非空即用），否则查系统 keyring**。两者都没有时报：

```text
no credential found for provider `{provider}`; set {env_name} or run `ccm auth set {provider}`
```

keyring 条目：service 名固定为 `ccm`，条目名 = provider 名。Windows 上位于凭据管理器，形如 `LegacyGeneric:target=zai.ccm`。

### 7.2 环境变量命名规则

`CCM_` + provider 名大写（所有非字母数字字符替换为 `_`）+ `_API_KEY`：

| provider 名 | 环境变量 |
|---|---|
| `zai` | `CCM_ZAI_API_KEY` |
| `minimax` | `CCM_MINIMAX_API_KEY` |
| `anthropic` | `CCM_ANTHROPIC_API_KEY` |
| `z-ai.test` | `CCM_Z_AI_TEST_API_KEY` |

`ccm doctor` 的凭据行会直接提示当前 provider 对应的变量名。

### 7.3 其他环境变量

| 变量 | 作用 |
|---|---|
| `CCM_HOME` | 重定位 `config.toml` 和 `state.toml`（默认 `~/.ccm`） |
| `CCM_PROXY_URL` | `ccm switch` / `ccm clients` 的默认代理地址（`--proxy-url` flag 可覆盖；最终默认 `http://127.0.0.1:13521`。注意 `ccm run --proxy` 不读它，只认 `--proxy-url` flag）；代理模式下会传给 claude 子进程 |
| `CCM_CLIENT_ID` | `ccm switch` / `ccm run --proxy` 的默认 client id（`--client` flag 可覆盖；`switch` 里 `--global` 优先于它）；代理模式下会传给 claude 子进程，会话内 `/switch` 靠它保持本会话作用域（见 4.6） |
| `OTEL_EXPORTER_OTLP_HEADERS` | OTLP 推送的收集器认证头（`k1=v1,k2=v2`，仅 `[observability.otlp]` 开启时读取；见 6.7。只认环境变量——凭据永不入 config.toml） |

### 7.4 无头场景（CI / 容器）

`ccm auth set` 是交互式（隐藏输入），且 keyring 依赖系统凭据服务——Linux 的 keyring 路径只验证了编译（WSL2 无 Secret Service，存储未实测），桌面 Linux 上的实际可用性未验证。无头/WSL 环境建议直接用环境变量：

```powershell
$env:CCM_ZAI_API_KEY = "sk-..."
```

```sh
export CCM_ZAI_API_KEY="sk-..."
```

---

## 8. FAQ / 故障排查

**为什么 config.toml 里找不到 API key？**
设计如此。key 只存系统凭据管理器或环境变量，永不入文件（`ccm auth set`）。这不是丢失，是隔离。

**装完后 `ccm` 提示找不到命令？**
`install.ps1` 修改的是用户 PATH，**需要新开一个终端**。仍不行就检查 `%LOCALAPPDATA%\Programs\ccm` 是否在 PATH 里；手动安装的话把 `target\release\ccm.exe` 所在目录加进 PATH。

**doctor 报 `✗ Claude Code: not found on PATH`？**
Windows 上 ccm（包括 doctor 的检测）通过 `cmd /c claude` 调用 claude，npm 的 `claude.cmd` shim 和原生 `claude.exe` 都兼容——前提是同一终端里 `claude --version` 真的能跑。启动失败时的报错是 `failed to launch `claude`; make sure Claude Code is installed and available on PATH`。

**启动 claude 时警告 `Both ANTHROPIC_AUTH_TOKEN and ANTHROPIC_API_KEY set`，或流量路由到了错误的网关？**
Claude Code 的 `~/.claude/settings.json`（和 `settings.local.json`）里的 `env` 块会在 ccm 注入的环境变量**之后**叠加、优先级更高。其中 `ANTHROPIC_BASE_URL` / `ANTHROPIC_API_KEY` / `ANTHROPIC_AUTH_TOKEN` / `ANTHROPIC_CUSTOM_HEADERS` 会静默覆盖 ccm 的注入——ccm 只能清理进程环境变量（`launcher.rs` 启动时 `env_remove`），管不到 claude 自己的配置层。症状就是上述警告，更隐蔽的症状是 `ccm run <目标>` 实际连的仍是 settings.json 里配置的网关；`ANTHROPIC_CUSTOM_HEADERS` 被覆盖时则会顶掉 ccm 注入的客户端身份头（多客户端分流失效）。`ccm doctor` 会检出并提示。修法：把这几个键从 settings.json 的 `env` 块移到**系统用户级环境变量**——裸用 `claude` 照常继承，而 ccm 启动子进程时能正确清除它们。

**两个终端还会互相切来切去吗（v0.3 的老问题）？**
不会了（v0.4）。每个 `ccm run --proxy` 会话带自己的 client id（默认随机，可 `--client` / `CCM_CLIENT_ID` 指定），会话内 `/switch` 继承 `CCM_CLIENT_ID` 只切本会话，`ccm run --proxy <target>` 的预切换也只作用于本会话。只有不带 id 的 `ccm switch`（或显式 `--global`）才动全局默认。没单独切过的客户端跟随全局。详见 4.6。client id 不是认证——别指望它隔离不可信的本地进程。

**`ccm proxy --bind 0.0.0.0`（或局域网 IP）被拒？**
ccm 只允许回环地址（`127.x.x.x`、`[::1]`），完整报错：

```text
refusing to bind non-loopback address 0.0.0.0: the CCM control API is unauthenticated, remote binding is not supported
```

控制 API 和凭据注入代理都没有鉴权，远程/LAN 绑定需要先做鉴权设计（仍在 backlog）。想从别的机器用，请在那台机器上各自跑 ccm。

**遇到 429 / 超时后，去哪确认有没有降级？**
三处：代理 stderr 的 `ccm route=... result=... action=fallback` 日志；`GET /_ccm/decisions`（每次请求的尝试序列）；`GET /_ccm/circuits` + `GET /_ccm/metrics`（谁被熔断、成功率如何）。

**为什么经 openai 网关（openai-compatible）没有缓存折扣？**
Anthropic 的 prompt cache 靠请求里的 `cache_control` 标记，OpenAI `chat/completions` 协议没有这个字段——翻译时只能丢弃（ccm 会警告一次），所以上游永远不会为你建立 prompt cache，每次都是全价 input。DeepSeek 这类上游自建的隐式缓存命中会在 usage 里报 `cached_tokens` / `prompt_cache_hit_tokens`，ccm 把它翻译成 Anthropic 的 `cache_read_input_tokens` 并从 `input_tokens` 里扣除——账单上那部分通常按缓存读取价计，但折扣幅度由上游决定，和 Anthropic 的 5 分钟/1 小时 cache 写入定价是两回事。想要 Anthropic 式缓存收益，请用 anthropic 类 provider 直连或走支持 Anthropic 协议的网关。

**MiniMax 模型怎么选？**
实测 `MiniMax-M3` 可用（`ccm health minimax` → `healthy: minimax / MiniMax-M3`）。注意官方菜单里 `MiniMax-M3.1-Flash-Preview` **强制开启 thinking**——请求 `thinking.type=disabled` 会返回 400，而 Claude Code 常会尝试关 thinking，所以不建议配成主力；M2 系（M2.1/M2.5/M2.7 等）同样无法关闭 thinking，且 `top_k`、`stop_sequences` 会被忽略（M2 上下文 204k）。

**上游返回 500 却没有 fallback？**
默认 `fallback_on` 只有 `429,502,503,504`。500 属于"终态"：原样透传，且按熔断的成功处理（会重置计数）。需要的话创建路由时加 `--fallback-on 429,500,502,503,504`。

**`ccm use` 换了默认，代理还在用旧模型？**
代理的活动目标是内存值（启动时的持久默认，或最近一次 `switch`），不随 state.toml 变。用 `ccm switch <target>` 切换，或重启代理。config.toml 本身是每个请求都重载的——改模型/路由参数即时生效，唯独活动目标不是。

**`ccm switch` 报 `failed to contact CCM proxy control API`？**
代理没在运行。先在一个终端 `ccm proxy`。目标名不存在则报 `CCM proxy rejected switch (400): ...`。

**`ccm health` 报 `unknown model`？**
health 只接受模型名 / profile 名，不接受路由名。传 `coding-route` 这类名字会当模型名去找。

**怎么删除模型 / provider / 路由？**
命令树里没有删除子命令（`add` 是 upsert）。手工编辑 `~/.ccm/config.toml` 即可；代理模式下每请求重载，改完即时生效。

**提示 `config not found at ...`？**
还没初始化：`ccm init`。

---

## 9. 命令速查表

| 命令 | 作用 | 关键 flag（默认值） |
|---|---|---|
| `ccm init` | 初始化 config.toml + state.toml | `--force` |
| `ccm add provider <name>` | 添加/覆盖 provider | `--base-url`、`--kind`（`anthropic` / `anthropic-compatible` / `openai-compatible`）、`--auth`（按 kind 默认：anthropic 类 `x-api-key`，openai 类 `bearer`） |
| `ccm add model <name>` | 添加模型 | `--provider`、`--model-id`、`--cost-weight`（1.0）、`--quality-weight`（1.0） |
| `ccm add route <name>` | 添加路由 | `--primary`、`--fallback`、`--selection`（`ordered`）、四个权重（0.4/0.2/0.2/0.2）、`--header-timeout-ms`（30000）、`--fallback-on`（429,502,503,504）、`--max-attempts`（3）、`--backoff-ms`（200）、`--circuit-enabled`（true，需显式传值）、`--failure-threshold`（3）、`--circuit-open-ms`（30000） |
| `ccm auth set <provider>` | 交互式存 key 进 keyring | — |
| `ccm auth delete <provider>` | 删除 keyring 条目 | — |
| `ccm use <target>` | 设持久默认（写 state.toml） | — |
| `ccm switch <target>` | 运行时切换代理目标（全局或某客户端） | `--proxy-url`（`CCM_PROXY_URL` > `http://127.0.0.1:13521`）、`--client <id>`（`CCM_CLIENT_ID` > 全局）、`--global`（强制全局，与 `--client` 互斥） |
| `ccm clients` | 列出代理各客户端的运行时目标、请求计数与 last_seen | `--proxy-url`（同 `switch`） |
| `ccm history decisions` | 离线查看持久化决策（JSONL，旧→新） | `--since`、`--until`（unix-ms，含边界）、`--model`、`--client`、`--limit`（保留最新 N，N ≥ 1） |
| `ccm history metrics` | 离线查看指标快照表格（默认最新一条） | `--limit`（N ≥ 1） |
| `ccm history circuit` | 离线查看熔断转换（UTC 时间表） | `--model`、`--limit`（N ≥ 1） |
| `ccm history cost` | 离线查看按 UTC 日聚合的使用量与成本表（见 6.4） | `--day`（`YYYY-MM-DD`，缺省今天）、`--client` |
| `ccm advise` | 从真实使用量建议 `cost_weight`（只打印不写配置，见 6.6） | `--window`（7 天）、`--min-samples`（20）、`--model` |
| `ccm proxy` | 启动本地代理 | `--bind`（`127.0.0.1:13521`，仅回环） |
| `ccm run [target]` | 启动 claude（直连或代理） | `--proxy`、`--proxy-url`（`http://127.0.0.1:13521`）、`--client <id>`（代理模式 client id，默认 `CCM_CLIENT_ID` > 随机短 id；带 target 时预切换只作用于本会话） |
| `ccm list` | 列出模型与路由（`*` = 当前） | — |
| `ccm current` | 显示持久默认目标 | — |
| `ccm doctor` | 本地体检 | — |
| `ccm health <target>` | 真实请求探测（模型/Profile 名） | — |
| `ccm discover [provider]` | 拉取并勾选注册网关模型清单（见 3.5） | `--all`（非交互全选） |
| `ccm integrate claude` | 安装/移除 `/switch` skill | `--remove` |

| 环境变量 | 作用 |
|---|---|
| `CCM_HOME` | 重定位 config.toml / state.toml |
| `CCM_PROXY_URL` | switch / clients 的默认代理地址（run --proxy 只认 `--proxy-url` flag）；代理模式下传给 claude |
| `CCM_CLIENT_ID` | switch / run --proxy 的默认 client id；代理模式下传给 claude（`/switch` 靠它保持会话作用域） |
| `OTEL_EXPORTER_OTLP_HEADERS` | OTLP 推送收集器认证头（仅环境变量，见 6.7） |
| `CCM_<PROVIDER>_API_KEY` | provider 凭据（优先于 keyring），如 `CCM_ZAI_API_KEY` |

| 文件 / 端点 | 说明 |
|---|---|
| `~/.ccm/config.toml`、`~/.ccm/state.toml` | 声明式配置 / 持久默认（可用 `CCM_HOME` 重定位） |
| `~/.ccm/clients.toml` | 客户端会话持久化：scoped 条目的 id / target / last_seen_ms，只由代理写入（见 4.6；`[clients] persist = false` 关闭） |
| `~/.ccm/history/` | 观测历史 JSONL：decisions / metrics / circuit / usage（见 6.3、6.4；`CCM_HOME` 同样生效） |
| `~/.claude/skills/switch/SKILL.md` | `/switch` skill 安装位置 |
| `http://127.0.0.1:13521/_ccm/{status,models,routes,traces,circuits,metrics,scores,decisions,clients}` | 观测接口（GET；status/traces/decisions 支持 `?client=` 过滤） |
| `http://127.0.0.1:13521/_ccm/switch/{target}` | 运行时切换（POST；`?client=<id>` 只切该客户端） |
| `http://127.0.0.1:13521/v1/messages` | 反向代理入口（仅此端点被转发） |
