# Skill Router 写作规范

| 字段 | 值 |
|------|-----|
| spec-version | `1.3` |
| 生效日期 | 2026-10-24（UTC+8） |
| 上游工程计划 | 工作区 `skill-routing-plan.md`（§2 / §5 / §6 / §9） |
| 基线证据 | 工作区 `skill-baseline-notes.md`（Phase 0） |
| Changelog | 见文末 |

本文是「什么是路由 skill、怎么写、何时不该写」的唯一约定源。  
**新模型只需读本文，即可写出合规路由 skill 或判断是否建路由。**  
阈值与工程上下文若与口头讨论冲突，以本文与 `skill-routing-plan.md` 更新后的正文为准。

---

## 1. 术语

| 术语 | 含义 |
|------|------|
| **leaf skill（子 skill / 普通 skill）** | 可被 listing 直接命中的独立 skill；正文是操作手册。 |
| **路由 skill（router）** | 仅有元数据进 listing；打开后提供**索引表 + 选路规则**，把任务导向 leaf skill 或模块文件。路由不是操作手册。 |
| **模块文件** | 路由包内、路径可打开的 `.md`（如 `modules/jadx-cli/jadx-cli.md`）。模块**可以**没有独立 listing 条目。 |
| **listing** | 会话中注入的 skill 元数据列表：`name` + `description` + `when-to-use` + 绝对路径。**不含正文。** |
| **台账** | 跨会话判断记录，默认 `~/.grok/skill-router-ledger.md`。 |

---

## 2. 硬性设计原则（不可在单份路由 skill 里违反）

1. **元数据进 listing，正文按需读**——路由与 leaf 的 listing 都只有 description / when-to-use；正文只在 `Skill` 打开或读文件时进入上下文。
2. **路由是加速器，不是门禁**——见 §7 无白名单硬句。
3. **路由自身也走渐进披露**——路由 listing 只有其 description / when-to-use；索引表只在打开路由后出现。
4. **判断可否决、可暂缓**——默认倾向「不建路由」，直到 §5 阈值满足。
5. **变更可回滚**——建路由只增文件与台账，不删 leaf skill；回滚 = 删路由文件 + 台账标 `cancelled`。

---

## 3. 何时算「路由 skill」（形态判定）

同时满足：

1. frontmatter 声明自己是领域**总入口 / 路由**（description 或正文定位句明确）。
2. 正文以 **§6 固定小节** 组织，含索引表；**不是**某一工具的长篇操作手册。
3. 打开路由后的主路径是：选路 → 打开**一个** leaf / 模块 → 在该文档内干活。

不满足 (2)(3) 的，即使名字带 `-router`，也按普通 skill 对待。

---

## 4. 文件布局

### 4.1 新建路由（默认，冻结）

一律采用 **形态 A**：

```text
~/.grok/skills/<domain>-router/SKILL.md
# 或项目级：
<project>/.grok/skills/<domain>-router/SKILL.md
```

- `name`：`<domain>-router`（如 `retrieval-router`、`android-device-router`）。
- 子目标是**已存在的独立 leaf skill** 时，索引表用 **skill 名** 指向，不复制其正文。

### 4.2 伞形包（既有 `re` 类，修订时允许）

**形态 B**（仅用于已存在的伞形结构，或子文档明确为「模块文件、不进 listing」时）：

```text
~/.grok/skill-store/<domain>/SKILL.md
~/.grok/skill-store/<domain>/modules/<module>/<module>.md
# 或经 junction：
<project>/.grok/skills/<domain>/SKILL.md
<project>/.grok/skills/<domain>/modules/...
```

- 伞形包的 `SKILL.md` 仍必须满足本文正文小节与无白名单条款。
- 模块文件可以自带 frontmatter（便于人类检索），但**默认不进入 listing**（它们不在 `.grok/skills/<name>/SKILL.md` 位置）。

### 4.3 决策（已定）

| 情形 | 布局 |
|------|------|
| 新建、子目标是独立 listing skill | **A**（强制默认） |
| 修订既有 `re` 式伞形包 | **B**（保持结构，改文案合规） |
| 新建且子文档只当路径手册、不需要独立 listing | 可 B，但台账须写 `form=b, modules-not-listable` |

---

## 5. 何时**不**建路由（反例 / 阈值）

执行顺序与工程计划 §5.2 一致。**任一「否」即停**；不要为了整齐建路由。

| # | 检查 | 否 → 动作 |
|---|------|-----------|
| 1 | 候选是否共享同一「分析对象层 / 用户目标动词 / 工具链」？ | **不建**（`rejected:cluster`） |
| 2 | 仅凭 listing 的 description，模型/用户是否**经常分不清**该用谁？（描述已正交则无收益） | **不建**（`rejected:ambiguity`）——先改差 description，歧义仍在再议 |
| 3 | 易混同类是否 **≥ 3**？（=2 且高度缠结、用户话术几乎总是总称 → 可建，台账 `accepted:size=2`） | **暂缓**（`pending:size`）或不建 |
| 4 | 能否用 **≤ 8 行**索引表 + **≤ 6 条**选路规则区分？ | **不建**（`rejected:not-tabular`）或先改描述 |
| 5 | 是否已有 accepted 路由覆盖同域？ | **不新建平行路由**，走吸收（工程计划 §6） |
| 6 | 创建/维护成本是否可接受？是否仍不删 leaf？回滚是否清楚？ | 否 → `pending` / `rejected` |

### 5.1 明确禁止的建路由理由

- 仅因「看起来整齐 / 好分类」。
- 用路由代替修 description（先改描述）。
- 为「隐藏」子 skill、做独占入口而建路由。
- 只有 2 个 skill 且 listing 描述已可分。

### 5.2 允许的边缘情形

- `size=2` 且用户话术高度总称、描述缠结 → 可 `accepted:size=2`。
- 试点/评测需要对照实验 → 台账 `pending`，不写进生产 listing，或短时 accepted 后可 `cancelled`。

### 5.3 建路由判定程序与输出块（Phase 3）

**输入**：候选集合 S（≥2 个 skill 的 name / description / when-to-use / path）；台账中 accepted 的现有路由集合 R。

**步骤**：按 §5 表 1→6 顺序执行；任一「否」即停，不得跳步。与 R 重叠时改走 §5.4，不新建平行路由。

**默认输出块**（每次判定原样写入台账 Decisions 表，便于跨会话续）：

```markdown
### <date> 建路由判定
- 集合: a, b, c
- 结论: accepted | rejected | pending
- 关键理由: <一行>
- 若 accepted: 路由名、路径、索引表草稿（≤8行）
- 若 rejected: 哪条阈值否决（如 rejected:ambiguity）
```

### 5.4 吸收判定：新 skill → 已有路由（Phase 3）

**触发**：SkillDiscoveryReminder 出现新 listing 条目；用户安装/放入 `skills/` 后；或用户明示「要不要挂进 X」。

**步骤**：

1. 只读新 skill 的 description + when-to-use（边界不清再读 frontmatter/首页；**不读全文**）。
2. 对每个台账 accepted 路由 R：领域是否重叠？R 索引是否已有等价行？
3. 按矩阵决策：

| 情形 | 动作 |
|------|------|
| 属 R 域，表无等价行，且仍满足 §5 歧义条件 | **absorb**：R 索引 +1 行 + 台账 |
| 属 R 域，但描述已正交、用户可直达 | **不吸收**，台账 `noted`（可选） |
| 不属任何 R 域 | **standalone**；同时跑 §5.3（是否与其它 leaf 新成簇） |
| 属 R 域但会破坏「≤8 行可表」 | **rejected-absorb**：建议改新 skill 描述或开新域路由 |
| 新 skill 实际是更好的总入口 | **demote/promote**：需人工确认后才 `accepted` |
| 不确定 | **pending**：不动 R 正文，台账留问题 |

4. **absorb 最小落盘**：路由索引 +1 行；台账 +1 条 `absorbed: new → R`。**不**改新 skill 的 frontmatter（无从属字段）。

---

## 6. Frontmatter 与正文

### 6.1 Frontmatter 必须

```yaml
---
name: <domain>-router          # 或既有领域名，如 re
description: <一句话：领域 + 总入口性质；含用户会说的关键词>
when-to-use: <触发短语：对象类型、动词、典型用户话术>
metadata:
  spec-version: "1.0"
  role: router                 # 可选但推荐
---
```

### 6.2 Listing 预算（Phase 0 实证约束；Phase 6 harness 上调）

| 项 | 限制 |
|----|------|
| `description` + `when-to-use` 合计 UTF-8 字节 | **≤ 900**（对齐 `MAX_LISTING_COMBINED_BYTES`，Phase 6 自 400 上调） |
| description | 建议 1–2 句；写清「总入口 / 路由」与领域关键词 |
| when-to-use | 分号分隔触发短语；避免重复 description 全文 |

超 900B 会在 listing 被截断（带 `…`），属于不合规草稿。路由 frontmatter 仍建议压在 400B 内——listing 更紧凑，且不依赖本上限。

### 6.3 Frontmatter 禁止

- `allowed-children`、`children`、`whitelist`、`exclusive`、`must-use-first` 等表示 **skill 排他/从属** 的字段。
- 任何「仅允许下列 skill 被使用」类 metadata。
- （`allowed-tools` **允许**：那是工具清单，不是 skill 门禁。）

### 6.4 正文固定小节（按序，便于 diff）

标题层级建议用 `##`。缺一节即不合规。

```markdown
## 定位

一句话：本 skill 是路由 / 总入口，不是操作手册。列出覆盖的子目标类型。

## 索引

| 目标/场景 | 子 skill 名或路径 | 何时选 |
|-----------|-------------------|--------|
| … | … | … |

## 选路

1. …（3–6 条决策）
2. …

## 纪律

- 只打开命中的那**一个**子文档（leaf `Skill` 或模块文件）。
- 跨模块协作是例外时，在具体任务里说明，不要求先读全表。

## 未命中

- listing 里其他 skill 可直接使用，无需经本路由。
- 用户意图落在本域之外时，直接做任务或问一句；禁止引导「必须先打开本路由」。
```

### 6.5 索引表列定义

| 列 | 定义 | 写法 |
|----|------|------|
| 目标/场景 | 用户意图或分析对象 | 短语，如「APK/DEX/JAR 反编译」 |
| 子 skill 名或路径 | 打开方式 | **优先 skill 名**（形态 A）；或 `~/.grok/...` / 项目相对路径（形态 B 模块） |
| 何时选 | 一句话边界 | 与相邻行可区分；冲突时靠选路规则 |

约束：

- 行数 **≤ 8**（超出 → 先拆域或改描述，不要塞表）。
- 路径写法允许：
  - skill 名：`search-routing`（模型用 `Skill` 调用）
  - 用户级绝对：`~/.grok/skills/<name>/SKILL.md`
  - 项目相对：`.grok/skills/<name>/SKILL.md`
  - 模块相对（形态 B）：`modules/<mod>/<mod>.md`（相对路由包根，并说明两种等价根路径时可写一行注释）
- **不要**在表里粘贴子 skill 正文或长教程。

### 6.6 选路规则写法

- 条数 3–6。
- 优先可操作判据：对象在哪一层、静态/动态、是否运行中进程、是否需要真实浏览器等。
- 不要写「凡本域皆先读本路由」。

### 6.7 纪律与未命中

- 纪律：**只打开命中的那一个**——防止路由变成「先读全部模块」的反渐进披露。
- 未命中：必须存在「可直接用 listing 其他 skill / 可不走路由」的明文。

---

## 7. 无白名单（G5 硬句）

> **路由 skill 不拥有对子 skill 的独占控制权。子 skill 保持独立 listing 与独立 `Skill` 调用。任何路由正文、frontmatter 或附录都不得写成「必须先经本路由才能使用子 skill」「仅允许下列 skill」「从 listing 隐藏子 skill」。**

验收时对路由全文搜索：`必须经`、`只能经`、`仅允许`、`白名单`、`allowed-children`、`不得直接`。命中即 **不合规**（`allowed-tools` 与「只打开命中的那一个模块文件」的纪律句除外，后者只约束打开顺序，不取消 leaf 独立性）。

子 skill 的 description / when-to-use **不得**被改成「从属某路由」字段；路由吸收只改：

1. 路由索引 +1 行；
2. 台账 +1 条。

---

## 8. 与 listing 的共存行为（预期，不是缺陷）

| 场景 | 预期 |
|------|------|
| 用户说领域总称 | 可命中路由 → 打开索引 → 读一个子文档 |
| 用户说极具体子任务 | 直接命中 leaf → `Skill` leaf |
| 路由与 leaf 描述重叠 | 先路由再确认，或直达 leaf，均可 |

路由上线后 listing **多一条元数据**，不删除任何 leaf 条目。

---

## 9. 版本与台账引用

- frontmatter：`metadata.spec-version: "1.1"`（本文版本；改规范时 bump，见 Changelog）。既有路由按旧兼容版本写成的，无需立即 bump——1.1 为纯增补、未收紧旧要求。
- 台账条目建议引用：`spec=SKILL_ROUTER_SPEC@1.1`。
- 正文首行可选重复 `<!-- spec: SKILL_ROUTER_SPEC@1.1 -->`，便于纯文本 diff。

---

## 10. 写作检查清单（交付前）

- [ ] description + when-to-use ≤ 900 字节（推荐仍压 ≤ 400B）
- [ ] 含 `metadata.spec-version`
- [ ] 五节齐全：定位 / 索引 / 选路 / 纪律 / 未命中
- [ ] 索引 ≤ 8 行；选路 3–6 条
- [ ] 无 §7 禁词与独占语义
- [ ] 子 skill 仍独立可 `Skill`
- [ ] 台账有对应 create/absorb 记录（或草稿标 pending，未当生产路由用）

---

## 11. 最小模板（复制用）

```markdown
---
name: example-router
description: <领域>总入口：一句话列对象类型与「路由/总入口」性质。
when-to-use: 要做…时；要分析…时；要处理…时。
metadata:
  spec-version: "1.0"
  role: router
---

<!-- spec: SKILL_ROUTER_SPEC@1.0 -->

## 定位

本 skill 是 **example 域路由**，只做选路与打开子文档，不含操作手册。

## 索引

| 目标/场景 | 子 skill 名或路径 | 何时选 |
|-----------|-------------------|--------|
| 场景 A | leaf-a | 对象是… |
| 场景 B | leaf-b | 对象是… |

## 选路

1. 先分清对象在哪一层：… → leaf-a；… → leaf-b。
2. 需要动态/运行时再选 leaf-b；默认静态。
3. 表内无法区分时，直接问用户，不要猜进错误子 skill。

## 纪律

- 只打开命中的那一个子 skill。
- 不要把索引表当成需要一次读完的清单。

## 未命中

- listing 中其他 skill 可直接使用，无需经本路由。
- 不属于本域的意图，直接执行任务或问一句。
```

---

## 12. 合规样例（对照 `re` 的草稿）

**文件（Phase 2 可直接采用或微调）：**  
`GrokZen/docs/skill-router-sample-re.md`

要点相对旧 `re`：

- frontmatter 压到 listing 友好长度（旧版合计 723B；Phase 6 预算 900B 前按 ≤400B 压缩）。
- 小节标题对齐本规范五节。
- 保留形态 B 模块路径；补「未命中」；去掉任何可被读成排他的表述。

---

## 13. 分发（随产品 bundle 提取，Phase 6）

| 文件 | 落盘位置 | 行为 |
|------|----------|------|
| 规范副本 | `~/.grok/SKILL_ROUTER_SPEC.md` | 启动时由二进制内置提取（`xai-grok-shell/src/builtin.rs` → `extract_builtin_files`）：缺失即写入；版本 bump 时刷新。**唯一源仍是 `GrokZen/docs/SKILL_ROUTER_SPEC.md`**，副本不手改。 |
| 台账模板 | `~/.grok/skill-router-ledger.md` | 仅在缺失时创建模板；**已存在则永不覆盖**（机器本地状态）。 |

- 通道与 README / CHANGELOG 相同：随发布包二进制走，安装与升级后首次启动即达，不依赖安装器额外拷贝；`auto-update` 替换二进制后同样生效。
- `skill-curator` 协议指针与 discovery 吸收提示引用的 `~/.grok/skill-router-ledger.md` 即此模板落点；机器上没有 GrokZen 仓库时，规范读提取副本。
- 台账的跨机同步不在本通道内：台账是每机状态，分发的是模板。

---

## 附录 A — 形态 B：junction 挂载与 listing 关系（Phase 2 补）

| 事实 | 行为 |
|------|------|
| 目录 junction（`<project>/.grok/skills/<domain>` → `~/.grok/skill-store/<domain>`） | 整包挂载；改 store 内 `SKILL.md` 对所有已挂项目即时生效，不断链 |
| 模块文件 `modules/<mod>/<mod>.md` | 路径可读（两种等价根）；**默认无独立 listing 条目**（不在 `.grok/skills/<name>/SKILL.md` 位置） |
| 未 junction 的项目 | 不出现该路由条目，也不出现其模块 |
| 子目标是独立 leaf skill（形态 A） | 各自保留在 listing；路由索引用 skill 名指向 |

本机挂载工具：`skilllink`（`~/.grok/skill-store` 为唯一集合源）。回滚修订：store 侧备份在 `skill-store/.attic/`。

---

## Changelog

| 版本 | 日期 | 变更 |
|------|------|------|
| 1.0 | 2026-10-24 | 初版：冻结形态 A 为新路由默认；400B listing 预算；无白名单硬句；§5 反例阈值；`re` 合规样例路径 |
| 1.0（补） | 2026-10-24 | Phase 2：附录 A（junction/`modules` 与 listing 关系、`.attic` 回滚位置） |
| 1.1 | 2026-10-24 | Phase 3：§5.3 建路由判定程序与输出块；§5.4 吸收判定矩阵；元 skill `skill-curator` 与台账 `~/.grok/skill-router-ledger.md` 落地 |
| 1.2 | 2026-10-24 | Phase 6：listing 单条预算 400→900（`MAX_LISTING_COMBINED_BYTES`）；§6.2/检查清单同步；路由仍建议 ≤400B |
| 1.3 | 2026-10-24 | Phase 6：§13 分发——规范副本与台账模板随二进制启动提取；台账仅缺失时创建、永不覆盖 |
