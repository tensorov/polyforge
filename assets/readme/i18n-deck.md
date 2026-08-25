# PolyForge README i18n copy deck

Source of truth: `.omo/plans/readme-i18n-redesign.md`, section "COPY DECK".
Locales: EN / RU / DE / ZH. Mono strings (commands, entry kinds, state names) stay literal in every locale.

## Hero

Geometry = current `hero.svg` skeleton, height 520, same IDs.

| Element        | Geometry                  | EN                                  | RU                                        | DE                                      | ZH                              |
| -------------- | ------------------------- | ----------------------------------- | ----------------------------------------- | --------------------------------------- | ------------------------------- |
| `promise-text` | x48 y196 fs28 sans #9B9690 | Make every AI claim provable        | Превращайте заявления ИИ в факты          | Mach jede KI-Aussage beweisbar          | 让 AI 声明变成可验证的事实      |
| `chain-caption`| x48 y384 fs20 mono #9B9690 | each append = a linked node         | каждая запись - звено цепи                | jeder Eintrag = ein Kettenglied         | 每条记录都链向前一条            |

`<desc>` per locale: translate the EN desc sentence, keep command names literal.

### Chips note

Chip-4 becomes `tui` (rect w=56) replacing `pf-cli` (w=88); keep chips 1-3 identical (`rust` / `merkle-chain` / `mcp`). Terminal block unchanged.

## Story card

NEW, 1200x360, rx12 bg #0a0a0a. Header y~72 fs38 sans bold #EDEBE0, x56.
Panels y110..320, each ~344w gap 24 starting x56 (cards fill #16130f, hairline stroke, rx8): step number mono #FF6A3D fs18 ("01/02/03"), title fs26 sans bold #EDEBE0, one line fs18 sans #9B9690, mono chip bottom (fs16 #E8A75B on #0a0a0a inset rect). Arrows between panels: simple path #9B9690 with arrowhead.

### Header

| EN                                                | RU                                            | DE                                              | ZH                                    |
| ------------------------------------------------- | --------------------------------------------- | ----------------------------------------------- | ------------------------------------- |
| AI agents say "done." PolyForge makes it provable. | ИИ говорит «готово». PolyForge делает это доказуемым. | KI-Agenten sagen „fertig". PolyForge macht es beweisbar. | AI 智能体说"完成了"。PolyForge 让它可证明。 |

### Step 1 - CLAIM

| Field | EN                          | RU                                     | DE                                   | ZH                    |
| ----- | --------------------------- | -------------------------------------- | ------------------------------------ | --------------------- |
| title | CLAIM                       | ЗАЯВЛЕНИЕ                              | ANSPRUCH                             | 声明                  |
| line  | The agent records what it did | Агент записывает, что сделал         | Der Agent protokolliert seine Arbeit | 智能体记录它做了什么  |
| mono  | `model_claim`               | `model_claim`                          | `model_claim`                        | `model_claim`         |

### Step 2 - PROVE

| Field | EN                            | RU                                          | DE                                       | ZH                          |
| ----- | ----------------------------- | ------------------------------------------- | ---------------------------------------- | --------------------------- |
| title | PROVE                         | ДОКАЗАТЕЛЬСТВО                              | NACHWEIS                                 | 证明                        |
| line  | A real tool run must confirm it | Подтвердить может только запуск инструмента | Nur ein echter Tool-Lauf bestätigt es  | 必须由真实的工具运行确认    |
| mono  | `tool_attestation`            | `tool_attestation`                          | `tool_attestation`                       | `tool_attestation`          |

### Step 3 - GATE

| Field | EN                             | RU                                            | DE                     | ZH                   |
| ----- | ------------------------------ | --------------------------------------------- | ---------------------- | -------------------- |
| title | GATE                           | ГЕЙТ                                          | GATE                   | 门禁                 |
| line  | Merge passes only with proof   | Слияние проходит только с доказательством     | Merge nur mit Nachweis | 链上有证据才能合并   |
| mono  | `gate PASSED` (amber #E8A75B)  | `gate PASSED` (amber #E8A75B)                 | `gate PASSED` (amber #E8A75B) | `gate PASSED` (amber #E8A75B) |

ZH line-length guard: CJK fs18 => max ~17 chars/line in 300px card; above strings fit.

## Glossary (locale-invariant)

State names and entry-kind labels are never translated; they render as-is in every locale.

| Term                | Type       | Rendering rule                       |
| ------------------- | ---------- | ------------------------------------ |
| `ModelClaimed`      | state name | literal, all locales                 |
| `Verified`          | state name | literal, all locales                 |
| `Validated`         | state name | literal, all locales                 |
| `Refuted`           | state name | literal, all locales                 |
| `model_claim`       | entry kind | literal mono, all locales            |
| `tool_attestation`  | entry kind | literal mono, all locales            |
| `validation`        | entry kind | literal mono, all locales            |
| `discrepancy`       | entry kind | literal mono, all locales            |
| `eval_attestation`  | entry kind | literal mono, all locales            |
| `gate PASSED`       | CLI output | literal mono amber #E8A75B           |

Step-title glossary across locales:

| Step | EN             | RU              | DE        | ZH     |
| ---- | -------------- | --------------- | --------- | ------ |
| s1   | CLAIM          | ЗАЯВЛЕНИЕ       | ANSPRUCH  | 声明   |
| s2   | PROVE          | ДОКАЗАТЕЛЬСТВО  | NACHWEIS  | 证明   |
| s3   | GATE           | ГЕЙТ            | GATE      | 门禁   |

## lifecycle.svg (single, locale-neutral)

Keep states/arrows exactly ModelClaimed -> Verified -> Validated (+Refuted side node grey). Only state names + entry kind labels allowed; explanations live in Markdown below the image in each README.

## cli-demo.svg (single, locale-neutral)

Real quickstart transcript (init -> model_claim -> tool_attestation -> gate PASSED), verbatim outputs, same terminal chrome grammar as hero. No prose.
