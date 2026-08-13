---
id: 001
status: resolved
requires: []
required: true
acceptance: |
  docs/MAP-CONTRACT.md 存在，且含六個必要段落：檔案、ticket frontmatter、
  完成式、寫入權責、啟用語意、worktree 規則。本檔 frontmatter 六欄
  （id/status/requires/required/acceptance/evidence）齊備且 status 為合法 enum 值。
  機器判定：
    test -f docs/MAP-CONTRACT.md
    rg -c '^## ' docs/MAP-CONTRACT.md   # ≥ 6
evidence: |
  2026-08-13 核對：docs/MAP-CONTRACT.md 建立，六個必要段落齊備；實際 H2
  段落 9 個（檔案、ticket frontmatter、完成式、寫入權責、啟用語意、worktree
  規則、close validator、MAP.md 骨架、紀律）。`rg -c '^## '` 回報 11——差額
  來自骨架範本 code fence 內的兩行，此判準會把 fenced 內容計入，步驟 5 寫
  validator 時需改用 fence-aware 解析。本 ticket 即 schema 的第一個實例。
---

# 001 — 凍結 MAP 資料契約

## 背景

「harness 管線修正案 v2」落地順序步驟 1：先凍結資料契約並手造一張 fixture
ticket，**不加任何 hook**。目的是讓 schema 在被 hook／validator 依賴之前，先
被一份真實文件用過一次——確認欄位表達得出真實狀態，而不是只在計畫裡好看。

權威文本：vault `.handoff/pipeline-plan-v2-converged.md`（經 codex 三輪收斂，
CONVERGED）。

## 做法

1. `docs/MAP-CONTRACT.md` 落成一頁契約（v2 §1 全文條目化）。
2. 本檔作為 fixture，self-hosting 驗證 frontmatter schema。
3. **不建立 `docs/MAP.md` 本體**：v2 §1 的啟用語意是「MAP.md 存在 →
   SessionStart 提示」，在 hook 未實作前放檔案會造成半啟用狀態。MAP.md 本體
   留給步驟 4（真實 slice 手動跑）。

## 後續（不在本 ticket 範圍）

- 步驟 2：refine-prompt 接 sender contract（chezmoi 受管，需使用者確認才動工）
- 步驟 3：sharpen-plan 產 acceptance 草稿，人工灌進 fixture
- 步驟 4：挑真實 slice 手動跑 MAP 流程，此時才建 `docs/MAP.md`
- 步驟 5：證實省事後才加 SessionStart／handoff 接點＋close validator
