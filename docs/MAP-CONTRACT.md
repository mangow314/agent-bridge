# MAP 資料契約（MAP contract）

（2026-08-13 立案。動機：跨 session 的 epic 目前靠 handoff 鏈接續，重建狀態
要讀最新 plan＋多份較舊 handoff 才能唯一決定 next action；MAP 把「專案史」從
交接檔抽出成單一投影，交接檔只留 session delta。本檔為凍結版契約，先於任何
hook 落地——目前**沒有任何 hook 讀寫這些檔案**，全流程手動。）

## 檔案

| 檔案 | 位置 | 份數 | 承載 |
|---|---|---|---|
| MAP | git root `docs/MAP.md` | 每 repo 至多一份 | goal gist、ticket 索引投影、fog 段、摘要計數 |
| ticket | `docs/tickets/NNN-<slug>.md` | 每張一檔 | frontmatter＋內文（背景、做法、驗收細節） |

MAP **禁止承載** ticket 內文與即時 WIP——索引投影只取 `id` / `status` /
`requires` 加一句 gist。投影欄位名與 ticket frontmatter 統一用 `requires`，
不造 `deps` 之類 alias。

## ticket frontmatter

| 欄位 | 型態 | 說明 |
|---|---|---|
| `id` | `NNN` | 與檔名前綴一致，全 repo 唯一 |
| `status` | enum | `open` / `claimed` / `blocked` / `resolved` / `waived` / `superseded` |
| `requires` | list of id | 空 list 表無前置；終態＝`resolved`／`waived`／`superseded` |
| `required` | bool | 是否計入完成式；`false` 表 nice-to-have |
| `acceptance` | text | 可機器判定的驗收條件（指令、斷言、或可獨立查核的數字）；無客觀 gate 者標 `human judgment` 並附理由 |
| `evidence` | text | 收案時填入的實際證據（輸出、commit、數字）；未收案填 `-` |

## 完成式

```
done ≡ required ticket 全終態 AND fog 清空或明示 waived AND 專案級 final gates 通過
```

`frontier 空且 required 未終態` ＝ **stalled / blocked，不是 done**。這條是
本契約最容易被誤讀的一行：沒有可認領的 ticket 不等於做完了。

## 寫入權責

- coordinator session 是 **ticket 與 MAP 的唯一 writer**。
- worker **只回報、不寫 ticket/MAP**，由 coordinator 統一更新兩者。
- coordinator 派發 ticket 時必須指派**互斥的檔案寫入 scope**；scope 重疊的
  ticket 一律**序列化**（bridge 不鎖檔，此為既有 sender contract 人工序列化
  慣例的延伸）。

## 啟用語意

- `docs/MAP.md` 存在時，SessionStart 只**提示**（pointer＋active/inactive）。
- 唯 brief／handoff 明示 `workflow: map`，或給出 active ticket 指標時，才強制
  三規則：
  - coordinator：讀 MAP → 指派或認領一張 → 收尾更新 ticket＋MAP。
  - worker：讀指派的 ticket → 完成後回報 coordinator。
- epic 收案後的單 session 小修走輕量流程，不強制。

## worktree 規則

MAP 只在 **main worktree** 維護。feature worktree 的 session 讀 main 的 MAP、
**不寫**，產出回報 coordinator。（本 repo 現有 `feat/tui-lineage-view`
worktree，是這條規則的第一個實例。）

## close validator（尚未實作，步驟 5）

ticket 轉終態時應檢查：dangling dep、cycle、重複 id、`required: true` 卻缺
`acceptance` 或 `evidence`。目前這四項靠人工核對。

## MAP.md 骨架（範本，本 repo 尚未建立本體）

```markdown
# MAP — <goal gist 一句話>

status: active
counts: open N / claimed N / blocked N / 終態 N

## Tickets

| id | status | requires | gist |
|---|---|---|---|
| 001 | open | - | <一句話> |

## Not yet specified（fog）

- <已知還沒切成 ticket 的區塊>
```

## 紀律

- 契約凍結後才動其他步驟；改契約要回到 plan 層級重議，不在落地途中順手改。
- 三檔（MAP／plan／handoff）分流不是 repo 永久模式，是 activation suggestion；
  升級判準見 vault 的管線修正案 v2 §5。
- handoff 不因 MAP 而瘦身欄位：MAP 只取代「重述完整專案史」的部分，session
  delta（未提交 diff、驗證缺口、授權契約）全部保留。
