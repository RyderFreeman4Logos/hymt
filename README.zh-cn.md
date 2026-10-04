[中文版](README.zh-cn.md)

# hymt

[![许可证：Apache-2.0](https://img.shields.io/badge/license-Apache--2.0-blue)](LICENSE)
![模型](https://img.shields.io/badge/model-Hy--MT2-orange)
![平台](https://img.shields.io/badge/platform-Linux-lightgrey)

Hy-MT2是一款实用的Rust命令行工具：具备分词器感知的分段处理能力、分段级缓存复用功能、管道安全的令牌流处理、支持Markdown格式的文档翻译、批量翻译功能、命令输出翻译功能，以及可热重载的配置系统。

`hymt`专为那些需要处理真实终端界面和Markdown格式内容的人设计，而不仅仅适用于一次性字符串的翻译。它会将处理进度输出到`stderr`，翻译结果输出到`stdout`，记录历史数据以估算完成时间，并能在模型表现远超历史预期时自动记录时间偏差问题。

## 为何选择hymt

- 通过一个命令即可翻译定位的文本、标准输入内容或文件。  
- 基于Hy-MT2分词器对长文本进行分段，而非盲目分割。  
- 在分段层面重用缓存翻译结果，使重复内容几乎能瞬间获取。  
- 默认以流式方式输出分词结果，从而保证`| less`、`| bat`和`| tee`等操作流程的响应速度。  
- 使用具备结构识别能力的边界对Markdown文本进行分段；具体多语言处理限制见下文。  
- 可批量处理整个目录树，在写入前预览缓存状态及预计完成时间。  
- 可用`hymt exec`包裹任意shell命令，或浏览已翻译的`man`和`info`文档。  
- 可调出之前的输出结果，并查看带有处理量统计信息的翻译历史记录。  
- 使用`hymt translate-doc`可保持双语Markdown文档的同步。  
- 还提供可选的Telegram机器人（`hymt telegram`），用于多用户私有场景下的内容管理，以及支持`.txt`和`.md`格式文档的中英互译。  

## 安装

### 安装方式

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
# first_chunk_priority = false
# debug_chunk_timing = false
# Refuse planning when the final chat template cannot be tokenized locally.
# Otherwise hymt emits a warning and uses a conservative approximate budget.
strict_token_budget = false
# Refuse translation before cache lookup if the backend runtime cannot be verified
# or differs materially from this configuration.
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
zh_to_en_min_ratio = 0.3
en_to_zh_min_ratio = 0.3
min_paragraph_ratio = 0.5
max_retries = 2
# After retries are exhausted, a non-empty best attempt meeting the 33% completeness
# floor is written. Set true (or pass --warn-only-completeness) to keep exit 0 with
# warnings for this degraded best-effort result.
# Empty or below-33% candidates are unrecoverably incomplete: they are rejected as
# errors and are not written, even when warn_only is true.
warn_only = false

[timing]
divergence_threshold = 2.0
```

该配置支持热重载，但`[endpoint].profile`除外，它在启动时会被固定（详见[模型配置](#model-profile-endpointprofile)）。长时间运行的工作流无需重启即可应用其他更改。

### 后端特定采样设置（`[endpoint].backend`）

需从服务器实现中明确选择后端类型；hymt不会从端点URL中推断后端类型。生成的配置会默认选择`llama_cpp`。如果未指定该键，hymt会使用保守的`openai_compatible`模式，且不发送任何非标准采样扩展参数。

| 后端类型 | 支持的覆盖字段 | 后端特定的数据传输规则 |
|---|---|---|
| `llama_cpp` | `temperature`、`top_p`、`top_k`、`repetition_penalty`、`min_p`、`repeat_last_n` | `repetition_penalty`以`repeat_penalty`形式发送；被禁用的`top_k`和`repeat_last_n`则发送为`0`。 |
| `vllm` | `temperature`、`top_p`、`top_k`、`repetition_penalty`、`min_p` | `repetition_penalty`以`repetition_penalty`形式发送；被禁用的`top_k`发送为`-1`；`repeat_last_n`会被忽略。 |
| `openai_compatible` | `temperature`、`top_p` | 仅发送常见的聊天完成相关字段；所有非标准的显式覆盖参数都会被拒绝，而不会被猜测。 |
若省略了`[inference.override]`键，其值始终为`Setting::ServerDefault`，因此该键不会出现在JSON请求中，服务会使用自身配置的数值。所有Hy-MT2配置文件也是如此：配置文件中的采样值仅作为服务部署的参考，绝不会自动被注入到请求数据中。只有当客户端需要刻意替换服务默认值时，才应设置数值型覆盖值；`"disabled"`和数值属于语义配置状态，适配器会选择文档中规定的数值，而不会随意将`0`和`-1`进行转换。明确的覆盖值会被包含在诊断信息以及推理/缓存指纹中。直接位于`[inference]`下的旧版标量采样值在某个版本中仍可被视作明确覆盖值，但会触发启动迁移警告；建议将其移至`[inference.override]`下。验证错误会指出对应的语义值，并说明何时不存在后端对应的数值表示形式。流式请求与非流式请求使用相同的适配器策略。

上述支持的扩展名仅限于文档中记载的llama.cpp服务器控制选项以及vLLM OpenAI-server采样参数：[llama.cpp服务器API](https://github.com/ggml-org/llama.cpp/blob/master/tools/server/README.md)和[vLLM OpenAI兼容服务器](https://docs.vllm.ai/en/latest/serving/openai_compatible_server/)。旧的`[inference].backend`键已被废弃，应将其移至`[endpoint].backend`下。

### 运行时后端预检与检查

在规划或缓存查找之前，翻译引擎会先检测配置的后端。llama.cpp使用`GET /props`；而vLLM及其他兼容OpenAI的服务在可用时会使用`GET /v1/models`。标准化的运行时状态包括服务版本/模型标识、上下文与槽位限制、采样器默认设置，以及明确声明的功能。缺失的字段将保持未知状态，不会被推测出来。

当llama.cpp的 `/props` 返回单个ASCII标点符号`eos_token`，且聊天模板中包含`<|...|>`控制标记时，hymt会检测到矛盾：模板中的标记才是真正的结束符，而元数据中的标记则是假的。它会在预检阶段通过 `/tokenize` 处理这个错误的标记，并向llama.cpp发送`logit_bias`参数`{ "<token_id>": -100 }`，从而避免错误停止，同时不会屏蔽聊天模板中真正的结束符标记（如`}`）。聊天模板本身使用的单字符结束符则保持不变。

运行`hymt backend inspect`可并列显示配置值和服务检测到的值。该命令不会输出API密钥、端点凭证或请求头信息。警告信息会指出上下文/模型/配置文件不匹配、服务采样器状态异常，以及llama.cpp与vLLM之间的重复惩罚参数不一致问题。

正常的预检机制为“失败即拒绝”模式：它会发出警告，标记推理指纹/缓存标识未被验证，并设置较为保守的上下文/输出限制。若无法验证身份或发现重大不匹配，可设置`[translation].strict_backend_preflight = true`，在查询缓存或调用模型之前直接拒绝请求。运行时状态会以TTL机制缓存60秒，当端点/后端/配置发生变化时会被刷新；新的服务身份将取代原有的解析状态和指纹。

## 模型配置文件（`[endpoint].profile`）

对于Hy-MT2端点，需明确设置`[endpoint].profile`。目前支持的值及其对应说明如下：

| 值 | 说明 |
|---|---|
| `hy_mt2_1_8b` | 经过测试的Hy-MT2 1.8B模型配置文件，配有固定的上游分词器来源及服务部署采样指南。 |
| `hy_mt2_7b` | 经过测试的Hy-MT2 7B模型配置文件，配有固定的上游分词器来源及服务部署采样指南。 |
| `hy_mt2_30b_a3b` | 经过测试的Hy-MT2 30B-A3B模型配置文件，配有固定的上游分词器来源及服务部署采样指南。 |
| `generic`（或未设置） | 无特定配置模式：不使用任何经过测试的Hy-MT2分词器或采样指南。 |

该配置文件会在进程启动时被读取并固定下来。其他配置值仍可热加载，但直接在磁盘上修改`[endpoint].profile`的值不会被正在运行的会话识别；如需使用不同配置文件，需重启`hymt`程序。段缓存键和翻译历史记录会保留标准的配置文件标识，因此不同配置文件之间的结果不会相互混用。

### Telegram机器人（`[telegram]`）

默认配置中包含一个已禁用的Telegram部分：

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

1. 通过[@BotFather](https://t.me/BotFather)创建机器人，设置`bot_token`（或`HYMT_TELEGRAM_BOT_TOKEN`）。  
2. 设置`enabled = true`。  
3. 运行`hymt telegram`（持续长轮询，直到按下Ctrl+C）。首次运行时，hymt会生成一个声明密码，将其存储在配置中并显示一次。  
4. 在与机器人的私聊中，发送该声明密码（或输入`/claim <password>`）即可成为所有者。支持多个所有者。  
5. 经授权的所有者（当`mode = "groups"`时还包括已配置的群组）可自动实现文本消息以及UTF-8格式的`.txt`/`.md`文档的中英互译。超过`max_document_size`限制的文档或非文本文档将被拒绝处理；简短翻译会以内联形式回复，较长翻译则会以原始扩展名保存为文档返回。  
6. 若希望关闭文档处理功能，可设置`accept_documents = false`。  
7. 使用`hymt telegram --regenerate-claim-password`可重新生成声明密码（新密码仅显示一次）。  

`bot_token`和`claim_password`这些敏感信息不会在每次运行时再次显示。  

## 快速开始  

### 翻译文本、标准输入内容或文件

```bash
hymt "Hello world" -t zh
printf 'Release notes go here.\n' | hymt -t ja
hymt -f CHANGELOG.md -t fr -o CHANGELOG.fr.md
```

### 目标语言代码

所有的提示语构建、验证、检测、输出文件名、CLI估算以及Telegram路由均使用统一的编码规范。支持的标准代码包括：

`zh`、`zh-Hant`、`en`、`fr`、`pt`、`es`、`ja`、`tr`、`ru`、`ar`、`ko`、`th`、`it`、`de`、`vi`、`ms`、`id`、`tl`、`hi`、`pl`、`cs`、`nl`、`km`、`my`、`fa`、`gu`、`ur`、`te`、`mr`、`he`、`bn`、`ta`、`uk`、`bo`、`kk`、`mn`、`ug`和`yue`。

这些代码不区分大小写，且会将`_`转换为`-`：`zh-CN`/`zh_CN`会被解析为`zh`，而`zh-TW`/`zh_Hant`则会被解析为`zh-Hant`。`zh`、`zh-Hant`和`yue`在处理中文相关内容时采用相同的规则；Hy-MT2配置文件也为所有支持的目标语言使用这一统一编码规范。

### 保持与流式处理的兼容性

流式处理为默认启用状态。这意味着你可以继续使用常规的shell管道操作：

```bash
hymt -f article.md -t zh | less
hymt -f notes.txt -t ja | bat -l markdown
hymt -f report.md -t zh | tee report.zh.preview.md
```

如果需要完全缓冲的响应，请使用 `--no-stream`。

若想强制单次运行的并发度，可使用 `--concurrency N`（会覆盖 `[translation].concurrency` 的设置）。在排查多段处理阻塞问题时，可使用 `--debug-chunk-timing`（或 `HYMT_DEBUG_CHUNK_TIMING=1`）在标准错误输出中打印每个数据块的队列处理时间、请求处理时间、首个字符处理时间以及整体完成时间。

### 混合语言文档的处理策略

对于中文系目标语言（`zh`、`zh-Hant` 和 `yue`），hymt 会在对请求进行分段处理之前，逐段规划文档结构。在默认设置 `[translation].language_detection = true` 的情况下，如果某段的 CJK 字符占比超过 60%，且至少包含四个已分析的非空白字符，则该段落会被保留。其原始 UTF-8 字节在重新构建时会保持不变；其他段落则会被发送给模型处理。

Markdown 标题、列表项、引文以及表格行也遵循相同的段落规则。代码块和开头的 YAML 前置内容始终会被保留。那些非常简短、类似代码或含义模糊的片段会被翻译，而不会被判定为已经是目标语言。对于文本、标准输入和文件输入，使用 `--plan` 选项可查看每段的检测元数据，包括 `is_target_language` 和 `should_translate` 的字段值。

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

`--force-translate-all`、`--no-language-detection`、`force_translate_all = true`以及`language_detection = false`都会选择全量翻译模式。显式的 `-l/--lang` 参数可指定目标语言，但**不会**禁用内容保留功能。该工具的检测功能仅针对中文字体：对于非中文目标语言，hymt会翻译所有非代码段落，而不会尝试进行通用多语言检测。

## 智能分词与缓存复用

hymt会根据Hy-MT2分词器及选定的提示模板来规划每次翻译任务。目前，每个翻译后的段落是通过以下信息进行缓存的：
- 段落内容哈希值
- 目标语言
- 模板类型
- 模板选项
- `profile_id`（标准配置文件ID）

因此可以实现配置文件的隔离。不过，段落缓存键目前还不包含端点/模型标识、分词器版本、量化设置或后端构建信息，以及推理采样参数（详见#115）。因此，这些参数的更改仍可复用旧版本配置文件中的缓存条目；`config_version`信息会记录在任务历史中，而非段落缓存键中。在这些参数实现自动隔离之前，需要先进行推理指纹识别。

这样一来就能实现：
- 在翻译过程中断后快速重新开始
- 当只有少数段落发生变化时能近乎即时地重新生成译文
- 在普通翻译、批量翻译、文档翻译以及手动翻译页面处理中都能复用缓存

翻译进度始终会以相同格式显示在`stderr`输出中：

```text
[done/total] XX.XX% | elapsed Xm Ys | eta Xm Ys | NN.NN tok/s
```

## 翻译 Markdown 文档

`translate-doc` 是用于处理双语 Markdown 文档的结构化命令。

```bash
hymt translate-doc README.md
hymt translate-doc README.md -t ja
hymt translate-doc README.md -t zh -o README.zh-cn.md
hymt translate-doc docs/ --recursive
```

行为特性：

- 默认目标语言为`zh`，Markdown输出文件会自动转换为`.zh-cn.md`格式。
- 当使用`--output-dir`参数时，目录模式会翻译Markdown文件并保留相对路径。
- 完整性验证采用快速的分层截断/结构检测机制——**并非**翻译质量评估或语义正确性验证。对于已校准的英语/中文目标语言，密度指的是输出内容与输入内容的比例，该比例采用分段工具在缺少分词器时的相同计算方式，即`ceil(utf8_bytes / 4)`。由于中文每个单词对应的Unicode标量值更少，因此这种基于标量值的比率无法用于判断完整的英语到中文文本转换结果。其他目标语言会明确标注`unverified_density`，而不会默认认为密度验证已通过。该机制还会检查调用方提供的空响应或终止响应、段落、Markdown标题和代码块、占位符、URL以及有效的JSON模板。缓存的分段内容在再次使用前会通过当前的验证机制进行重新检查。
- 失败的片段会最多重试 `[completeness].max_retries` 次；普通模式、流式处理、批处理以及 `translate-doc` 模式下的片段验证均适用此规则。当所有重试次数用尽后，系统会保留验证得分最高的尝试结果（该得分基于可观察的防护信号，而非 QE 分数），并记录 `reason=highest_validation_score`。若存在符合 33% 源文本token最低要求且非空的最佳尝试结果，则视为普通降级处理：`hymt` 会将其保存，并在标准错误流中输出 `completeness_status=degraded_best_effort` 以及 `completeness_degraded_segments=…`。若不存在此类最佳尝试结果，或其得分低于最低要求，则表示该结果已无法修复，系统会直接拒绝并报错，而不会将其保存。对于 `translate-doc` 模式，被拒绝的文档不会影响任何已存在的输出文件；目录级翻译会继续处理后续文档，但只要有任何文档处理失败，整体任务就会以非零状态退出。顶级文本/文件/标准输入命令及其流式处理版本在出现普通降级结果时也会以非零状态退出，以便脚本能够检测到这些情况；若希望仅针对符合条件的降级结果发出警告而保持退出码为 0，可传递 `--warn-only-completeness` 参数或设置 `[completeness].warn_only = true`。默认情况下，`batch`、`translate-doc` 和 `exec` 模式会输出相同的标准错误标记，但不会因存在符合条件的降级片段而导致整个任务失败。验证通过后，流式处理会暂存该片段直至其通过验证；而乐观流式处理无法撤回已输出的无效token，因此会报告降级处理结果而非重新尝试。
- 源文本段受扩展量/上下文预算以及`[translation].max_source_tokens_per_segment`（默认值为384，设为0则禁用该限制）的约束。这一保守的默认设置可避免7B模型在达到预设输出长度前就结束长文本翻译；只有在确认部署后的接口能生成完整输出后，才可提高该限制值。对于已下载对应分词器的固定Hy-MT2配置文件，规划器会在预留输出令牌之前，先处理并统计完整的聊天请求内容（角色设定、提示语/上下文、助手标识以及完整性重试相关信息）。「--plan」选项会显示统计来源、配置文件/分词器/模板信息、各字段容量、输入/输出数据分布，以及任何文本段修改情况。
- 若当前使用的配置文件/模板或分词器不可用，hymt会输出明确的标准错误警告，并采用保守的2倍输入量估算值，同时额外预留64个令牌用于聊天内容格式处理。如需拒绝这种近似处理方式，可设置`[translation].strict_token_budget = true`；若需本地进行预算控制，则需配置`[endpoint].profile`并运行`hymt tokenizer download`命令。
- 尺寸过大的代码块或受Markdown表格保护的块，在提交任何HTTP请求之前就会因`ProtectedBlockTooLarge`错误而无法处理；此类内容需拆分处理，或移出模型处理范围。

## 批量翻译目录树结构

若希望对`.md`和`.txt`文件采用先预览的流程，可使用`batch`命令：

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

- 最新输出回溯
- 处理量统计
- 中位数/百分位数预计完成时间估算
- 反映实际字符处理量的进度条

## Telegram机器人

```bash
# after configuring [telegram] and enabling it
hymt telegram
hymt telegram --regenerate-claim-password
```

有关声明所有权和群组模式的信息，请参见上文的`[telegram]`配置部分。

## 自动提交时间偏差问题报告

在完成交互式翻译后，`hymt`会将实际运行时间与历史预估值进行比较。当运行时间偏差超过`[timing].divergence_threshold`时，它会提示用户提交一个GitHub问题，其中应包含以下信息：

- 令牌数量
- 分段数量
- 处理效率统计数据
- 配置版本
- 模型元数据

这样就能更便于追踪服务器设置、并发处理能力或提示词处理行为方面的问题。

## 通过Tailscale实现远程Hy-MT2运行

该仓库的`[services/`](services)目录下包含两个互不兼容的systemd用户服务示例。`-c`用于定义整个服务的上下文池；`--parallel`参数则用于将上下文分配给多个并发请求：

| 服务名称 | 模型量化级别 | KV缓存大小 | 总上下文容量 | 并发槽位数 | 每个槽位的上下文容量 |
|---|---|---|---:|---:|---:|
| `hy-mt2-quality.service` | Q6_K | Q8 (`q8_0`) | 24,576 | 3 | 8,192 |
| `hy-mt2-throughput.service` | Q4_K_M | Q4 (`q4_0`) | 65,536 | 8 | 8,192 |

这两个服务都仅绑定到Tailscale上的`100.78.159.38:8401`端口，而非`0.0.0.0`。它们均使用CUDA版本的`llama-server`；质量优化版本对应的是本地持久化构建的版本，而高处理效率版本则对应`llama-cpp/9294-cuda`构建版本。这些绝对路径是特定于宿主机的，但替代的后端必须支持所指定的`llama-server`上下文配置、并行槽位设置以及KV缓存功能。

两个服务配置均明确设置了`--temp 0.7`、`--top-k 20`、`--top-p 0.6`以及`--repeat-penalty 1.05`，这些均为Hy-MT2的推荐参数。同时，它们还设置了`--min-p 0`（禁用该仅适用于llama.cpp的扩展功能）和`--repeat-last-n 64`（这是为适配1.05版本的重复惩罚机制而特意采用的llama.cpp兼容设置，并非Hy-MT2的原始推荐值）。因此，这些参数不会从已安装的llama.cpp版本中随机继承任何采样配置。除非有`[inference.override]`指令明确指定替代值，否则HymT会忽略所有采样相关字段，所以修改服务默认参数也会改变默认的翻译结果。在启动时，llama.cpp客户端会发送`GET /props`请求，并输出其报告的原始`default_generation_settings`值；如果该接口不可用或版本过旧，系统会发出警告并仅忽略相关请求，不会报错。

## 架构

- `crates/hymt-core`：支持热重载的TOML配置、提示词模板、CJK语言处理工具以及完整性检测算法。  
- `crates/hymt-segment`：集成Hy-MT2分词器，同时具备分层分割和Markdown兼容的分割功能。  
- `crates/hymt-client`：异步的OpenAI兼容HTTP客户端，支持重试处理、并发限制以及SSE流式传输。  
- `crates/hymt-cache`：SQLite格式的片段缓存与执行缓存，以及任务历史记录、召回率统计和预计完成时间数据。  
- `crates/hymt-translate`：翻译任务协调功能、完整性检测重试机制、批量/文档处理流程以及翻译后的文档输出。  
- `crates/hymt-cli`：基于Clap框架的`hymt`命令行工具，负责命令分发、Shell交互功能，还支持可选的Telegram子命令。  

## 开发指南

只需安装一次仓库钩子，然后运行本地质量检测工具即可：

```bash
just install-hooks
just pre-commit
```

验证在没有Telegram依赖的情况下该二进制文件仍能构建：

```bash
just check-no-telegram
```

Lefthook在提交代码前会执行“just pre-commit”操作，并同步更新README文件的翻译内容。GitHub Actions CI则在拉取请求以及代码推送到`main`分支时运行，负责处理格式检查、Clippy工具检测、工作区测试/检查、禁止默认功能的CLI检查、Shell脚本检查、服务单元验证以及TOML格式解析等工作。
