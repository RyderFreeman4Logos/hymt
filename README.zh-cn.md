[中文版](README.zh-cn.md)

# hymt

[![许可证：Apache-2.0](https://img.shields.io/badge/license-Apache--2.0-blue)](LICENSE)
![模型](https://img.shields.io/badge/model-Hy--MT2-orange)
![平台](https://img.shields.io/badge/platform-Linux-lightgrey)

Hy-MT2是一款实用的Rust命令行工具：具备分词器感知的分段功能、分段级缓存复用、管道安全的令牌流处理、支持Markdown格式的文档翻译、批量翻译、命令输出翻译功能，同时还支持热重载配置。

`hymt` 是为那些需要处理真实终端界面及 Markdown 工作流程的翻译任务而设计的，而不仅仅是处理零散的文本字符串。它会将进度信息输出到 `stderr`，翻译内容则输出到 `stdout`，还会记录历史数据以帮助估算完成时间；当模型表现与历史预期相差较大时，它还能自动记录时间偏差问题。

## 为什么唱圣歌

- 使用一个命令即可翻译定位文本、标准输入内容或文件。
- 使用Hy-MT2分词器对长输入进行分节，而非盲目分割。
- 当您希望重用片段和完整输出时，请启用应用程序翻译缓存。
- 默认按完成的原文段落流式输出，使 `| less`、`| bat` 和 `| tee` 工作流程及时显示进度。原子 JSON 和 CLI 帮助、用法、选项类输入仍完全缓冲。
- 使用具备结构感知边界的段落标记语法；请参见下文的混合语言限制。
- 批量处理整个目录树，并在写入前预览缓存状态及预计完成时间。
- 用……包裹任意的shell命令 `hymt exec`，或浏览译文 `man` 和 `info` 页面。
- 回顾之前的输出，并通过吞吐量统计查看翻译历史。
- 保持双语Markdown同步 `hymt translate-doc`.
- 可选的Telegram机器人（`hymt telegram`）用于私人多所有者索赔，以及中英互译，包括 `.txt` 和 `.md` 文件。

## 安装

### 安装

```bash
just install
```

该二进制文件默认会启用`telegram` cargo功能。如需在不依赖Telegram Bot API的情况下构建：

```bash
just install-no-telegram
```

经过记录和测试的安装路径是Linux x86上的Rust工作区。_64.

## 配置端点

首次使用时，`hymt`会创建`~/.config/hymt/config.toml`文件。典型的配置如下：

```toml
[endpoint]
url = "http://100.78.159.38:8401/v1"
api_key = ""
model = ""
# Select one supported, tested Hy-MT2 profile, or use "generic" for an unprofiled endpoint.
profile = "hy_mt2_7b"
# Choose the adapter from the server implementation, not its URL.
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
max_source_tokens_per_segment = 384
concurrency = 8 # use 3 with hy-mt2-quality.service
stream = true
config_version = 1
timeout = 600
max_retries = 100 # Additional network attempts; 0 disables retries.
# first_chunk_priority = false
# debug_chunk_timing = false
# Refuse planning when the final chat template cannot be tokenized locally.
# Otherwise hymt emits a warning and uses a conservative approximate budget.
strict_token_budget = false
# Except for verified local replay, refuse live work before segment-cache lookup
# or model invocation when the backend runtime is unverified or incompatible.
strict_backend_preflight = false
# For Chinese-family targets, preserve confidently target-language paragraphs.
language_detection = true
# Override detection and submit every non-code paragraph.
force_translate_all = false

[inference]
# The inference service owns sampler defaults. With no explicit override, hymt
# omits every sampler field from the JSON request, including for Hy-MT2 profiles.
# Profiles remain tokenizer/model metadata and service-deployment guidance.

[inference.override]
# A number is an explicit semantic value; "disabled" is mapped by the selected
# adapter to that backend's documented wire value.
# temperature = 0.7
# top_p = 0.6
# top_k = "disabled"
# repetition_penalty = 1.05
# min_p = 0.1
# repeat_last_n = 64 # llama.cpp only

[completeness]
max_retries = 2
# Retain marked complete original paragraphs after transport completion retries fail.
# Set false for strict failure instead. Runtime errors and atomic JSON remain strict.
warn_only = true

[timing]
divergence_threshold = 2.0
```

除`[endpoint].profile`在启动时会被固定外，其他配置均可热加载（详见[模型配置](#model-profile-endpointprofile)）。长时间运行的工作流无需重启即可应用其他更改。

### 后端特定采样（`[endpoint].backend`)

请从服务器实现中明确选择`backend`；hymt绝不会从端点URL中推断它。生成的配置会选择`llama_cpp`。如果省略该键，hymt会采用保守的`openai_compatible`模式，并不发送任何非标准的采样扩展。

| 后端 | 支持的覆盖字段 | 后端特定的线路行为 |
|---|---|---|
| `llama_cpp` | `temperature`,你应该只输出中文。 `top_p`,你应该只输出中文。 `top_k`,你应该只输出中文。 `repetition_penalty`,你应该只输出中文。 `min_p`,你应该只输出中文。 `repeat_last_n` | `repetition_penalty` 被发送为 `repeat_penalty`；已禁用 `top_k` 和 `repeat_last_n` 被发送为 `0`. |
| `vllm` | `temperature`,你应该只输出中文。 `top_p`,你应该只输出中文。 `top_k`,你应该只输出中文。 `repetition_penalty`,你应该只输出中文。 `min_p` | `repetition_penalty` 被发送为 `repetition_penalty`；已禁用 `top_k` 被发送为 `-1`. `repeat_last_n` 被拒绝。 |
| `openai_compatible` | `temperature`,你应该只输出中文。 `top_p` | 仅发送常见的聊天补全字段；所有非标准的显式覆盖值都会被拒绝，而不会被猜测。 |

若省略了`[inference.override]`键，其值始终为`Setting::ServerDefault`，因此该键不会出现在JSON请求中，服务会使用自身配置的默认值。所有Hy-MT2配置文件也是如此：配置文件中的采样值仅作为服务部署的参考，绝不会自动被注入到请求数据中。只有当客户端需要刻意替换服务默认值时，才应设置数值型覆盖值；“disabled”和数值属于语义配置状态，适配器会选择文档中规定的值，而不会随意将`0`和`-1`进行转换。明确的覆盖值会被包含在诊断信息以及推理/缓存指纹中。直接位于`[inference]`下的旧版标量采样值在某个版本中仍可视为明确覆盖值，但会触发启动迁移警告；建议将其移至`[inference.override]`下。验证错误会指出相应的语义值，并说明何时不存在后端对应的数值表示形式。流式请求与非流式请求均遵循相同的适配器策略。

上述支持的扩展名仅限于文档中记载的llama.cpp服务器控制参数以及vLLM OpenAI服务器采样参数：[llama.cpp服务器API](https://github.com/ggml-org/llama.cpp/blob/master/tools/server/README.md)和[vLLM OpenAI兼容服务器](https://docs.vllm.ai/en/latest/serving/openai_compatible_server/)。旧的 `[inference].backend` 键将被忽略，请将其移至 `[endpoint].backend`。

### 运行时后端预检与检查

在正式进行规划或查询分段缓存之前，翻译路径会先检查已配置的后端；如果明确启用了经过验证的本地完整输出回放功能，那么它会优先被使用。llama.cpp使用`GET /props`接口；而vLLM及其他兼容OpenAI的服务在可用时会使用`GET /v1/models`接口。标准化的运行时状态包括服务版本/模型标识、上下文及槽位限制、采样器默认设置，以及明确声明的功能特性。缺失的字段将保持未知状态，不会被自行推断。

当使用llama.cpp时 `/props` 报告一个ASCII标点符号 `eos_token` *和* 聊天模板中包含一个 `<|...|>` 控制标记方面，hymt检测到矛盾：模板中的标记才是真正的结束符号，而元数据标记则是错误的。它通过某种方式解决了这个错误的标记问题。 `/tokenize` 在预飞行阶段，并仅发送 llama.cpp 相关内容 `logit_bias` `{ "<token_id>": -100 }`从而防止错误停止，同时不会抑制真正的聊天模板EOS标记（例如 `<|eos|>`). 聊天模板本身使用的单字符EOS保持不变。

运行 `hymt backend inspect` 可以将配置值与服务解析后的值并列显示。该命令不会输出 API 密钥、端点凭证或请求头信息。警告信息会指出上下文/模型/配置文件不匹配、服务采样器状态异常，以及 llama.cpp/vLLM 的重复惩罚机制相关密钥不匹配的问题。

正常的预检机制为“失败即终止”模式：它会发出警告，将推理指纹/缓存标识标记为未验证状态，并设置较为严格的上下文/输出限制。若无法验证身份或发现重大差异，在进行分段缓存查询或模型调用之前，可通过设置`[translation].strict_backend_preflight = true`来拒绝执行相关操作。运行时状态会以TTL机制缓存60秒，当端点/后端/配置发生变化时会被刷新；新的服务身份将会取代原有的已解析状态和指纹。

## 模型简介（`[endpoint].profile`)

为Hy-MT2端点明确设置`[endpoint].profile`。已识别的值及其适用范围如下：

| 价值 | 覆盖范围 |
|---|---|
| `hy_mt2_1_8b` | 已使用固定的上游分词器源以及服务部署采样指导，对Hy-MT2 1.8B模型配置进行了测试。 |
| `hy_mt2_7b` | 已测试带有固定上游分词器源及服务部署采样指导的Hy-MT2 7B模型配置。 |
| `hy_mt2_30b_a3b` | 已对带有固定上游分词器源及服务部署采样指导的Hy-MT2 30B-A3B模型配置进行了测试。 |
| `generic` （或省略） | 未配置模式：未测试Hy-MT2分词器或采样指导规则。 |

该配置文件会在进程启动时被读取并**固定使用**。其他配置值仍可热加载，但磁盘上对 `[endpoint].profile` 的修改会被正在运行的会话忽略；如需使用不同配置文件，请重启 `hymt`。段缓存键和翻译历史记录会保留标准的配置文件标识，因此不同配置文件之间的结果不会相互共享。

### Telegram机器人（`[telegram]`)

默认配置中包含一个已禁用的Telegram板块：

```toml
[telegram]
enabled = false
stream = true              # progressively edit one response for multi-segment text translations
accept_documents = true      # accept .txt/.md documents
max_document_size = 1048576  # bytes; default 1 MiB
bot_token = ""          # or set HYMT_TELEGRAM_BOT_TOKEN
claim_password = ""     # auto-generated on first `hymt telegram` if empty
owners = []             # private chat ids after claim
groups = []             # group chat ids when mode = "groups"
mode = "owners"         # "owners" | "groups"
```

1. 创建一个机器人，使用 [@BotFather](https://t.me/BotFather), 设置 `bot_token` （或 `HYMT_TELEGRAM_BOT_TOKEN`).
2. 设置 `enabled = true`.
3. 跑 `hymt telegram` （长时间轮询，直到收到SIGINT或SIGTERM信号）。首次运行时，hymt会生成一个临时密码，将其存储在配置文件中，并打印一次。同时还会创建一个systemd用户服务。 `services/hymt-telegram.service`; `services/hymt-telegram-reload.path` 在……时重新启动它 `/usr/local/bin/hymt` 会被替换。SIGTERM会在进程退出前完成当前的更新。
4. 在与机器人的私聊中，发送声明密码（或 `/claim <password>`）成为所有者。支持多个所有者。
5. 授权所有者（以及配置的组在 `mode = "groups"`）为短信提供自动的中英互译功能，并支持UTF-8编码。 `.txt`/`.md` 文件。超过的文件 `max_document_size` 非文本文档将被拒绝；简短的译文会以内联形式回复，较长的译文则会以保留原始扩展名的文档形式返回。
6. 设置 `accept_documents = false` 选择不参与文档处理。
7. 使用该方式重新生成声明密码 `hymt telegram --regenerate-claim-password` （仅打印一次新值）。

密钥（`bot_token`、`claim_password`）不会在每次运行时重新显示。

## 快速开始

### 翻译文本、标准输入或文件

```bash
hymt "Hello world" -l zh
printf 'Release notes go here.\n' | hymt -l ja
hymt CHANGELOG.md -l fr -o CHANGELOG.fr.md
```

从文本、标准输入、文件、“batch”以及“translate-doc”生成的输出文件，会在目标目录中创建一个专用的临时文件，将所有字节写入并刷新后关闭该临时文件，然后再将其重命名为原文件名。写入失败或被取消时，原文件及其硬链接别名会保留；成功替换后，旧的别名则保持不变。指向普通文件的有效叶级符号链接以及目录符号链接依然有效。悬空链接、非普通目标以及权限错误的情况将被拒绝。现有文件的普通权限位会保留；新文件的权限则遵循进程的umask设置。ACL、扩展属性、所有权、特殊模式位、断电后的数据持久性以及并发路径替换等功能不在此保证范围内。在输出错误发生之前已写入的缓存/历史记录不会被回滚。

### 目标语言代码

所有的快速构建、验证、检测、输出文件名、CLI估算以及Telegram路由都使用同一个标准注册表。支持的标准代码有：

`zh`、`zh-Hant`、`en`、`fr`、`pt`、`es`、`ja`、`tr`、`ru`、`ar`、`ko`、`th`、`it`、`de`、`vi`、`ms`、`id`、`tl`、`hi`、`pl`、`cs`、`nl`、`km`、`my`、`fa`、`gu`、`ur`、`te`、`mr`、`he`、`bn`、`ta`、`uk`、`bo`、`kk`、`mn`、`ug`以及`yue`。

代码不区分大小写，并将“_”统一转换为“-”：`zh-CN`/`zh_CN`会转换为`zh`，而`zh-TW`/`zh_Hant`则转换为`zh-Hant`。`zh`、`zh-Hant`和`yue`都采用相同的中文类CJK处理方式；Hy-MT2配置文件对所有支持的目标语言也使用同一套规则。

### 保持流畅的流媒体体验

流式传输默认是启用的。这意味着你可以继续使用常规的shell管道。

```bash
hymt article.md -l zh | less
hymt notes.txt -l ja | bat -l markdown
hymt report.md -l zh | tee report.zh.preview.md
```

如果需要完全缓冲的响应，请使用`--no-stream`。

使用 `--concurrency N` 强制单次运行时的并发数（会覆盖 `[translation].concurrency` 的设置）。在排查多段处理阻塞问题时，可使用 `--debug-chunk-timing`（或 `HYMT_DEBUG_CHUNK_TIMING=1`）将每个数据块的队列处理时间、请求处理时间、首个令牌处理时间以及完成时间输出到标准错误流中。

### 混合语言文档规划

对于中文系目标语言（`zh`、`zh-Hant`和`yue`），hymt会在对请求进行分块处理之前先逐段处理文档。在默认设置`[translation].language_detection = true`的情况下，如果某段的CJK字符比例超过60%，且至少包含四个经过分析的非空白字符，则该段落会被保留下来。其原始的UTF-8字节在重建过程中会保持不变；其他段落则会被发送给模型处理。

Markdown标题、列表项、引文块和表格行都遵循相同的段落规则。带边框的代码块以及开头的YAML前置内容始终会被保留。那些非常简短、类似代码或含义模糊的片段会被翻译，而非直接被视为目标语言内容。对于文本、标准输入和文件输入，使用`--plan`选项时，会输出每个段落的检测元数据，包括`is_target_language`和`should_translate`字段。

全局`--plan`参数支持文本、标准输入、文件输入以及`batch`模式；其他子命令，包括`translate-doc`，在访问配置、后端或输出内容之前就会拒绝该参数。支持的规划器可以读取后端元数据以及兼容的缓存/历史数据，但不会进行翻译或生成输出结果。

使用单次覆盖或配置来翻译所有非代码段落：

```bash
hymt --force-translate-all -l zh "English text\n\n已有中文段落"
hymt --no-language-detection -l zh article.md
```

```toml
[translation]
language_detection = true      # default: use CJK detection for Chinese-family targets
force_translate_all = false    # default: false; set true to translate all non-code paragraphs
```

`--force-translate-all`、`--no-language-detection`、`force_translate_all = true`以及`language_detection = false`都会选择全量翻译模式。明确的 `-l/--lang` 参数可指定目标语言，但**不会**关闭内容保留功能。语言检测功能仅针对中文字体：对于非中文目标语言，hymt会翻译所有非代码段落，而不会尝试进行通用多语言检测。

## 智能分割与缓存重用

应用缓存和整体输出缓存默认处于关闭状态。如需启用，可设置 `[cache].enabled = true`。`--refresh-app-cache` 会跳过现有的应用缓存条目，处理当前的翻译请求，然后将符合条件的新结果写回；即使在 `[cache].enabled = false` 的情况下它也能正常工作，且不会改变这一默认设置。服务推理缓存仍可能重复使用相同的请求。而 `--no-cache` 则会绕过应用缓存和服务缓存的读写操作，它与 `--refresh-app-cache` 存在冲突。这两个参数均适用于普通翻译、批量处理、手册/信息查询、执行命令以及 Telegram 命令。禁用应用缓存不会影响旧条目、翻译历史记录、时间估算值以及召回功能。协议完整性重试会绕过服务缓存，但不会改变相关请求；网络重试则会保留原始请求和缓存头部信息。即使处于默认设置下，`hymt exec precache` 也会明确为应用缓存填充数据；若将其与任何缓存覆盖参数一起使用，则会被拒绝。

通过设置`[cache].enabled = true`，符合条件的已完成运行任务会在任务输出之外保留有限度的证明信息。当服务不可达时，可通过匹配的源数据、目标数据、当前生成的提示词以及适配器请求字节、分词器、有序所有权规则、限制条件与传输策略，在远程预检之前在本地进行重放操作，包括严格的预检流程。历史运行记录会用于重放验证，但不会用于授权新的请求或生成冷启动计时样本。缺失、过时、损坏或已变更的记录则会按正常实时处理方式运作，而不会生成临时前缀。只有当每个环节都使用原始逻辑提示词且全部成功时，具有经过验证身份的完整运行任务才符合条件。成功的网络重试会重新发送相同的请求，并可能生成证明信息；而遵循协议完整性要求的重试则会使用修改后的提示词，因此不会生成证明。质量下降的结果以及通过旧版分段缓存获取的数据则不会生成证明。网络重试和离线重放操作均不会进行冷启动计时校准。流式请求与缓冲式请求仍保持独立处理。执行`hymt history --clear`命令可同时删除这些证明信息及其对应的任务记录。重放验证会检查请求和传输相关证据，但不会对模型生成的文本进行评判，也不会对当前的后端服务进行认证。


`hymt`会依据Hy-MT2分词器以及选定的提示模板来规划每次翻译。当启用应用缓存时，翻译后的内容会以特定键进行标识：

- 段落内容哈希值
- 目标语言
- 模板类型
- 模板选项
- `profile_id` （标准配置文件ID）
- 经过验证的推理指纹

该指纹包含配置的端点、后端及模型信息、配置文件模型/分词器来源、明确的采样设置，以及可获取时的运行时元数据。要重用片段缓存，需先启用缓存并验证推理身份；若身份未通过验证，则可在不使用片段缓存的情况下继续翻译。对于未出现在元数据中的模型字节或量化信息，该指纹无法提供证明。在完成不可见的模型就地替换后，需运行`hymt history --clear`以清除片段条目和任务历史记录，然后再进行重用。`config_version`会记录在任务历史中，而非片段缓存键中。

这可以实现：

- 运行中断后快速重试
- 仅修改几段内容时即可实现近乎即时的重写
- 在常规翻译、批量翻译、文档翻译以及已翻译的手册页面中重用缓存

进度始终以相同格式通过`stderr`输出：

```text
[done/total] XX.XX% | elapsed Xm Ys | eta Xm Ys | NN.NN tok/s
```

## 翻译 Markdown 文档

`translate-doc` 是用于双语 Markdown 树的结构化处理命令。

```bash
hymt translate-doc README.md
hymt translate-doc README.md -l ja
hymt translate-doc README.md -l zh --output README.zh-cn.md
hymt translate-doc docs/ --recursive
```

行为：

- 默认目标为 `zh`，并且Markdown输出会进行标准化处理 `.zh-cn.md`.
- 目录模式在转换Markdown文件时会保留相对路径。 `--output-dir` 被使用。
- 该框架能够保持完整的有序源代码覆盖、源代码特有的文字片段、请求预算以及回复路由。生成的模型文本会原样输出，包括空字符串、经过修改的Markdown格式、被省略的URL/占位符，以及作为JSON输入的非JSON文本。翻译遗漏或语义错误属于模型本身的问题，不会触发重试或原始源代码的替换。缓存命中时，会直接回放存储的文本，而不会对其进行内容判断。
- 普通的行内 Markdown 段落在源代码规划允许且最终请求预算充足时，会使用一个完整的请求。而引用、数学公式、转义后的语法、块级容器以及过大的段落则会采用基于源代码的分割方式；编译器会保留原始的段落和片段顺序，不会修改模型生成的标记或空白内容。
- 短暂的连接故障、超时、HTTP 408/429/500/502/503/504错误以及响应内容被中断的情况，可重新尝试最多 `[translation].max_retries` （默认） `100` 再次尝试； `0` 仍然会发送初始请求）。一个预算用于处理每次请求中的连接故障和请求体故障，退避时间分别为0.5秒、1秒、2秒、4秒，最多为8秒。 `translation.timeout` 适用于每次推理尝试。必需的后端元数据/分词请求也会重试，每次尝试的超时时间为3秒；可选的诊断功能仍为单次尝试。默认的缓冲流式处理会丢弃被中断的尝试，并重新播放相同的完整源数据块，而不会生成重复文本。明确严格模式 `warn_only=false` 可以选择“乐观流式处理”模式，这种模式下已发布的标记无法被撤销：如果传输过程中出现中断，系统会返回错误而不会重新发送这些标记。认证/配置错误以及格式完全错误的JSON数据会立即导致处理失败。资源耗尽时则会出现分段上下文错误，且不会将部分数据视为成功的翻译结果保存下来。取消操作会停止重试。50秒内的中断仍在可承受范围内，但无论预算多充足，都无法保证能从更长时间的中断中恢复过来。
- 协议截断（`length`) 以及缺失的流终止重试最多次数 `[completeness].max_retries`. 在耗尽之后，现有的 `warn_only=true` 该策略会保留完整的原始段落，并显示相应提示；严格模式以及原子JSON输入会导致错误。被保留的结果不会被存入成功的缓存中，也不会用于离线回放验证或定时采样。默认流式输出在发布前恢复失败段落的完整原文，并在正文末尾追加保留提示；完全缓冲的输出仍在正文前放置提示。网络重试次数耗尽，以及运行时、认证、配置或取消方面的永久性故障仍属于错误情况。完整的空字符串与无响应的情况是不同的。
- 源片段受扩展/上下文预算的限制。 `[translation].max_source_tokens_per_segment` （默认） `384`,你应该只输出中文。 `0` 禁用）。保守的默认设置可避免70亿参数模型在达到预设输出限制前就结束长文本翻译；只有在通过实际部署的端点验证了完整输出后，才可提高该限制。对于已下载分词器的固定Hy-MT2配置文件，规划器会在预留输出令牌之前，先处理并统计完整的聊天请求内容（角色设定、提示语/上下文、助手标识以及完整性重试相关信息）。 `--plan` 报告计数来源、配置文件/分词器/模板标识、每个槽位的容量、输入/输出统计情况以及任何片段修订信息。
- 如果活跃的配置文件/模板或标记器不可用，hymt会打印明确的标准错误警告，并采用保守的处理方式。 `2x` 输入的估算值加上一个数值 `64`– 令牌聊天框架限额。已设置 `[translation].strict_token_budget = true` 改为拒绝该近似路径；进行配置 `[endpoint].profile` 然后逃跑 `hymt tokenizer download` 用于地方预算编制。
- 被标记的代码会以源码形式保留。当规划器要排除过大的受保护Markdown块（包括表格）时，它会发出警告并保留原始字节数据，而不会将其拆分以便模型处理；周围的文本仍可被翻译。超出容量限制的原子JSON会在任何翻译请求之前被拒绝。

## 批量翻译目录树

对于`.md`和`.txt`文件，请使用`batch`命令。添加`--dry-run`参数可预览而不进行写入；否则它会同时预览并写入选定的输出内容：

```bash
hymt batch docs -l zh --dry-run
hymt batch docs -l zh --yes
hymt batch docs -l zh --output-dir translated-docs
```

批量预览报告：

- 已选择与已跳过文件
- 每个文件的缓存状态： `full`,你应该只输出中文。 `partial`，或 `none`
- 缓存的分段计数
- 每份文件的预计处理时间
- 预计总到达时间

## 翻译命令输出和手册

### 包裹终端命令

```bash
hymt exec -- cargo test
hymt exec -- git status
hymt exec precache
```

`hymt exec`会保留原始命令的输出，随后再添加翻译后的输出。它对于不熟悉的CLI工具、构建失败情况以及冗长的帮助文本非常有用。

### 阅读译文 `man` 和 `info`

```bash
hymt man git-rebase
hymt man --original git-rebase
hymt info coreutils
hymt info bash
```

## 回忆、历史记录与预计到达时间估算

翻译历史记录存储在 `~/.local/share/hymt/history.db` 的 SQLite 数据库中。

常用命令：

```bash
hymt history
hymt history stats
hymt recall
hymt estimate 10000 -l zh
```

历史力量：

- 近期输出召回
- 吞吐量统计
- 中位数/百分位数预计到达时间估算
- 反映实际令牌吞吐量的进度条

## Telegram机器人

```bash
# after configuring [telegram] and enabling it
hymt telegram
hymt telegram --regenerate-claim-password
```

关于声明所有权和群组模式，请参见上面的`[telegram]`配置部分。

## 时机差异自动提交备案

在完成交互式翻译后，`hymt`会将实际运行时间与历史估算值进行比较。当运行时间偏差超过`[timing].divergence_threshold`时，它会提示用户提交一个包含以下内容的GitHub问题：

- 令牌计数
- 段落数量
- 吞吐量统计
- 配置版本
- 模型元数据

这样就能更轻松地追踪服务器设置、并发性或提示行为方面的退化问题。

## 通过Tailscale远程连接Hy-MT2

该仓库在[`services/`](services)目录下包含两个互斥的systemd用户服务示例。`-c`表示服务级的上下文池；`--parallel`则用于将上下文分配给多个并发请求：

| 服务 | 模型量化 | KV缓存 | 全部上下文 | 并行插槽 | 每个槽位的大致上下文量 |
|---|---|---|---:|---:|---:|
| `hy-mt2-quality.service` | Q6_K | Q8（`q8_0`) | 24,576 | 3 | 8,192 |
| `hy-mt2-throughput.service` | Q4_K_M | Q4（`q4_0`) | 65,536 | 8 | 8,192 |

两者都仅绑定到Tailscale上的`100.78.159.38:8401`，而非`0.0.0.0`。它们使用CUDA版本的`llama-server`；两个版本均使用位于`/home/obj/.local/share/llama-cpp-cuda/llama-server`的本地构建版本及其对应的库目录。这些绝对路径是特定于主机的，但替代的后端必须支持所提到的`llama-server`配置、并行槽数以及KV缓存相关选项。

这两个服务配置文件均明确设置了`--temp 0.7`、`--top-k 20`、`--top-p 0.6`以及`--repeat-penalty 1.05`，这些参数均为Hy-MT2推荐的设置。同时，它们还设置了`--min-p 0`（禁用该仅适用于llama.cpp的扩展功能）和`--repeat-last-n 64`（这是为与llama.cpp的1.05重复惩罚机制兼容而特意选择的设置，并非Hy-MT2的原始推荐）。因此，这些参数不会从已安装的llama.cpp版本中继承任何偶然的采样器配置。除非有`[inference.override]`明确指定替代值，否则hymt会省略采样相关字段，所以更改服务默认设置也会改变默认的翻译结果。普通翻译会在启动前通过`GET /props`查询llama.cpp的状态并输出相关警告；而`hymt backend inspect`则可显示已标准化配置及最终确定的采样器参数。如果 `/props`接口不可用或版本过旧，系统的身份验证将无法进行，除非启用了严格的后端预检功能，否则系统会继续采用保守的处理方式。

### 可选的共享推理缓存

独立的明文缓存存储着完整的原始推理响应，包括各客户端以及API/VK凭据的使用情况。该缓存的标识信息来源于上游模型的版本、构建信息、默认设置、模板以及上下文元数据；在查询之前会先检测上游系统状态。在发生传输故障或HTTP 502/503/504错误时，已填写好的请求可以重新使用现有SQLite缓存中最后验证过的同源有效标识及时间戳。该缓存不会替代hymt的传输处理机制或分段缓存功能。

仅允许受信任的Tailnet调用者直接访问缓存，或将其置于现有的认证网关之后。API/VK凭证无法对缓存命中进行区分。

`just install-inference-cache`会将脚本和用户单元复制到固定路径，重新加载systemd，为后续登录启用缓存并启动该缓存。再次运行安装程序会使用已安装的代码重启正在运行的缓存。所提供的用户单元是针对特定主机的：它在Tailscale的`100.78.159.38:8402`端口上监听，并将请求转发到`100.78.159.38:8401`端的模型。

普通兼容客户端会自动使用共享路由缓存，无需任何特殊配置。对于Hymt，需设置`[endpoint].url = "http://100.78.159.38:8402/v1"`，保持`backend = "llama_cpp"`不变，并将`[endpoint].model`设置为上游 `/v1/models`所公布的实际ID或别名。示例配置中空的`model`字段意味着省略了该请求字段，因此无法触发缓存命中。直接使用端口`8401`会刻意绕过共享缓存；浏览器客户端应使用现有的认证网关。通用CLI端点的默认设置保持不变。应用程序的缓存功能默认处于关闭状态；安装完成后，共享服务器缓存才会启用。

跑 `python3 "$HOME/.local/share/hymt/inference-cache.py" clear --database "$HOME/.local/state/hymt/inference-cache.sqlite3"` 在替换模型字节而不更改已记录的元数据之后，清除操作会使已保存的条目以及正在处理的请求失效。同时，它还会移除保留的规划相关数据。只要缓存HTTP服务仍可访问，即便模型后端不可用——包括在缓存服务重启之后——它也能返回完整的JSON/SSE格式响应。保留数据则会被清除。 `/props` 和 `/v1/models` 携带 `X-Hymt-Planning: historical` 和 `X-Hymt-Observed-At`；CLI使用这些历史规划限制，而无需进行新的验证。有上限的精确分词响应会保留虚假EOS请求的偏差。严格的预检流程仍需实时验证。错误处理和显式绕过仍需后端支持。身份验证失败、元数据格式错误、TLS验证错误以及本地容量/数据库故障均不会允许重放操作。源地址/模型变更会导致旧证据失效。每个数据库都有一个私有的1字节字段。 `.guard` 该机制支持一个缓存进程的文件操作与许可处理。在完全恢复在线状态之前，被中断的身份查询或失败的无效化操作将不会被重试；守护进程会与其数据库保持同步。显式绕过机制仍保持透明性，不会检测缓存身份信息。该缓存独立于Hymt的翻译历史记录；但这并不会让无法访问的缓存服务在离线状态下可用。

## 建筑学

- `crates/hymt-core`：热重载TOML配置、提示模板、CJK语言工具，以及输入覆盖度/传输检查功能。
- `crates/hymt-segment`：Hy-MT2分词器集成，以及分层式和Markdown感知型分词功能。
- `crates/hymt-client`：异步的OpenAI兼容HTTP客户端，具备重试处理、并发限制以及SSE流式传输功能。
- `crates/hymt-cache`：SQLite段缓存和执行缓存、任务历史记录、召回率以及预计完成时间统计信息。
- `crates/hymt-translate`：翻译编排、传输完成重试、批量/文档工作流以及已翻译文档。
- `crates/hymt-cli`：鼓掌 `hymt` 二进制格式、命令调度、面向shell的交互行为，以及可选的Telegram子命令。

## 发展

只需安装一次仓库钩子，然后运行本地质量检查即可：

```bash
just install-hooks
just pre-commit
```

提交后，在干净的HEAD上运行一次完整的本地验收测试。

```bash
just pre-push-gate head
```

快速提交钩子会检查格式以及锁定工作区的功能。完整验证流程则还包括锁定工作区的测试/检查、推理缓存相关的Python契约测试、无需Telegram的命令行界面相关测试，每个被跟踪的Shell脚本的语法检查及ShellCheck检测，三个systemd单元的静态验证，以及每个被跟踪的TOML文件的解析检查。所有诊断信息依然可见；仅在未配置好环境的主机上，才允许缺少已知模型服务单元对应的标准持久可执行文件。

随后获取一份独立的完整“main...HEAD”原生审查报告，该报告需显示“通过”状态且无任何可采取行动的问题。将`HYMT_LOCAL_GATE_RECEIPT`设置为该审查环节生成的私有收据路径，将`HYMT_NATIVE_REVIEW_RECEIPT`设置为[原生审查收据格式](#native-review-receipt-format)中规定的独立审查收据路径。Lefthook会将指定的HEAD版本/树结构、范围以及日志/报告内容与实际推送的特征分支更新内容进行比对；对于未发生变化的内容，推送操作不会重新执行相关检查。源代码的更改需要生成新的收据。构建产物存储在私有SSD任务目录`/mnt/ssd/mirror-rootfs/home/obj/tmp/hymt-stop-ai-20261012`中，目前没有基于GitHub Actions的托管工作流。

### 原生审核回执格式

独立审查员必须对整个`main...HEAD`范围进行最彻底的审查，确认检查前后代码状态均无问题，并在上述私有SSD根目录下（目录权限为`0700`，普通非符号链接文件权限为`0600`）撰写正式的报告和记录。源代码的创建者不得自行给出“通过”结论，也不得复制伪造的测试用例证据。通过推送操作，应将代码状态固定在被审查的HEAD版本上。关卡生成器会编写真实的关卡记录，该记录并非基于此示例生成。

以下JSON为字段布局示意图，并非可使用的收据。真实收据恰好包含这13个字段，没有多余或重复项。请使用实际内容。 `git symbolic-ref --short HEAD`,你应该只输出中文。 `git rev-parse HEAD`,你应该只输出中文。 `git rev-parse 'HEAD^{tree}'`，和 `git merge-base main HEAD` 价值观； `outgoing_oid` 是同一个HEAD。 `report_sha256` 是实际报告字节的SHA256值，例如来自 `sha256sum -- "$report"`. 这个 `command` 字段显示的是确切的元数据字面值，而非生成收据的命令。

```json
{
  "version": 1,
  "status": "PASS",
  "branch": "<checked-out-feature-branch>",
  "head": "<actual-HEAD>",
  "tree": "<actual-HEAD-tree>",
  "base": "<actual-main-HEAD-merge-base>",
  "range": "main...HEAD",
  "outgoing_oid": "<same-actual-HEAD>",
  "clean_before": true,
  "clean_after": true,
  "command": "native review main...HEAD",
  "report": "<absolute-private-SSD-report-path>",
  "report_sha256": "<SHA256-of-that-actual-report>"
}
```

实际报告必须恰好包含一行匹配的`HEAD`、`TREE`、`BASE`和`RANGE`元数据行。其前两行和后两行非空白内容必须分别为`VERDICT: PASS`和`ACTIONABLE_COUNT: 0`；所有判定/计数行内容必须一致。只有当该概要报告被独立审核员的实际全范围检测结果及真实的Git标识所替代时，才具有有效证据效力：

```text
VERDICT: PASS
ACTIONABLE_COUNT: 0
HEAD: <actual-HEAD>
TREE: <actual-HEAD-tree>
BASE: <actual-main-HEAD-merge-base>
RANGE: main...HEAD
<independent whole-range review evidence>
VERDICT: PASS
ACTIONABLE_COUNT: 0
```

请将下面的占位路径替换为真实的收据信息。从已检出的特性分支向同一个远程分支名称推送恰好一次非删除类型的更新；此明确的引用规范会携带当前的HEAD指针。多次更新、标签操作、删除操作、受保护的分支、不匹配的引用/OID、分离的HEAD状态、脏状态以及过时或被篡改的收据信息都将被拒绝。

```bash
export HYMT_LOCAL_GATE_RECEIPT="<gate producer's actual printed receipt path>"
export HYMT_NATIVE_REVIEW_RECEIPT="<independent reviewer's actual receipt path>"
branch="$(git symbolic-ref --short HEAD)" &&
    git push origin "refs/heads/$branch:refs/heads/$branch"
```
