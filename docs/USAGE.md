# CCM 使用指南

CCM（Claude Code Model Manager）让你把 Claude Code 指向本地或第三方模型网关（zai、minimax 这类 anthropic / anthropic-compatible 端点），并在中间加一层路由：fallback、重试、熔断、指标与运行时切换。

本文是操作手册。架构背景见 `docs/DESIGN.md`，版本规划见 `docs/V0.3_PLAN.md`。

---

## 1. 三十秒理解 CCM

三个核心概念：

| 概念 | 是什么 | 定义位置 |
|---|---|---|
| 模型（model） | 一个具体可用的模型：所属 provider + `model_id` + 相对成本/质量权重 | config.toml |
| 路由（route） | 候选列表（primary + fallback 顺序）+ 选择策略 + 重试/超时/熔断参数 | config.toml |
| 本地代理（proxy） | 运行在 `127.0.0.1:13521` 的反向代理：注入凭据、执行路由/fallback/熔断、暴露 `/_ccm/*` 观测接口 | 进程（`ccm proxy`） |

另有 **profile**（模型别名，如 `coding` → `claude`）：凡是接受模型名的命令也接受 profile 名。

三类数据的存放位置——理解这个划分能解释 90% 的"为什么找不到"：

| 位置 | 存什么 | 路径 / 形式 |
|---|---|---|
| `config.toml` | 声明式配置：providers、models、profiles、routes | `$CCM_HOME/config.toml`，默认 `~/.ccm/config.toml` |
| `state.toml` | 只有一项 `current`：持久化的默认目标 | `$CCM_HOME/state.toml`，默认 `~/.ccm/state.toml` |
| 系统凭据管理器 | 各 provider 的 API key，**永不写入任何文件** | Windows 凭据管理器（service 名为 `ccm`，条目名 = provider 名，如 `LegacyGeneric:target=zai.ccm`）；也可用环境变量 |

两个直接推论：

- API key 在 config.toml 里永远找不到，这是设计（见第 8 节 FAQ）。
- 路由策略（fallback/熔断/多候选选择）只在**代理模式**下生效；直连模式是单模型直通。

---

## 2. 安装

前置条件：Rust 工具链（三种方式都要 `cargo build --release`）、Claude Code 已安装（终端里 `claude --version` 可用）。

### Windows（PowerShell，仓库根目录）

```powershell
.\scripts\install.ps1
# 如被执行策略拦截：
powershell -ExecutionPolicy Bypass -File scripts/install.ps1
```

脚本会先 `cargo build --release`，再把 `ccm.exe` 装到 `%LOCALAPPDATA%\Programs\ccm\ccm.exe`，并把该目录追加进用户 PATH（幂等，重复执行无副作用），最后用 `ccm --version` 验证。

**注意：装完后需要新开一个终端**，当前会话看不到 PATH 变更。

### Linux / macOS（sh，仓库根目录）

```sh
./scripts/install.sh
# 自定义安装目录：
CCM_INSTALL_DIR=/usr/local/bin ./scripts/install.sh
```

默认装到 `~/.local/bin`（请确认该目录在你的 PATH 中），同样先构建、后用 `ccm --version` 验证。

**注意：v0.3 只声称支持 Windows 和 Linux**（Linux 在 WSL2 Ubuntu 24.04 上验证）。macOS 没有可用主机跑验证，**不声称支持**——`install.sh` 是通用 POSIX sh，理论上可跑，但未经确认。

### 手动构建

```powershell
cargo build --release
# 产物：target\release\ccm.exe —— 复制到任意 PATH 目录
```

```sh
cargo build --release
cp target/release/ccm ~/.local/bin/
```

### 验证

```powershell
ccm --version
```

输出 `ccm 0.3.0`。

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
| `--kind` | `anthropic` / `anthropic-compatible` / `compatible`（不分大小写） | 交互提示 `Kind (anthropic / anthropic-compatible):` |
| `--auth` | `x-api-key`（别名 `x_api_key` / `apikey` / `api-key`）或 `bearer`（别名 `authorization`） | 默认 `x-api-key` |

`ccm add model` 参数：`--provider`（必须指向已存在的 provider）、`--model-id`（省略则交互提示）；`--cost-weight` / `--quality-weight` 默认各 `1.0`（语义见 5.3）。

注意：`add provider` 是 upsert，同名会**静默覆盖**。另外所有 `ccm add` 都是交互友好的，但在脚本/CI 里请把 flag 传全，避免卡在提示上。

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

✓ Claude Code: installed
✓ settings.json: no ANTHROPIC_* env overrides
✓ current target: glm
✓ primary model: glm
✓ model id: glm-5.3
✓ provider: zai
✓ base URL: https://api.z.ai/api/anthropic
✓ credential: present
✓ endpoint: reachable (200)

For a full authenticated model check, run `ccm health glm`.
```

- `endpoint` 一行是对 base_url 发起普通 GET 的状态码，因网关而异。
- `settings.json` 一行检查 Claude Code 自己的 `~/.claude/settings.json` / `settings.local.json` 的 `env` 块——那里的 `ANTHROPIC_*` 键会覆盖 ccm 的注入（见 FAQ）。
- 链路中途失败会提前结束（例如 `✗ target `...`: ...`），后面的检查不再打印。
- 刚 `init` 完（current 还是 `claude` 且没存 anthropic key）时，凭据行会是：
  `✗ credential: missing (set CCM_ANTHROPIC_API_KEY or run `ccm auth set anthropic`)`

`ccm health <target>` 发一条真实的 `/v1/messages` 请求（`max_tokens: 1`、内容 `ping`），验证鉴权和模型名：

```text
healthy: zai / glm-5.3
healthy: minimax / MiniMax-M3
```

注意 `health` 只接受**模型名或 profile 名，不接受路由名**（`use` / `switch` / `proxy` 三者才接受路由名）。常见报错：401/403 → `provider reachable but authentication failed (401)`；名字拼错 → `unknown model `...``。

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

**代理模式是主推方式**：只有它有 fallback/熔断/观测，且能在不退出 claude 的情况下切换目标。直连模式适合临时、单模型、不想多开一个终端的场景。

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

外部已存在的 `ANTHROPIC_API_KEY` / `ANTHROPIC_AUTH_TOKEN` 会先被清除、再注入 ccm 解析的凭据——claude 子进程只会用到 ccm 认可的那一个。stdio 直接继承，claude 的输出原样透传。Windows 上 claude 通过 `cmd /c claude` 启动，npm 的 `claude.cmd` shim 和原生 `claude.exe` 都兼容。

没选过默认目标时：`no current model selected; run `ccm use <name>` first`。直连模式下路由名不可用（解析口径是模型/profile；持久默认是路由名时的直连行为待确认）。

### 4.3 代理模式

终端 A 启动代理（需要先有持久默认目标，否则报 `no current target selected; run `ccm use <name>` first`）：

```powershell
ccm proxy
```

```text
CCM proxy listening on http://127.0.0.1:13521
Claude Code base URL: http://127.0.0.1:13521
Runtime switch: `ccm switch <model-or-profile-or-route>`.
```

终端 B 启动 claude：

```powershell
ccm run --proxy                # 用代理当前的目标
ccm run --proxy coding-route   # 先切到 coding-route 再启动
```

claude 拿到的是占位配置：`ANTHROPIC_BASE_URL=<代理地址>`、`ANTHROPIC_MODEL=ccm`、`ANTHROPIC_AUTH_TOKEN=ccm-local`、`CCM_PROXY_URL=<代理地址>`——真实路由与鉴权全部由运行中的代理完成。

代理运行期间，每次路由尝试都会向 stderr 打一行日志（fallback 时追加 ` action=fallback`）：

```text
ccm route=coding-route attempt=1 model=glm result=HTTP 429 action=fallback
ccm route=coding-route attempt=2 model=minimax result=HTTP 200
```

`--bind` 可改监听地址，但**只允许回环地址**（默认 `127.0.0.1:13521`，`[::1]` 也可以；`0.0.0.0`、局域网 IP、`[::]` 一律拒绝），原因见 FAQ。

代理只转发 `/v1/messages`（`count_tokens` 等其他端点不转发；实测 Claude Code 可以容忍）。

### 4.4 `use` vs `switch`（持久默认 vs 运行时切换）

| | `ccm use <target>` | `ccm switch <target>` |
|---|---|---|
| 改什么 | state.toml 的 `current`（持久默认） | 运行中代理的内存目标 |
| 是否要求代理在运行 | 否 | 是（否则 `failed to contact CCM proxy control API`） |
| 生效范围 | 之后的 `ccm run`、下次 `ccm proxy` 启动、`ccm doctor` 等 | 仅当前代理进程 |
| 输出 | `Selected {target} as persisted default` | `Runtime target switched to {target}` |

两者都接受模型 / profile / 路由名（解析顺序：路由 → 模型 → profile）。`switch` 的代理地址解析顺序：`--proxy-url` flag > `CCM_PROXY_URL` 环境变量 > `http://127.0.0.1:13521`。

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
- 需要代理在运行，否则报 `failed to contact CCM proxy control API`。
- skill 带 `disable-model-invocation: true` 和 `allowed-tools: ["Bash(ccm switch:*)"]`——只有你手动 `/switch` 才会触发，且只允许执行 `ccm switch` 命令。
- 安装时会顺带删除旧版命令文件 `~/.claude/commands/switch.md`（如果存在）。
- 卸载：`ccm integrate claude --remove` → `Removed Claude Code /switch integration`（文件已不存在也不报错）。

### 4.6 多个终端用不同模型

一个代理只有一个全局运行时目标——两个终端挂同一个代理时，任何一端 `/switch`，**两端同时切**。要各用各的模型，两条路：

**方案 A：直连模式（最简单）**

```powershell
ccm run claude      # 终端 1
ccm run glm         # 终端 2
```

各自独立进程、独立环境变量直连上游，互不影响。代价：没有 fallback / 熔断 / 观测（这些只在代理模式下存在）。

**方案 B：双代理（保留路由能力）**

两个代理、两个端口，各自独立的内存目标 / 熔断 / 指标：

```powershell
# ── 终端组 1：代理 A 跑 glm ──
ccm proxy --bind 127.0.0.1:13521
ccm switch glm      --proxy-url http://127.0.0.1:13521
ccm run --proxy --proxy-url http://127.0.0.1:13521

# ── 终端组 2：代理 B 跑 minimax ──
ccm proxy --bind 127.0.0.1:13522
ccm switch minimax --proxy-url http://127.0.0.1:13522
ccm run --proxy --proxy-url http://127.0.0.1:13522
```

`ccm switch` 是 runtime-only（只改指定代理的内存目标，不写 state.toml），两个代理互不影响；会话内 `/switch` 也不会串——每个 claude 拿到的是自己代理的 `CCM_PROXY_URL`。按客户端 / 会话自动分流（如按 header 路由）目前不支持，属于 v0.3 之后的特性。

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

### 5.3 模型的 cost / quality 权重

`ccm add model` 的 `--cost-weight` / `--quality-weight`（默认各 `1.0`）：

- `cost_weight` 是**相对成本**，越小越便宜：`lowest-cost` 直接按它升序，`weighted` 里它得分更高。样例配置给 glm 设 0.25、claude 设 1.0，即"glm 约便宜 4 倍"。
- `quality_weight` 是**相对质量**（0~1），只影响 `weighted` 策略的质量分量。

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

---

## 6. 观察与诊断

| 工具 | 一句话 |
|---|---|
| `ccm doctor` | 本地体检：PATH、目标解析、配置链、凭据在不在、端点通不通（见 3.4） |
| `ccm health <model-or-profile>` | 发真实请求验证"鉴权 + 模型名"是否可用 |
| `/_ccm/*` 控制接口 | 代理运行时的实时观测与切换（本节） |

### 6.1 控制接口一览（默认 `http://127.0.0.1:13521`）

| 端点 | 用途 / 返回要点 |
|---|---|
| `GET /health` | 存活检查，返回字面量 `ok` |
| `GET /_ccm/status` | 当前目标的解析结果：primary、model_id、provider、fallback 列表、完整 policy |
| `GET /_ccm/models` | 模型清单：model_id、provider、cost_weight、quality_weight |
| `GET /_ccm/routes` | 路由清单 + `active` 标记（哪条是当前内存目标） |
| `GET /_ccm/traces` | 最近 100 条请求尝试记录 |
| `GET /_ccm/circuits` | 各模型熔断状态（CLOSED / OPEN / HALF_OPEN / HALF_OPEN_READY） |
| `GET /_ccm/metrics` | 各模型指标：attempts、successes、success_rate、health_score、http_errors、fallback_failures、timeouts、request_errors、rate_limited、latency_ewma_ms、last_success_ms、last_failure_ms |
| `GET /_ccm/scores` | 当前路由候选的打分明细（reliability/latency/cost/quality/weighted，按加权分降序） |
| `GET /_ccm/decisions` | 最近 100 条路由决策（见下） |
| `POST /_ccm/switch/{target}` | 运行时切换（`ccm switch` 即调它；未知目标返回 400） |
| `POST /v1/messages` | 反向代理本体，Claude Code 的流量入口；非 POST 返回 405 `POST required` |

```powershell
Invoke-RestMethod http://127.0.0.1:13521/_ccm/status
Invoke-RestMethod http://127.0.0.1:13521/_ccm/circuits
Invoke-RestMethod http://127.0.0.1:13521/_ccm/metrics
```

```sh
curl -s http://127.0.0.1:13521/_ccm/decisions
```

traces 和 decisions 是进程内环形缓冲，各保留**最近 100 条**，重启代理即清零；metrics/circuits 同样只活在进程内。

### 6.2 怎么读 `/_ccm/decisions`

每条决策记录一次请求的路由过程，关注四个层次：

1. `selection` + `ranked_candidates`：这次按什么策略、候选打分排序如何（`ordered` 时排序即配置顺序）；
2. `attempts`：实际尝试序列，每项含 model、当时的熔断状态、result、是否 fallback。`result` 的取值包括 `HTTP {status}`、`timeout after {ms}ms`、`request error: {err}`、`skipped: circuit OPEN until {until}`、`skipped: HALF_OPEN probe already in flight`；
3. `selected`：最终承接下来的模型；全部失败时代理返回 502；
4. `outcome`：结果概述（取值枚举待确认）。

一个"熔断跳过 + fallback 成功"的决策示例（结构示意，个别字段名以实际响应为准）：

```json
{
  "id": 12,
  "timestamp_ms": 1759300000000,
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
    { "model": "glm", "circuit": "OPEN", "result": "skipped: circuit OPEN until 1759300030000", "fallback": true },
    { "model": "minimax", "circuit": "CLOSED", "result": "HTTP 200", "fallback": false }
  ],
  "selected": "minimax",
  "outcome": "..."
}
```

读法：glm 熔断中，被跳过（没打上游、不占尝试预算）→ 立刻落到 minimax → 成功。配合 stderr 的 `ccm route=... attempt=... result=...` 日志可以实时看到同样的信息。

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
| `CCM_PROXY_URL` | `ccm switch` / `ccm run --proxy` 的默认代理地址（`--proxy-url` flag 可覆盖；最终默认 `http://127.0.0.1:13521`）；代理模式下会传给 claude 子进程 |

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
Claude Code 的 `~/.claude/settings.json`（和 `settings.local.json`）里的 `env` 块会在 ccm 注入的环境变量**之后**叠加、优先级更高。其中 `ANTHROPIC_BASE_URL` / `ANTHROPIC_API_KEY` / `ANTHROPIC_AUTH_TOKEN` 会静默覆盖 ccm 的注入——ccm 只能清理进程环境变量（`launcher.rs` 启动时 `env_remove`），管不到 claude 自己的配置层。症状就是上述警告，更隐蔽的症状是 `ccm run <目标>` 实际连的仍是 settings.json 里配置的网关。`ccm doctor` 会检出并提示。修法：把这几个键从 settings.json 的 `env` 块移到**系统用户级环境变量**——裸用 `claude` 照常继承，而 ccm 启动子进程时能正确清除它们。

**`ccm proxy --bind 0.0.0.0`（或局域网 IP）被拒？**
v0.3 只允许回环地址（`127.x.x.x`、`[::1]`），完整报错：

```text
refusing to bind non-loopback address 0.0.0.0: the CCM control API is unauthenticated, remote binding is not supported in v0.3
```

控制 API 和凭据注入代理都没有鉴权，远程/LAN 绑定需要先做鉴权设计（v0.3 之后）。想从别的机器用，请在那台机器上各自跑 ccm。

**遇到 429 / 超时后，去哪确认有没有降级？**
三处：代理 stderr 的 `ccm route=... result=... action=fallback` 日志；`GET /_ccm/decisions`（每次请求的尝试序列）；`GET /_ccm/circuits` + `GET /_ccm/metrics`（谁被熔断、成功率如何）。

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
| `ccm add provider <name>` | 添加/覆盖 provider | `--base-url`、`--kind`、`--auth`（`x-api-key`） |
| `ccm add model <name>` | 添加模型 | `--provider`、`--model-id`、`--cost-weight`（1.0）、`--quality-weight`（1.0） |
| `ccm add route <name>` | 添加路由 | `--primary`、`--fallback`、`--selection`（`ordered`）、四个权重（0.4/0.2/0.2/0.2）、`--header-timeout-ms`（30000）、`--fallback-on`（429,502,503,504）、`--max-attempts`（3）、`--backoff-ms`（200）、`--circuit-enabled`（true，需显式传值）、`--failure-threshold`（3）、`--circuit-open-ms`（30000） |
| `ccm auth set <provider>` | 交互式存 key 进 keyring | — |
| `ccm auth delete <provider>` | 删除 keyring 条目 | — |
| `ccm use <target>` | 设持久默认（写 state.toml） | — |
| `ccm switch <target>` | 运行时切换当前代理目标 | `--proxy-url`（`CCM_PROXY_URL` > `http://127.0.0.1:13521`） |
| `ccm proxy` | 启动本地代理 | `--bind`（`127.0.0.1:13521`，仅回环） |
| `ccm run [target]` | 启动 claude（直连或代理） | `--proxy`、`--proxy-url`（`http://127.0.0.1:13521`） |
| `ccm list` | 列出模型与路由（`*` = 当前） | — |
| `ccm current` | 显示持久默认目标 | — |
| `ccm doctor` | 本地体检 | — |
| `ccm health <target>` | 真实请求探测（模型/Profile 名） | — |
| `ccm integrate claude` | 安装/移除 `/switch` skill | `--remove` |

| 环境变量 | 作用 |
|---|---|
| `CCM_HOME` | 重定位 config.toml / state.toml |
| `CCM_PROXY_URL` | switch / run --proxy 的默认代理地址；代理模式下传给 claude |
| `CCM_<PROVIDER>_API_KEY` | provider 凭据（优先于 keyring），如 `CCM_ZAI_API_KEY` |

| 文件 / 端点 | 说明 |
|---|---|
| `~/.ccm/config.toml`、`~/.ccm/state.toml` | 声明式配置 / 持久默认（可用 `CCM_HOME` 重定位） |
| `~/.claude/skills/switch/SKILL.md` | `/switch` skill 安装位置 |
| `http://127.0.0.1:13521/_ccm/{status,models,routes,traces,circuits,metrics,scores,decisions}` | 观测接口（GET） |
| `http://127.0.0.1:13521/_ccm/switch/{target}` | 运行时切换（POST） |
| `http://127.0.0.1:13521/v1/messages` | 反向代理入口（仅此端点被转发） |
