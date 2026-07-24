[中文版](README.zh-cn.md)

# hymt

[![许可证：Apache-2.0](https://img.shields.io/badge/license-Apache--2.0-blue)](LICENSE)
![模型](https://img.shields.io/badge/model-Hy--MT2-orange)
![平台](https://img.shields.io/badge/platform-Linux-lightgrey)

Hy-MT2是一款实用的Rust命令行工具：具备分词器感知的文本分割功能、分段级缓存复用、管道安全的令牌流处理、支持Markdown格式的文档翻译、批量翻译、命令输出翻译功能，以及可热重载的配置系统。

`hymt`专为那些需要处理真实终端和Markdown工作流的人设计，而不仅仅是处理零散的字符串。它会将处理进度输出到`stderr`，翻译结果输出到`stdout`，记录历史数据以便估算完成时间，当模型表现与历史预期相差较大时，还能自动记录时间偏差问题。

## 为何选择hymt

- 仅需一条命令即可翻译定位文本、标准输入内容或文件。
- 基于Hy-MT2分词器对长文本进行分割，而非盲目拆分。
- 在分段级别复用缓存翻译结果，使重复内容能几乎瞬间处理完成。
- 默认以流式方式处理令牌，确保`| less`、`| bat`和`| tee`等命令行操作保持响应灵敏。
- 以考虑结构特性的边界对Markdown文本进行分割；具体多语言支持限制见下文。
- 可批量处理整个目录树，在写入前预览缓存状态及预计完成时间。
- 可用`hymt exec`封装任意Shell命令，或查看已翻译的`man`和`info`文档页面。
- 可调出之前的输出结果，并查看带有处理效率统计信息的翻译历史。
- 使用`hymt translate-doc`可保持双语Markdown文档的同步。
- 还提供可选的Telegram机器人（`hymt telegram`），用于实现私有多用户权限管理以及群组内的中英互译功能。

## 安装

### 安装

```bash
just install
```

该二进制文件默认会启用`telegram` cargo功能。如需在不依赖Telegram Bot API的情况下构建：

```bash
just install-no-telegram
```

经过记录和测试的安装路径是Linux x86_64系统上的Rust工作区。

## 配置端点

首次使用时，`hymt`会创建`~/.config/hymt/config.toml`文件。典型的配置如下：

```toml
[endpoint]
url = "http://100.78.159.38:8401/v1"
api_key = ""
model = ""
# Select one supported, tested Hy-MT2 profile, or use "generic" for an unprofiled endpoint.
profile = "hy_mt2_7b"
# 根据服务实现选择适配器，不要从端点 URL 推断。
backend = "llama_cpp" # "llama_cpp" | "vllm" | "openai_compatible"

[backend]
# `llama-server -c` is the service-wide allocation. The throughput unit uses
# 65,536 total tokens across 8 slots; the quality unit uses 24,576 across 3.
total_context = 65536
parallel_slots = 8
# Optional: omit to derive total_context / parallel_slots. Set this explicitly
# when the backend guarantees a lower per-request limit.
per_request_context = 8192

[translation]
max_output_tokens = 4096
max_source_tokens_per_segment = 1024
concurrency = 8 # use 3 with hy-mt2-quality.service
stream = true
config_version = 1
timeout = 600
# first_chunk_priority = false
# debug_chunk_timing = false
# 当最终聊天模板无法在本地分词时拒绝规划；否则 hymt 会发出警告并使用保守的近似预算。
strict_token_budget = false
# For Chinese-family targets, preserve confidently target-language paragraphs.
language_detection = true
# Override detection and submit every non-code paragraph.
force_translate_all = false

[inference]
# 推理服务拥有采样器默认值。没有显式覆盖项时，hymt 会从 JSON 请求中省略
# 所有采样字段，Hy-MT2 配置文件也不例外。配置文件仍提供分词器/模型元数据
# 和服务部署指导。

[inference.override]
# 数值表示显式语义值；"disabled" 会被所选适配器映射为该后端文档规定的 wire 值。
# temperature = 0.7
# top_p = 0.6
# top_k = "disabled"
# repetition_penalty = 1.05
# min_p = 0.1
# repeat_last_n = 64 # 仅 llama.cpp

[completeness]
zh_to_en_min_ratio = 0.3
en_to_zh_min_ratio = 0.3
min_paragraph_ratio = 0.5
max_retries = 2
# When false (default), top-level text/file/stdin exits non-zero after writing best
# attempt if any segment exhausted completeness retries. Set true (or pass
# --warn-only-completeness) to keep exit 0 with warnings only.
warn_only = false

[timing]
divergence_threshold = 2.0
```

除`[endpoint].profile`外，其他配置均可热加载，该配置在进程启动时会被固定（详见[模型配置文件](#model-profile-endpointprofile)）。长时间运行的工作流无需重启即可应用其他更改。

### 后端专用采样（`[endpoint].backend`）

应根据服务实现显式选择`backend`；hymt绝不会从端点 URL 推断它。生成的默认配置选择`llama_cpp`。省略此键时，hymt使用保守的`openai_compatible`模式，且不会发送任何非标准采样扩展字段。

| 后端 | 支持的覆盖字段 | 后端专用 wire 行为 |
|---|---|---|
| `llama_cpp` | `temperature`、`top_p`、`top_k`、`repetition_penalty`、`min_p`、`repeat_last_n` | `repetition_penalty`在线上发送为`repeat_penalty`；禁用的`top_k`和`repeat_last_n`发送为`0`。 |
| `vllm` | `temperature`、`top_p`、`top_k`、`repetition_penalty`、`min_p` | `repetition_penalty`在线上发送为`repetition_penalty`；禁用的`top_k`发送为`-1`。`repeat_last_n`会被拒绝。 |
| `openai_compatible` | `temperature`、`top_p` | 只发送通用的聊天补全字段；所有非标准显式覆盖都会被拒绝，不会猜测字段名。 |

省略`[inference.override]`键始终表示`Setting::ServerDefault`，因此该字段不会出现在 JSON 请求中，由服务应用其自身配置的值。所有 Hy-MT2 配置文件同样如此：配置文件中的采样值仅作为服务部署指导，绝不会自动注入请求负载。只有在客户端必须有意替换服务默认值时才设置数值覆盖项；`"disabled"`和数值是语义配置状态，适配器会采用文档规定的 wire 值，而不会不加区分地在`0`和`-1`之间转换。显式覆盖项会显示在诊断信息中，并进入推理/缓存指纹。直接写在`[inference]`下的旧标量采样值会在一个发布周期内继续作为显式覆盖项接受，并产生启动迁移警告；请将它们移到`[inference.override]`。验证错误会给出语义值，并在不存在后端 wire 表示时明确说明。流式与非流式请求使用相同的适配器策略。

上述扩展名严格限制为已文档化的 llama.cpp 服务端控制项和 vLLM OpenAI 服务端采样参数：[llama.cpp 服务端 API](https://github.com/ggml-org/llama.cpp/blob/master/tools/server/README.md)与[vLLM OpenAI 兼容服务端](https://docs.vllm.ai/en/latest/serving/openai_compatible_server/)。旧的`[inference].backend`键会被拒绝；请将其移动到`[endpoint].backend`。

## 模型配置文件（`[endpoint].profile`）

需为Hy-MT2端点明确设置`[endpoint].profile`。支持的值及其对应功能如下：

| 值 | 功能说明 |
|---|---|
| `hy_mt2_1_8b` | 经测试的Hy-MT2 1.8B模型配置文件，使用固定的上游分词器源，并提供服务部署采样指导。 |
| `hy_mt2_7b` | 经测试的Hy-MT2 7B模型配置文件，使用固定的上游分词器源，并提供服务部署采样指导。 |
| `hy_mt2_30b_a3b` | 经测试的Hy-MT2 30B-A3B模型配置文件，使用固定的上游分词器源，并提供服务部署采样指导。 |
| `generic`（或省略） | 无配置模式：不使用任何经过测试的Hy-MT2分词器或采样指导。 |

该配置文件会在进程启动时被读取并**固定下来**。其他配置值仍可热加载，但盘中对`[endpoint].profile`的更改会被正在运行的进程忽略；如需使用不同配置文件，需重启`hymt`。分段缓存键和翻译历史记录会保留原始的配置文件ID，因此不同配置文件之间的结果不会相互影响。

### Telegram机器人（`[telegram]`）

默认配置中包含一个已禁用的Telegram相关部分：

```toml
[telegram]
enabled = false
bot_token = ""          # or set HYMT_TELEGRAM_BOT_TOKEN
claim_password = ""     # auto-generated on first `hymt telegram` if empty
owners = []             # private chat ids after claim
groups = []             # group chat ids when mode = "groups"
mode = "owners"         # "owners" | "groups"
```

1. 通过[@BotFather](https://t.me/BotFather)创建一个机器人，设置`bot_token`（或`HYMT_TELEGRAM_BOT_TOKEN`）。
2. 将`enabled`设置为`true`。
3. 运行`hymt telegram`（会持续轮询直到按下Ctrl+C）。首次运行时，hymt会生成一个申请密码，将其存储在配置文件中，并显示一次。
4. 在与机器人的私聊中，发送该申请密码（或输入`/claim <password>`）即可成为所有者。支持多个所有者。
5. 经过授权的所有者（以及当`mode = "groups"`时`groups`字段中列出的群组成员）能够自动实现文本消息的中文与英文互译；未经授权的聊天会收到简短的拒绝回复。
6. 可使用`hymt telegram --regenerate-claim-password`命令重新生成申请密码（新密码仅显示一次）。

`bot_token`和`claim_password`这些敏感信息不会在每次运行时都再次显示。

## 快速开始

### 翻译文本、标准输入内容或文件

```bash
hymt "Hello world" -t zh
printf 'Release notes go here.\n' | hymt -t ja
hymt -f CHANGELOG.md -t fr -o CHANGELOG.fr.md
```

### 目标语言代码

所有的提示词构建、验证、检测、输出文件名、CLI估算以及Telegram路由均使用同一套标准代码表。支持的标准代码包括：

`zh`、`zh-Hant`、`en`、`fr`、`pt`、`es`、`ja`、`tr`、`ru`、`ar`、`ko`、`th`、`it`、`de`、`vi`、`ms`、`id`、`tl`、`hi`、`pl`、`cs`、`nl`、`km`、`my`、`fa`、`gu`、`ur`、`te`、`mr`、`he`、`bn`、`ta`、`uk`、`bo`、`kk`、`mn`、`ug`和`yue`。

这些代码不区分大小写，且会将下划线 `_` 转换为连字符 `-`：`zh-CN`/`zh_CN` 会被视为 `zh`，而 `zh-TW`/`zh_Hant` 会被视为 `zh-Hant`。`zh`、`zh-Hant` 和 `yue` 在处理中文相关内容时采用相同的规则；Hy-MT2配置文件也对所有支持的目标语言使用这套代码表。

### 保持与流式处理的兼容性

默认情况下已启用流式处理功能。这意味着你可以继续使用常规的shell管道操作：

```bash
hymt -f article.md -t zh | less
hymt -f notes.txt -t ja | bat -l markdown
hymt -f report.md -t zh | tee report.zh.preview.md
```

如果需要完全缓冲的响应，请使用 `--no-stream`。

若希望强制单次运行时的并发数，可使用 `--concurrency N`（该参数会覆盖 `[translation].concurrency` 的设置）。在排查多段处理阻塞问题时，可使用 `--debug-chunk-timing`（或 `HYMT_DEBUG_CHUNK_TIMING=1`）在标准错误流中输出每个数据块的队列处理时间、请求处理时间、首个字符处理时间以及整体完成时间。

### 混合语言文档的处理策略

对于中文系目标语言（`zh`、`zh-Hant` 和 `yue`），hymt 会在对请求进行分段处理之前，逐段规划文档结构。在默认设置 `[translation].language_detection = true` 的情况下，当某段的 CJK 字符比例超过 60%，且至少包含四个已分析的非空白字符时，该段落将会被保留。其原始的 UTF-8 字节在重建过程中会保持不变；其他段落则会被发送给模型处理。

Markdown 标题、列表项、引文以及表格行也遵循相同的段落处理规则。用代码块标记的内容以及开头的 YAML 前置信息始终会被保留。那些非常简短、类似代码或含义模糊的片段会被翻译，而不会被判定为已经是目标语言内容。对于文本、标准输入和文件输入，使用 `--plan` 参数可输出每段的检测元数据，包括 `is_target_language` 和 `should_translate` 这两个字段。

若要翻译所有非代码段落，可通过单次运行时的覆盖设置或配置文件来实现：

```bash
hymt --force-translate-all -l zh "English text\n\n已有中文段落"
hymt --no-language-detection -l zh -f article.md
```

```toml
[translation]
language_detection = true      # default: use CJK detection for Chinese-family targets
force_translate_all = false    # default: false; set true to translate all non-code paragraphs
```

`--force-translate-all`、`--no-language-detection`、`force_translate_all = true`以及`language_detection = false`都会选择全量翻译策略。显式的 `-l/--lang` 参数可以指定目标语言，但**不会**禁用内容保留功能。该工具的检测功能仅适用于中文系目标语言（`zh`、`zh-Hant` 和 `yue`）：对于非中文系目标语言，hymt会翻译所有非代码段落，而不会尝试进行通用多语言检测。

## 智能分块与缓存复用

hymt会根据Hy-MT2分词器及选定的提示模板来规划每次翻译任务。目前，每个翻译后的段落是通过以下信息进行缓存的：
- 段落内容哈希值
- 目标语言
- 模板类型
- 模板选项
- `profile_id`（标准配置文件ID）

因此实现了配置文件的隔离功能。不过，段落缓存键中**还不包括**端点/模型标识、分词器版本、量化设置或后端构建信息，以及推理采样设置（参见#115）。因此，这些设置发生变化时，仍可复用旧配置文件中的缓存条目；`config_version`信息会记录在任务历史中，而非段落缓存键中。要实现这些设置的自动隔离，还需要进行推理指纹识别。

这样一来就可以实现：
- 在翻译过程被中断后快速重新开始
- 当只有少数段落发生变化时能近乎即时地重新生成译文
- 在普通翻译、批量翻译、文档翻译以及手动翻译页面处理中复用缓存

翻译进度始终会以相同格式显示在`stderr`输出中：

```text
[done/total] XX.XX% | elapsed Xm Ys | eta Xm Ys | NN.NN tok/s
```

## 翻译 Markdown 文档

`translate-doc` 是用于处理双语 Markdown 树的结构化命令。

```bash
hymt translate-doc README.md
hymt translate-doc README.md -t ja
hymt translate-doc README.md -t zh -o README.zh-cn.md
hymt translate-doc docs/ --recursive
```

行为特点：

- 默认目标语言为`zh`，Markdown输出文件会自动命名为`.zh-cn.md`。
- 当使用`--output-dir`参数时，目录模式会翻译Markdown文件并保留相对路径。
- 完整性验证是一项快速的分层截断/结构防护机制，**不是**翻译质量估计（QE），也不能证明语义正确性。它对已校准的英语/中文目标使用 Unicode 标量密度上下限；其他目标会显式报告 `unverified_density`，而不会默默声称密度已通过。它还会检查由调用方提供的空响应/终止信息、段落、Markdown 标题和围栏代码块、占位符、URL 以及 JSON 模板是否有效。缓存片段在复用前会由当前防护机制重新验证。
- 失败的翻译片段会最多重试`[completeness].max_retries`次；普通模式、流式处理、批量处理以及`translate-doc`模式的片段验证都适用此阈值。重试耗尽后，`hymt`会保留验证得分最高的尝试（仅根据可观察的防护信号排序，绝非 QE 得分），并记录 `reason=highest_validation_score`。`hymt`会写入这一降级的尽力结果，并在标准错误流中输出`completeness_status=degraded_best_effort`和`completeness_degraded_segments=…`信息。顶层文本/文件/标准输入命令（包括其流式处理形式）会以非零状态退出，以便脚本能够检测到降级结果；若希望仅显示警告而不改变退出码，可传递`--warn-only-completeness`参数或设置`[completeness].warn_only = true`。默认情况下，`batch`、`translate-doc`和`exec`模式也会输出相同的标准错误信息，但不会因部分片段降级而使整个任务失败。验证式流处理会缓冲一个片段直到其通过；乐观流处理无法收回已发出的无效令牌，因此会报告降级的尽力结果，而不是重试。
- 源文本片段的长度也受到扩展量/上下文限制以及`[translation].max_source_tokens_per_segment`参数的约束（默认值为`1024`，设置为`0`则取消该限制）。对于已固定的 Hy-MT2 配置文件且已下载分词器，规划器会在预留输出令牌前渲染并计数完整聊天请求（角色框架、提示词/上下文、助手标记以及完整性重试预留）。`--plan` 会报告计数来源、配置文件/分词器/模板标识、每槽容量、输入/输出拆分和任何片段重分割。
- 如果活动配置文件/模板或分词器不可用，hymt 会在标准错误流中显示明确警告，并采用保守的`2x`输入估算加上`64`令牌的聊天框架预留。设置`[translation].strict_token_budget = true`可拒绝该近似路径；请配置`[endpoint].profile`并运行`hymt tokenizer download`以使用本地预算。
- 过大的围栏代码块和 Markdown 表格受保护块会在任何 HTTP 请求提交前以`ProtectedBlockTooLarge`失败关闭；请将其拆分或在模型外保留。

## 批量翻译目录树

当需要对`.md`和`.txt`文件进行预览式批量翻译时，可使用`batch`模式：

```bash
hymt batch docs -t zh
hymt batch docs -t zh --write --yes
hymt batch docs -t zh --write --output-dir translated-docs
```

批量预览报告：

- 已选择与已跳过的文件
- 每个文件的缓存状态：`完整`、`部分`或`无`
- 缓存的片段数量
- 每个文件的预计完成时间
- 总体预计完成时间

## 翻译命令输出和手册

### 包装终端命令

```bash
hymt exec -- cargo test
hymt exec -- git status
hymt exec precache --recursive
```

`hymt exec` 会保留原始命令的输出，随后再添加翻译后的输出。它对于不熟悉的命令行工具、构建失败以及冗长的帮助文本非常有用。

### 查看翻译后的 `man` 和 `info` 文档

```bash
hymt man git-rebase
hymt man --original git-rebase
hymt info coreutils
hymt info --refresh bash
```

## 回忆、历史记录及预计到达时间估算

翻译历史记录存储在 `~/.local/share/hymt/history.db` 的 SQLite 数据库中。

常用命令：

```bash
hymt history
hymt history --stats
hymt recall
hymt recall --list
hymt estimate 10000 -l zh
```

历史记录功能：

- 最近输出回溯
- 处理量统计
- 中位数/百分位数预计完成时间估算
- 反映实际token处理量的进度条

## Telegram机器人

```bash
# after configuring [telegram] and enabling it
hymt telegram
hymt telegram --regenerate-claim-password
```

有关声明所有权和群组模式的信息，请参见上文的 `[telegram]` 配置部分。

## 自动提交时间偏差问题报告

在完成交互式翻译后，`hymt` 会将实际运行时间与历史预估值进行比较。当运行时间偏差超过 `[timing].divergence_threshold` 时，它会提示用户提交包含以下信息的 GitHub issue：
- 令牌数量
- 分段数量
- 处理效率统计数据
- 配置版本
- 模型元数据

这样就能更轻松地追踪服务器设置、并发处理能力或提示词行为方面的问题。

## 通过 Tailscale 实现远程 Hy-MT2 运行

该仓库的 [`services/`](services) 目录下包含两个互斥的 systemd 用户服务示例。`-c` 表示整个服务的上下文池；`--parallel` 则用于在多个并发请求之间分配上下文资源：

| 服务名称 | 模型量化级别 | KV 缓存 | 总上下文容量 | 并行处理槽数 | 每个槽的上下文容量 |
|---|---|---|---:|---:|---:|
| `hy-mt2-quality.service` | Q6_K | Q8 (`q8_0`) | 24,576 | 3 | 8,192 |
| `hy-mt2-throughput.service` | Q4_K_M | Q4 (`q4_0`) | 65,536 | 8 | 8,192 |

这两个服务仅绑定到 Tailscale 上的 `100.78.159.38:8401` 地址，而非 `0.0.0.0`。它们都使用 CUDA 版本的 `llama-server`；质量优化版本使用本地构建的程序，而高处理效率版本则使用 `llama-cpp/9294-cuda` 构建版本。这些可执行文件和模型路径是特定于主机的，但替代的后端必须支持所指定的 `llama-server` 上下文配置、并行处理槽数以及 KV 缓存相关参数。

两个服务都显式设置了 `--temp 0.7`、`--top-k 20`、`--top-p 0.6` 和 `--repeat-penalty 1.05`，这些是 Hy-MT2 的部署建议。它们还显式设置 `--min-p 0`（关闭这一 llama.cpp 专用扩展）和 `--repeat-last-n 64`（为 1.05 的重复惩罚刻意选择的 llama.cpp 兼容性设置，并非上游 Hy-MT2 建议）。因此，这些 unit 不会意外继承已安装 llama.cpp 版本的采样配置。除非`[inference.override]`明确替换某个值，hymt 不会发送采样字段，所以更改服务默认值会改变默认翻译。启动时 llama.cpp 客户端会请求`GET /props`，并输出其报告的原始`default_generation_settings`；`/props`不可用或旧版本未提供该字段时，会发出失败开放的警告，请求仍只省略字段。

## 架构说明

- `crates/hymt-core`：支持热重载的 TOML 配置文件、提示词模板、中文等语言处理工具以及完整性检测算法。
- `crates/hymt-segment`：集成 Hy-MT2 分词器，同时具备分层分段和 Markdown 兼容的分段功能。
- `crates/hymt-client`：异步的 OpenAI 兼容 HTTP 客户端，具备重试处理、并发限制以及 SSE 流式传输功能。
- `crates/hymt-cache`：基于 SQLite 的分段缓存和执行缓存，用于存储任务历史记录、召回率数据以及预计完成时间统计信息。
- `crates/hymt-translate`：负责翻译任务的协调处理、完整性检测重试、批量/文档级处理流程，以及生成翻译后的文档。
- `crates/hymt-cli`：基于 Clap 库开发的 `hymt` 命令行工具，负责命令分发、Shell 接口功能，还支持通过 Telegram 发送指令的子命令。

## 开发指南

只需安装一次仓库钩子，然后运行本地质量检测工具即可：

```bash
just install-hooks
just pre-commit
```

验证在不依赖Telegram的情况下该二进制文件仍能构建：

```bash
just check-no-telegram
```

Lefthook在提交代码前会执行“just pre-commit”命令，并实现README文件的翻译同步功能。GitHub Actions CI则在拉取请求处理及代码推送到`main`分支时运行，负责处理格式检查、Clippy工具检测、工作区测试/检查、禁止默认功能的CLI检查、Shell脚本检查、服务单元验证以及TOML文件解析等工作。
